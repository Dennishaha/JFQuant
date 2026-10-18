//! Calendar-only allocation admission, before training or account execution.
use super::*;
use crate::allocation::AllocationMethod;

pub(super) fn report(
    c: &crate::allocation::AllocationConfig,
    top_n: usize,
    h: usize,
    calendar: &[Date],
    folds: &[analytics_core::schedule::ResolvedFold],
) -> Result<serde_json::Value> {
    c.validate_count(top_n)?;
    let first = folds
        .first()
        .ok_or("allocation_prediction_range_empty")?
        .actual
        .test
        .start;
    let end = folds.last().unwrap().actual.test.end;
    let prediction = c.prediction_start.unwrap_or(first);
    let formal = c.execution_start.unwrap_or(first);
    let reference = c.reference_start.unwrap_or(formal);
    let index = |date| calendar.binary_search(&date).ok();
    let mut issues = Vec::new();
    if first > prediction
        || prediction > reference
        || reference > formal
        || formal > end
        || [prediction, reference, formal]
            .iter()
            .any(|d| index(*d).is_none())
    {
        issues.push("prediction/reference/execution ranges must use frozen trading dates in order and be covered by the prediction schedule");
    }
    let needs_risk = c.method != AllocationMethod::Equal
        || c.candidate_policy == crate::allocation::CandidatePolicy::CompleteRiskWindow;
    let needs_calibration = c.method == AllocationMethod::MeanVariance;
    let risk_ready = needs_risk
        .then(|| calendar.get(c.risk_window + h).copied())
        .flatten();
    let calibration_ready = if needs_calibration {
        index(prediction)
            .map(|i| {
                if c.candidate_policy == crate::allocation::CandidatePolicy::CompleteRiskWindow {
                    i.max(c.risk_window + h)
                } else {
                    i
                }
            })
            .and_then(|i| calendar.get(i + c.calibration_min_days + h))
            .copied()
    } else {
        None
    };
    if needs_risk && risk_ready.is_none_or(|d| reference < d) {
        issues.push("risk warmup does not contain enough fully realized H-day observations at reference start");
    }
    if needs_calibration && calibration_ready.is_none_or(|d| reference < d) {
        issues.push("prediction warmup cannot yet supply the minimum realized calibration dates at reference start");
    }
    let earliest = [Some(prediction), risk_ready, calibration_ready]
        .into_iter()
        .flatten()
        .max();
    Ok(serde_json::json!({
        "schema_version":1,"ready":issues.is_empty(),"issues":issues,
        "prediction_warmup":{"start":prediction,"end":reference},
        "reference_warmup":{"start":reference,"end":formal},
        "execution":{"start":formal,"end":end},
        "horizon":h,"risk_window":c.risk_window,"overlapping_observations":h>1,
        "calibration_min_days":c.calibration_min_days,"calibration_min_pairs":c.calibration_min_pairs,
        "earliest_risk_date":risk_ready,"earliest_calibration_date_lower_bound":calibration_ready,
        "earliest_possible_reference_date_lower_bound":earliest,
        "candidate_pair_and_endpoint_checks":"checked against generated out-of-sample predictions before account execution; calendar readiness alone is not data readiness",
        "automatic_start_shift":false
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn readiness_reports_lower_bound_and_never_shifts_formal_start() {
        let first: Date = "2024-01-01".parse().unwrap();
        let calendar = (0..40)
            .map(|i| first + chrono::Duration::days(i))
            .collect::<Vec<_>>();
        let fold = analytics_core::schedule::Fold {
            train: DateRange {
                start: calendar[0],
                end: calendar[9],
            },
            validation: None,
            test: DateRange {
                start: calendar[20],
                end: calendar[39],
            },
        };
        let folds = vec![analytics_core::schedule::ResolvedFold {
            requested: fold.clone(),
            actual: fold,
            warmup_start: calendar[0],
            warmup_sessions: 20,
        }];
        let config = crate::allocation::AllocationConfig {
            method: AllocationMethod::MeanVariance,
            risk_aversion: Some(10.),
            risk_window: 100,
            ..Default::default()
        };
        let value = report(&config, 10, 5, &calendar, &folds).unwrap();
        assert_eq!(value["ready"], false);
        assert_eq!(
            value["execution"]["start"],
            folds[0].actual.test.start.to_string()
        );
        assert_eq!(value["automatic_start_shift"], false);
        assert!(!value["issues"].as_array().unwrap().is_empty());
    }
}
