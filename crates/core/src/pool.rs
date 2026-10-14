//! Date-effective, deterministic stock-pool composition. No provider or filesystem access.
use crate::{Date, DateRange};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// A versioned source or explicitly selected securities, composed using set operations.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PoolExpression {
    /// Historical intervals from an immutable market version.
    Universe {
        /// Immutable source version.
        version: String,
        /// Named historical universe.
        universe: String,
    },
    /// Explicit user selection, effective over the definition's coverage.
    Symbols {
        /// Explicit internal identifiers.
        instruments: BTreeSet<String>,
    },
    /// Union of all inputs.
    Union {
        /// Set-operation operands.
        inputs: Vec<PoolExpression>,
    },
    /// Intersection of all inputs.
    Intersection {
        /// Set-operation operands.
        inputs: Vec<PoolExpression>,
    },
    /// Securities in base but not excluded.
    Difference {
        /// Positive operand.
        base: Box<PoolExpression>,
        /// Securities to exclude.
        exclude: Box<PoolExpression>,
    },
}
/// User-authored, effective-date membership override.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PoolAdjustment {
    /// Calendar date; the next trading date is used when necessary.
    pub effective_date: Date,
    /// Securities added persistently from the effective date.
    pub add: BTreeSet<String>,
    /// Securities removed persistently from the effective date.
    pub remove: BTreeSet<String>,
}
/// Immutable pool recipe. Historical index membership remains separate from today's list.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PoolDefinition {
    /// Current pool format, 1.
    pub schema_version: u32,
    /// User-visible pool name.
    pub name: String,
    /// Declared coverage; must also be supported by every referenced input.
    pub coverage: DateRange,
    /// Nested composition.
    pub expression: PoolExpression,
    /// Persistent manual overrides applied after composition.
    pub adjustments: Vec<PoolAdjustment>,
}
/// Treatment of an existing position that leaves the eligible pool.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PoolExitPolicy {
    /// Retry liquidation at each tradable open, independently of model signals.
    #[default]
    NextTradableDay,
    /// Wait until the next scheduled rebalance, then retry liquidation.
    NextRebalance,
}
/// Effective dated membership, ready for model and execution consumers.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct PoolMembership {
    /// Coverage of the source, including intentionally empty trading dates.
    pub coverage: DateRange,
    /// Inclusive effective membership intervals.
    pub intervals: BTreeMap<String, Vec<DateRange>>,
}
impl PoolMembership {
    /// Validate coverage, identifiers and effective intervals without filling missing dates.
    pub fn validate(&self) -> Result<(), String> {
        if self.coverage.start > self.coverage.end {
            return Err("reversed pool coverage".into());
        }
        for (id, ranges) in &self.intervals {
            validate_instrument(id)?;
            for r in ranges {
                if r.start > r.end || r.start < self.coverage.start || r.end > self.coverage.end {
                    return Err("invalid membership interval/coverage".into());
                }
            }
        }
        Ok(())
    }
    /// Query only effective membership; never substitutes current members.
    pub fn on(&self, day: Date) -> BTreeSet<String> {
        if day < self.coverage.start || day > self.coverage.end {
            return BTreeSet::new();
        }
        self.intervals
            .iter()
            .filter(|(_, ranges)| ranges.iter().any(|r| r.start <= day && day <= r.end))
            .map(|(id, _)| id.clone())
            .collect()
    }
}
/// Strict internal A-share/index identifier validation.
pub fn validate_instrument(id: &str) -> Result<(), String> {
    if crate::security::is_security_id(id) {
        return Ok(());
    }
    validate_exchange_symbol(id)
}
/// Validate a source/exchange locator separately from opaque securities.
pub fn validate_exchange_symbol(id: &str) -> Result<(), String> {
    if !id.is_ascii()
        || id.len() != 8
        || !matches!(&id[..2], "SH" | "SZ" | "BJ")
        || !id[2..].bytes().all(|b| b.is_ascii_digit())
    {
        return Err(format!("invalid instrument {id}"));
    }
    Ok(())
}
impl PoolDefinition {
    /// Validate structure, duplicate-day conflicts and bounded expression depth.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 2
            || self.name.trim().is_empty()
            || self.coverage.start > self.coverage.end
        {
            return Err("invalid pool definition/version".into());
        }
        fn expression(e: &PoolExpression, depth: usize) -> Result<(), String> {
            if depth > 16 {
                return Err("pool expression too deep".into());
            }
            match e {
                PoolExpression::Universe { version, universe } => {
                    if version.is_empty()
                        || universe.is_empty()
                        || !universe
                            .bytes()
                            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
                    {
                        return Err("invalid universe reference".into());
                    }
                }
                PoolExpression::Symbols { instruments } => {
                    for id in instruments {
                        validate_instrument(id)?;
                    }
                }
                PoolExpression::Union { inputs } | PoolExpression::Intersection { inputs } => {
                    if inputs.len() < 2 || inputs.len() > 32 {
                        return Err("composition requires 2..32 inputs".into());
                    }
                    for e in inputs {
                        expression(e, depth + 1)?;
                    }
                }
                PoolExpression::Difference { base, exclude } => {
                    expression(base, depth + 1)?;
                    expression(exclude, depth + 1)?;
                }
            }
            Ok(())
        }
        expression(&self.expression, 0)?;
        let mut days = BTreeMap::<Date, (BTreeSet<String>, BTreeSet<String>)>::new();
        for a in &self.adjustments {
            if a.effective_date < self.coverage.start || a.effective_date > self.coverage.end {
                return Err("adjustment outside pool coverage".into());
            }
            let (add, remove) = days.entry(a.effective_date).or_default();
            for id in &a.add {
                validate_instrument(id)?;
                add.insert(id.clone());
            }
            for id in &a.remove {
                validate_instrument(id)?;
                remove.insert(id.clone());
            }
            if !add.is_disjoint(remove) {
                return Err("conflicting pool adjustment".into());
            }
        }
        Ok(())
    }
    /// Resolve input versions once and evaluate chronologically without look-ahead.
    pub fn evaluate(
        &self,
        calendar: &[Date],
        resolve: impl Fn(&str, &str) -> Result<PoolMembership, String>,
    ) -> Result<PoolMembership, String> {
        self.validate()?;
        if calendar.is_empty() || calendar.windows(2).any(|w| w[0] >= w[1]) {
            return Err("invalid pool calendar".into());
        }
        let dates: Vec<_> = calendar
            .iter()
            .copied()
            .filter(|d| self.coverage.start <= *d && *d <= self.coverage.end)
            .collect();
        if dates.is_empty() {
            return Err("pool has no trading dates".into());
        }
        fn load(
            e: &PoolExpression,
            out: &mut BTreeMap<(String, String), PoolMembership>,
            resolve: &impl Fn(&str, &str) -> Result<PoolMembership, String>,
            start: Date,
            end: Date,
        ) -> Result<(), String> {
            match e {
                PoolExpression::Universe { version, universe } => {
                    let key = (version.clone(), universe.clone());
                    if let std::collections::btree_map::Entry::Vacant(entry) = out.entry(key) {
                        let mut m = resolve(version, universe)?;
                        m.intervals.retain(|_, rs| {
                            rs.retain(|r| r.end >= start && r.start <= end);
                            !rs.is_empty()
                        });
                        m.validate()?;
                        if m.coverage.start > start || m.coverage.end < end {
                            return Err(format!("universe coverage incomplete: {universe}"));
                        }
                        entry.insert(m);
                    }
                }
                PoolExpression::Union { inputs } | PoolExpression::Intersection { inputs } => {
                    for e in inputs {
                        load(e, out, resolve, start, end)?;
                    }
                }
                PoolExpression::Difference { base, exclude } => {
                    load(base, out, resolve, start, end)?;
                    load(exclude, out, resolve, start, end)?;
                }
                PoolExpression::Symbols { .. } => {}
            };
            Ok(())
        }
        fn on(
            e: &PoolExpression,
            day: Date,
            sources: &BTreeMap<(String, String), PoolMembership>,
        ) -> BTreeSet<String> {
            match e {
                PoolExpression::Universe { version, universe } => {
                    sources[&(version.clone(), universe.clone())].on(day)
                }
                PoolExpression::Symbols { instruments } => instruments.clone(),
                PoolExpression::Union { inputs } => {
                    inputs.iter().flat_map(|e| on(e, day, sources)).collect()
                }
                PoolExpression::Intersection { inputs } => inputs
                    .iter()
                    .skip(1)
                    .fold(on(&inputs[0], day, sources), |a, e| {
                        a.intersection(&on(e, day, sources)).cloned().collect()
                    }),
                PoolExpression::Difference { base, exclude } => on(base, day, sources)
                    .difference(&on(exclude, day, sources))
                    .cloned()
                    .collect(),
            }
        }
        let mut sources = BTreeMap::new();
        load(
            &self.expression,
            &mut sources,
            &resolve,
            dates[0],
            *dates.last().unwrap(),
        )?;
        let mut events = BTreeMap::<Date, (BTreeSet<String>, BTreeSet<String>)>::new();
        for a in &self.adjustments {
            if let Some(day) = dates.iter().find(|d| **d >= a.effective_date) {
                let (add, remove) = events.entry(*day).or_default();
                add.extend(a.add.clone());
                remove.extend(a.remove.clone());
                if !add.is_disjoint(remove) {
                    return Err("adjustments conflict on effective trading day".into());
                }
            }
        }
        let mut overrides = BTreeMap::new();
        let mut intervals = BTreeMap::<String, Vec<DateRange>>::new();
        let mut previous = BTreeSet::new();
        for (i, day) in dates.iter().enumerate() {
            if let Some((add, remove)) = events.get(day) {
                for id in add {
                    overrides.insert(id.clone(), true);
                }
                for id in remove {
                    overrides.insert(id.clone(), false);
                }
            }
            let mut members = on(&self.expression, *day, &sources);
            for (id, yes) in &overrides {
                if *yes {
                    members.insert(id.clone());
                } else {
                    members.remove(id);
                }
            }
            for id in &members {
                let ranges = intervals.entry(id.clone()).or_default();
                if i > 0 && previous.contains(id) {
                    ranges.last_mut().unwrap().end = *day;
                } else {
                    ranges.push(DateRange {
                        start: *day,
                        end: *day,
                    });
                }
            }
            previous = members;
        }
        Ok(PoolMembership {
            // The recipe may include future dates, but only evaluated trading dates are verified.
            coverage: DateRange {
                start: dates[0],
                end: *dates.last().unwrap(),
            },
            intervals,
        })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn d(v: &str) -> Date {
        v.parse().unwrap()
    }
    fn symbols(ids: &[&str]) -> PoolExpression {
        PoolExpression::Symbols {
            instruments: ids.iter().map(|s| s.to_string()).collect(),
        }
    }
    fn definition(expression: PoolExpression) -> PoolDefinition {
        PoolDefinition {
            schema_version: 2,
            name: "test".into(),
            coverage: DateRange {
                start: d("2020-01-03"),
                end: d("2020-01-07"),
            },
            expression,
            adjustments: vec![],
        }
    }
    fn calendar() -> Vec<Date> {
        vec![d("2020-01-03"), d("2020-01-06"), d("2020-01-07")]
    }
    #[test]
    fn compositions_are_deduplicated_and_dated() {
        let mut p = definition(PoolExpression::Union {
            inputs: vec![symbols(&["SH600000"]), symbols(&["SH600000", "SZ000001"])],
        });
        p.adjustments.push(PoolAdjustment {
            effective_date: d("2020-01-04"),
            add: BTreeSet::new(),
            remove: BTreeSet::from(["SH600000".into()]),
        });
        let m = p.evaluate(&calendar(), |_, _| unreachable!()).unwrap();
        assert_eq!(m.on(d("2020-01-03")).len(), 2);
        assert_eq!(m.on(d("2020-01-06")), BTreeSet::from(["SZ000001".into()]));
        p.expression = PoolExpression::Intersection {
            inputs: vec![symbols(&["SH600000"]), symbols(&["SH600000", "SZ000001"])],
        };
        assert_eq!(
            p.evaluate(&calendar(), |_, _| unreachable!())
                .unwrap()
                .on(d("2020-01-03"))
                .len(),
            1
        );
        p.expression = PoolExpression::Difference {
            base: Box::new(symbols(&["SH600000"])),
            exclude: Box::new(symbols(&["SH600000"])),
        };
        assert!(
            p.evaluate(&calendar(), |_, _| unreachable!())
                .unwrap()
                .intervals
                .is_empty()
        );
    }
    #[test]
    fn conflicts_and_unsupported_coverage_fail() {
        let mut p = definition(symbols(&[]));
        p.adjustments.push(PoolAdjustment {
            effective_date: d("2020-01-04"),
            add: BTreeSet::from(["SH600000".into()]),
            remove: BTreeSet::from(["SH600000".into()]),
        });
        assert!(p.validate().is_err());
        p.adjustments.clear();
        p.expression = PoolExpression::Universe {
            version: "market-1".into(),
            universe: "csi300".into(),
        };
        assert!(
            p.evaluate(&calendar(), |_, _| Ok(PoolMembership {
                coverage: DateRange {
                    start: d("2020-01-06"),
                    end: d("2020-01-07")
                },
                intervals: BTreeMap::new()
            }))
            .is_err()
        );
    }
    #[test]
    fn future_adjustments_do_not_expand_verified_calendar_coverage() {
        let mut p = definition(symbols(&["SH600000"]));
        p.coverage.start = d("2019-01-01");
        p.coverage.end = d("2020-01-10");
        let before = p.evaluate(&calendar(), |_, _| unreachable!()).unwrap();
        p.adjustments.push(PoolAdjustment {
            effective_date: d("2020-01-09"),
            add: BTreeSet::from(["SZ000001".into()]),
            remove: BTreeSet::from(["SH600000".into()]),
        });
        let after = p.evaluate(&calendar(), |_, _| unreachable!()).unwrap();
        assert_eq!(before, after);
        assert_eq!(after.coverage.start, d("2020-01-03"));
        assert_eq!(after.coverage.end, d("2020-01-07"));
        assert!(after.on(d("2020-01-09")).is_empty());
        let mut extended = calendar();
        extended.push(d("2020-01-10"));
        let expanded = p.evaluate(&extended, |_, _| unreachable!()).unwrap();
        assert_eq!(
            expanded.on(d("2020-01-10")),
            BTreeSet::from(["SZ000001".into()])
        );
        assert_eq!(expanded.on(d("2020-01-03")), before.on(d("2020-01-03")));
    }
}
