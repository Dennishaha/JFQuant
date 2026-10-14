//! Dated input qualification precedes factor calculation; sample masks follow it.
use super::*;
use analytics_core::pool::PoolMembership;
use analytics_core::research_quality::{
    ProblemDisposition as Handling, ProblemKind, QualityDecision, ResearchQualityPolicy,
    ResearchStrictness,
};
use analytics_storage::{daily::DailyDataset, qlib::DailyField};
use serde::{Deserialize, Serialize};

/// Frozen source pool and its date-effective qualification; originals are never modified.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QualitySnapshot {
    /// Qualification artifact format 1.
    pub schema_version: u32,
    /// Actual policy.
    pub policy: ResearchQualityPolicy,
    /// Original membership, before qualification.
    pub original_pool: PoolMembership,
    /// Pool used by the factor and execution workers.
    pub effective_pool: PoolMembership,
    /// Failed source checks and handling, compressed into consecutive calendar intervals.
    pub decisions: Vec<QualityDecision>,
    /// Every calendar day's original and retained candidate count.
    pub daily_counts: Vec<serde_json::Value>,
}

fn append(out: &mut Vec<QualityDecision>, mut decision: QualityDecision, calendar: &[Date]) {
    if let Some(previous) = out.last_mut()
        && previous.security == decision.security
        && previous.code == decision.code
        && previous.disposition == decision.disposition
        && previous.message == decision.message
        && calendar
            .binary_search(&previous.coverage.end)
            .ok()
            .zip(calendar.binary_search(&decision.coverage.start).ok())
            .is_some_and(|(a, b)| a + 1 == b)
    {
        previous.coverage.end = decision.coverage.end;
        return;
    }
    decision.evidence.sort();
    out.push(decision);
}

fn affected_inputs(fields: &[&str], ids: &[String]) -> Vec<String> {
    let mut inputs = BTreeSet::new();
    for field in fields {
        for input in match *field {
            "open" => vec!["open", "dtm", "dbm"],
            "high" => vec!["high", "hd", "tr", "dtm"],
            "low" => vec!["low", "ld", "tr", "dbm"],
            "close" => vec!["close", "ret", "tr"],
            "volume" => vec!["volume", "vwap"],
            "amount" => vec!["amount", "vwap"],
            "vwap" => vec!["vwap"],
            _ => return ids.to_vec(),
        } {
            inputs.insert(input);
        }
    }
    let registry = FactorRegistry::default();
    ids.iter()
        .filter(|id| {
            registry
                .get(id)
                .is_some_and(|s| s.input_columns.iter().any(|c| inputs.contains(c.as_str())))
        })
        .cloned()
        .collect()
}

fn first_event_gap(
    book: &analytics_storage::corporate_actions::ActionBundle,
    id: &str,
    start: Date,
    end: Date,
) -> Option<Date> {
    if !book.queried_symbols.contains(id) {
        return Some(start);
    }
    analytics_storage::corporate_actions::EVENT_CATEGORIES
        .iter()
        .filter_map(|category| {
            let mut ranges: Vec<_> = book
                .coverage
                .get(id)
                .into_iter()
                .flatten()
                .filter(|c| {
                    c.category == *category
                        && c.status == analytics_core::admission::CheckStatus::Passed
                        && !c.evidence.is_empty()
                        && !c.reason.is_empty()
                })
                .map(|c| &c.dates)
                .collect();
            ranges.sort_by_key(|r| r.start);
            let mut next = start;
            for range in ranges {
                if range.end < next {
                    continue;
                }
                if range.start > next {
                    return Some(next);
                }
                if range.end >= end {
                    return None;
                }
                next = range.end.succ_opt()?;
            }
            (next <= end).then_some(next)
        })
        .min()
}

/// Generate qualification with each observation's own date; later missing values
/// cannot remove an earlier candidate. Integrity failures remain globally blocking.
pub fn qualify(
    dataset: &DailyDataset,
    pool: &PoolMembership,
    registry: Option<&analytics_core::security::SecurityRegistry>,
    policy: &ResearchQualityPolicy,
    range: &DateRange,
    factor_ids: &[String],
    book: Option<&analytics_storage::corporate_actions::ActionBundle>,
) -> Result<QualitySnapshot> {
    policy.validate()?;
    let mut effective = pool.clone();
    let mut decisions = vec![];
    let fields = [
        "open", "high", "low", "close", "volume", "amount", "vwap", "factor",
    ];
    if policy.strictness != ResearchStrictness::Strict {
        for (id, windows) in &pool.intervals {
            analytics_core::task::TaskContext::current().check()?;
            let evidence = vec![format!("market:{}", dataset.root.display())];
            let known = registry.is_some_and(|r| r.securities.contains_key(id));
            let mut data = BTreeMap::new();
            let mut absent = vec![];
            let mut damage = vec![];
            if known {
                for field in fields {
                    match dataset.research_field(id, field, true) {
                        Ok(values) => {
                            data.insert(field, values);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                            absent.push(field);
                            data.insert(
                                field,
                                DailyField {
                                    start: 0,
                                    values: vec![None; dataset.calendar.len()],
                                },
                            );
                        }
                        Err(e) => {
                            damage.push(format!("{field}: {e}"));
                        }
                    }
                }
            }
            let mut retained = vec![];
            let coverage_start = registry
                .and_then(|r| r.securities.get(id))
                .map_or(range.start, |r| range.start.max(r.lifetime.start));
            let event_gap = book.and_then(|b| first_event_gap(b, id, coverage_start, range.end));
            for (day, date) in dataset
                .calendar
                .iter()
                .enumerate()
                .filter(|(_, d)| **d >= range.start && **d <= range.end)
            {
                if !windows.iter().any(|w| w.start <= *date && *date <= w.end) {
                    continue;
                }
                let historical_problem =
                    if known && policy.strictness == ResearchStrictness::QualifiedOnly {
                        book.and_then(|book| {
                            if book.failures.contains_key(id) {
                                return Some((
                                    "action_source_unresolved",
                                    ProblemKind::Conflict,
                                    "公司行动来源失败或冲突尚未裁定".to_string(),
                                    true,
                                ));
                            }
                            if event_gap.is_some_and(|gap| gap <= *date) {
                                Some((
                                "event_category_coverage_missing",
                                ProblemKind::Missing,
                                "截至当日的公司行动类别覆盖不足；详见所选事件版本的coverage证据"
                                    .to_string(),
                                false,
                            ))
                            } else {
                                None
                            }
                        })
                    } else {
                        None
                    };
                let mut affected = factor_ids.to_vec();
                let problem = if !known {
                    Some((
                        "identity_missing",
                        ProblemKind::Missing,
                        "证券身份未解决".to_string(),
                        false,
                    ))
                } else if !damage.is_empty() {
                    Some((
                        "input_damaged",
                        ProblemKind::Damaged,
                        damage.join("; "),
                        true,
                    ))
                } else if historical_problem.is_some() {
                    historical_problem
                } else if !absent.is_empty() {
                    affected = affected_inputs(&absent, factor_ids);
                    Some((
                        "input_file_missing",
                        ProblemKind::Missing,
                        format!("缺少输入文件：{}", absent.join(",")),
                        false,
                    ))
                } else {
                    let missing: Vec<_> = fields
                        .iter()
                        .filter(|name| {
                            let value = data[**name].get(day);
                            if **name == "vwap" && data["volume"].get(day) == Some(0.) {
                                return false;
                            }
                            value.is_none()
                        })
                        .copied()
                        .collect();
                    affected = affected_inputs(&missing, factor_ids);
                    (!missing.is_empty()).then(|| {
                        (
                            "input_observation_missing",
                            ProblemKind::Missing,
                            format!("当日缺少输入：{}", missing.join(",")),
                            false,
                        )
                    })
                };
                if let Some((code, kind, message, blocking)) = problem {
                    let disposition = if blocking || (!known && policy.allows_missing()) {
                        Handling::Block
                    } else if policy.allows_missing() {
                        Handling::KeepNull
                    } else {
                        Handling::ExcludeSecurity
                    };
                    append(
                        &mut decisions,
                        QualityDecision {
                            security: id.clone(),
                            coverage: DateRange {
                                start: *date,
                                end: *date,
                            },
                            code: code.into(),
                            kind,
                            disposition,
                            message,
                            evidence: evidence.clone(),
                            affected_factors: affected,
                        },
                        &dataset.calendar,
                    );
                    if policy.allows_missing() {
                        retained.push(*date);
                    }
                } else {
                    retained.push(*date);
                }
            }
            if policy.strictness == ResearchStrictness::QualifiedOnly {
                let mut ranges: Vec<DateRange> = vec![];
                for date in retained {
                    if let Some(last) = ranges.last_mut()
                        && dataset
                            .calendar
                            .binary_search(&last.end)
                            .ok()
                            .zip(dataset.calendar.binary_search(&date).ok())
                            .is_some_and(|(a, b)| a + 1 == b)
                    {
                        last.end = date;
                    } else {
                        ranges.push(DateRange {
                            start: date,
                            end: date,
                        });
                    }
                }
                if ranges.is_empty() {
                    effective.intervals.remove(id);
                } else {
                    effective.intervals.insert(id.clone(), ranges);
                }
            }
        }
    }
    let daily_counts=dataset.calendar.iter().filter(|d| **d>=range.start && **d<=range.end).map(|date|serde_json::json!({"date":date,"original":pool.on(*date).len(),"retained":effective.on(*date).len()})).collect();
    Ok(QualitySnapshot {
        schema_version: 1,
        policy: policy.clone(),
        original_pool: pool.clone(),
        effective_pool: effective,
        decisions,
        daily_counts,
    })
}

/// Conservatively prove quotes for every security that could be held, before fitting.
/// Later missing data cannot retroactively remove its earlier buy eligibility.
pub(super) fn possible_holding_issues(
    dataset: &DailyDataset,
    pool: &PoolMembership,
    book: Option<&analytics_storage::corporate_actions::ActionBundle>,
    start: Date,
    end: Date,
) -> Result<BTreeMap<String, String>> {
    let mut issues = BTreeMap::new();
    let lo = dataset.calendar.partition_point(|d| *d < start);
    let hi = dataset.calendar.partition_point(|d| *d <= end);
    for (id, ranges) in &pool.intervals {
        analytics_core::task::TaskContext::current().check()?;
        if !ranges.iter().any(|r| r.start <= end && r.end >= start) {
            continue;
        }
        let inputs = dataset.research_raw_field(id, "open", true).and_then(|o| {
            dataset
                .research_raw_field(id, "close", true)
                .and_then(|c| dataset.research_raw_volume(id, true).map(|v| (o, c, v)))
        });
        match inputs {
            Err(e) => {
                issues.insert(id.clone(), format!("执行输入不可读取：{e}"));
            }
            Ok((open, close, volume)) => {
                let first = (lo..hi).find(|day| {
                    ranges.iter().any(|r| {
                        r.start <= dataset.calendar[*day] && dataset.calendar[*day] <= r.end
                    }) && open.get(*day).is_some_and(|p| p > 0.)
                        && volume.get(*day).is_some_and(|v| v > 0.)
                });
                if let Some(first) = first {
                    for day in first..hi {
                        if (close.get(day).is_some() && volume.get(day).is_some())
                            || volume.get(day) == Some(0.)
                        {
                            continue;
                        }
                        let date = dataset.calendar[day];
                        let documented = evidenced_nonquote(book, id, date);
                        if (close.get(day).is_none() || volume.get(day).is_none())
                            && volume.get(day) != Some(0.)
                            && !documented
                        {
                            issues.insert(id.clone(),format!("{date}: 此前可能已持有，缺少收盘价/成交量、已核验停牌或权益估值；不能倒删早期股票"));
                            break;
                        }
                    }
                }
            }
        }
    }
    Ok(issues)
}

/// A disappeared quote is valid only for an evidenced transfer or dated non-trading claim.
pub(super) fn evidenced_nonquote(
    book: Option<&analytics_storage::corporate_actions::ActionBundle>,
    id: &str,
    date: Date,
) -> bool {
    book.is_some_and(|b| {
        b.actions.iter().any(|a| {
            a.instrument == id
                && a.ex_date <= date
                && match &a.terms {
                    analytics_core::actions::EventTerms::Conversion {
                        successor: Some(next),
                        ordinary_holder: true,
                        ..
                    } => next != id,
                    analytics_core::actions::EventTerms::Delisting { valuations, .. } => valuations
                        .iter()
                        .any(|v| v.date <= date && date <= v.valid_until),
                    _ => false,
                }
        })
    })
}
