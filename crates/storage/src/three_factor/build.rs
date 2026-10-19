//! Point-in-time June 2×3 construction on explicitly dated historical inputs.
use super::{Observation, ThreeFactorReturns, invalid};
use crate::{daily::DailyDataset, qlib::DailyField};
use analytics_core::{Date, DateRange, pool::PoolMembership, research_quality::PriceBasis};
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, io};

/// Independent version of the construction, separate from GTJA191 formulas.
pub const VERSION: &str = "jfquant-cn-ff3-june-2x3-v2";
/// Only accepted portable input format; older files require explicit offline migration.
pub const SCHEMA_VERSION: u32 = 2;
/// Fixed portfolio order in all diagnostics.
pub const PORTFOLIOS: [&str; 6] = ["SL", "SN", "SH", "BL", "BN", "BH"];

/// Exact builder and daily-reader implementation identity, frozen by queued jobs.
pub fn implementation_signature() -> String {
    let mut hash = Sha256::new();
    for bytes in [
        include_bytes!("build.rs").as_slice(),
        include_bytes!("acquire.rs").as_slice(),
        include_bytes!("../daily.rs").as_slice(),
        include_bytes!("../three_factor.rs").as_slice(),
        include_bytes!("../qlib.rs").as_slice(),
        include_bytes!("../adjustment.rs").as_slice(),
    ] {
        hash.update(bytes);
        hash.update([0]);
    }
    format!("sha256:{:x}", analytics_core::hash::Hex(hash.finalize()))
}

/// A dated total-share observation, including the disclosure of its revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Shares {
    /// Exchange-qualified A-share identifier.
    pub instrument: String,
    /// First date the reported capital structure applies.
    pub effective_date: Date,
    /// Date this particular version became publicly available.
    pub published_on: Date,
    /// Actual shares, not ten-thousand shares; unknown values remain null.
    pub total_shares: Option<f64>,
}
/// An annual parent-common-equity observation in CNY, retaining each revision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BookEquity {
    /// Exchange-qualified A-share identifier.
    pub instrument: String,
    /// December 31 fiscal year end.
    pub period_end: Date,
    /// Publication date of this particular version, not the original report's date.
    pub published_on: Date,
    /// Parent common equity in CNY; nonpositive equity is ineligible.
    pub book_equity_cny: Option<f64>,
}
/// Explicit market-return convention; no implicit risk-free-rate substitution.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MarketBasis {
    /// Value-weighted adjusted simple stock returns.
    MarketReturn,
    /// The same market return minus the supplied daily risk-free return.
    ExcessReturn,
}
/// Independently supplied daily risk-free return in decimal units.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RiskFree {
    /// Exchange date, unique in the input.
    pub trade_date: Date,
    /// Daily return, not an annualized interest rate.
    pub value: Option<f64>,
}
/// Portable historical facts. These declarations do not certify market-wide coverage.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildRequest {
    /// Current input format, SCHEMA_VERSION.
    pub schema_version: u32,
    /// Must explicitly equal VERSION.
    pub construction_version: String,
    /// Provider/release/revision descriptions for universe, shares, book_equity and optionally rf.
    pub sources: BTreeMap<String, String>,
    /// Human-readable market and universe description.
    pub market: String,
    /// Inclusive output dates; history must also cover preceding formation dates.
    pub range: DateRange,
    /// Explicit price trust used to read the selected market's adjusted closing prices.
    pub price_basis: PriceBasis,
    /// Declared risk-free treatment.
    pub market_basis: MarketBasis,
    /// Effective historical listing/eligibility intervals, including subsequently delisted stocks.
    pub universe: PoolMembership,
    /// All effective-date and revision-date observations; no latest-value backfill.
    pub shares: Vec<Shares>,
    /// Annual observations; previous year's December equity is used in each June formation.
    pub book_equity: Vec<BookEquity>,
    /// Empty for market_return; dated decimal returns for excess_return.
    pub risk_free: Vec<RiskFree>,
    /// Optional native provider snapshots with an explicit historical availability assumption.
    /// Mutually exclusive with manually dated shares/book_equity; missing dates never carry.
    #[serde(deserialize_with = "required_history")]
    pub provider_history: Option<super::acquire::History>,
}
fn required_history<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<super::acquire::History>, D::Error> {
    Option::deserialize(deserializer)
}
impl BuildRequest {
    /// Validate units, identities, coverage and unambiguous revision keys before reading prices.
    pub fn validate(&self) -> io::Result<()> {
        self.range.validate().map_err(invalid)?;
        self.universe.validate().map_err(invalid)?;
        if self.schema_version != SCHEMA_VERSION
            || self.construction_version != VERSION
            || self.market.trim().is_empty()
            || self.universe.intervals.is_empty()
            || self.universe.coverage.start > self.range.start
            || self.universe.coverage.end < self.range.end
        {
            return Err(invalid("three_factor_build_schema_market_or_coverage"));
        }
        for name in ["universe", "shares", "book_equity"] {
            if self.sources.get(name).is_none_or(|s| s.trim().is_empty()) {
                return Err(invalid(format!("three_factor_build_missing_source:{name}")));
            }
        }
        let mut seen = std::collections::BTreeSet::new();
        for row in &self.shares {
            self.instrument(&row.instrument)?;
            if row.total_shares.is_some_and(|v| !v.is_finite() || v <= 0.)
                || !seen.insert((&row.instrument, row.effective_date, row.published_on))
            {
                return Err(invalid("three_factor_build_invalid_or_duplicate_shares"));
            }
        }
        seen.clear();
        for row in &self.book_equity {
            self.instrument(&row.instrument)?;
            if row.period_end.month() != 12
                || row.period_end.day() != 31
                || row.published_on <= row.period_end
                || row.book_equity_cny.is_some_and(|v| !v.is_finite())
                || !seen.insert((&row.instrument, row.period_end, row.published_on))
            {
                return Err(invalid(
                    "three_factor_build_invalid_or_duplicate_book_equity",
                ));
            }
        }
        if let Some(history) = &self.provider_history {
            history.validate(self)?;
        }
        let mut dates = std::collections::BTreeSet::new();
        for row in &self.risk_free {
            if !dates.insert(row.trade_date) || row.value.is_some_and(|v| !v.is_finite() || v < -1.)
            {
                return Err(invalid("three_factor_build_invalid_or_duplicate_rf"));
            }
        }
        match self.market_basis {
            MarketBasis::MarketReturn if !self.risk_free.is_empty() => {
                return Err(invalid("three_factor_build_unused_rf"));
            }
            MarketBasis::ExcessReturn
                if self.sources.get("rf").is_none_or(|s| s.trim().is_empty()) =>
            {
                return Err(invalid("three_factor_build_missing_source:rf"));
            }
            _ => {}
        }
        Ok(())
    }
    fn instrument(&self, id: &str) -> io::Result<()> {
        analytics_core::pool::validate_exchange_symbol(id).map_err(invalid)?;
        if !self.universe.intervals.contains_key(id) {
            return Err(invalid("three_factor_build_fact_outside_universe"));
        }
        Ok(())
    }
}

/// Exact June grouping and exclusions, kept even when the resulting factors are null.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Formation {
    /// June formation year.
    pub year: i32,
    /// Last trading day in June, when covered by the selected calendar.
    pub formation_date: Option<Date>,
    /// Last trading day in the preceding December.
    pub december_date: Option<Date>,
    /// Size median, book-to-market 30th percentile and 70th percentile (R7).
    pub breakpoints: Option<[f64; 3]>,
    /// Members in the fixed SL/SN/SH/BL/BN/BH order.
    pub groups: [Vec<String>; 6],
    /// Per-security exclusion or unavailable fact.
    pub exclusions: BTreeMap<String, String>,
    /// Missing calendar/history/facts prevent an auditable formation.
    pub problems: Vec<String>,
}
/// Daily portfolio returns and missing-data diagnostics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DayReport {
    /// Output date.
    pub trade_date: Date,
    /// Number of market members active at both endpoints.
    pub market_members: usize,
    /// Number of active members in each fixed portfolio.
    pub portfolio_members: [usize; 6],
    /// Nullable returns, without reweighting around missing observations.
    pub portfolio_returns: [Option<f64>; 6],
    /// Missing facts per market/portfolio/rf; values identify the affected securities.
    pub missing: BTreeMap<String, Vec<String>>,
}
/// Serializable calculation report; its bytes are frozen with the output snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildReport {
    /// Report format, 1.
    pub schema_version: u32,
    /// Exact independent construction version.
    pub construction_version: String,
    /// Fixed diagnostic portfolio column order.
    pub portfolio_order: [String; 6],
    /// No claim of matching a provider's stock-selection or accounting rules.
    pub coverage_basis: String,
    /// Every formation used by the requested output range.
    pub formations: Vec<Formation>,
    /// One report per output date.
    pub days: Vec<DayReport>,
}

struct Prices {
    raw: DailyField,
    adjusted: DailyField,
}
struct Inputs<'a> {
    request: &'a BuildRequest,
    calendar: &'a [Date],
    prices: BTreeMap<String, Prices>,
    shares: BTreeMap<String, Vec<&'a Shares>>,
    books: BTreeMap<String, Vec<&'a BookEquity>>,
}
impl Inputs<'_> {
    fn cap(&self, id: &str, index: usize) -> Option<f64> {
        let date = self.calendar[index];
        let shares = if let Some(history) = &self.request.provider_history {
            history.shares(id, date)?
        } else {
            self.shares
                .get(id)?
                .iter()
                .filter(|r| r.effective_date <= date && r.published_on < date)
                .max_by_key(|r| (r.effective_date, r.published_on))?
                .total_shares?
        };
        let price = self.prices.get(id)?.raw.get(index)?;
        let cap = price * shares;
        (price > 0. && cap.is_finite() && cap > 0.).then_some(cap)
    }
    fn book(&self, id: &str, year: i32, formation: Date) -> Option<f64> {
        if let Some(history) = &self.request.provider_history {
            return history.book(id, year, formation);
        }
        self.books
            .get(id)?
            .iter()
            .filter(|r| r.period_end.year() == year && r.published_on < formation)
            .max_by_key(|r| r.published_on)?
            .book_equity_cny
    }
    fn active(&self, id: &str, date: Date) -> bool {
        self.request.universe.intervals[id]
            .iter()
            .any(|r| r.contains(date))
    }
    fn return_on(&self, id: &str, index: usize) -> Option<f64> {
        let prices = &self.prices.get(id)?.adjusted;
        let prior = prices.get(index.checked_sub(1)?)?;
        let current = prices.get(index)?;
        let value = current / prior - 1.;
        (prior > 0. && current > 0. && value.is_finite()).then_some(value)
    }
    fn weighted(&self, ids: &[String], index: usize) -> (Option<f64>, Vec<String>) {
        if ids.is_empty() || index == 0 {
            return (None, vec!["empty_portfolio_or_no_previous_session".into()]);
        }
        let mut missing = Vec::new();
        let mut pairs = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(pair) = self.cap(id, index - 1).zip(self.return_on(id, index)) {
                pairs.push(pair);
            } else {
                missing.push(id.clone());
            }
        }
        if !missing.is_empty() {
            return (None, missing);
        }
        // Rescale before summing so a finite capitalization cannot overflow the sum.
        let scale = pairs.iter().map(|p| p.0).fold(0., f64::max);
        let denominator: f64 = pairs.iter().map(|p| p.0 / scale).sum();
        let value: f64 = pairs
            .iter()
            .map(|p| (p.0 / scale / denominator) * p.1)
            .sum();
        if value.is_finite() {
            (Some(value), missing)
        } else {
            (None, vec!["nonfinite_portfolio_return".into()])
        }
    }
    fn formation(&self, year: i32) -> Formation {
        let june = self
            .calendar
            .iter()
            .rposition(|d| d.year() == year && d.month() == 6);
        let december = self
            .calendar
            .iter()
            .rposition(|d| d.year() == year - 1 && d.month() == 12);
        let mut result = Formation {
            year,
            formation_date: june.map(|i| self.calendar[i]),
            december_date: december.map(|i| self.calendar[i]),
            breakpoints: None,
            groups: std::array::from_fn(|_| vec![]),
            exclusions: BTreeMap::new(),
            problems: vec![],
        };
        let (Some(june), Some(december)) = (june, december) else {
            result
                .problems
                .push("formation_calendar_history_missing".into());
            return result;
        };
        let date = self.calendar[june];
        if !self
            .request
            .universe
            .coverage
            .contains(self.calendar[december])
            || !self.request.universe.coverage.contains(date)
        {
            result
                .problems
                .push("formation_universe_history_missing".into());
            return result;
        }
        let mut eligible = vec![];
        for id in self.request.universe.on(date) {
            if !self.active(&id, self.calendar[december]) {
                result
                    .exclusions
                    .insert(id, "not_listed_at_previous_december".into());
                continue;
            }
            let Some(book) = self.book(&id, year - 1, date) else {
                result
                    .exclusions
                    .insert(id, "annual_book_equity_missing_as_of_formation".into());
                result.problems.push("formation_facts_missing".into());
                continue;
            };
            if book <= 0. {
                result
                    .exclusions
                    .insert(id, "nonpositive_book_equity".into());
                continue;
            }
            let Some((size, dec_cap)) = self.cap(&id, june).zip(self.cap(&id, december)) else {
                result
                    .exclusions
                    .insert(id, "june_or_december_capitalization_missing".into());
                result.problems.push("formation_facts_missing".into());
                continue;
            };
            let bm = book / dec_cap;
            if !bm.is_finite() || bm <= 0. {
                result
                    .exclusions
                    .insert(id, "invalid_book_to_market".into());
                result.problems.push("formation_facts_missing".into());
                continue;
            }
            eligible.push((id, size, bm));
        }
        result.problems.sort();
        result.problems.dedup();
        if !result.problems.is_empty() {
            return result;
        }
        if eligible.is_empty() {
            result.problems.push("no_eligible_formation_members".into());
            return result;
        }
        let mut sizes = eligible.iter().map(|r| r.1).collect::<Vec<_>>();
        let mut bms = eligible.iter().map(|r| r.2).collect::<Vec<_>>();
        sizes.sort_by(f64::total_cmp);
        bms.sort_by(f64::total_cmp);
        let size = quantile(&sizes, 0.5);
        let low = quantile(&bms, 0.3);
        let high = quantile(&bms, 0.7);
        result.breakpoints = Some([size, low, high]);
        for (id, cap, bm) in eligible {
            let size_group = if cap <= size { 0 } else { 3 };
            let value_group = if bm <= low {
                0
            } else if bm <= high {
                1
            } else {
                2
            };
            result.groups[size_group + value_group].push(id);
        }
        result
    }
}
fn quantile(sorted: &[f64], p: f64) -> f64 {
    let index = (sorted.len() - 1) as f64 * p;
    let lo = index.floor() as usize;
    let hi = index.ceil() as usize;
    sorted[lo] * (1. - index.fract()) + sorted[hi] * index.fract()
}

/// Build daily factors from the selected immutable market and declared dated facts.
/// Missing constituents invalidate the affected weighted portfolio, never trigger reweighting.
pub fn calculate(
    dataset: &DailyDataset,
    request: &BuildRequest,
) -> io::Result<(ThreeFactorReturns, BuildReport)> {
    request.validate()?;
    let start = dataset
        .calendar
        .binary_search(&request.range.start)
        .map_err(|_| invalid("three_factor_build_start_not_in_calendar"))?;
    let end = dataset
        .calendar
        .binary_search(&request.range.end)
        .map_err(|_| invalid("three_factor_build_end_not_in_calendar"))?;
    if start == 0
        || !request
            .universe
            .coverage
            .contains(dataset.calendar[start - 1])
    {
        return Err(invalid("three_factor_build_previous_session_missing"));
    }
    let mut inputs = Inputs {
        request,
        calendar: &dataset.calendar,
        prices: BTreeMap::new(),
        shares: BTreeMap::new(),
        books: BTreeMap::new(),
    };
    for (n, id) in request.universe.intervals.keys().enumerate() {
        analytics_core::task::TaskContext::current().check()?;
        if n % 64 == 0 {
            analytics_core::task::TaskContext::current().report(format!(
                "三因子读取证券 {}/{}",
                n + 1,
                request.universe.intervals.len()
            ))?;
        }
        request.instrument(id)?;
        if matches!(request.price_basis, PriceBasis::Verified) {
            if !dataset.has_independent_coefficients() || dataset.uses_supplier_prices(id) {
                return Err(invalid(
                    "three_factor_build_independent_adjustment_required",
                ));
            }
            dataset.validate_coefficient_origin(id, false)?;
            dataset.validate_factor_basis(id)?;
        }
        inputs.prices.insert(
            id.clone(),
            Prices {
                raw: dataset.research_raw_field(id, "close", true)?,
                adjusted: dataset.research_field(id, "close", true)?,
            },
        );
    }
    for row in &request.shares {
        inputs
            .shares
            .entry(row.instrument.clone())
            .or_default()
            .push(row);
    }
    for row in &request.book_equity {
        inputs
            .books
            .entry(row.instrument.clone())
            .or_default()
            .push(row);
    }
    let rf: BTreeMap<_, _> = request
        .risk_free
        .iter()
        .map(|r| (r.trade_date, r.value))
        .collect();
    let mut formations = BTreeMap::new();
    let mut observations = vec![];
    let mut days = vec![];
    for index in start..=end {
        analytics_core::task::TaskContext::current().check()?;
        if (index - start) % 128 == 0 {
            analytics_core::task::TaskContext::current()
                .report(format!("三因子计算日期 {}", dataset.calendar[index]))?;
        }
        let date = dataset.calendar[index];
        let year = if date.month() > 6 {
            date.year()
        } else {
            date.year() - 1
        };
        let formation = formations
            .entry(year)
            .or_insert_with(|| inputs.formation(year));
        let active =
            |id: &String| inputs.active(id, date) && inputs.active(id, dataset.calendar[index - 1]);
        let ids: Vec<_> = request
            .universe
            .on(date)
            .into_iter()
            .filter(&active)
            .collect();
        let (mut mkt, missing) = inputs.weighted(&ids, index);
        let mut report = DayReport {
            trade_date: date,
            market_members: ids.len(),
            portfolio_members: [0; 6],
            portfolio_returns: [None; 6],
            missing: BTreeMap::new(),
        };
        if !missing.is_empty() {
            report.missing.insert("MKT".into(), missing);
        }
        if request.market_basis == MarketBasis::ExcessReturn {
            let rate = rf.get(&date).copied().flatten();
            if rate.is_none() {
                report
                    .missing
                    .insert("RF".into(), vec!["daily_risk_free_return_missing".into()]);
            }
            mkt = mkt.zip(rate).map(|(m, r)| m - r).filter(|v| v.is_finite());
        }
        if formation.problems.is_empty() {
            for (p, name) in PORTFOLIOS.iter().enumerate() {
                let members: Vec<_> = formation.groups[p]
                    .iter()
                    .filter(|id| active(id))
                    .cloned()
                    .collect();
                report.portfolio_members[p] = members.len();
                let (value, missing) = inputs.weighted(&members, index);
                report.portfolio_returns[p] = value;
                if !missing.is_empty() {
                    report.missing.insert((*name).into(), missing);
                }
            }
        } else {
            report
                .missing
                .insert("formation".into(), formation.problems.clone());
        }
        let p = report.portfolio_returns;
        let smb = match p {
            [Some(sl), Some(sn), Some(sh), Some(bl), Some(bn), Some(bh)] => {
                Some((sl + sn + sh) / 3. - (bl + bn + bh) / 3.)
            }
            _ => None,
        };
        let hml = match (p[0], p[2], p[3], p[5]) {
            (Some(sl), Some(sh), Some(bl), Some(bh)) => Some((sh + bh) / 2. - (sl + bl) / 2.),
            _ => None,
        };
        observations.push(Observation {
            trade_date: date,
            mkt,
            smb: smb.filter(|v| v.is_finite()),
            hml: hml.filter(|v| v.is_finite()),
        });
        days.push(report);
    }
    let factors = ThreeFactorReturns {
        schema_version: 1,
        source: format!(
            "{VERSION}; source-declared historical facts; {}",
            serde_json::to_string(&request.sources).map_err(invalid)?
        ),
        market: request.market.clone(),
        return_basis: format!(
            "{:?}; adjusted-close simple returns ({:?}); lagged raw close * total shares; June annual formation; decimal; no missing-value fill",
            request.market_basis, request.price_basis
        ),
        unit: "decimal".into(),
        observations,
    };
    factors.validate()?;
    Ok((
        factors,
        BuildReport {
            schema_version: 1,
            construction_version: VERSION.into(),
            portfolio_order: PORTFOLIOS.map(str::to_owned),
            coverage_basis:
                "caller-declared dated universe; not independently certified market-wide coverage"
                    .into(),
            formations: formations.into_values().collect(),
            days,
        },
    ))
}

#[cfg(test)]
mod tests;
