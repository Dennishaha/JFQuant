//! Partial rights-issue facts and explicit research choices, independent of account support.
use chrono::NaiveDate as Date;
use serde::{Deserialize, Serialize};

fn decimal(text: &str) -> Result<f64, String> {
    if text.is_empty() || text.trim() != text || !text.is_ascii() {
        return Err("invalid rights decimal".into());
    }
    let v = text.parse::<f64>().map_err(|_| "invalid rights decimal")?;
    if !v.is_finite() || v < 0. {
        return Err("invalid rights decimal".into());
    }
    Ok(v)
}

/// Unknown values and evidence-backed inapplicability have different meanings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Fact<T> {
    /// No supported conclusion is available.
    #[default]
    Unknown,
    /// A reviewed fact; provenance remains in the immutable revision receipt.
    Confirmed {
        /// Exact value, including an explicitly evidenced zero.
        value: T,
    },
    /// Evidence states this field does not apply to the realized branch.
    NotApplicable {
        /// Explanation supported by the revision's field evidence.
        reason: String,
    },
}
impl<T> Fact<T> {
    /// Return only a confirmed value; inapplicability is never a default value.
    pub fn confirmed(&self) -> Option<&T> {
        match self {
            Self::Confirmed { value } => Some(value),
            _ => None,
        }
    }
    fn validate(&self) -> Result<(), String> {
        if matches!(self, Self::NotApplicable { reason } if reason.trim().is_empty()) {
            return Err("rights inapplicability requires an evidenced reason".into());
        }
        Ok(())
    }
}
/// Participation is explicit; the old unsupported-event skip is not an abandon policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Participation {
    /// Apply no subscription cash or new shares.
    #[default]
    Abandon,
    /// Subscribe subject to the selected cash policy.
    Participate,
}
/// Exactly one subscription attempt per registered entitlement.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Attempt {
    /// First eligible session in the verified payment window.
    #[default]
    FirstDay,
    /// Final eligible session in the verified payment window.
    LastDay,
}
/// Cash already reserved by open orders is excluded before this policy is applied.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InsufficientCash {
    /// Allocate proportionally, then distribute affordable units by security code.
    #[default]
    Partial,
    /// Process by security code; subscribe a whole event or abandon that event.
    AllOrAbandon,
    /// Stop without silently changing participation.
    Stop,
}
/// Frozen account choices; defaults do not claim evidence for event terms.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Policy {
    /// Whether registered rights are exercised.
    pub participation: Participation,
    /// Timing of the single attempt.
    pub attempt: Attempt,
    /// Treatment of insufficient unreserved cash.
    pub insufficient_cash: InsufficientCash,
}
/// Reviewed partial facts. This type deliberately permits incomplete acquisition.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Terms {
    /// Earliest verified disclosure; retrieval time is not disclosure time.
    pub announced: Fact<Date>,
    /// First payment date.
    pub subscription_start: Fact<Date>,
    /// Last payment date.
    pub subscription_end: Fact<Date>,
    /// CNY per subscribed share.
    pub price: Fact<String>,
    /// New shares per old registered share, not company-wide issuance.
    pub ratio: Fact<String>,
    /// Verified submission increment in shares.
    pub unit: Fact<u64>,
    /// CNY per subscribed share; an unknown fee is not zero.
    pub fee_per_share: Fact<String>,
    /// Allocated shares per subscribed share.
    pub allotment_ratio: Fact<String>,
    /// Date allocation becomes effective.
    pub allotment_date: Fact<Date>,
    /// Actual refund date, or evidenced inapplicability for a successful branch.
    pub refund_date: Fact<Date>,
    /// First date the new shares are sellable.
    pub listing_date: Fact<Date>,
    /// Conditional failure/refund clause, not an actual successful-issue date.
    pub conditional_refund: Fact<String>,
}
impl Terms {
    /// Validate known values and chronology without inventing missing values.
    pub fn validate(&self, record: Option<Date>, ex: Date) -> Result<(), String> {
        for field in [
            &self.announced,
            &self.subscription_start,
            &self.subscription_end,
            &self.allotment_date,
            &self.refund_date,
            &self.listing_date,
        ] {
            field.validate()?;
        }
        for field in [
            &self.price,
            &self.ratio,
            &self.fee_per_share,
            &self.allotment_ratio,
            &self.conditional_refund,
        ] {
            field.validate()?;
        }
        self.unit.validate()?;
        if matches!(self.refund_date, Fact::NotApplicable { .. })
            && self
                .allotment_ratio
                .confirmed()
                .is_some_and(|v| decimal(v).is_ok_and(|v| v < 1.))
        {
            return Err("unallocated principal requires an actual refund date".into());
        }
        if matches!(self.listing_date, Fact::NotApplicable { .. })
            && self
                .allotment_ratio
                .confirmed()
                .is_some_and(|v| decimal(v).is_ok_and(|v| v > 0.))
        {
            return Err("positive allocation requires actual listing terms".into());
        }
        for field in [
            &self.price,
            &self.ratio,
            &self.fee_per_share,
            &self.allotment_ratio,
        ] {
            if let Some(value) = field.confirmed() {
                decimal(value)?;
            }
        }
        if self.price.confirmed().is_some_and(|v| decimal(v) == Ok(0.))
            || self.ratio.confirmed().is_some_and(|v| decimal(v) == Ok(0.))
            || self.unit.confirmed() == Some(&0)
            || self
                .allotment_ratio
                .confirmed()
                .is_some_and(|v| decimal(v).is_ok_and(|v| v > 1.))
        {
            return Err("invalid known rights quantity/price".into());
        }
        let a = self.announced.confirmed();
        let start = self.subscription_start.confirmed();
        let end = self.subscription_end.confirmed();
        let allot = self.allotment_date.confirmed();
        let refund = self.refund_date.confirmed();
        let listing = self.listing_date.confirmed();
        if a.zip(record.as_ref()).is_some_and(|(a, r)| a > r)
            || a.is_some_and(|a| *a > ex)
            || start.zip(record.as_ref()).is_some_and(|(s, r)| s <= r)
            || start.zip(end).is_some_and(|(s, e)| s > e)
            || allot.zip(start).is_some_and(|(a, s)| a < s)
            || refund.zip(allot).is_some_and(|(r, a)| r < a)
            || listing.zip(allot).is_some_and(|(l, a)| l < a)
            || listing.is_some_and(|l| *l < ex)
        {
            return Err("conflicting known rights chronology".into());
        }
        Ok(())
    }
    /// Required participation gaps; conditional failure dates do not satisfy these.
    pub fn participation_gaps(&self) -> Vec<&'static str> {
        let mut missing = Vec::new();
        for (name, present) in [
            ("announced", self.announced.confirmed().is_some()),
            (
                "subscription_start",
                self.subscription_start.confirmed().is_some(),
            ),
            (
                "subscription_end",
                self.subscription_end.confirmed().is_some(),
            ),
            ("price", self.price.confirmed().is_some()),
            ("ratio", self.ratio.confirmed().is_some()),
            ("unit", self.unit.confirmed().is_some()),
            ("fee_per_share", self.fee_per_share.confirmed().is_some()),
            (
                "allotment_ratio",
                self.allotment_ratio.confirmed().is_some(),
            ),
            ("allotment_date", self.allotment_date.confirmed().is_some()),
            ("refund_date", !matches!(self.refund_date, Fact::Unknown)),
            (
                "listing_date",
                self.listing_date.confirmed().is_some()
                    || (matches!(self.listing_date, Fact::NotApplicable { .. })
                        && self
                            .allotment_ratio
                            .confirmed()
                            .is_some_and(|v| decimal(v) == Ok(0.))),
            ),
        ] {
            if !present {
                missing.push(name);
            }
        }
        missing
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_inapplicable_and_zero_are_distinct() {
        let mut t = Terms::default();
        let ex = "2022-01-27".parse().unwrap();
        t.validate(None, ex).unwrap();
        assert!(t.participation_gaps().contains(&"refund_date"));
        t.refund_date = Fact::NotApplicable {
            reason: "reviewed successful issue without unused funds".into(),
        };
        assert!(!t.participation_gaps().contains(&"refund_date"));
        assert!(t.participation_gaps().contains(&"fee_per_share"));
        t.fee_per_share = Fact::Confirmed { value: "0".into() };
        assert!(!t.participation_gaps().contains(&"fee_per_share"));
        t.unit = Fact::NotApplicable {
            reason: String::new(),
        };
        assert!(t.validate(None, ex).is_err());
        let bytes = serde_json::to_vec(&t).unwrap();
        assert_eq!(serde_json::from_slice::<Terms>(&bytes).unwrap(), t);
    }
    #[test]
    fn partial_facts_validate_without_becoming_executable() {
        let t = Terms {
            price: Fact::Confirmed {
                value: "14.43".into(),
            },
            ratio: Fact::Confirmed {
                value: "0.15".into(),
            },
            ..Default::default()
        };
        t.validate(
            Some("2022-01-18".parse().unwrap()),
            "2022-01-27".parse().unwrap(),
        )
        .unwrap();
        assert!(!t.participation_gaps().is_empty());
        let mut bad = t;
        bad.subscription_start = Fact::Confirmed {
            value: "2022-01-18".parse().unwrap(),
        };
        assert!(
            bad.validate(
                Some("2022-01-18".parse().unwrap()),
                "2022-01-27".parse().unwrap()
            )
            .is_err()
        );
        assert_eq!(Policy::default().participation, Participation::Abandon);
        assert_eq!(Policy::default().attempt, Attempt::FirstDay);
        assert_eq!(
            Policy::default().insufficient_cash,
            InsufficientCash::Partial
        );
    }
}
