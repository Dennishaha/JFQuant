//! Training-price verification, independent of the execution backend.
use super::*;
use analytics_storage::{corporate_actions::ActionBundle, daily::DailyDataset, qlib::DailyField};

struct RawFields {
    close: DailyField,
    factor: DailyField,
}
impl RawFields {
    fn adjustment_inputs(d: &DailyDataset, id: &str) -> Result<Self> {
        let close = match d.raw_field(id, "close") {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => DailyField {
                start: 0,
                values: vec![None; d.calendar.len()],
            },
            r => r?,
        };
        Ok(Self {
            close,
            factor: d.adjustment_factors(id)?,
        })
    }
    fn close(&self, day: usize) -> Option<f64> {
        self.close.get(day)
    }
}
/// Independently verify every coefficient transition and every implemented event,
/// including training and warmup; return individual securities without starting training.
pub(super) fn adjustment_issues(
    dataset: &DailyDataset,
    book: &ActionBundle,
    symbols: &BTreeSet<String>,
    start: Date,
    end: Date,
) -> Result<BTreeMap<String, Vec<String>>> {
    let lo = dataset.calendar.partition_point(|d| *d < start).max(1);
    let hi = dataset.calendar.partition_point(|d| *d <= end);
    let mut out = BTreeMap::new();
    for id in symbols {
        analytics_core::task::TaskContext::current().check()?;
        let result = (|| -> Result<Vec<String>> {
            let range = dataset.security_range(id, start, end)?;
            let start = range.start;
            let end = range.end;
            let lo = dataset.calendar.partition_point(|d| *d < start).max(lo);
            let hi = dataset.calendar.partition_point(|d| *d <= end).min(hi);
            if book.failures.contains_key(id) {
                return Err("company-action source failure remains unresolved".into());
            }
            for action in book
                .actions
                .iter()
                .filter(|a| a.instrument == *id && a.ex_date >= start && a.ex_date <= end)
            {
                action.validate_adjustment_terms()?;
            }

            let fields = RawFields::adjustment_inputs(dataset, id)?;
            let explicit = dataset.has_independent_coefficients();
            let events: BTreeMap<_, _> = book
                .actions
                .iter()
                .filter(|a| a.instrument == *id)
                .map(|a| (a.ex_date, a))
                .collect();
            let mut issues = Vec::new();
            for a in events
                .values()
                .filter(|a| a.ex_date >= start && a.ex_date <= end)
            {
                match dataset.calendar.binary_search(&a.ex_date) {
                    Err(_) => issues.push((
                        a.ex_date,
                        "company action outside verified trading calendar".to_string(),
                    )),
                    Ok(0) => issues.push((
                        a.ex_date,
                        "ex-date reference price unavailable at first calendar observation"
                            .to_string(),
                    )),
                    _ => {}
                }
            }
            for date in dataset.adjustment_change_dates(id)? {
                if date >= start && date <= end {
                    match dataset.calendar.binary_search(&date) {
                        Err(_) => issues.push((
                            date,
                            "coefficient change outside verified trading calendar".to_string(),
                        )),
                        Ok(0) if !events.contains_key(&date) => issues.push((
                            date,
                            "first-day coefficient change without verifiable event/reference price"
                                .to_string(),
                        )),
                        _ => {}
                    }
                }
            }
            let mut previous_factor = (0..lo).rev().find_map(|d| fields.factor.get(d));
            for day in lo..hi {
                analytics_core::task::TaskContext::current().check()?;
                let date = dataset.calendar[day];
                let current = fields.factor.get(day);
                let transition = previous_factor.zip(current).map(|(a, b)| b / a);
                if let Some(a) = events.get(&date) {
                    let reference = fields
                        .close(day - 1)
                        .and_then(|c| a.reference_price(c).ok().map(|p| (c, p)));
                    let reference = if matches!(
                        a.terms.reference(),
                        Some(analytics_core::actions::Reference::Unchanged { .. })
                    ) {
                        Some((1., 1.))
                    } else {
                        reference
                    };
                    match (reference,transition) {
                        (Some((previous,reference)),Some(ratio)) if reference>0. && ratio.is_finite() => {
                            if (ratio-1.).abs()<=1e-12 && !matches!(a.terms.reference(),Some(analytics_core::actions::Reference::Unchanged{..})) {
                                issues.push((date, "company action without coefficient change".to_string()));
                            } else if (ratio/(previous/reference)-1.).abs()>0.002 {
                                issues.push((date, "distribution disagrees with independent coefficient (possible special distribution/rights issue/source revision)".to_string()));
                            }
                        }
                        _=>issues.push((date, "ex-date reference price or coefficient unavailable; require verified exchange reference data".to_string())),
                    }
                } else if let Some(ratio) = transition {
                    let threshold = if explicit { 1e-12 } else { 0.00002 };
                    if !ratio.is_finite() || (ratio - 1.).abs() > threshold {
                        issues.push((
                            date,
                            "unexplained coefficient change; company-action history incomplete"
                                .to_string(),
                        ));
                    }
                }
                if current.is_some() {
                    previous_factor = current;
                }
            }
            Ok(compress_adjustment_issues(&dataset.calendar, issues))
        })();
        analytics_core::task::TaskContext::current().check()?;
        match result {
            Ok(issues) if !issues.is_empty() => {
                out.insert(id.clone(), issues);
            }
            Err(error) => {
                out.insert(id.clone(), vec![error.to_string()]);
            }
            _ => {}
        }
    }
    Ok(out)
}

fn compress_adjustment_issues(calendar: &[Date], issues: Vec<(Date, String)>) -> Vec<String> {
    let mut groups = BTreeMap::<String, Vec<Date>>::new();
    for (date, message) in issues {
        groups.entry(message).or_default().push(date);
    }
    let mut output = Vec::new();
    for (message, mut dates) in groups {
        dates.sort();
        dates.dedup();
        let mut ranges = Vec::<(Date, Date)>::new();
        for date in dates {
            if let Some((_, end)) = ranges.last_mut()
                && calendar
                    .binary_search(end)
                    .ok()
                    .zip(calendar.binary_search(&date).ok())
                    .is_some_and(|(a, b)| b == a + 1)
            {
                *end = date;
            } else {
                ranges.push((date, date));
            }
        }
        output.extend(ranges.into_iter().map(|(start, end)| {
            if start == end {
                format!("{start}: {message}")
            } else {
                format!("{start}..{end}: {message}")
            }
        }));
    }
    output
}
