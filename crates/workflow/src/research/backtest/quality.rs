//! Dated input qualification precedes factor calculation; sample masks follow it.
use super::*;
use analytics_core::pool::PoolMembership;
use analytics_core::research_quality::{
    ProblemDisposition as Handling, ProblemKind, QualityDecision, ResearchQualityPolicy,
    ResearchStrictness,
};
use analytics_storage::daily::DailyDataset;
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

/// Batch qualification: fields and factor dependencies are loaded once per task/security.
pub fn qualify(
    dataset: &DailyDataset,
    pool: &PoolMembership,
    policy: &ResearchQualityPolicy,
    range: &DateRange,
    factor_ids: &[String],
) -> Result<QualitySnapshot> {
    qualify_with_registry(
        dataset,
        pool,
        policy,
        range,
        factor_ids,
        &FactorRegistry::default(),
    )
}

pub(super) fn qualify_with_registry(
    dataset: &DailyDataset,
    pool: &PoolMembership,
    policy: &ResearchQualityPolicy,
    range: &DateRange,
    factor_ids: &[String],
    registry: &FactorRegistry,
) -> Result<QualitySnapshot> {
    policy.validate()?;
    let dependencies: BTreeMap<_, _> = factor_ids
        .iter()
        .filter_map(|id| {
            registry
                .get(id)
                .map(|s| (id.clone(), s.input_columns.clone()))
        })
        .collect();
    let mut affected_cache = BTreeMap::<u16, Vec<String>>::new();
    let mut effective = pool.clone();
    let mut decisions = vec![];
    let fields = [
        "open", "high", "low", "close", "volume", "amount", "vwap", "factor",
    ];
    let lo = dataset.calendar.partition_point(|d| *d < range.start);
    let hi = dataset.calendar.partition_point(|d| *d <= range.end);
    for (id, windows) in &pool.intervals {
        analytics_core::task::TaskContext::current().check()?;
        let data: Vec<_> = fields
            .iter()
            .map(|f| dataset.research_field(id, f, policy.strictness != ResearchStrictness::Strict))
            .collect();
        let damage: Vec<_> = data
            .iter()
            .filter_map(|r| {
                r.as_ref()
                    .err()
                    .filter(|e| e.kind() != std::io::ErrorKind::NotFound)
                    .map(ToString::to_string)
            })
            .collect();
        let mut retained = vec![];
        for day in lo..hi {
            if day % 256 == 0 {
                analytics_core::task::TaskContext::current().check()?;
            }
            let date = dataset.calendar[day];
            if !windows.iter().any(|r| r.contains(date)) {
                continue;
            }
            let mut missing = 0u16;
            for (i, r) in data.iter().enumerate() {
                if fields[i] == "vwap" && data[4].as_ref().ok().and_then(|f| f.get(day)) == Some(0.)
                {
                    continue;
                }
                if r.as_ref().ok().and_then(|f| f.get(day)).is_none() {
                    missing |= 1 << i;
                }
            }
            let source_problem = dataset
                .source_map()
                .exceptions
                .get(id)
                .is_some_and(|e| !e.coverage.contains(date));
            if (missing == 0
                || (policy.strictness == ResearchStrictness::Strict
                    && data.iter().all(|r| r.is_ok())))
                && damage.is_empty()
                && !source_problem
            {
                retained.push(date);
                continue;
            }
            let blocking = !damage.is_empty()
                || (source_problem && policy.strictness != ResearchStrictness::QualifiedOnly);
            let disposition = if blocking || policy.strictness == ResearchStrictness::Strict {
                Handling::Block
            } else if policy.allows_missing() {
                Handling::KeepNull
            } else {
                Handling::ExcludeSecurity
            };
            let affected = affected_cache
                .entry(missing)
                .or_insert_with(|| {
                    if missing & (1 << 7) != 0 {
                        return factor_ids.to_vec();
                    }
                    let mut inputs = BTreeSet::new();
                    for (i, f) in fields.iter().enumerate() {
                        if missing & (1 << i) != 0 {
                            for input in match *f {
                                "open" => vec!["open", "dtm", "dbm"],
                                "high" => vec!["high", "hd", "tr", "dtm"],
                                "low" => vec!["low", "ld", "tr", "dbm"],
                                "close" => vec!["close", "ret", "tr"],
                                "volume" => vec!["volume", "vwap"],
                                "amount" => vec!["amount", "vwap"],
                                "vwap" => vec!["vwap"],
                                _ => vec![],
                            } {
                                inputs.insert(input);
                            }
                        }
                    }
                    factor_ids
                        .iter()
                        .filter(|id| {
                            dependencies
                                .get(*id)
                                .is_some_and(|ds| ds.iter().any(|d| inputs.contains(d.as_str())))
                        })
                        .cloned()
                        .collect()
                })
                .clone();
            let (code, kind, message) = if !damage.is_empty() {
                ("input_damaged", ProblemKind::Damaged, damage.join("; "))
            } else if source_problem {
                (
                    "source_interval_invalid",
                    ProblemKind::Conflict,
                    "例外来源映射不覆盖该日期，不能接入另一证券行情".into(),
                )
            } else {
                (
                    "input_observation_missing",
                    ProblemKind::Missing,
                    format!(
                        "缺少输入：{}",
                        fields
                            .iter()
                            .enumerate()
                            .filter(|(i, _)| missing & (1 << i) != 0)
                            .map(|(_, f)| *f)
                            .collect::<Vec<_>>()
                            .join(",")
                    ),
                )
            };
            append(
                &mut decisions,
                QualityDecision {
                    security: id.clone(),
                    coverage: DateRange {
                        start: date,
                        end: date,
                    },
                    code: code.into(),
                    kind,
                    disposition,
                    message,
                    evidence: vec![format!("market:{}", dataset.root.display())],
                    affected_factors: affected,
                },
                &dataset.calendar,
            );
            if disposition != Handling::ExcludeSecurity {
                retained.push(date);
            }
        }
        if policy.strictness == ResearchStrictness::QualifiedOnly {
            let mut rs: Vec<DateRange> = vec![];
            for date in retained {
                if let Some(last) = rs.last_mut()
                    && dataset
                        .calendar
                        .binary_search(&last.end)
                        .ok()
                        .zip(dataset.calendar.binary_search(&date).ok())
                        .is_some_and(|(a, b)| a + 1 == b)
                {
                    last.end = date;
                } else {
                    rs.push(DateRange {
                        start: date,
                        end: date,
                    });
                }
            }
            if rs.is_empty() {
                effective.intervals.remove(id);
            } else {
                effective.intervals.insert(id.clone(), rs);
            }
        }
    }
    fn counts(pool: &PoolMembership, calendar: &[Date]) -> Vec<i64> {
        let mut delta = vec![0i64; calendar.len() + 1];
        for rs in pool.intervals.values() {
            for r in rs {
                let a = calendar.partition_point(|d| *d < r.start);
                let b = calendar.partition_point(|d| *d <= r.end);
                if a < b {
                    delta[a] += 1;
                    delta[b] -= 1;
                }
            }
        }
        let mut n = 0;
        delta
            .into_iter()
            .take(calendar.len())
            .map(|d| {
                n += d;
                n
            })
            .collect()
    }
    let original = counts(pool, &dataset.calendar);
    let kept = counts(&effective, &dataset.calendar);
    let daily_counts=(lo..hi).map(|i|serde_json::json!({"date":dataset.calendar[i],"original":original[i],"retained":kept[i]})).collect();
    Ok(QualitySnapshot {
        schema_version: 1,
        policy: policy.clone(),
        original_pool: pool.clone(),
        effective_pool: effective,
        decisions,
        daily_counts,
    })
}
