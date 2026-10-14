//! Pinned Qlib Alpha158 definitions; dedicated operators preserve GTJA191 semantics.
use super::{EvaluationPanel, FactorError, FactorExpr, FactorRegistry, FactorSpec, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Formula definitions and operator contract, with shares/CNY JFQuant inputs.
pub const FORMULA_VERSION: &str = "qlib-alpha158-b87a2c2+jfquant-units-v1";
/// Frozen data-only expression; never executes provider or Python text.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(missing_docs)]
pub enum Expr {
    Input(String),
    Constant(f64),
    Binary {
        op: String,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Call {
        name: String,
        args: Vec<Expr>,
    },
}
impl Expr {
    /// Input dependencies, derived once from the pinned syntax tree.
    pub fn inputs(&self) -> BTreeSet<String> {
        match self {
            Self::Input(s) => BTreeSet::from([s.clone()]),
            Self::Constant(_) => BTreeSet::new(),
            Self::Binary { left, right, .. } => {
                left.inputs().union(&right.inputs()).cloned().collect()
            }
            Self::Call { args, .. } => args.iter().flat_map(Self::inputs).collect(),
        }
    }
    /// Number of historical observations including nested delays.
    pub fn lookback(&self) -> usize {
        match self {
            Self::Input(_) | Self::Constant(_) => 1,
            Self::Binary { left, right, .. } => left.lookback().max(right.lookback()),
            Self::Call { name, args } => {
                let base = args.iter().map(Self::lookback).max().unwrap_or(1);
                let window = args
                    .last()
                    .and_then(|a| {
                        if let Self::Constant(n) = a {
                            Some(*n as usize)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(1);
                if name == "Ref" {
                    base + window
                } else if matches!(
                    name.as_str(),
                    "Mean"
                        | "Sum"
                        | "Std"
                        | "Max"
                        | "Min"
                        | "Rank"
                        | "Quantile"
                        | "Slope"
                        | "Rsquare"
                        | "Resi"
                        | "IdxMax"
                        | "IdxMin"
                        | "Corr"
                ) {
                    let window = if name == "Quantile" {
                        if let Self::Constant(n) = args[1] {
                            n as usize
                        } else {
                            1
                        }
                    } else {
                        window
                    };
                    base + window.saturating_sub(1)
                } else {
                    base
                }
            }
        }
    }
}
/// Family-specific formula version selected by templates and manifests.
pub fn version(set: analytics_core::research_quality::FactorSet) -> &'static str {
    match set {
        analytics_core::research_quality::FactorSet::Gtja191 => super::FORMULA_VERSION,
        analytics_core::research_quality::FactorSet::Alpha158 => FORMULA_VERSION,
    }
}
#[derive(Deserialize)]
struct Catalogue {
    features: Vec<Definition>,
}
#[derive(Deserialize)]
struct Definition {
    name: String,
    formula: String,
    expression: Expr,
}
pub(super) fn registry() -> FactorRegistry {
    let catalog: Catalogue =
        serde_json::from_str(include_str!("../testdata/alpha158_formulas.json"))
            .expect("compiled Alpha158 catalogue");
    let specs = catalog
        .features
        .into_iter()
        .enumerate()
        .map(|(i, f)| {
            let id = format!("alpha158_{:03}_{}", i + 1, f.name);
            let spec = FactorSpec {
                id: id.clone(),
                lookback: f.expression.lookback() as u32,
                requires_recursive_state: false,
                input_columns: f.expression.inputs().into_iter().collect(),
                expression: FactorExpr::Alpha158(f.expression),
                version: FORMULA_VERSION.into(),
                formula: f.formula,
            };
            (id, spec)
        })
        .collect::<BTreeMap<_, _>>();
    FactorRegistry { specs }
}
fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}
fn rolling_std(values: impl Iterator<Item = f64>) -> f64 {
    let v: Vec<_> = values.collect();
    if v.len() < 2 {
        return f64::NAN;
    }
    let m = v.iter().sum::<f64>() / v.len() as f64;
    (v.iter().map(|x| (x - m).powi(2)).sum::<f64>() / (v.len() - 1) as f64).sqrt()
}
pub(super) fn evaluate(expr: &Expr, panel: &EvaluationPanel<'_>) -> Result<Vec<Option<f64>>> {
    let n = panel.instrument_id.len();
    match expr {
        Expr::Input(name) => panel
            .columns
            .get(name)
            .cloned()
            .ok_or_else(|| FactorError::MissingColumn(name.clone())),
        Expr::Constant(v) => Ok(vec![finite(*v); n]),
        Expr::Binary { op, left, right } => {
            let (a, b) = (evaluate(left, panel)?, evaluate(right, panel)?);
            Ok(a.iter()
                .zip(&b)
                .map(|(a, b)| {
                    if matches!(op.as_str(), "gt" | "lt") && (a.is_none() || b.is_none()) {
                        return Some(0.);
                    }
                    a.zip(*b).and_then(|(a, b)| match op.as_str() {
                        "add" => finite(a + b),
                        "sub" => finite(a - b),
                        "mul" => finite(a * b),
                        "div" => finite(a / b),
                        "gt" => Some(f64::from(a > b)),
                        "lt" => Some(f64::from(a < b)),
                        _ => None,
                    })
                })
                .collect())
        }
        Expr::Call { name, args } => {
            let data = evaluate(&args[0], panel)?;
            if name == "Abs" || name == "Log" {
                return Ok(data
                    .into_iter()
                    .map(|v| v.and_then(|x| finite(if name == "Abs" { x.abs() } else { x.ln() })))
                    .collect());
            }
            if name == "Greater" || name == "Less" {
                let other = evaluate(&args[1], panel)?;
                return Ok(data
                    .iter()
                    .zip(&other)
                    .map(|(a, b)| {
                        a.zip(*b).map(|(a, b)| {
                            if name == "Greater" {
                                a.max(b)
                            } else {
                                a.min(b)
                            }
                        })
                    })
                    .collect());
            }
            let window_index = if name == "Corr" { 2 } else { 1 };
            let window = if let Expr::Constant(w) = args[window_index] {
                w as usize
            } else {
                0
            };
            if window == 0 {
                return Err(FactorError::InvalidRequest(
                    "Alpha158 window must be positive".into(),
                ));
            }
            let other = if name == "Corr" {
                Some(evaluate(&args[1], panel)?)
            } else {
                None
            };
            let mut result = vec![None; n];
            for indices in panel.groups_by_instrument().values() {
                for (pos, &idx) in indices.iter().enumerate() {
                    if pos % 256 == 0 {
                        analytics_core::task::TaskContext::current()
                            .check()
                            .map_err(|e| FactorError::InvalidRequest(e.to_string()))?;
                    }
                    if name == "Ref" {
                        result[idx] = pos.checked_sub(window).and_then(|p| data[indices[p]]);
                        continue;
                    }
                    let rows = &indices[(pos + 1).saturating_sub(window)..=pos];
                    let values: Vec<_> = rows.iter().map(|&i| data[i]).collect();
                    let valid: Vec<f64> = values.iter().flatten().copied().collect();
                    if valid.is_empty() {
                        continue;
                    }
                    let mean = valid.iter().sum::<f64>() / valid.len() as f64;
                    let value = match name.as_str() {
                        "Mean" => Some(mean),
                        "Sum" => Some(valid.iter().sum()),
                        "Std" if valid.len() > 1 => finite(
                            (valid.iter().map(|x| (x - mean).powi(2)).sum::<f64>()
                                / (valid.len() - 1) as f64)
                                .sqrt(),
                        ),
                        "Max" => valid.iter().copied().reduce(f64::max),
                        "Min" => valid.iter().copied().reduce(f64::min),
                        "Quantile" => {
                            let q = if let Expr::Constant(q) = args[2] {
                                q
                            } else {
                                0.
                            };
                            let mut sorted = valid.clone();
                            sorted.sort_by(f64::total_cmp);
                            let x = q * (sorted.len() - 1) as f64;
                            let lo = x.floor() as usize;
                            let hi = x.ceil() as usize;
                            finite(sorted[lo] + (sorted[hi] - sorted[lo]) * (x - lo as f64))
                        }
                        "Rank" => data[idx].map(|x| {
                            let below = valid.iter().filter(|&&v| v < x).count();
                            let equal = valid.iter().filter(|&&v| v == x).count();
                            (below as f64 + (equal as f64 + 1.) / 2.) / valid.len() as f64
                        }),
                        "IdxMax" | "IdxMin" => {
                            let target = if name == "IdxMax" {
                                valid.iter().copied().reduce(f64::max)
                            } else {
                                valid.iter().copied().reduce(f64::min)
                            };
                            let p = values
                                .iter()
                                .position(Option::is_none)
                                .or_else(|| values.iter().position(|v| *v == target));
                            p.map(|p| (p + 1) as f64)
                        }
                        "Slope" | "Rsquare" | "Resi" => {
                            let points: Vec<_> = values
                                .iter()
                                .enumerate()
                                .filter_map(|(i, v)| v.map(|v| ((i + 1) as f64, v)))
                                .collect();
                            let mx = points.iter().map(|p| p.0).sum::<f64>() / points.len() as f64;
                            let xx = points.iter().map(|p| (p.0 - mx).powi(2)).sum::<f64>();
                            let yy = points.iter().map(|p| (p.1 - mean).powi(2)).sum::<f64>();
                            let xy = points
                                .iter()
                                .map(|p| (p.0 - mx) * (p.1 - mean))
                                .sum::<f64>();
                            if name == "Slope" {
                                finite(xy / xx)
                            } else if name == "Rsquare" {
                                if (yy / ((valid.len() - 1) as f64)).sqrt() <= 2e-5 {
                                    None
                                } else {
                                    finite(xy * xy / (xx * yy))
                                }
                            } else {
                                data[idx].and_then(|v| {
                                    finite(v - (mean + xy / xx * (values.len() as f64 - mx)))
                                })
                            }
                        }
                        "Corr" => {
                            let b = other.as_ref().expect("correlation inputs");
                            let pairs: Vec<_> =
                                rows.iter().filter_map(|&i| data[i].zip(b[i])).collect();
                            if pairs.len() < 2 {
                                None
                            } else {
                                let count = pairs.len() as f64;
                                let a = pairs.iter().map(|p| p.0).sum::<f64>() / count;
                                let b = pairs.iter().map(|p| p.1).sum::<f64>() / count;
                                let aa = pairs.iter().map(|p| (p.0 - a).powi(2)).sum::<f64>();
                                let bb = pairs.iter().map(|p| (p.1 - b).powi(2)).sum::<f64>();
                                if rolling_std(rows.iter().filter_map(|&i| data[i])) <= 2e-5
                                    || rolling_std(
                                        rows.iter().filter_map(|&i| other.as_ref().unwrap()[i]),
                                    ) <= 2e-5
                                {
                                    None
                                } else {
                                    finite(
                                        pairs.iter().map(|p| (p.0 - a) * (p.1 - b)).sum::<f64>()
                                            / (aa * bb).sqrt(),
                                    )
                                }
                            }
                        }
                        _ => None,
                    };
                    result[idx] = value.and_then(finite);
                }
            }
            Ok(result)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_158_columns_match_pinned_oracle_including_missing_constant_and_short_windows() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../testdata/alpha158_expected.json")).unwrap();
        let days = fixture["days"].as_u64().unwrap() as usize;
        let stocks = fixture["stocks"].as_u64().unwrap() as usize;
        let cols = serde_json::from_value(fixture["inputs"].clone()).unwrap();
        let panel = super::super::Panel {
            instrument_id: (0..days)
                .flat_map(|_| (0..stocks).map(|s| format!("S{s}")))
                .collect(),
            trade_date: (0..days)
                .flat_map(|d| {
                    std::iter::repeat_n(
                        analytics_core::Date::from_ymd_opt(2020, 1, 1).unwrap()
                            + chrono::Duration::days(d as i64),
                        stocks,
                    )
                })
                .collect(),
            columns: cols,
        };
        let context = EvaluationPanel::new(&panel);
        let registry = registry();
        assert_eq!(registry.len(), 158);
        for spec in registry.all() {
            let FactorExpr::Alpha158(expr) = &spec.expression else {
                panic!("wrong expression family")
            };
            let actual = evaluate(expr, &context).unwrap();
            let expected: Vec<Option<f64>> =
                serde_json::from_value(fixture["expected"][&spec.id].clone()).unwrap();
            for (row, (a, b)) in actual.iter().zip(&expected).enumerate() {
                match (a, b) {
                    (Some(a), Some(b)) => assert!(
                        (a - b).abs() <= 1e-7 * b.abs().max(1.),
                        "{} row {}: {:?} != {:?}",
                        spec.id,
                        row,
                        a,
                        b
                    ),
                    (None, None) => {}
                    _ => panic!("{} row {}: {:?} != {:?}", spec.id, row, a, b),
                }
            }
        }
        assert!(registry.all().map(|s| s.lookback).max().unwrap() <= 61);
    }
}
