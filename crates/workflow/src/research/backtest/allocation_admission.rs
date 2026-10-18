//! Price-history admission before factor/model work; never changes the training universe.
use super::*;
use crate::allocation::{AllocationMethod, CandidatePolicy, STOCK_EXPOSURE};
use analytics_core::task::TaskContext;
use serde_json::{Value, json};

/// Build frozen prices, missing-price reasons and per-date candidate eligibility.
pub(super) fn build(
    request: &ResearchRequest,
    dataset: &DailyDataset,
    book: Option<&analytics_storage::corporate_actions::ActionBundle>,
    pool: &analytics_core::pool::PoolMembership,
    first_score: Date,
    end: Date,
) -> Result<Value> {
    let c = &request.config.allocation;
    let required = c.method != AllocationMethod::Equal
        || c.candidate_policy == CandidatePolicy::CompleteRiskWindow;
    let mut prices = BTreeMap::new();
    let mut missing = BTreeMap::new();
    let mut eligibility = BTreeMap::new();
    let calendar: Vec<_> = dataset
        .calendar
        .iter()
        .copied()
        .take_while(|d| *d <= end)
        .collect();
    let status = book
        .map(analytics_storage::trading_status::StatusIndex::build)
        .transpose()?
        .unwrap_or_default();
    let mut invalid = BTreeMap::new();
    if required {
        for symbol in pool.intervals.keys() {
            TaskContext::current().check()?;
            let opens = dataset.research_field(symbol, "open", true)?;
            let volume = dataset.research_raw_volume(symbol, true)?;
            let mut observed = BTreeMap::new();
            let mut gaps = BTreeMap::new();
            let mut bad = Vec::new();
            for (i, date) in calendar.iter().enumerate() {
                if volume.get(i).is_some_and(|v| v.is_finite() && v > 0.)
                    && let Some(value) = opens.get(i).filter(|v| v.is_finite() && *v > 0.)
                {
                    observed.insert(date.to_string(), value);
                } else {
                    gaps.insert(
                        date.to_string(),
                        if status.suspended(symbol, *date) {
                            "verified_suspension"
                        } else {
                            "unknown_price_or_tradeability"
                        },
                    );
                    bad.push(i);
                }
            }
            prices.insert(symbol.clone(), observed);
            missing.insert(symbol.clone(), gaps);
            invalid.insert(symbol.clone(), bad);
        }
    }
    if c.candidate_policy == CandidatePolicy::CompleteRiskWindow {
        for (day, date) in calendar.iter().enumerate() {
            if *date < first_score {
                continue;
            }
            TaskContext::current().check()?;
            let mut eligible = Vec::new();
            let mut excluded = BTreeMap::new();
            for (symbol, ranges) in &pool.intervals {
                if !ranges.iter().any(|r| r.contains(*date)) {
                    continue;
                }
                if day < c.risk_window + request.config.rebalance_days {
                    excluded.insert(
                        symbol.clone(),
                        json!({"reason":"insufficient_calendar_history"}),
                    );
                    continue;
                }
                let first = day - request.config.rebalance_days - c.risk_window + 1;
                let last = day - request.config.rebalance_days;
                let gaps = &invalid[symbol];
                let mut endpoints = BTreeSet::new();
                for (lo, hi) in [(first, last), (first + request.config.rebalance_days, day)] {
                    endpoints.extend(
                        gaps[gaps.partition_point(|i| *i < lo)..gaps.partition_point(|i| *i <= hi)]
                            .iter()
                            .copied(),
                    );
                }
                if let Some(first) = endpoints.first() {
                    let dates = endpoints
                        .iter()
                        .map(|i| calendar[*i].to_string())
                        .collect::<Vec<_>>();
                    excluded.insert(symbol.clone(), json!({"reason":missing[symbol][&calendar[*first].to_string()],"first_missing_date":calendar[*first],"missing_endpoints":dates}));
                } else {
                    eligible.push(symbol.clone());
                }
            }
            eligibility.insert(date.to_string(), json!({"eligible":eligible,"excluded_count":excluded.len(),"excluded_sha256":crate::signature(&excluded)?,"excluded":excluded,"as_of":date}));
        }
    }
    Ok(
        json!({"policy":c.candidate_policy,"risk_window":c.risk_window,"horizon":request.config.rebalance_days,"stock_exposure":STOCK_EXPOSURE,"prices":prices,"missing_prices":missing,"eligibility":eligibility,"eligibility_sha256":crate::signature(&eligibility)?}),
    )
}

/// Check scheduled candidate counts before training, preserving explicit empty-signal semantics.
pub(super) fn preflight(
    request: &ResearchRequest,
    admission: &Value,
    dates: &[Date],
    reference: Date,
) -> Result<Vec<Value>> {
    let mut issues = Vec::new();
    if request.config.allocation.candidate_policy != CandidatePolicy::CompleteRiskWindow {
        return Ok(issues);
    }
    let formal_start = request
        .config
        .allocation
        .execution_start
        .unwrap_or_else(|| dates.first().copied().unwrap_or(reference));
    let mut formal = false;
    let mut clock = 0;
    for date in dates.iter().filter(|d| **d >= reference) {
        if *date >= formal_start && !formal {
            clock = 0;
            formal = true;
        }
        let scheduled = clock % request.config.rebalance_days == 0;
        clock += 1;
        if !scheduled {
            continue;
        }
        let row = &admission["eligibility"][date.to_string()];
        let count = row["eligible"]
            .as_array()
            .map_or(0, Vec::len)
            .min(request.config.top_n);
        if count == 0 && row["excluded_count"] == 0 {
            continue;
        }
        if count < request.config.top_n
            || count as f64 * request.config.allocation.max_stock_weight < STOCK_EXPOSURE - 1e-6
        {
            issues.push(json!({"date":date,"code":"allocation_eligible_candidates_insufficient","eligible_count":row["eligible"].as_array().map_or(0,Vec::len),"required_top_n":request.config.top_n,"excluded_count":row["excluded_count"],"excluded_sha256":row["excluded_sha256"]}));
        }
    }
    Ok(issues)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn insufficient_candidate_history_blocks_before_training_and_preserves_requested_start() {
        let tmp = tempfile::tempdir().unwrap();
        let w = crate::Workspace::open(tmp.path()).unwrap();
        let (market, actions, mut t) = crate::demo::create_canonical(&w).unwrap();
        t.config.allocation.candidate_policy = CandidatePolicy::CompleteRiskWindow;
        let request = crate::WorkflowRequest::Research {
            template: Box::new(t),
            market: market.id,
            actions: Some(actions.id),
            audit_only: false,
        };
        let context = TaskContext::default();
        let run = w
            .begin(
                context.id,
                "admission",
                crate::snapshot(&w, &request).unwrap(),
            )
            .unwrap();
        assert!(crate::execute(&w, &request, &run, &context).is_err());
        let output = w.resolve(&run.output);
        let ready: Value =
            serde_json::from_slice(&fs::read(output.join("allocation-readiness.json")).unwrap())
                .unwrap();
        assert_eq!(ready["ready"], false);
        assert_eq!(ready["execution"]["start"], "2009-01-02");
        assert_eq!(
            ready["candidate_admission"]["issues"][0]["code"],
            "allocation_eligible_candidates_insufficient"
        );
        assert!(!output.join("training.json").exists());
        assert!(!output.join("summary.json").exists());
    }
}
