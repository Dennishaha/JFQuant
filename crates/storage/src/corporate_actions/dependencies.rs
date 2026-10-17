//! Current task coverage, independently of immutable acquisition failures.
//!
//! Build from an authenticated `ActionBundle::read` result once per worker task.
use super::*;
use analytics_core::{DateRange, admission::CheckStatus, execution::EventCheckPolicy};

/// Uncovered inclusive dates for one required event category.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CoverageGap {
    /// Required event category.
    pub category: String,
    /// Dates without accepted evidence.
    pub dates: DateRange,
}
/// Current history coverage; does not assert that known events are executable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageResolution {
    /// Canonical security.
    pub instrument: String,
    /// available, partial, or blocked, relative to the requested ranges/categories.
    pub status: String,
    /// Current unresolved coverage, not historical acquisition errors.
    pub gaps: Vec<CoverageGap>,
    /// Accepted evidence overlapping this request.
    pub evidence: Vec<String>,
    /// Original failure retained for audit even when current coverage is available.
    pub historical_failure: Option<String>,
    /// Current blocking code, absent when coverage is complete.
    pub blocking_code: Option<String>,
}
impl CoverageResolution {
    /// All requested coverage dependencies are satisfied.
    pub fn ready(&self) -> bool {
        self.gaps.is_empty()
    }
}
/// Worker-local compiled coverage index; no file access during resolution.
pub struct DependencyIndex {
    audited: BTreeMap<(String, String), Vec<DateRange>>,
    evidence: BTreeMap<(String, String), Vec<EventCoverage>>,
    structured: BTreeMap<String, crate::execution_data::StructuredCoverage>,
    failures: BTreeMap<String, String>,
    queried: BTreeSet<String>,
}
fn merge(mut spans: Vec<DateRange>) -> Vec<DateRange> {
    spans.sort_by_key(|r| r.start);
    let mut out: Vec<DateRange> = vec![];
    for span in spans {
        if let Some(last) = out.last_mut()
            && (span.start <= last.end || last.end.succ_opt() == Some(span.start))
        {
            last.end = last.end.max(span.end);
        } else {
            out.push(span);
        }
    }
    out
}
fn subtract(range: &DateRange, spans: &[DateRange]) -> Vec<DateRange> {
    let mut next = range.start;
    let mut gaps = vec![];
    for span in spans.iter().skip_while(|s| s.end < range.start) {
        if span.start > range.end {
            break;
        }
        if span.start > next {
            gaps.push(DateRange {
                start: next,
                end: span.start.pred_opt().unwrap(),
            });
        }
        if span.end >= range.end {
            return gaps;
        }
        next = next.max(span.end.succ_opt().unwrap());
    }
    gaps.push(DateRange {
        start: next,
        end: range.end,
    });
    gaps
}
impl DependencyIndex {
    /// Compile an already authenticated bundle. Test fixtures may be constructed in memory.
    pub fn build(book: &ActionBundle) -> io::Result<Self> {
        book.validate_identity()?;
        let mut evidence = BTreeMap::<(String, String), Vec<EventCoverage>>::new();
        for (id, claims) in &book.coverage {
            for c in claims.iter().filter(|c| {
                c.status == CheckStatus::Passed
                    && !c.evidence.is_empty()
                    && !c.reason.trim().is_empty()
            }) {
                evidence
                    .entry((id.clone(), c.category.clone()))
                    .or_default()
                    .push(c.clone());
            }
        }
        let audited = evidence
            .iter()
            .map(|(k, cs)| {
                (
                    k.clone(),
                    merge(cs.iter().map(|c| c.dates.clone()).collect()),
                )
            })
            .collect();
        Ok(Self {
            audited,
            evidence,
            structured: book.structured_coverage.clone(),
            failures: book.failures.clone(),
            queried: book.queried_symbols.clone(),
        })
    }
    /// Allocation-light point lookup for the account loop; compiled intervals are not rebuilt.
    pub fn available_on(
        &self,
        id: &str,
        date: Date,
        categories: &[&str],
        policy: EventCheckPolicy,
    ) -> bool {
        if categories.is_empty() || categories.iter().any(|c| !EVENT_CATEGORIES.contains(c)) {
            return false;
        }
        let provider = policy == EventCheckPolicy::Provider
            && self.queried.contains(id)
            && !self.failures.contains_key(id)
            && self.structured.get(id).is_some_and(|c| {
                c.dates.contains(date) && !c.source.is_empty() && !c.evidence.is_empty()
            });
        provider
            || categories.iter().all(|category| {
                self.audited
                    .get(&(id.to_owned(), (*category).to_owned()))
                    .is_some_and(|spans| {
                        let n = spans.partition_point(|s| s.end < date);
                        spans.get(n).is_some_and(|s| s.contains(date))
                    })
            })
    }
    /// Resolve only the supplied interval set and categories, retaining exact uncovered spans.
    pub fn resolve(
        &self,
        id: &str,
        ranges: &[DateRange],
        categories: &[&str],
        policy: EventCheckPolicy,
    ) -> io::Result<CoverageResolution> {
        analytics_core::pool::validate_instrument(id).map_err(invalid)?;
        if ranges.is_empty()
            || categories.is_empty()
            || categories.iter().any(|c| !EVENT_CATEGORIES.contains(c))
        {
            return Err(invalid("coverage requires ranges and known categories"));
        }
        for range in ranges {
            range.validate().map_err(invalid)?;
        }
        let ranges = merge(ranges.to_vec());
        let mut gaps = vec![];
        let mut evidence = BTreeSet::new();
        let mut any_covered = false;
        for category in categories.iter().copied().collect::<BTreeSet<_>>() {
            let key = (id.to_string(), category.to_string());
            let mut spans = self.audited.get(&key).cloned().unwrap_or_default();
            if policy == EventCheckPolicy::Provider
                && self.queried.contains(id)
                && !self.failures.contains_key(id)
                && let Some(c) = self
                    .structured
                    .get(id)
                    .filter(|c| !c.source.is_empty() && !c.evidence.is_empty())
            {
                spans.push(c.dates.clone());
                if ranges
                    .iter()
                    .any(|r| r.start <= c.dates.end && r.end >= c.dates.start)
                {
                    evidence.extend(c.evidence.clone());
                }
            }
            let spans = merge(spans);
            for range in &ranges {
                any_covered |= spans
                    .iter()
                    .any(|s| s.start <= range.end && s.end >= range.start);
                gaps.extend(
                    subtract(range, &spans)
                        .into_iter()
                        .map(|dates| CoverageGap {
                            category: category.into(),
                            dates,
                        }),
                );
            }
            for c in self.evidence.get(&key).into_iter().flatten().filter(|c| {
                ranges
                    .iter()
                    .any(|r| r.start <= c.dates.end && r.end >= c.dates.start)
            }) {
                evidence.extend(c.evidence.clone());
            }
        }
        let blocking_code = if gaps.is_empty() {
            None
        } else if any_covered {
            Some("execution_event_coverage_missing")
        } else if self.failures.contains_key(id) {
            Some("execution_source_failed")
        } else if !self.queried.contains(id) {
            Some("execution_history_not_queried")
        } else if policy == EventCheckPolicy::Audited {
            Some("execution_event_coverage_missing")
        } else {
            Some("execution_structured_coverage_missing")
        };
        Ok(CoverageResolution {
            instrument: id.into(),
            status: if gaps.is_empty() {
                "available"
            } else if any_covered {
                "partial"
            } else {
                "blocked"
            }
            .into(),
            gaps,
            evidence: evidence.into_iter().collect(),
            historical_failure: self.failures.get(id).cloned(),
            blocking_code: blocking_code.map(str::to_owned),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn range(a: &str, b: &str) -> DateRange {
        DateRange {
            start: a.parse().unwrap(),
            end: b.parse().unwrap(),
        }
    }
    fn book() -> ActionBundle {
        serde_json::from_value(serde_json::json!({"schema_version":5,"downloader":"JFQuant analytics-workflow synthetic-demo-v1","downloaded_at":"fixture","source_bindings":{},"coverage":{},"queried_symbols":[],"actions":[],"failures":{"SH600000":"original failed query"},"raw_sha256":{}})).unwrap()
    }
    fn claim(category: &str, dates: DateRange) -> EventCoverage {
        EventCoverage {
            category: category.into(),
            dates,
            status: CheckStatus::Passed,
            evidence: vec!["fixture independent audit".into()],
            reason: "explicit complete fixture".into(),
        }
    }
    #[test]
    fn historical_failure_and_current_coverage_are_independent() {
        let mut b = book();
        b.coverage.insert(
            "SH600000".into(),
            vec![
                claim("distribution", range("2024-01-01", "2024-01-03")),
                claim("distribution", range("2024-01-04", "2024-01-05")),
                claim("distribution", range("2024-01-10", "2024-01-12")),
            ],
        );
        let index = DependencyIndex::build(&b).unwrap();
        let ranges = [
            range("2024-01-02", "2024-01-05"),
            range("2024-01-10", "2024-01-11"),
        ];
        for policy in [EventCheckPolicy::Audited, EventCheckPolicy::Provider] {
            let r = index
                .resolve("SH600000", &ranges, &["distribution"], policy)
                .unwrap();
            assert!(r.ready());
            assert!(r.historical_failure.is_some());
            assert!(index.available_on("SH600000", ranges[0].start, &["distribution"], policy));
            let partial = index
                .resolve(
                    "SH600000",
                    &[range("2024-01-02", "2024-01-11")],
                    &["distribution"],
                    policy,
                )
                .unwrap();
            assert_eq!(partial.status, "partial");
            assert_eq!(
                partial.gaps,
                vec![CoverageGap {
                    category: "distribution".into(),
                    dates: range("2024-01-06", "2024-01-09")
                }]
            );
            let different = index
                .resolve("SH600000", &ranges, &["merger"], policy)
                .unwrap();
            assert!(!different.ready());
            assert_eq!(different.status, "blocked");
        }
        assert!(b.failures.contains_key("SH600000"));
        assert!(b.queried_symbols.is_empty());
    }
    #[test]
    fn exact_interval_subtraction_and_union() {
        let spans = merge(vec![
            range("2024-01-01", "2024-01-03"),
            range("2024-01-04", "2024-01-05"),
            range("2024-01-08", "2024-01-09"),
        ]);
        assert_eq!(
            subtract(&range("2024-01-02", "2024-01-10"), &spans),
            vec![
                range("2024-01-06", "2024-01-07"),
                range("2024-01-10", "2024-01-10")
            ]
        );
    }
}
