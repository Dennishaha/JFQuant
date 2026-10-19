//! Per-input observation dependencies of the exact executable syntax tree.
//! This describes reads, not a replacement for operator warmup or recursive seeds.
use crate::{FactorExpr, Result, alpha158};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Inclusive observation lags, zero being the output observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Window {
    /// Nearest observation read.
    pub first: usize,
    /// Furthest observation read.
    pub last: usize,
    /// Read all loaded seed history, preserving the caller's initialization.
    pub recursive: bool,
    /// Requires the membership cross section at dependency dates.
    pub cross_sectional: bool,
    /// Membership context at the nearest enclosing cross-sectional node.
    pub context_first: usize,
    /// Furthest observation of that cross-sectional membership context.
    pub context_last: usize,
}
/// Each input can have disjoint reads (e.g. Delay(close, 20) + close).
pub type Dependencies = BTreeMap<String, Vec<Window>>;

/// Resolve exact input windows without executing or altering a formula.
pub fn resolve(expression: &FactorExpr) -> Result<Dependencies> {
    let mut out = BTreeMap::new();
    native(
        expression,
        Window {
            first: 0,
            last: 0,
            recursive: false,
            cross_sectional: false,
            context_first: 0,
            context_last: 0,
        },
        &mut out,
    )?;
    for windows in out.values_mut() {
        windows.sort_by_key(|w| {
            (
                w.recursive,
                w.cross_sectional,
                w.context_first,
                w.context_last,
                w.first,
                w.last,
            )
        });
        let mut merged: Vec<Window> = Vec::new();
        for w in windows.drain(..) {
            if let Some(p) = merged.last_mut()
                && p.recursive == w.recursive
                && p.cross_sectional == w.cross_sectional
                && p.context_first == w.context_first
                && p.context_last == w.context_last
                && w.first <= p.last.saturating_add(1)
            {
                p.last = p.last.max(w.last);
            } else {
                merged.push(w);
            }
        }
        *windows = merged;
    }
    Ok(out)
}
fn shifted(w: Window, n: usize) -> Window {
    Window {
        first: w.first.saturating_add(n),
        last: w.last.saturating_add(n),
        ..w
    }
}
fn rolling(w: Window, n: usize) -> Window {
    Window {
        last: w.last.saturating_add(n.saturating_sub(1)),
        ..w
    }
}
fn native(e: &FactorExpr, w: Window, out: &mut Dependencies) -> Result<()> {
    use FactorExpr::*;
    match e {
        Unified(e) => native(&e.lower()?, w, out)?,
        Alpha158(e) => qlib(e, w, out)?,
        Column(name) => out.entry(name.clone()).or_default().push(w),
        Const(_) | Sequence(_) => {}
        Unsupported { formula } => {
            return Err(crate::FactorError::Unsupported(
                "dependency".into(),
                formula.clone(),
            ));
        }
        Delay(a, n) => native(a, shifted(w, *n), out)?,
        Delta(a, n) => {
            native(a, w, out)?;
            native(a, shifted(w, *n), out)?;
        }
        Ema(a, ..) | CumulativeSum(a) | UpReturnProduct(a) => native(
            a,
            Window {
                recursive: true,
                ..w
            },
            out,
        )?,
        Rank(a) | ZScore(a) | Scale(a) => native(
            a,
            Window {
                cross_sectional: true,
                // Raw values feed the nearest enclosing cross section. An outer
                // rank does not turn the dates between nested ranks into reads.
                context_first: w.first,
                context_last: w.last,
                ..w
            },
            out,
        )?,
        Neg(a) | Abs(a) | Log(a) | Sign(a) | Pow(a, _) => native(a, w, out)?,
        TsSum(a, n)
        | TsMean(a, n)
        | TsStd(a, n)
        | TsMin(a, n)
        | TsMax(a, n)
        | TsRank(a, n)
        | DecayLinear(a, n)
        | Count(a, n)
        | Product(a, n)
        | WeightedMean(a, n)
        | HighDay(a, n)
        | LowDay(a, n)
        | RescaledRange(a, n) => native(a, rolling(w, *n), out)?,
        Corr(a, b, n) | Cov(a, b, n) | RegBeta(a, b, n) | RegResi(a, b, n) | SumIf(a, b, n) => {
            native(a, rolling(w, *n), out)?;
            native(b, rolling(w, *n), out)?;
        }
        RegResiMulti(y, x, n) => {
            native(y, rolling(w, *n), out)?;
            for a in x {
                native(a, rolling(w, *n), out)?;
            }
        }
        If(c, a, b) => {
            native(c, w, out)?;
            native(a, w, out)?;
            native(b, w, out)?;
        }
        Add(a, b)
        | Sub(a, b)
        | Mul(a, b)
        | Div(a, b)
        | PowExpr(a, b)
        | Eq(a, b)
        | Gt(a, b)
        | Lt(a, b)
        | Min(a, b)
        | Max(a, b)
        | Filter(a, b)
        | And(a, b)
        | Or(a, b) => {
            native(a, w, out)?;
            native(b, w, out)?;
        }
    }
    Ok(())
}
fn qlib(e: &alpha158::Expr, w: Window, out: &mut Dependencies) -> Result<()> {
    use alpha158::Expr::*;
    match e {
        Unified(e) => native(&e.lower()?, w, out)?,
        Input(name) => out.entry(name.clone()).or_default().push(w),
        Constant(_) => {}
        Binary { left, right, .. } => {
            qlib(left, w, out)?;
            qlib(right, w, out)?;
        }
        Call { name, args } => {
            let n = if name == "Quantile" {
                args.get(1)
            } else {
                args.last()
            };
            let n = match n {
                Some(Constant(n)) => *n as usize,
                _ => 1,
            };
            let w = if name == "Ref" {
                shifted(w, n)
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
                rolling(w, n)
            } else {
                w
            };
            for arg in args {
                qlib(arg, w, out)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_are_per_field_and_keep_disjoint_delays() {
        let e = FactorExpr::Add(
            Box::new(FactorExpr::TsMean(
                Box::new(FactorExpr::Column("volume".into())),
                60,
            )),
            Box::new(FactorExpr::Delta(
                Box::new(FactorExpr::Column("close".into())),
                20,
            )),
        );
        let d = resolve(&e).unwrap();
        assert_eq!(d["volume"][0].last, 59);
        assert_eq!(
            d["close"]
                .iter()
                .map(|w| (w.first, w.last))
                .collect::<Vec<_>>(),
            [(0, 0), (20, 20)]
        );
    }
    #[test]
    fn both_catalogues_resolve_every_declared_input() {
        for set in [
            analytics_core::research_quality::FactorSet::Alpha158,
            analytics_core::research_quality::FactorSet::Gtja191,
        ] {
            for spec in crate::FactorRegistry::for_set(set)
                .all()
                .filter(|s| !matches!(s.expression, FactorExpr::Unsupported { .. }))
            {
                let d = resolve(&spec.expression).unwrap();
                assert_eq!(
                    d.keys().cloned().collect::<Vec<_>>(),
                    spec.input_columns,
                    "{}",
                    spec.id
                );
                assert!(
                    d.values()
                        .flatten()
                        .all(|w| w.last < spec.lookback as usize || w.recursive)
                );
            }
        }
    }
}
