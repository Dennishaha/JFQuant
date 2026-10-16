//! Typed A-share events and evidence-backed reference/entitlement rules.
use crate::{Date, pool::validate_instrument};
use serde::{Deserialize, Serialize};
type Result<T> = std::result::Result<T, String>;
/// Decimal text is preserved independently of source table rounding.
pub fn decimal(text: &str) -> Result<f64> {
    if text.is_empty() || text.trim() != text || !text.is_ascii() {
        return Err("invalid event decimal".into());
    }
    let value: f64 = text.parse().map_err(|_| "invalid event decimal")?;
    if !value.is_finite() || value < 0. {
        return Err("event amount must be finite and nonnegative".into());
    }
    Ok(value)
}
/// Explicit registrar handling of non-integral shares.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Allocation {
    /// Require integral entitlement; unknown registrar rounding is a missing input.
    #[default]
    Exact,
    /// An implementation announcement explicitly specifies floor allocation.
    Floor {
        /// Evidence supporting this allocation rule.
        evidence: String,
    },
    /// Fractional remainder receives an explicitly priced cash payment.
    CashInLieu {
        /// CNY per fractional share.
        price: String,
        /// Actual payment date.
        pay_date: Date,
        /// Allocation and compensation evidence.
        evidence: String,
    },
}
impl Allocation {
    /// Return listed integer quantity and fractional cash compensation, never guess rounding.
    pub fn allocate(&self, old: u64, ratio: f64) -> Result<(u64, f64)> {
        match self {
            Self::Floor { evidence } | Self::CashInLieu { evidence, .. }
                if evidence.trim().is_empty() =>
            {
                return Err("missing registrar allocation evidence".into());
            }
            Self::CashInLieu { price, .. } => {
                decimal(price)?;
            }
            _ => {}
        }
        let n = old as f64 * ratio;
        if !n.is_finite() || !(0.0..=1e12).contains(&n) {
            return Err("share allocation overflow".into());
        }
        if (n - n.round()).abs() < 1e-7 {
            return Ok((n.round() as u64, 0.));
        }
        match self {
            Self::Exact => Err("fractional shares require actual registrar allocation".into()),
            Self::Floor { evidence } if !evidence.is_empty() => Ok((n.floor() as u64, 0.)),
            Self::CashInLieu {
                price, evidence, ..
            } if !evidence.is_empty() => Ok((n.floor() as u64, (n - n.floor()) * decimal(price)?)),
            _ => Err("missing fractional allocation evidence".into()),
        }
    }
}
/// Reference price treatment, distinct from the ordinary holder's actual proceeds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Reference {
    /// Announcement says no exchange ex adjustment applies.
    Unchanged {
        /// Implementation evidence.
        evidence: String,
    },
    /// (previous - reference cash + subscription cash)/(reference share multiplier).
    Formula {
        /// Per-old-share reference dividend.
        cash: String,
        /// Total post-event share multiplier, not additional shares.
        share_multiplier: String,
        /// Per-old-share paid-in subscription capital used for the reference.
        subscription_cash: String,
        /// Implementation/reference evidence.
        evidence: String,
    },
    /// Exchange explicitly publishes the reference price for the effective date.
    Price {
        /// Verified unadjusted reference CNY.
        value: String,
        /// Exchange implementation evidence.
        evidence: String,
    },
}
impl Reference {
    /// Validate reference evidence and calculate the exchange's theoretical raw mark.
    pub fn price(&self, previous: f64) -> Result<f64> {
        let (p, e) = match self {
            Self::Unchanged { evidence } => (previous, evidence),
            Self::Formula {
                cash,
                share_multiplier,
                subscription_cash,
                evidence,
            } => {
                let m = decimal(share_multiplier)?;
                if m == 0. {
                    return Err("zero reference share multiplier".into());
                }
                (
                    (previous - decimal(cash)? + decimal(subscription_cash)?) / m,
                    evidence,
                )
            }
            Self::Price { value, evidence } => (decimal(value)?, evidence),
        };
        if e.is_empty() || !p.is_finite() || p <= 0. {
            return Err("invalid/missing exchange reference".into());
        }
        Ok(p)
    }
}
/// Ordinary-holder conversion category. No active optional exercise is implied.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConversionKind {
    /// Reverse split or capital reduction.
    Reduction,
    /// Share reform consideration.
    ShareReform,
    /// Restructuring allocation to identified beneficiaries.
    Reorganization,
    /// Mandatory merger consideration, without exercising a cash option.
    Merger,
    /// Same interest with a new canonical trading code.
    CodeChange,
}
/// Explicit event classification for evidence that covers historical adjustment only.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum HistoricalKind {
    /// Rights issue reference, without verified subscription settlement dates.
    RightsIssue,
    /// Mandatory capital reduction.
    Reduction,
    /// Share reform.
    ShareReform,
    /// Restructuring.
    Reorganization,
    /// Merger reference.
    Merger,
    /// Security identity change.
    CodeChange,
    /// Delisting reference.
    Delisting,
}
/// Evidence-based finite valuation for an untradeable claim.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Valuation {
    /// First effective valuation date.
    pub date: Date,
    /// CNY per old share (may be zero only with evidence of extinguishment).
    pub per_old_share: String,
    /// Last date on which this valuation is supported.
    pub valid_until: Date,
    /// Source supporting the valuation.
    pub evidence: String,
}
/// Special event terms; absent ordinary terms are an intentional provider default, not a legacy format adapter.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EventTerms {
    /// Plain cash dividend/bonus/transfer, using common event fields.
    Distribution {
        /// Verified ordinary bonus-share allocation; absence is unverified for fractional ratios.
        allocation: Option<Allocation>,
    },
    /// Complete historical reference evidence without executable settlement terms.
    Historical {
        /// Event class.
        category: HistoricalKind,
        /// Verified exchange reference, sufficient only for past factor inputs.
        reference: Reference,
    },
    /// Actual common fields differ from the exchange reference.
    Differential {
        /// Verified reference terms.
        reference: Reference,
        /// Registrar rounding.
        allocation: Allocation,
    },
    /// Entitlement to subscribe new shares for cash.
    RightsIssueFacts {
        /// Partial reviewed facts; absence does not imply inapplicability.
        facts: Box<crate::rights::Terms>,
        /// Independently evidenced exchange reference, when available.
        reference: Option<Reference>,
        /// Allocation rule remains unknown until explicitly reviewed.
        allocation: Option<Allocation>,
    },
    /// Complete legacy rights terms, retained for historical readers.
    RightsIssue {
        /// First subscription date, inclusive.
        subscription_start: Date,
        /// Final subscription date, inclusive.
        subscription_end: Date,
        /// CNY per subscribed share.
        price: String,
        /// New shares offered per old share.
        ratio: String,
        /// Minimum subscription step.
        unit: u64,
        /// Fee per subscribed share, no ordinary trade fee guessed.
        fee_per_share: String,
        /// Confirmed allocation per subscribed share.
        allotment_ratio: String,
        /// Date actual allocation becomes effective, distinct from payment and listing.
        allotment_date: Date,
        /// Date unused subscription cash returns.
        refund_date: Date,
        /// New shares' actual tradability.
        listing_date: Date,
        /// Exchange reference terms.
        reference: Reference,
        /// Registrar rounding rule.
        allocation: Allocation,
    },
    /// Mandatory conversion/cash consideration affecting existing shares.
    Conversion {
        /// Event classification.
        category: ConversionKind,
        /// Optional dated value of an undelivered successor claim, per old share.
        #[serde(default)]
        claim_valuation: Option<Valuation>,
        /// Successor, or None for a pure cash/extinguishment event.
        successor: Option<String>,
        /// Final successor shares per old share.
        ratio: String,
        /// Ordinary A-share holder receives this consideration when true.
        ordinary_holder: bool,
        /// True for additional consideration while old shares remain listed; ratio then denotes additional shares.
        retain_original: bool,
        /// Cash consideration per old share.
        cash: String,
        /// Actual cash date (required for positive proceeds).
        pay_date: Option<Date>,
        /// Successor tradability date (required for shares).
        listing_date: Option<Date>,
        /// Exchange reference for the predecessor (no price series splice).
        reference: Reference,
        /// Registrar rule.
        allocation: Allocation,
    },
    /// Quoted shares become nontradeable claims until a verified cash settlement.
    Delisting {
        /// Dated valuation evidence covering the claim period.
        valuations: Vec<Valuation>,
        /// Actual final cash per old share, if already known.
        settlement_cash: Option<String>,
        /// Actual final settlement date.
        settlement_date: Option<Date>,
        /// Exchange reference treatment.
        reference: Reference,
    },
}
impl Default for EventTerms {
    fn default() -> Self {
        Self::Distribution { allocation: None }
    }
}
impl EventTerms {
    /// Registrar policy for ordinary distributions and explicitly referenced special events.
    pub fn allocation(&self) -> Option<&Allocation> {
        match self {
            Self::Distribution { allocation } | Self::RightsIssueFacts { allocation, .. } => {
                allocation.as_ref()
            }
            Self::Differential { allocation, .. }
            | Self::RightsIssue { allocation, .. }
            | Self::Conversion { allocation, .. } => Some(allocation),
            _ => None,
        }
    }

    /// All canonical successor securities needed by data admission and ledger marking.
    pub fn successor(&self) -> Option<&str> {
        match self {
            Self::Conversion { successor, .. } => successor.as_deref(),
            _ => None,
        }
    }
    /// Return the explicit exchange reference, or None for the ordinary distribution formula.
    pub fn reference(&self) -> Option<&Reference> {
        match self {
            Self::Distribution { .. } => None,
            Self::RightsIssueFacts { reference, .. } => reference.as_ref(),
            Self::Historical { reference, .. } => Some(reference),
            Self::Differential { reference, .. }
            | Self::RightsIssue { reference, .. }
            | Self::Conversion { reference, .. }
            | Self::Delisting { reference, .. } => Some(reference),
        }
    }
    /// Structural and implementation checks; actual schedule coverage is checked separately.
    pub fn validate(&self, ex: Date) -> Result<()> {
        if let Some(Allocation::CashInLieu { pay_date, .. }) = self.allocation()
            && *pay_date < ex
        {
            return Err("fractional compensation cannot precede the effective date".into());
        }
        if let Some(r) = self.reference() {
            r.price(1e6)?;
        }
        match self {
            Self::RightsIssueFacts { facts, .. } => facts.validate(None, ex)?,
            Self::Distribution { allocation } => {
                if let Some(a) = allocation {
                    a.allocate(1, 1.)?;
                }
            }
            Self::Historical { .. } => {}
            Self::Differential { allocation, .. } => {
                allocation.allocate(1, 1.)?;
            }
            Self::RightsIssue {
                subscription_start,
                subscription_end,
                price,
                ratio,
                unit,
                fee_per_share,
                allotment_ratio,
                allotment_date,
                refund_date,
                listing_date,
                ..
            } => {
                if subscription_start > subscription_end
                    || allotment_date < subscription_start
                    || refund_date < allotment_date
                    || listing_date < allotment_date
                    || refund_date < subscription_start
                    || listing_date < subscription_start
                    || *unit == 0
                    || decimal(price)? <= 0.
                    || decimal(ratio)? <= 0.
                    || decimal(allotment_ratio)? > 1.
                {
                    return Err("invalid rights issue implementation".into());
                }
                decimal(fee_per_share)?;
            }
            Self::Conversion {
                successor,
                ratio,
                cash,
                pay_date,
                listing_date,
                ordinary_holder,
                retain_original,
                category,
                claim_valuation,
                ..
            } => {
                if let Some(v) = claim_valuation {
                    decimal(&v.per_old_share)?;
                    if v.date > ex || v.valid_until < v.date || v.evidence.is_empty() {
                        return Err("invalid successor claim valuation".into());
                    }
                }
                if *retain_original
                    && matches!(
                        category,
                        ConversionKind::Merger
                            | ConversionKind::Reduction
                            | ConversionKind::CodeChange
                    )
                {
                    return Err("conversion category cannot retain original listed shares".into());
                }
                let r = decimal(ratio)?;
                let c = decimal(cash)?;
                if let Some(id) = successor {
                    validate_instrument(id)?;
                }
                if *ordinary_holder
                    && ((r > 0. && (successor.is_none() || !listing_date.is_some_and(|d| d >= ex)))
                        || (c > 0. && !pay_date.is_some_and(|d| d >= ex)))
                {
                    return Err("missing conversion settlement/successor".into());
                }
                if matches!(category, ConversionKind::CodeChange)
                    && (r != 1. || c != 0. || successor.is_none())
                {
                    return Err("invalid code-change consideration".into());
                }
            }
            Self::Delisting {
                valuations,
                settlement_cash,
                settlement_date,
                ..
            } => {
                if valuations.is_empty()
                    || valuations[0].date > ex
                    || valuations.windows(2).any(|w| w[0].date >= w[1].date)
                {
                    return Err("missing/unordered delisting valuations".into());
                }
                for v in valuations {
                    decimal(&v.per_old_share)?;
                    if v.evidence.is_empty() || v.valid_until < v.date {
                        return Err("missing valuation evidence".into());
                    }
                }
                if settlement_cash.is_some() != settlement_date.is_some()
                    || settlement_date.is_some_and(|d| d < ex)
                {
                    return Err("incomplete delisting settlement".into());
                }
                if let Some(c) = settlement_cash {
                    decimal(c)?;
                }
            }
        }
        Ok(())
    }
}
/// Frozen passive ordinary-holder policies.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ActionPolicy {
    /// Subscribe on the first eligible date with available cash; false explicitly waives rights.
    pub subscribe_with_available_cash: bool,
    /// Always false in this ordinary-holder platform scope.
    pub exercise_optional_cash_choice: bool,
}
impl Default for ActionPolicy {
    fn default() -> Self {
        Self {
            subscribe_with_available_cash: true,
            exercise_optional_cash_choice: false,
        }
    }
}
