//! Dated, source-declared Fama–French inputs and reproducible local construction.
pub mod acquire;
pub mod build;
use crate::qlib::DailyField;
use analytics_core::Date;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    io,
    path::{Path, PathBuf},
};

/// Snapshot-local optional input. With a single market file, use its parent.
pub const FILE_NAME: &str = "three_factor_returns.json";
/// Return columns, in regression predictor order.
pub const FIELDS: [&str; 3] = ["mkt", "smb", "hml"];

/// One observation; absent values remain absent and dates are never forward filled.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Observation {
    /// Exchange-local effective trading date.
    pub trade_date: Date,
    /// Market factor return in decimal units (not percent).
    pub mkt: Option<f64>,
    /// Small-minus-big factor return in decimal units.
    pub smb: Option<f64>,
    /// High-minus-low factor return in decimal units.
    pub hml: Option<f64>,
}
/// Explicit provenance and units for externally obtained daily factors.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreeFactorReturns {
    /// Format 1.
    pub schema_version: u32,
    /// Identifiable provider, publication or dataset revision; required.
    pub source: String,
    /// Market/universe represented by these factor returns; required.
    pub market: String,
    /// Definition of the market return (e.g. provider MKT-RF); required.
    pub return_basis: String,
    /// Must be `decimal`; 0.01 means 1%.
    pub unit: String,
    /// Strictly increasing, unique effective dates.
    pub observations: Vec<Observation>,
}
fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}
/// Resolve the sidecar without changing an existing market file format.
pub fn path(market: &Path) -> PathBuf {
    if market.is_dir() {
        market.join(FILE_NAME)
    } else {
        market
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(FILE_NAME)
    }
}
impl ThreeFactorReturns {
    /// Read a source-declared file. Missing file stays `NotFound`, malformed data errors.
    pub fn read(path: &Path) -> io::Result<Self> {
        let value: Self = serde_json::from_slice(&std::fs::read(path)?).map_err(invalid)?;
        value.validate()?;
        Ok(value)
    }
    /// Check schema, provenance, units, chronological uniqueness and finite values.
    pub fn validate(&self) -> io::Result<()> {
        if self.schema_version != 1
            || self.unit != "decimal"
            || [&self.source, &self.market, &self.return_basis]
                .iter()
                .any(|s| s.trim().is_empty())
        {
            return Err(invalid(
                "three_factor_schema_source_market_basis_or_units_invalid",
            ));
        }
        let mut previous = None;
        for row in &self.observations {
            if previous.is_some_and(|d| row.trade_date <= d)
                || [row.mkt, row.smb, row.hml]
                    .into_iter()
                    .flatten()
                    .any(|v| !v.is_finite())
            {
                return Err(invalid("three_factor_dates_or_values_invalid"));
            }
            previous = Some(row.trade_date);
        }
        Ok(())
    }
    /// Align by effective date to a market calendar. No scaling, interpolation or fill.
    pub fn fields(&self, calendar: &[Date]) -> io::Result<BTreeMap<String, DailyField>> {
        self.validate()?;
        if calendar.windows(2).any(|ds| ds[0] >= ds[1]) {
            return Err(invalid("three_factor_calendar_not_strictly_increasing"));
        }
        let mut columns: [Vec<Option<f64>>; 3] =
            std::array::from_fn(|_| vec![None; calendar.len()]);
        for row in &self.observations {
            if let Ok(index) = calendar.binary_search(&row.trade_date) {
                for (column, value) in columns.iter_mut().zip([row.mkt, row.smb, row.hml]) {
                    column[index] = value;
                }
            }
        }
        Ok(FIELDS
            .into_iter()
            .zip(columns)
            .map(|(name, values)| (name.into(), DailyField { start: 0, values }))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> ThreeFactorReturns {
        serde_json::from_value(
            serde_json::json!({"schema_version":1,"source":"synthetic unit fixture",
            "market":"fixture","return_basis":"fixture decimal MKT","unit":"decimal",
            "observations":[{"trade_date":"2024-01-02","mkt":0.01,"smb":-0.02,"hml":null},
            {"trade_date":"2024-01-04","mkt":-0.03,"smb":0.02,"hml":0.04}]}),
        )
        .unwrap()
    }
    #[test]
    fn date_alignment_preserves_units_gaps_and_nulls_and_rejects_bad_provenance() {
        let mut data = fixture();
        let dates = ["2024-01-02", "2024-01-03", "2024-01-04"].map(|d| d.parse().unwrap());
        let columns = data.fields(&dates).unwrap();
        assert_eq!(columns["mkt"].values, [Some(0.01), None, Some(-0.03)]);
        assert_eq!(columns["hml"].values, [None, None, Some(0.04)]);
        data.observations.push(data.observations[0].clone());
        assert!(data.validate().is_err());
        data = fixture();
        data.unit = "percent".into();
        assert!(data.validate().is_err());
        data = fixture();
        data.source.clear();
        assert!(data.validate().is_err());
        data = fixture();
        data.observations[0].mkt = Some(f64::NAN);
        assert!(data.validate().is_err());
    }
}
