//! Build once per account task: source ranges and known events are separate dependencies.
use super::*;
use analytics_core::execution::EventCheckPolicy;
/// Worker-local immutable account-data index. No native handles cross threads.
pub struct ExecutionIndex {
    policy: EventCheckPolicy,
    events: BTreeMap<String, Vec<CorporateAction>>,
    on_ex: BTreeMap<Date, Vec<CorporateAction>>,
    on_record: BTreeMap<Date, Vec<CorporateAction>>,
    coverage: BTreeMap<String, Vec<EventCoverage>>,
    structured: BTreeMap<String, crate::execution_data::StructuredCoverage>,
    dependencies: dependencies::DependencyIndex,
    issues: BTreeMap<String, Vec<crate::execution_data::ExecutionIssue>>,
    synthetic: bool,
    /// Frozen execution auxiliary data, independently validated.
    pub data: Option<crate::execution_data::ExecutionData>,
}

impl ExecutionIndex {
    /// Validate content once. Incomplete event instances remain dated blocking inputs.
    pub fn build(book: &ActionBundle, policy: EventCheckPolicy) -> io::Result<Self> {
        book.validate_identity()?;
        if let Some(d) = &book.execution_data {
            d.validate()?;
        }
        let mut events = BTreeMap::<String, Vec<CorporateAction>>::new();
        let mut on_ex = BTreeMap::<Date, Vec<CorporateAction>>::new();
        let mut on_record = BTreeMap::<Date, Vec<CorporateAction>>::new();
        let mut ids = BTreeSet::new();
        for a in &book.actions {
            if !ids.insert(&a.id) {
                return Err(invalid("duplicate_corporate_action"));
            }
            events
                .entry(a.instrument.clone())
                .or_default()
                .push(a.clone());
            on_ex.entry(a.ex_date).or_default().push(a.clone());
            if let Some(date) = a.record_date {
                on_record.entry(date).or_default().push(a.clone());
            }
        }
        for v in events.values_mut() {
            v.sort_by_key(|a| a.record_date);
        }
        let mut issues = BTreeMap::<String, Vec<crate::execution_data::ExecutionIssue>>::new();
        for i in &book.execution_issues {
            issues
                .entry(i.instrument.clone())
                .or_default()
                .push(i.clone());
        }
        if let Some(data) = &book.execution_data {
            for (id, points) in &data.coefficients {
                let mut previous = None;
                for (date, value) in points {
                    if previous.is_some_and(|p: f64| (value / p - 1.).abs() > 1e-12)
                        && !on_ex
                            .get(date)
                            .is_some_and(|v| v.iter().any(|a| a.instrument == *id))
                    {
                        issues.entry(id.clone()).or_default().push(
                            crate::execution_data::ExecutionIssue {
                                id: String::new(),
                                event_category: String::new(),
                                evidence: Vec::new(),
                                instrument: id.clone(),
                                announced: Some(*date),
                                dates: analytics_core::DateRange {
                                    start: *date,
                                    end: *date,
                                },
                                code: "unexplained_coefficient_transition".into(),
                                message: "独立系数变化缺对应实施事件".into(),
                            },
                        );
                    }
                    previous = Some(*value);
                }
            }
        }
        Ok(Self {
            policy,
            events,
            on_ex,
            on_record,
            coverage: book.coverage.clone(),
            structured: book.structured_coverage.clone(),
            dependencies: dependencies::DependencyIndex::build(book)?,
            issues,
            synthetic: book.downloader == "JFQuant analytics-workflow synthetic-demo-v1",
            data: book.execution_data.clone(),
        })
    }
    /// Only dependencies effective and publicly known by this date affect today's account.
    pub fn check(&self, id: &str, date: Date) -> io::Result<()> {
        if !self
            .dependencies
            .available_on(id, date, EVENT_CATEGORIES, self.policy)
        {
            return Err(invalid(format!("action_history_unavailable: {id} {date}")));
        }
        if let Some(issues) = self.issues.get(id) {
            for i in issues {
                if i.dates.contains(date) && i.announced.is_none_or(|d| d <= date) {
                    return Err(invalid(format!("{}: {id}: {}", i.code, i.message)));
                }
            }
        }
        if let Some(events) = self.events.get(id) {
            // Future incomplete settlements do not delete today's candidate. At registration
            // the actual entitlement must already have all information required by the ledger.
            for a in events
                .iter()
                .take_while(|a| a.record_date.is_none_or(|d| d <= date))
                .filter(|a| a.ex_date >= date)
            {
                a.validate()
                    .map_err(|e| invalid(format!("event_terms_missing: {}: {e}", a.id)))?;
            }
        }
        Ok(())
    }
    /// Events changing today's opening reference/rights.
    pub fn ex_events(&self, date: Date) -> &[CorporateAction] {
        self.on_ex.get(&date).map_or(&[], Vec::as_slice)
    }
    /// Today's entitlement events; a sold security's rights stay in the ledger.
    pub fn record_events(&self, date: Date) -> &[CorporateAction] {
        self.on_record.get(&date).map_or(&[], Vec::as_slice)
    }
    /// A verified full-day suspension permits carrying the last mark, not a new fill.
    pub fn full_day_suspension(&self, id: &str, date: Date) -> bool {
        self.data
            .as_ref()
            .and_then(|d| d.suspensions.get(id))
            .and_then(|d| d.get(&date))
            .is_some_and(|v| v == "full_day")
    }
    /// Unknown special-category coverage is a warning under the explicit supplier policy.
    pub fn warnings(&self) -> Vec<String> {
        if self.policy == EventCheckPolicy::Audited || self.synthetic {
            return vec![];
        }
        let mut warnings: Vec<_> = self
            .structured
            .keys()
            .filter(|id| {
                !EVENT_CATEGORIES.iter().all(|cat| {
                    self.coverage.get(*id).is_some_and(|v| {
                        v.iter().any(|c| {
                            c.category == *cat
                                && c.status == analytics_core::admission::CheckStatus::Passed
                        })
                    })
                })
            })
            .map(|id| format!("special_event_coverage_unknown: {id}"))
            .collect();
        if let Some(data) = &self.data {
            warnings.extend(
                data.capabilities
                    .iter()
                    .filter(|(_, v)| v["status"] != "available")
                    .map(|(key, v)| {
                        format!("execution_source_unavailable: {key}: {}", v["status"])
                    }),
            );
        }
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn date(s: &str) -> Date {
        s.parse().unwrap()
    }
    fn book() -> ActionBundle {
        let id = "SH600000".to_string();
        ActionBundle {
            field_facts: Default::default(),
            schema_version: 5,
            downloader: "JFQuant analytics-storage corporate-actions".into(),
            downloaded_at: "fixture".into(),
            source_bindings: Default::default(),
            coverage: Default::default(),
            queried_symbols: BTreeSet::from([id.clone()]),
            actions: vec![],
            failures: Default::default(),
            raw_sha256: Default::default(),
            execution_issues: vec![],
            execution_data: None,
            structured_coverage: BTreeMap::from([(
                id,
                crate::execution_data::StructuredCoverage {
                    dates: analytics_core::DateRange {
                        start: date("2024-01-01"),
                        end: date("2024-12-31"),
                    },
                    source: "hithink.events/per-share-v1".into(),
                    evidence: vec!["fixture".into()],
                },
            )]),
        }
    }
    #[test]
    fn complete_supplier_history_is_not_a_nine_category_claim() {
        let b = book();
        let p = ExecutionIndex::build(&b, EventCheckPolicy::Provider).unwrap();
        p.check("SH600000", date("2024-02-01")).unwrap();
        assert_eq!(p.warnings().len(), 1);
        assert!(p.check("SH600001", date("2024-02-01")).is_err());
        assert!(p.check("SH600000", date("2025-01-01")).is_err());
        assert!(
            ExecutionIndex::build(&b, EventCheckPolicy::Audited)
                .unwrap()
                .check("SH600000", date("2024-02-01"))
                .is_err()
        );
    }
    #[test]
    fn future_missing_payment_does_not_retroactively_remove_candidates() {
        let mut b = book();
        b.actions.push(CorporateAction {
            id: "SH600000:2024-06-04".into(),
            instrument: "SH600000".into(),
            announced: Some(date("2024-06-01")),
            record_date: Some(date("2024-06-03")),
            ex_date: date("2024-06-04"),
            pay_date: None,
            listing_date: None,
            cash_per_share: (1.).to_string(),
            shares_per_share: (0.).to_string(),
            terms: Default::default(),
            source: "fixture".into(),
        });
        let p = ExecutionIndex::build(&b, EventCheckPolicy::Provider).unwrap();
        p.check("SH600000", date("2024-05-01")).unwrap();
        assert!(p.check("SH600000", date("2024-06-03")).is_err());
        assert_eq!(p.record_events(date("2024-06-03")).len(), 1);
        b.actions[0].pay_date = Some(date("2024-06-04"));
        // No shares distributed: a listing date is not an execution dependency.
        ExecutionIndex::build(&b, EventCheckPolicy::Provider)
            .unwrap()
            .check("SH600000", date("2024-06-03"))
            .unwrap();
    }
    #[test]
    fn known_conflicts_and_unexplained_coefficient_jumps_block() {
        let mut b = book();
        b.execution_data = Some(crate::execution_data::ExecutionData {
            schema_version: 1,
            suspensions: BTreeMap::from([(
                "SH600000".into(),
                BTreeMap::from([
                    (date("2024-06-03"), "full_day".into()),
                    (date("2024-06-04"), "intraday".into()),
                ]),
            )]),
            coefficients: BTreeMap::from([(
                "SH600000".into(),
                BTreeMap::from([(date("2024-06-03"), 1.), (date("2024-06-04"), 2.)]),
            )]),
            ..Default::default()
        });
        let p = ExecutionIndex::build(&b, EventCheckPolicy::Provider).unwrap();
        assert!(p.full_day_suspension("SH600000", date("2024-06-03")));
        assert!(!p.full_day_suspension("SH600000", date("2024-06-04")));
        assert!(!p.full_day_suspension("SH600001", date("2024-06-03")));
        p.check("SH600000", date("2024-06-03")).unwrap();
        assert!(
            p.check("SH600000", date("2024-06-04"))
                .unwrap_err()
                .to_string()
                .contains("unexplained_coefficient_transition")
        );
        b.execution_issues
            .push(crate::execution_data::ExecutionIssue {
                id: String::new(),
                event_category: String::new(),
                evidence: Vec::new(),
                instrument: "SH600000".into(),
                announced: Some(date("2024-06-01")),
                dates: analytics_core::DateRange {
                    start: date("2024-06-01"),
                    end: date("2024-06-30"),
                },
                code: "source_conflict".into(),
                message: "fixture".into(),
            });
        assert!(
            ExecutionIndex::build(&b, EventCheckPolicy::Provider)
                .unwrap()
                .check("SH600000", date("2024-06-03"))
                .unwrap_err()
                .to_string()
                .contains("source_conflict")
        );
    }
}
