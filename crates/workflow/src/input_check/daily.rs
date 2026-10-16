//! DailyDataset adapter. Keeps the research executor's annual seed and normalization.
use super::*;
use analytics_core::{Date, pool::PoolMembership};
use std::collections::{BTreeMap, BTreeSet};
/// Required raw observations by security and field, separately from output membership.
pub type RequiredInputs = BTreeMap<String, BTreeMap<String, BTreeSet<usize>>>;
type Intervals = Vec<std::ops::Range<usize>>;
type PendingInputs = BTreeMap<String, BTreeMap<String, Intervals>>;
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct CrossContext {
    input: String,
    seed: Option<usize>,
    before: usize,
    after: usize,
}
/// Expand actual phase intervals, derived inputs and membership into field-specific reads.
pub fn requirements(
    plan: &InputCheckPlan,
    calendar: &[Date],
    pool: &PoolMembership,
    benchmark: &str,
) -> RequiredInputs {
    let mut pending = PendingInputs::new();
    let mut cross = BTreeMap::<CrossContext, Intervals>::new();
    let dependencies = plan.input_dependencies();
    for phase in &plan.phases {
        let phase_lo = calendar.partition_point(|d| *d < phase.effective.start);
        let phase_hi = calendar.partition_point(|d| *d <= phase.effective.end);
        for (id, memberships) in &pool.intervals {
            for membership in memberships {
                let lo = phase_lo.max(calendar.partition_point(|d| *d < membership.start));
                let hi = phase_hi.min(calendar.partition_point(|d| *d <= membership.end));
                if lo >= hi {
                    continue;
                }
                for (input, windows) in &dependencies {
                    let benchmark_input = input.starts_with("banchmarkindex");
                    let input = match input.as_str() {
                        "banchmarkindexclose" => "close",
                        "banchmarkindexopen" => "open",
                        v => v,
                    };
                    for window in windows {
                        let seed = if plan.context["seed"] == "parquet-full-history" {
                            0
                        } else {
                            calendar
                                .partition_point(|d| d.year() < calendar[lo].year())
                                .saturating_sub(756.max(window.last + 1))
                        };
                        let read_lo = if window.recursive {
                            seed
                        } else {
                            lo.saturating_sub(window.last)
                        };
                        let read_hi = hi.saturating_sub(window.first);
                        if read_lo >= read_hi {
                            continue;
                        }
                        if window.cross_sectional && !benchmark_input {
                            let context_lo = lo.saturating_sub(window.context_last);
                            let context_hi = hi.saturating_sub(window.context_first);
                            let key = CrossContext {
                                input: input.into(),
                                seed: window.recursive.then_some(seed),
                                before: window.last.saturating_sub(window.context_last),
                                after: window.first.saturating_sub(window.context_first),
                            };
                            union(cross.entry(key).or_default(), context_lo, context_hi);
                        } else {
                            insert(
                                &mut pending,
                                if benchmark_input { benchmark } else { id },
                                input,
                                read_lo,
                                read_hi,
                            );
                        }
                    }
                }
                if plan.context["seed"] != "parquet-full-history" {
                    for field in ["close", "factor"] {
                        insert(&mut pending, id, field, lo, hi);
                    }
                }
                if matches!(phase.phase.as_str(), "train" | "validation") {
                    // Labels are next-open / following-open, clipped to this split.
                    insert(
                        &mut pending,
                        id,
                        "open",
                        lo.saturating_add(1).min(phase_hi),
                        hi.saturating_add(2).min(phase_hi),
                    );
                }
            }
        }
    }
    // Expand each union of identical cross-sectional contexts once, rather than
    // once per output security. Recursive seeds remain separate contexts.
    for (context, intervals) in cross {
        for (other, memberships) in &pool.intervals {
            for r in memberships {
                let member_lo = calendar.partition_point(|d| *d < r.start);
                let member_hi = calendar.partition_point(|d| *d <= r.end);
                for interval in &intervals {
                    let lo = interval.start.max(member_lo);
                    let hi = interval.end.min(member_hi);
                    if lo < hi {
                        insert(
                            &mut pending,
                            other,
                            &context.input,
                            context
                                .seed
                                .unwrap_or_else(|| lo.saturating_sub(context.before)),
                            hi.saturating_sub(context.after),
                        );
                    }
                }
            }
        }
    }
    let mut needs: RequiredInputs = pending
        .into_iter()
        .map(|(id, fields)| {
            (
                id,
                fields
                    .into_iter()
                    .map(|(field, ranges)| (field, ranges.into_iter().flatten().collect()))
                    .collect(),
            )
        })
        .collect();
    // Normalization and panel-row admission are real dependencies on history too.
    for fields in needs
        .values_mut()
        .filter(|_| plan.context["seed"] != "parquet-full-history")
    {
        let dates: BTreeSet<_> = fields.values().flatten().copied().collect();
        for field in ["close", "factor"] {
            fields.entry(field.into()).or_default().extend(&dates);
        }
    }
    needs
}
fn insert(needs: &mut PendingInputs, id: &str, input: &str, lo: usize, hi: usize) {
    for (field, extra) in raw_inputs(input) {
        let dates = needs
            .entry(id.into())
            .or_default()
            .entry(field.into())
            .or_default();
        union(dates, lo.saturating_sub(extra), hi.saturating_sub(extra));
    }
}
fn union(ranges: &mut Intervals, mut lo: usize, mut hi: usize) {
    if lo >= hi {
        return;
    }
    let first = ranges.partition_point(|r| r.end < lo);
    let mut end = first;
    while end < ranges.len() && ranges[end].start <= hi {
        lo = lo.min(ranges[end].start);
        hi = hi.max(ranges[end].end);
        end += 1;
    }
    ranges.splice(first..end, std::iter::once(lo..hi));
}
#[cfg(test)]
mod tests;
use chrono::Datelike;
/// Freeze each actual fold phase separately; no gap between phases is a task range.
pub fn phases(
    folds: &[analytics_core::schedule::ResolvedFold],
    predicting: bool,
    train_only: bool,
) -> Vec<PhaseRange> {
    let mut phases = Vec::new();
    for f in folds {
        if !predicting {
            phases.push(PhaseRange {
                phase: "train".into(),
                requested: f.requested.train.clone(),
                effective: f.actual.train.clone(),
            });
            if let (Some(requested), Some(effective)) =
                (&f.requested.validation, &f.actual.validation)
            {
                phases.push(PhaseRange {
                    phase: "validation".into(),
                    requested: requested.clone(),
                    effective: effective.clone(),
                });
            }
        }
        if !train_only {
            phases.push(PhaseRange {
                phase: "prediction".into(),
                requested: f.requested.test.clone(),
                effective: f.actual.test.clone(),
            });
        }
    }
    phases
}
