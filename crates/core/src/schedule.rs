//! Date-level research schedules shared by the service, GUI and CLI.
use crate::{Date, DateRange};
use chrono::{Datelike, Days, Months};
use serde::{Deserialize, Serialize};
type Result<T> = std::result::Result<T, String>;
/// Explicit inclusive model and execution windows.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Fold {
    /// Training observations; labels must also end within these bounds.
    pub train: DateRange,
    /// Optional early-stopping observations.
    pub validation: Option<DateRange>,
    /// Out-of-sample observations and trading days.
    pub test: DateRange,
}
impl Fold {
    /// Reject reversed, overlapping or forward-looking windows.
    pub fn validate(&self) -> Result<()> {
        self.train.validate().map_err(|e| e.to_string())?;
        self.test.validate().map_err(|e| e.to_string())?;
        if let Some(v) = &self.validation {
            v.validate().map_err(|e| e.to_string())?;
            if self.train.end >= v.start || v.end >= self.test.start {
                return Err("require train < validation < test".into());
            }
        } else if self.train.end >= self.test.start {
            return Err("require train < test".into());
        }
        Ok(())
    }
}
/// Unit for rolling the first fold, always relative to its original dates.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum StepUnit {
    /// Calendar days.
    Days,
    /// Calendar months, preserving original month-end dates.
    Months,
    /// Calendar years, preserving original month-end dates.
    Years,
    /// Positions in the selected verified exchange calendar.
    TradingDays,
}
/// Positive rolling interval.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// Number of units, at least one.
    pub count: u32,
    /// Time unit.
    pub unit: StepUnit,
}
/// Date-level public schedule; no year-only fallback parsing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
pub enum Schedule {
    /// Fit once on arbitrary dates.
    Single {
        /// Explicit windows.
        fold: Fold,
    },
    /// Shift the first fold until the selected last day.
    Rolling {
        /// First explicit windows; their lengths define subsequent windows.
        first: Fold,
        /// Positive offset between folds.
        step: Step,
        /// Keep the first training start fixed when true.
        expanding: bool,
        /// Explicit last execution date; None means the snapshot's last complete day.
        until: Option<Date>,
    },
    /// Independently specified windows, executed in test-date order.
    Manual {
        /// Explicit non-overlapping folds.
        folds: Vec<Fold>,
    },
}
/// Frozen requested and calendar-resolved dates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResolvedFold {
    /// Original user/generated dates.
    pub requested: Fold,
    /// Actual included exchange dates.
    pub actual: Fold,
    /// First available historical day for factor warmup.
    pub warmup_start: Date,
    /// Available warmup sessions before training starts, at most 756.
    pub warmup_sessions: usize,
}
fn shift(date: Date, count: u32, unit: StepUnit, calendar: &[Date]) -> Result<Date> {
    let next = match unit {
        StepUnit::Days => date.checked_add_days(Days::new(u64::from(count))),
        StepUnit::Months | StepUnit::Years => {
            let months = if unit == StepUnit::Years {
                count.checked_mul(12)
            } else {
                Some(count)
            }
            .ok_or("rolling offset overflow")?;
            date.checked_add_months(Months::new(months)).and_then(|d| {
                let last = date.succ_opt().is_some_and(|n| n.month() != date.month());
                if last {
                    let first = Date::from_ymd_opt(d.year(), d.month(), 1)?;
                    first.checked_add_months(Months::new(1))?.pred_opt()
                } else {
                    Some(d)
                }
            })
        }
        StepUnit::TradingDays => {
            let i = calendar.partition_point(|d| *d < date);
            calendar
                .get(
                    i.checked_add(count as usize)
                        .ok_or("rolling offset overflow")?,
                )
                .copied()
        }
    };
    next.ok_or_else(|| "rolling date exceeds calendar/date bounds".into())
}
impl Schedule {
    /// Annual 5+1 recipe represented using the same general rolling API.
    pub fn annual(first_test_year: i32) -> Self {
        let d = |y, m, n| Date::from_ymd_opt(y, m, n).expect("builtin dates");
        Self::Rolling {
            first: Fold {
                train: DateRange {
                    start: d(first_test_year - 6, 1, 1),
                    end: d(first_test_year - 2, 12, 31),
                },
                validation: Some(DateRange {
                    start: d(first_test_year - 1, 1, 1),
                    end: d(first_test_year - 1, 12, 31),
                }),
                test: DateRange {
                    start: d(first_test_year, 1, 1),
                    end: d(first_test_year, 12, 31),
                },
            },
            step: Step {
                count: 1,
                unit: StepUnit::Years,
            },
            expanding: false,
            until: None,
        }
    }
    /// Structural checks before selecting data or saving a template.
    pub fn validate(&self) -> Result<()> {
        match self {
            Self::Single { fold } => fold.validate(),
            Self::Manual { folds } => {
                if folds.is_empty() || folds.len() > 10000 {
                    return Err("require 1..10000 folds".into());
                }
                for f in folds {
                    f.validate()?;
                }
                let mut order: Vec<_> = folds.iter().collect();
                order.sort_by_key(|f| f.test.start);
                if order.windows(2).any(|w| w[0].test.end >= w[1].test.start) {
                    return Err("overlapping test folds".into());
                }
                Ok(())
            }
            Self::Rolling {
                first, step, until, ..
            } => {
                first.validate()?;
                if step.count == 0 || until.is_some_and(|e| e < first.test.start) {
                    return Err("invalid rolling step/end".into());
                }
                Ok(())
            }
        }
    }
    /// Resolve against a verified, complete exchange calendar; explicit ranges never clip.
    pub fn resolve(&self, calendar: &[Date]) -> Result<Vec<ResolvedFold>> {
        self.validate()?;
        if calendar.is_empty() || calendar.windows(2).any(|w| w[0] >= w[1]) {
            return Err("invalid trading calendar".into());
        }
        let last = *calendar.last().unwrap();
        let range = |r: &DateRange| -> Result<DateRange> {
            if r.start < calendar[0] || r.end > last {
                return Err(format!(
                    "schedule coverage unavailable: {}..{}",
                    r.start, r.end
                ));
            }
            let lo = calendar.partition_point(|d| *d < r.start);
            let hi = calendar.partition_point(|d| *d <= r.end);
            if lo >= hi {
                return Err("window contains no trading day".into());
            }
            Ok(DateRange {
                start: calendar[lo],
                end: calendar[hi - 1],
            })
        };
        let mut requested = match self {
            Self::Single { fold } => vec![fold.clone()],
            Self::Manual { folds } => folds.clone(),
            Self::Rolling {
                first,
                step,
                expanding,
                until,
            } => {
                let stop = until.unwrap_or(last);
                if stop > last {
                    return Err("explicit rolling end exceeds data coverage".into());
                }
                let mut out = Vec::new();
                for n in 0u32..10000 {
                    let count = step.count.checked_mul(n).ok_or("rolling offset overflow")?;
                    let move_range = |r: &DateRange| -> Result<DateRange> {
                        Ok(DateRange {
                            start: shift(r.start, count, step.unit, calendar)?,
                            end: shift(r.end, count, step.unit, calendar)?,
                        })
                    };
                    // Terminate before shifting any trailing range beyond a finite trading calendar.
                    let start = match shift(first.test.start, count, step.unit, calendar) {
                        Ok(d) => d,
                        Err(_) if step.unit == StepUnit::TradingDays => break,
                        Err(e) => return Err(e),
                    };
                    if start > stop {
                        break;
                    }
                    let mut f = Fold {
                        train: move_range(&first.train)?,
                        validation: first.validation.as_ref().map(move_range).transpose()?,
                        test: DateRange {
                            start,
                            end: shift(first.test.end, count, step.unit, calendar)
                                .unwrap_or(stop)
                                .min(stop),
                        },
                    };
                    if *expanding {
                        f.train.start = first.train.start;
                    }
                    out.push(f);
                }
                if out.len() == 10000 {
                    return Err("rolling schedule exceeds 10000 folds".into());
                }
                out
            }
        };
        requested.sort_by_key(|f| f.test.start);
        if requested.is_empty() {
            return Err("no test folds within data".into());
        }
        let out: Vec<_> = requested
            .into_iter()
            .map(|f| {
                f.validate()?;
                let actual = Fold {
                    train: range(&f.train)?,
                    validation: f.validation.as_ref().map(range).transpose()?,
                    test: range(&f.test)?,
                };
                actual.validate()?;
                let i = calendar.partition_point(|d| *d < actual.train.start);
                Ok(ResolvedFold {
                    requested: f,
                    actual,
                    warmup_start: calendar[i.saturating_sub(756)],
                    warmup_sessions: i.min(756),
                })
            })
            .collect::<Result<_>>()?;
        if out
            .windows(2)
            .any(|w| w[0].actual.test.end >= w[1].actual.test.start)
        {
            return Err("overlapping test folds".into());
        }
        Ok(out)
    }
}
impl Default for Schedule {
    fn default() -> Self {
        Self::annual(2013)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn d(s: &str) -> Date {
        s.parse().unwrap()
    }
    fn cal() -> Vec<Date> {
        (0..=1500)
            .map(|n| d("2018-01-01") + chrono::Duration::days(n))
            .filter(|d| d.weekday().number_from_monday() <= 5)
            .collect()
    }
    fn first() -> Fold {
        Fold {
            train: DateRange {
                start: d("2018-01-01"),
                end: d("2018-12-31"),
            },
            validation: None,
            test: DateRange {
                start: d("2019-01-01"),
                end: d("2019-03-31"),
            },
        }
    }
    #[test]
    fn month_end_leap_and_trading_offsets() {
        let c = cal();
        assert_eq!(
            shift(d("2020-02-29"), 12, StepUnit::Months, &c).unwrap(),
            d("2021-02-28")
        );
        assert_eq!(
            shift(d("2019-01-31"), 2, StepUnit::Months, &c).unwrap(),
            d("2019-03-31")
        );
        assert_eq!(
            shift(d("2019-01-04"), 1, StepUnit::TradingDays, &c).unwrap(),
            d("2019-01-07")
        );
    }
    #[test]
    fn modes_resolve_and_reject_overlap_coverage_leakage() {
        let c = cal();
        let s = Schedule::Single { fold: first() };
        assert_eq!(s.resolve(&c).unwrap()[0].actual.test.end, d("2019-03-29"));
        let r = Schedule::Rolling {
            first: first(),
            step: Step {
                count: 3,
                unit: StepUnit::Months,
            },
            expanding: true,
            until: Some(d("2019-12-31")),
        }
        .resolve(&c)
        .unwrap();
        assert_eq!(r.len(), 4);
        assert_eq!(r[3].actual.train.start, d("2018-01-01"));
        let mut f = first();
        f.test.start = f.train.end;
        assert!(Schedule::Single { fold: f }.resolve(&c).is_err());
        assert!(
            Schedule::Manual {
                folds: vec![first(), first()]
            }
            .resolve(&c)
            .is_err()
        );
        let mut f = first();
        f.test.end = d("2030-01-01");
        assert!(Schedule::Single { fold: f }.resolve(&c).is_err());
    }
}
