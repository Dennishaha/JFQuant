//! Raw corporate-action records, source terms and historical adjustment validation.
//! Account simulation belongs to AKQuant; this module contains no portfolio state.
use crate::{CoreError, Date};
use serde::{Deserialize, Serialize};
type Result<T> = std::result::Result<T, CoreError>;
fn invalid(message: &str) -> CoreError {
    CoreError::InvalidRequest(message.into())
}
fn positive(p: f64) -> bool {
    p.is_finite() && p > 0.0
}

/// Account-data trust is independent of factor missing-value and training-price policies.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventCheckPolicy {
    /// Accept complete structured histories; unresolved events require actual account exposure validation.
    #[default]
    Provider,
    /// Require independently audited coverage for every event category.
    Audited,
}
impl EventCheckPolicy {
    /// User-facing execution policy, shared by App and CLI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Provider => "供应商资料；实际持仓事件异常停止，特殊事件警告",
            Self::Audited => "要求全类别覆盖核验",
        }
    }
}

/// One implemented distribution. Missing settlement dates remain missing data.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CorporateAction {
    /// Stable source event key (instrument and ex date).
    pub id: String,
    /// Canonical A-share symbol.
    pub instrument: String,
    /// Public implementation announcement date.
    pub announced: Option<Date>,
    /// Holdings at this day's close establish entitlement.
    pub record_date: Option<Date>,
    /// Price loses the cash/share right before this day's opening auction.
    pub ex_date: Date,
    /// Optional actual cash payment date; AKQuant executes on the ex-date.
    pub pay_date: Option<Date>,
    /// Optional additional-share tradability date; retained as source evidence.
    pub listing_date: Option<Date>,
    /// Gross CNY dividend per old share, not per ten shares.
    #[serde(deserialize_with = "decimal_text")]
    pub cash_per_share: String,
    /// Additional shares per old share (bonus plus capital transfer).
    #[serde(deserialize_with = "decimal_text")]
    pub shares_per_share: String,
    /// Typed event terms for ordinary A-share holders.
    #[serde(default)]
    pub terms: crate::actions::EventTerms,
    /// Evidence URL(s), never used as executable instructions.
    pub source: String,
}
/// Legacy format 5 numbers are accepted only by the enclosing version reader.
fn decimal_text<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    let v = serde_json::Value::deserialize(d)?;
    match v {
        serde_json::Value::String(s) => Ok(s),
        serde_json::Value::Number(n) => n
            .as_f64()
            .map(|v| v.to_string())
            .ok_or_else(|| serde::de::Error::custom("invalid event decimal")),
        _ => Err(serde::de::Error::custom("event decimal must be text")),
    }
}
impl CorporateAction {
    /// Numeric boundary for historical reference calculations, never cross-source reconciliation.
    pub fn cash(&self) -> f64 {
        self.cash_per_share.parse().unwrap_or(f64::NAN)
    }
    /// Numeric boundary for historical reference calculations.
    pub fn shares(&self) -> f64 {
        self.shares_per_share.parse().unwrap_or(f64::NAN)
    }
    /// Exact total-share multiplier for the upstream decimal constructor.
    pub fn share_multiplier(&self) -> Result<String> {
        let (whole, fraction) = self
            .shares_per_share
            .split_once('.')
            .unwrap_or((&self.shares_per_share, ""));
        if !fraction.bytes().all(|b| b.is_ascii_digit()) {
            return Err(invalid("invalid share decimal"));
        }
        let whole: u128 = whole
            .parse()
            .map_err(|_| invalid("invalid share decimal"))?;
        let whole = whole
            .checked_add(1)
            .ok_or_else(|| invalid("share decimal overflow"))?;
        Ok(if fraction.is_empty() {
            whole.to_string()
        } else {
            format!("{whole}.{fraction}")
        })
    }

    /// Integer shareholder allocation plus explicit compensation.
    pub fn share_entitlement(&self, shares: u64) -> Result<(u64, f64)> {
        match &self.terms {
            crate::actions::EventTerms::Distribution {
                allocation: Some(allocation),
            }
            | crate::actions::EventTerms::Differential { allocation, .. } => allocation
                .allocate(shares, self.shares())
                .map_err(|e| invalid(&e)),
            _ => Ok((entitlement_shares(shares, self.shares())?, 0.)),
        }
    }

    /// Exchange reference is separate from actual holder rights.
    pub fn reference_price(&self, previous: f64) -> Result<f64> {
        match self.terms.reference() {
            Some(r) => r.price(previous).map_err(|e| invalid(&e)),
            None => {
                let p = (previous - self.cash()) / (1. + self.shares());
                if positive(p) {
                    Ok(p)
                } else {
                    Err(invalid("invalid distribution reference"))
                }
            }
        }
    }
    /// Validate implemented adjustment terms and chronology, without requiring settlement
    /// dates for historical factor inputs that never enter the execution account.
    pub fn validate_adjustment_terms(&self) -> Result<()> {
        crate::pool::validate_instrument(&self.instrument).map_err(|e| invalid(&e))?;
        self.terms.validate(self.ex_date).map_err(|e| invalid(&e))?;
        if !matches!(
            self.terms,
            crate::actions::EventTerms::Distribution { .. }
                | crate::actions::EventTerms::Differential { .. }
        ) && (self.cash() != 0. || self.shares() != 0.)
        {
            return Err(invalid(
                "special event rights must use typed terms, not duplicate common distribution fields",
            ));
        }
        if let crate::actions::EventTerms::RightsIssue {
            subscription_start, ..
        } = &self.terms
            && self.record_date.is_some_and(|d| *subscription_start <= d)
        {
            return Err(invalid("subscription must follow entitlement registration"));
        }
        if self.id.is_empty()
            || self.source.is_empty()
            || self
                .announced
                .zip(self.record_date)
                .is_some_and(|(a, r)| a > r)
            || self.announced.is_some_and(|d| d > self.ex_date)
            || self.record_date.is_some_and(|d| d >= self.ex_date)
            || !self.cash().is_finite()
            || self.cash() < 0.0
            || !self.shares().is_finite()
            || !(0.0..=100.0).contains(&self.shares())
            || (self.cash() + self.shares() == 0.0
                && matches!(self.terms, crate::actions::EventTerms::Distribution { .. }))
        {
            return Err(invalid(
                "invalid corporate-action identity, chronology or distribution",
            ));
        }
        Ok(())
    }
    /// Validate actual account rights, including verified payment/listing dates.
    pub fn validate(&self) -> Result<()> {
        self.validate_adjustment_terms()?;
        if matches!(self.terms, crate::actions::EventTerms::Historical { .. }) {
            return Err(invalid(
                "historical reference lacks actual account settlement terms",
            ));
        }
        if self.cash() > 0.0 && !self.pay_date.is_some_and(|d| d >= self.ex_date) {
            return Err(invalid(
                "cash dividend requires verified pay date >= ex date",
            ));
        }
        if self.shares() > 0.0 && !self.listing_date.is_some_and(|d| d >= self.ex_date) {
            return Err(invalid(
                "stock distribution requires verified listing date >= ex date",
            ));
        }
        Ok(())
    }
}

fn entitlement_shares(shares: u64, ratio: f64) -> Result<u64> {
    let value = shares as f64 * ratio;
    if !value.is_finite() || value > 1e12 || (value - value.round()).abs() > 1e-7 {
        return Err(invalid(
            "fractional bonus entitlement requires actual registrar allocation; cannot invent or discard shares",
        ));
    }
    Ok(value.round() as u64)
}
