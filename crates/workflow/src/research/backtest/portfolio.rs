use super::data::Sample;
use super::*;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DayResult {
    pub date: String,
    pub net_return: f64,
    pub benchmark_return: f64,
    pub turnover: f64,
    pub cost: f64,
    pub execution_fee_cny: f64,
    pub holdings: usize,
    pub blocked_orders: usize,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Performance {
    pub execution_fee_cny: f64,
    pub observations: usize,
    pub start: Option<String>,
    pub end: Option<String>,
    pub total_return: f64,
    pub benchmark_total_return: f64,
    pub annualized_return: f64,
    pub benchmark_annualized_return: f64,
    pub annualized_excess: f64,
    pub max_drawdown: f64,
    pub sharpe: Option<f64>,
    pub mean_daily_turnover: f64,
    pub blocked_orders: usize,
}

fn correlation(a: &[f64], b: &[f64]) -> Option<f64> {
    if a.len() < 2 || a.len() != b.len() {
        return None;
    }
    let n = a.len() as f64;
    let ma = a.iter().sum::<f64>() / n;
    let mb = b.iter().sum::<f64>() / n;
    let cov = a
        .iter()
        .zip(b)
        .map(|(a, b)| (a - ma) * (b - mb))
        .sum::<f64>();
    let va = a.iter().map(|a| (a - ma).powi(2)).sum::<f64>();
    let vb = b.iter().map(|b| (b - mb).powi(2)).sum::<f64>();
    (va > 0.0 && vb > 0.0).then_some(cov / (va * vb).sqrt())
}
fn ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<_> = (0..values.len()).collect();
    order.sort_by(|a, b| values[*a].total_cmp(&values[*b]));
    let mut out = vec![0.0; values.len()];
    let mut start = 0;
    while start < order.len() {
        let mut end = start + 1;
        while end < order.len() && values[order[end]] == values[order[start]] {
            end += 1;
        }
        let rank = (start + end - 1) as f64 / 2.0;
        for i in start..end {
            out[order[i]] = rank;
        }
        start = end;
    }
    out
}
pub(super) fn information_coefficients(
    rows: &[Sample],
    scores: &[f64],
) -> (Option<f64>, Option<f64>) {
    let mut days = BTreeMap::<usize, (Vec<f64>, Vec<f64>)>::new();
    for (row, score) in rows.iter().zip(scores) {
        if let Some(label) = row.label
            && score.is_finite()
        {
            let day = days.entry(row.day).or_default();
            day.0.push(*score);
            day.1.push(label);
        }
    }
    let mut ic = Vec::new();
    let mut ric = Vec::new();
    for (a, b) in days.values() {
        if a.len() >= 30 {
            if let Some(v) = correlation(a, b) {
                ic.push(v);
            }
            if let Some(v) = correlation(&ranks(a), &ranks(b)) {
                ric.push(v);
            }
        }
    }
    let mean = |v: &Vec<f64>| (!v.is_empty()).then(|| v.iter().sum::<f64>() / v.len() as f64);
    (mean(&ic), mean(&ric))
}
pub(super) fn performance(days: &[DayResult]) -> Performance {
    let n = days.len();
    let mut curve = 1.0_f64;
    let mut benchmark = 1.0;
    let mut peak = 1.0_f64;
    let mut drawdown = 0.0_f64;
    for day in days {
        curve *= 1.0 + day.net_return;
        benchmark *= 1.0 + day.benchmark_return;
        peak = peak.max(curve);
        drawdown = drawdown.min(curve / peak - 1.0);
    }
    let annual = if n > 0 {
        curve.powf(252.0 / n as f64) - 1.0
    } else {
        0.0
    };
    let benchmark_annual = if n > 0 {
        benchmark.powf(252.0 / n as f64) - 1.0
    } else {
        0.0
    };
    let mean = days.iter().map(|d| d.net_return).sum::<f64>() / n.max(1) as f64;
    let variance = days
        .iter()
        .map(|d| (d.net_return - mean).powi(2))
        .sum::<f64>()
        / n.saturating_sub(1).max(1) as f64;
    Performance {
        execution_fee_cny: days.iter().map(|d| d.execution_fee_cny).sum(),
        observations: n,
        start: days.first().map(|d| d.date.clone()),
        end: days.last().map(|d| d.date.clone()),
        total_return: curve - 1.0,
        benchmark_total_return: benchmark - 1.0,
        annualized_return: annual,
        benchmark_annualized_return: benchmark_annual,
        annualized_excess: (1.0 + annual) / (1.0 + benchmark_annual) - 1.0,
        max_drawdown: drawdown,
        sharpe: (variance > 0.0).then_some(mean / variance.sqrt() * 252.0_f64.sqrt()),
        mean_daily_turnover: days.iter().map(|d| d.turnover).sum::<f64>() / n.max(1) as f64,
        blocked_orders: days.iter().map(|d| d.blocked_orders).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn drawdown_includes_initial_capital_and_rank_ties_use_average() {
        let days = vec![DayResult {
            date: "2020-01-02".into(),
            net_return: -0.1,
            benchmark_return: 0.0,
            turnover: 0.0,
            cost: 0.0,
            execution_fee_cny: 0.0,
            holdings: 0,
            blocked_orders: 0,
        }];
        assert!((performance(&days).max_drawdown + 0.1).abs() < 1e-12);
        assert_eq!(ranks(&[4.0, 1.0, 1.0]), vec![2.0, 0.5, 0.5]);
    }
}
