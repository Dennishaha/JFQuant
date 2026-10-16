//! Static execution prerequisites, shared by admission and the AKQuant adapter.
use super::*;
use analytics_core::actions::EventTerms;
use analytics_core::admission::{AdmissionCheck, AdmissionStage, CheckStatus};
use analytics_core::execution::EventCheckPolicy;
use analytics_storage::corporate_actions::ActionBundle;

// Raw execution bars already contain the exchange reference adjustment. A verified
// cash-only differential therefore projects its actual ordinary-holder cash to Dividend.
pub(super) fn supported_distribution(event: &analytics_core::execution::CorporateAction) -> bool {
    match &event.terms {
        EventTerms::Distribution { allocation: None } => true,
        EventTerms::Distribution {
            allocation: Some(analytics_core::actions::Allocation::Exact),
        }
        | EventTerms::Differential {
            allocation: analytics_core::actions::Allocation::Exact,
            ..
        } => event.shares() == 0.,
        _ => false,
    }
}
pub(super) fn scope(
    pool: &analytics_core::pool::PoolMembership,
    range: &DateRange,
) -> BTreeMap<String, DateRange> {
    pool.intervals
        .iter()
        .filter_map(|(id, intervals)| {
            intervals
                .iter()
                .filter(|r| r.start <= range.end && r.end >= range.start)
                .map(|r| r.start.max(range.start))
                .min()
                .map(|start| {
                    (
                        id.clone(),
                        DateRange {
                            start,
                            end: range.end,
                        },
                    )
                })
        })
        .collect()
}
fn check(
    id: Option<String>,
    range: &DateRange,
    code: &str,
    error: Option<String>,
    evidence: Vec<String>,
) -> AdmissionCheck {
    AdmissionCheck {
        stage: AdmissionStage::Execution,
        kind: ProblemKind::ExecutionDependency,
        disposition: ProblemDisposition::Block,
        category: "execution_inputs".into(),
        security: id,
        coverage: Some(range.clone()),
        status: if error.is_some() {
            CheckStatus::Failed
        } else {
            CheckStatus::Passed
        },
        code: code.into(),
        message: error.unwrap_or_else(|| "execution prerequisite verified".into()),
        evidence,
        affected_factors: vec![],
    }
}
pub(super) fn audited_coverage(book: &ActionBundle, id: &str, range: &DateRange) -> bool {
    analytics_storage::corporate_actions::dependencies::DependencyIndex::build(book)
        .and_then(|index| {
            index.resolve(
                id,
                std::slice::from_ref(range),
                analytics_storage::corporate_actions::EVENT_CATEGORIES,
                EventCheckPolicy::Audited,
            )
        })
        .is_ok_and(|r| r.ready())
}
pub(crate) fn event_checks(
    book: Option<&ActionBundle>,
    scope: &BTreeMap<String, DateRange>,
    policy: EventCheckPolicy,
) -> Vec<AdmissionCheck> {
    let mut checks = vec![];
    let index = book
        .map(analytics_storage::corporate_actions::dependencies::DependencyIndex::build)
        .transpose();
    for (id, range) in scope {
        let Some(book) = book else {
            checks.push(check(
                Some(id.clone()),
                range,
                "execution_event_version_missing",
                Some("register an event version covering this interval".into()),
                vec![],
            ));
            continue;
        };
        let resolution = index.as_ref().map_err(|e| e.to_string()).and_then(|index| {
            index
                .as_ref()
                .ok_or_else(|| "event version missing".to_string())?
                .resolve(
                    id,
                    std::slice::from_ref(range),
                    analytics_storage::corporate_actions::EVENT_CATEGORIES,
                    policy,
                )
                .map_err(|e| e.to_string())
        });
        match resolution {
            Ok(current) => {
                let code = current
                    .blocking_code
                    .as_deref()
                    .unwrap_or("execution_history_verified");
                let error = (!current.ready()).then(|| {
                    format!(
                        "current coverage gaps: {:?}; historical acquisition: {:?}",
                        current.gaps, current.historical_failure
                    )
                });
                checks.push(check(
                    Some(id.clone()),
                    range,
                    code,
                    error,
                    current.evidence,
                ));
            }
            Err(error) => checks.push(check(
                Some(id.clone()),
                range,
                "execution_evidence_invalid",
                Some(error),
                vec![],
            )),
        }
        for event in book
            .actions
            .iter()
            .filter(|a| a.instrument == *id && range.contains(a.ex_date))
        {
            let invalid = event
                .validate_adjustment_terms()
                .err()
                .map(|e| e.to_string());
            let ordinary = matches!(
                event.terms,
                EventTerms::Distribution { .. } | EventTerms::Differential { .. }
            );
            let (code, error) = if ordinary && invalid.is_some() {
                ("execution_event_terms_invalid", invalid)
            } else if !supported_distribution(event) {
                (
                    "execution_event_unsupported",
                    Some("event cannot be represented by AKQuant ex-date dividends/splits".into()),
                )
            } else {
                (
                    "execution_event_terms_invalid",
                    invalid.or_else(|| {
                        let plain=|s:&str| {let (whole,fraction)=s.split_once('.').unwrap_or((s,"")); !whole.is_empty() && whole.bytes().all(|b|b.is_ascii_digit()) && fraction.bytes().all(|b|b.is_ascii_digit()) && fraction.len()<=28};
                        if !plain(&event.cash_per_share) || !plain(&event.shares_per_share) {Some("AKQuant projection requires plain nonnegative decimal text with supported precision".into())}
                        else {event.share_multiplier().err().map(|e|e.to_string())}
                    }),
                )
            };
            if let Some(error) = error {
                let mut c = check(
                    Some(id.clone()),
                    &DateRange {
                        start: event.ex_date,
                        end: event.ex_date,
                    },
                    code,
                    Some(error),
                    vec![event.source.clone()],
                );
                if code == "execution_event_unsupported" {
                    c.category = "account_events".into();
                    c.disposition = ProblemDisposition::Warn;
                    c.status = CheckStatus::NotRun;
                    c.code = "execution_event_unsupported_skipped".into();
                }
                checks.push(c);
            }
        }
        for issue in book.execution_issues.iter().filter(|i| {
            i.instrument == *id && i.dates.start <= range.end && i.dates.end >= range.start
        }) {
            let mut c = check(
                Some(id.clone()),
                &issue.dates,
                &issue.code,
                Some(issue.message.clone()),
                issue.evidence.clone(),
            );
            if policy == EventCheckPolicy::Provider && !issue.skipped_by_engine() {
                c.category = "account_events".into();
                c.disposition = ProblemDisposition::Warn;
                c.status = CheckStatus::NotRun;
                c.message
                    .push_str("；待实际持仓暴露核验，未受影响不阻断收益发布");
            }
            if issue.skipped_by_engine() {
                c.category = "account_events".into();
                c.disposition = ProblemDisposition::Warn;
                c.status = CheckStatus::NotRun;
            }
            checks.push(c);
        }
    }
    checks
}
/// Verify actual opening exposure before publishing any portfolio result.
/// Engine starts in cash; an ex-date trade does not create yesterday's entitlement.
pub(super) fn exposure_checks(
    book: &ActionBundle,
    snapshots: &BTreeMap<Date, serde_json::Value>,
    range: &DateRange,
    policy: EventCheckPolicy,
) -> Vec<serde_json::Value> {
    let mut checks = Vec::new();
    let mut previous: Option<&serde_json::Value> = None;
    let mut previous_date = range.start.pred_opt().unwrap_or(range.start);
    for (date, snapshot) in snapshots {
        if !range.contains(*date) {
            continue;
        }
        for issue in &book.execution_issues {
            if issue.skipped_by_engine()
                || issue.dates.start > *date
                || issue.dates.end <= previous_date
                || issue.dates.end < range.start
            {
                continue;
            }
            let quantity = previous
                .and_then(|v| v["positions"][&issue.instrument].as_f64())
                .unwrap_or(0.);
            let reference_only = policy == EventCheckPolicy::Provider
                && issue.code == "tdx_reference_precision_difference";
            checks.push(serde_json::json!({"date":date,"instrument":issue.instrument,
                "code":issue.code,"opening_quantity":quantity,"evidence":issue.evidence,
                "status":if quantity != 0. && !reference_only {"blocked"} else {"unexposed_or_reference_only"}}));
        }
        previous = Some(snapshot);
        previous_date = *date;
    }
    checks
}

pub(super) fn skipped_events(
    book: &ActionBundle,
    scope: &BTreeMap<String, DateRange>,
) -> Vec<serde_json::Value> {
    let mut events = Vec::new();
    for a in &book.actions {
        if scope
            .get(&a.instrument)
            .is_some_and(|r| r.contains(a.ex_date))
            && !supported_distribution(a)
        {
            events.push(serde_json::json!({"id":a.id,"instrument":a.instrument,"date":a.ex_date,"category":serde_json::to_value(&a.terms).ok().and_then(|v|v["category"].as_str().or_else(||v["kind"].as_str()).map(str::to_owned)),"evidence":[a.source],"code":"execution_event_unsupported_skipped","message":"AKQuant未模拟此事件；继续执行，不补偿现金或股份"}));
        }
    }
    for i in &book.execution_issues {
        if i.skipped_by_engine()
            && scope
                .get(&i.instrument)
                .is_some_and(|r| i.dates.start <= r.end && i.dates.end >= r.start)
        {
            events.push(serde_json::json!({"id":i.id,"instrument":i.instrument,"date":if i.dates.start==i.dates.end {Some(i.dates.start)}else{None},"dates":i.dates,"category":i.event_category,"evidence":i.evidence,"code":i.code,"message":i.message}));
        }
    }
    events
}
pub(super) fn checks(
    request: &ResearchRequest,
    dataset: &DailyDataset,
    book: Option<&ActionBundle>,
    pool: &analytics_core::pool::PoolMembership,
    range: &DateRange,
) -> Result<Vec<AdmissionCheck>> {
    let scope = scope(pool, range);
    save(
        request.output.join("execution-dependencies.json"),
        &serde_json::json!({"schema_version":1,"policy":request.event_check_policy,"scope":scope}),
    )?;
    let mut checks = event_checks(book, &scope, request.event_check_policy);
    checks.push(check(
        None,
        range,
        "execution_rule_mapping",
        super::akquant::validate_rules(request)
            .err()
            .map(|e| e.to_string()),
        vec![],
    ));
    for (id, dates) in scope
        .iter()
        .chain(std::iter::once((&request.benchmark, range)))
    {
        analytics_core::task::TaskContext::current().check()?;
        let result = (|| -> Result<()> {
            if id != &request.benchmark {
                analytics_core::market_rules::classify(id)?;
            }
            let fields: &[&str] = if id == &request.benchmark {
                &["close"]
            } else {
                &["open", "high", "low", "close"]
            };
            let prices = fields
                .iter()
                .map(|name| dataset.research_raw_field(id, name, false))
                .collect::<std::io::Result<Vec<_>>>()?;
            if id != &request.benchmark {
                let volume = dataset.research_raw_volume(id, false)?;
                for (day, date) in dataset
                    .calendar
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| dates.contains(**d))
                {
                    if prices
                        .iter()
                        .all(|p| p.get(day).is_some_and(|v| v.is_finite() && v > 0.))
                        && !volume.get(day).is_some_and(|v| v.is_finite() && v >= 0.)
                    {
                        return Err(
                            format!("execution_volume_missing_or_invalid: {id} {date}").into()
                        );
                    }
                }
            } else {
                let close = dataset.research_field(id, "close", false)?;
                for (day, _) in dataset
                    .calendar
                    .iter()
                    .enumerate()
                    .filter(|(_, d)| dates.contains(**d))
                {
                    if day == 0
                        || !close.get(day).is_some_and(|v| v.is_finite() && v > 0.)
                        || !close.get(day - 1).is_some_and(|v| v.is_finite() && v > 0.)
                    {
                        return Err("benchmark return missing".into());
                    }
                }
            }
            Ok(())
        })();
        checks.push(check(
            Some(id.clone()),
            dates,
            "execution_market_structure",
            result.err().map(|e| e.to_string()),
            vec![],
        ));
    }
    Ok(checks)
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
        serde_json::from_value(serde_json::json!({"schema_version":5,"downloader":"JFQuant analytics-storage corporate-actions","downloaded_at":"2026-01-01T00:00:00Z","source_bindings":{},"coverage":{},"queried_symbols":["SH600000"],"actions":[],"failures":{},"raw_sha256":{},"structured_coverage":{"SH600000":{"dates":{"start":"2012-01-01","end":"2026-12-31"},"source":"hithink.events/per-share-v1","evidence":["raw"]}}})).unwrap()
    }
    #[test]
    fn unresolved_events_require_actual_opening_exposure() {
        let mut b = book();
        b.execution_issues.push(
            serde_json::from_value(serde_json::json!({
                "id":"event", "instrument":"SH600000", "announced":null,
                "dates":{"start":"2021-01-04","end":"2021-01-04"},
                "code":"source_event_conflict","message":"unresolved","evidence":["raw"]
            }))
            .unwrap(),
        );
        let r = range("2021-01-01", "2021-01-05");
        let scope = BTreeMap::from([("SH600000".into(), r.clone())]);
        let checks = event_checks(Some(&b), &scope, EventCheckPolicy::Provider);
        assert_eq!(checks.last().unwrap().status, CheckStatus::NotRun);
        assert_eq!(
            event_checks(Some(&b), &scope, EventCheckPolicy::Audited)
                .last()
                .unwrap()
                .status,
            CheckStatus::Failed
        );
        let mut snapshots = BTreeMap::from([
            (
                "2021-01-01".parse().unwrap(),
                serde_json::json!({"positions":{}}),
            ),
            (
                "2021-01-04".parse().unwrap(),
                serde_json::json!({"positions":{"SH600000":100}}),
            ),
        ]);
        assert_eq!(
            exposure_checks(&b, &snapshots, &r, EventCheckPolicy::Provider)[0]["status"],
            "unexposed_or_reference_only"
        );
        snapshots.insert(
            "2021-01-01".parse().unwrap(),
            serde_json::json!({"positions":{"SH600000":100}}),
        );
        assert_eq!(
            exposure_checks(&b, &snapshots, &r, EventCheckPolicy::Provider)[0]["status"],
            "blocked"
        );
        b.execution_issues[0].code = "tdx_reference_precision_difference".into();
        assert_eq!(
            exposure_checks(&b, &snapshots, &r, EventCheckPolicy::Provider)[0]["status"],
            "unexposed_or_reference_only"
        );
        assert_eq!(
            exposure_checks(&b, &snapshots, &r, EventCheckPolicy::Audited)[0]["status"],
            "blocked"
        );
    }
    #[test]
    fn former_members_do_not_block_and_exit_does_not_end_coverage() {
        let pool = analytics_core::pool::PoolMembership {
            intervals: BTreeMap::from([("SH600001".into(),vec![range("2005-01-01","2009-12-31")]),("SH600000".into(),vec![range("2013-01-01","2014-12-31")])]),
            ..serde_json::from_value(serde_json::json!({"coverage":{"start":"2005-01-01","end":"2026-12-31"},"intervals":{}})).unwrap()
        };
        let s = scope(&pool, &range("2012-01-01", "2026-12-31"));
        assert_eq!(s.len(), 1);
        assert_eq!(s["SH600000"], range("2013-01-01", "2026-12-31"));
        let mut b = book();
        b.failures.insert("SH600001".into(), "unavailable".into());
        assert!(
            event_checks(Some(&b), &s, EventCheckPolicy::Provider)
                .iter()
                .all(|c| c.status == CheckStatus::Passed)
        );
    }
    #[test]
    fn empty_history_requires_coverage_and_failures_never_pass() {
        let s = BTreeMap::from([("SH600000".into(), range("2012-01-01", "2026-12-31"))]);
        let mut b = book();
        assert_eq!(
            event_checks(Some(&b), &s, EventCheckPolicy::Provider)[0].status,
            CheckStatus::Passed
        );
        b.structured_coverage.clear();
        assert_eq!(
            event_checks(Some(&b), &s, EventCheckPolicy::Provider)[0].code,
            "execution_structured_coverage_missing"
        );
        b.queried_symbols.clear();
        assert_eq!(
            event_checks(Some(&b), &s, EventCheckPolicy::Provider)[0].code,
            "execution_history_not_queried"
        );
        b.failures.insert("SH600000".into(), "failed".into());
        assert_eq!(
            event_checks(Some(&b), &s, EventCheckPolicy::Provider)[0].code,
            "execution_source_failed"
        );
    }
    #[test]
    fn verified_cash_only_differential_projects_actual_cash_and_preserves_reference() {
        let scope = BTreeMap::from([("SH600000".into(), range("2012-01-01", "2026-12-31"))]);
        let mut b = book();
        let event:analytics_core::execution::CorporateAction=serde_json::from_value(serde_json::json!({"id":"SH600000:2020-06-19","instrument":"SH600000","announced":"2020-06-12","record_date":"2020-06-18","ex_date":"2020-06-19","pay_date":null,"listing_date":null,"cash_per_share":"0.18","shares_per_share":"0","source":"verified implementation","terms":{"kind":"differential","reference":{"kind":"formula","cash":"0.15","share_multiplier":"1","subscription_cash":"0","evidence":"exchange reference clause"},"allocation":{"kind":"exact"}}})).unwrap();
        assert!(supported_distribution(&event));
        assert_eq!(event.cash_per_share, "0.18");
        assert_eq!(event.reference_price(10.).unwrap(), 9.85);
        b.actions.push(event);
        assert!(
            event_checks(Some(&b), &scope, EventCheckPolicy::Provider)
                .iter()
                .all(|c| c.status == CheckStatus::Passed)
        );
        assert!(skipped_events(&b, &scope).is_empty());
        b.actions[0].shares_per_share = "invalid".into();
        assert!(
            event_checks(Some(&b), &scope, EventCheckPolicy::Provider)
                .iter()
                .any(|c| c.status == CheckStatus::Failed)
        );
        b.actions[0].shares_per_share = "0".into();
        b.actions[0].cash_per_share = "-1".into();
        assert!(
            event_checks(Some(&b), &scope, EventCheckPolicy::Provider)
                .iter()
                .any(|c| c.status == CheckStatus::Failed)
        );
    }
    #[test]
    fn dividend_needs_no_settlement_dates_but_unsupported_terms_block() {
        let s = BTreeMap::from([("SH600000".into(), range("2012-01-01", "2026-12-31"))]);
        let mut b = book();
        b.actions.push(serde_json::from_value(serde_json::json!({"id":"SH600000:2013-06-02","instrument":"SH600000","announced":"2013-05-01","record_date":"2013-06-01","ex_date":"2013-06-02","pay_date":null,"listing_date":null,"cash_per_share":0.2,"shares_per_share":0.1,"source":"hithink.events/per-share-v1"})).unwrap());
        assert!(
            event_checks(Some(&b), &s, EventCheckPolicy::Provider)
                .iter()
                .all(|c| c.status == CheckStatus::Passed)
        );
        b.actions[0].cash_per_share = (-1.).to_string();
        assert!(
            event_checks(Some(&b), &s, EventCheckPolicy::Provider)
                .iter()
                .any(|c| c.code == "execution_event_terms_invalid")
        );
    }
}
