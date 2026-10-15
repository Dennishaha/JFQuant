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
    /// Accept complete structured histories, retaining unknown special-category coverage warnings.
    #[default]
    Provider,
    /// Require independently audited coverage for every event category.
    Audited,
}
impl EventCheckPolicy {
    /// User-facing execution policy, shared by App and CLI.
    pub fn label(self) -> &'static str {
        match self {
            Self::Provider => "采用供应商资料，异常停止",
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
    pub announced: Date,
    /// Holdings at this day's close establish entitlement.
    pub record_date: Date,
    /// Price loses the cash/share right before this day's opening auction.
    pub ex_date: Date,
    /// Actual cash payment date, required only when cash is positive.
    pub pay_date: Option<Date>,
    /// Actual additional-share tradability date, required for bonus/transfer shares.
    pub listing_date: Option<Date>,
    /// Gross CNY dividend per old share, not per ten shares.
    pub cash_per_share: f64,
    /// Additional shares per old share (bonus plus capital transfer).
    pub shares_per_share: f64,
    /// Typed event terms for ordinary A-share holders.
    #[serde(default)]
    pub terms: crate::actions::EventTerms,
    /// Evidence URL(s), never used as executable instructions.
    pub source: String,
}
impl CorporateAction {
    /// Integer shareholder allocation plus explicit compensation.
    pub fn share_entitlement(&self, shares: u64) -> Result<(u64, f64)> {
        match &self.terms {
            crate::actions::EventTerms::Distribution {
                allocation: Some(allocation),
            }
            | crate::actions::EventTerms::Differential { allocation, .. } => allocation
                .allocate(shares, self.shares_per_share)
                .map_err(|e| invalid(&e)),
            _ => Ok((entitlement_shares(shares, self.shares_per_share)?, 0.)),
        }
    }

    /// Exchange reference is separate from actual holder rights.
    pub fn reference_price(&self, previous: f64) -> Result<f64> {
        match self.terms.reference() {
            Some(r) => r.price(previous).map_err(|e| invalid(&e)),
            None => {
                let p = (previous - self.cash_per_share) / (1. + self.shares_per_share);
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
        ) && (self.cash_per_share != 0. || self.shares_per_share != 0.)
        {
            return Err(invalid(
                "special event rights must use typed terms, not duplicate common distribution fields",
            ));
        }
        if let crate::actions::EventTerms::RightsIssue {
            subscription_start, ..
        } = &self.terms
            && *subscription_start <= self.record_date
        {
            return Err(invalid("subscription must follow entitlement registration"));
        }
        if self.id.is_empty()
            || self.source.is_empty()
            || self.announced > self.record_date
            || self.record_date >= self.ex_date
            || !self.cash_per_share.is_finite()
            || self.cash_per_share < 0.0
            || !self.shares_per_share.is_finite()
            || !(0.0..=100.0).contains(&self.shares_per_share)
            || (self.cash_per_share + self.shares_per_share == 0.0
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
        if self.cash_per_share > 0.0 && !self.pay_date.is_some_and(|d| d >= self.ex_date) {
            return Err(invalid(
                "cash dividend requires verified pay date >= ex date",
            ));
        }
        if self.shares_per_share > 0.0 && !self.listing_date.is_some_and(|d| d >= self.ex_date) {
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
