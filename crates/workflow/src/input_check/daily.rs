//! DailyDataset adapter. Keeps the research executor's annual seed and normalization.
use super::*;
use analytics_core::{Date, pool::PoolMembership};
use std::collections::{BTreeMap, BTreeSet};
/// Required raw observations by security and field, separately from output membership.
pub type RequiredInputs = BTreeMap<String, BTreeMap<String, BTreeSet<usize>>>;
/// Expand actual phase intervals, derived inputs and membership into field-specific reads.
pub fn requirements(
    plan: &InputCheckPlan,
    calendar: &[Date],
    pool: &PoolMembership,
    benchmark: &str,
) -> RequiredInputs {
    let mut needs = RequiredInputs::new();
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
                            for (other, rs) in &pool.intervals {
                                for r in rs {
                                    let a =
                                        context_lo.max(calendar.partition_point(|d| *d < r.start));
                                    let b =
                                        context_hi.min(calendar.partition_point(|d| *d <= r.end));
                                    if a >= b {
                                        continue;
                                    }
                                    let a = if window.recursive {
                                        seed
                                    } else {
                                        a.saturating_sub(
                                            window.last.saturating_sub(window.context_last),
                                        )
                                    };
                                    let b = b.saturating_sub(
                                        window.first.saturating_sub(window.context_first),
                                    );
                                    insert(&mut needs, other, input, a, b);
                                }
                            }
                        } else {
                            insert(
                                &mut needs,
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
                        insert(&mut needs, id, field, lo, hi);
                    }
                }
                if matches!(phase.phase.as_str(), "train" | "validation") {
                    // Labels are next-open / following-open, clipped to this split.
                    insert(
                        &mut needs,
                        id,
                        "open",
                        lo.saturating_add(1).min(phase_hi),
                        hi.saturating_add(2).min(phase_hi),
                    );
                }
            }
        }
    }
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
fn insert(needs: &mut RequiredInputs, id: &str, input: &str, lo: usize, hi: usize) {
    for (field, extra) in raw_inputs(input) {
        let dates = needs
            .entry(id.into())
            .or_default()
            .entry(field.into())
            .or_default();
        dates.extend(lo.saturating_sub(extra)..hi.saturating_sub(extra));
    }
}
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
