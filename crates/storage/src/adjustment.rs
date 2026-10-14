//! Independent, dated multiplicative coefficients. Provider-adjusted quotations are not coefficients.
use analytics_core::{Date, pool::validate_instrument};
use serde::{Deserialize, Serialize};
use std::{fs, io, path::Path};

/// Current canonical price/volume transformation; included in source and research signatures.
pub const NORMALIZATION: &str = "explicit-hfq-first-raw-close-v1;shares;cny;nulls";
/// Public acquisition selection. None leaves a candidate unsuitable for strict research.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdjustmentSource {
    /// Independent dated factors from the fixed Sina endpoint documented by AKShare.
    SinaHfq,
    /// No verified adjustment factors (including Yahoo candidate prices).
    None,
}
/// Exact source decimal plus effective date; no rounding to quote precision.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdjustmentPoint {
    /// First effective calendar date.
    pub effective_date: Date,
    /// Original positive, finite decimal coefficient.
    pub factor: String,
}
/// Supported independent coefficient identities.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FactorSource {
    /// Documented, fixed Sina endpoint.
    SinaHfq,
    /// Explicit coefficients belonging only to a marked synthetic fixture.
    SyntheticFixture,
}
/// The basis is explicit; selecting a benchmark does not turn a stock into an index.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum AdjustmentBasis {
    /// Full independent coefficient history, never inferred from adjusted/raw prices.
    Explicit {
        /// Source identity.
        source: FactorSource,
        /// Fixed endpoint or synthetic fixture identity.
        source_url: String,
        /// Source was observed through this completed date; future dates are not supported.
        verified_through: Date,
        /// Ascending, unique effective dates. Original response order is preserved in raw evidence.
        points: Vec<AdjustmentPoint>,
    },
    /// Whitelisted, verified price index with identity coefficient.
    PriceIndex {
        /// Verified acquisition interface or synthetic fixture identity.
        source: String,
    },
    /// Saved candidate data without verified execution or factor inputs.
    Unavailable {
        /// Explicit admission failure reason.
        reason: String,
    },
}
/// Independent per-security file under adjustments/ID.json, format 1.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdjustmentRecord {
    /// Adjustment record schema, 1.
    pub schema_version: u32,
    /// Exact internal security identity.
    pub instrument_id: String,
    /// Exact source/exchange locator for origin checks, never an account key.
    pub source_symbol: String,
    /// Source and actual dated values.
    pub basis: AdjustmentBasis,
}
/// Frozen first valid raw closing observation and its independent coefficient.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdjustmentAnchor {
    /// Earliest raw close date in the snapshot.
    pub trade_date: Date,
    /// Source value at that date, never from a future point.
    pub factor: String,
}
fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_string())
}
/// Decode an exact source representation into the finite numeric computation value.
pub fn coefficient(value: &str) -> io::Result<f64> {
    if value.is_empty() || !value.is_ascii() || value.trim() != value {
        return Err(invalid("invalid adjustment coefficient"));
    }
    let v: f64 = value.parse().map_err(invalid)?;
    if !v.is_finite() || v <= 0.0 {
        return Err(invalid(
            "adjustment coefficient must be positive and finite",
        ));
    }
    Ok(v)
}
impl AdjustmentRecord {
    /// Validate source, securities, positivity and chronological uniqueness.
    pub fn validate(&self) -> io::Result<()> {
        validate_instrument(&self.instrument_id).map_err(invalid)?;
        analytics_core::pool::validate_exchange_symbol(&self.source_symbol).map_err(invalid)?;
        if self.schema_version != 3 {
            return Err(invalid("unsupported_data_format: adjustment record"));
        }
        match &self.basis {
            AdjustmentBasis::Explicit {
                source,
                source_url,
                verified_through,
                points,
            } => {
                if matches!(self.source_symbol.as_str(), "SH000300" | "SH000905") {
                    return Err(invalid("verified price index requires identity basis"));
                }
                let expected = match source {
                    FactorSource::SinaHfq => format!(
                        "https://finance.sina.com.cn/realstock/company/{}/hfq.js",
                        self.source_symbol.to_ascii_lowercase()
                    ),
                    FactorSource::SyntheticFixture => {
                        "JFQuant analytics-workflow synthetic-demo-v1".into()
                    }
                };
                if *source_url != expected
                    || points.is_empty()
                    || points
                        .windows(2)
                        .any(|w| w[0].effective_date >= w[1].effective_date)
                    || points[0].effective_date > *verified_through
                {
                    return Err(invalid("invalid adjustment source/history"));
                }
                for point in points {
                    coefficient(&point.factor)?;
                }
            }
            AdjustmentBasis::PriceIndex { source } => {
                if !matches!(self.source_symbol.as_str(), "SH000300" | "SH000905")
                    || !matches!(
                        source.as_str(),
                        "akshare.index_zh_a_hist" | "JFQuant analytics-workflow synthetic-demo-v1"
                    )
                {
                    return Err(invalid("unverified price-index identity"));
                }
            }
            AdjustmentBasis::Unavailable { reason } => {
                if reason.is_empty() {
                    return Err(invalid("missing adjustment admission reason"));
                }
            }
        }
        Ok(())
    }
    /// Load only the fixed security filename and require the matching identity.
    pub fn read(root: &Path, id: &str) -> io::Result<Self> {
        validate_instrument(id).map_err(invalid)?;
        let record: Self = serde_json::from_slice(&fs::read(
            root.join("adjustments").join(format!("{id}.json")),
        )?)
        .map_err(invalid)?;
        record.validate()?;
        if record.instrument_id != id {
            return Err(invalid("adjustment security mismatch"));
        }
        Ok(record)
    }
    /// Value effective on the given date. Only coefficients, never missing quotes, carry forward.
    pub fn on(&self, date: Date) -> io::Result<Option<f64>> {
        match &self.basis {
            AdjustmentBasis::Explicit {
                verified_through,
                points,
                ..
            } => {
                if date > *verified_through {
                    return Ok(None);
                }
                let index = points.partition_point(|p| p.effective_date <= date);
                index
                    .checked_sub(1)
                    .map(|i| coefficient(&points[i].factor))
                    .transpose()
            }
            AdjustmentBasis::PriceIndex { .. } => Ok(Some(1.0)),
            AdjustmentBasis::Unavailable { reason } => Err(invalid(reason)),
        }
    }
    /// Freeze the known source value at the first raw close, without backward filling.
    pub fn anchor(&self, date: Date) -> io::Result<AdjustmentAnchor> {
        let factor = match &self.basis {
            AdjustmentBasis::Explicit { points, .. } => {
                self.on(date)?
                    .ok_or_else(|| invalid("adjustment coverage missing before first raw close"))?;
                points[points.partition_point(|p| p.effective_date <= date) - 1]
                    .factor
                    .clone()
            }
            AdjustmentBasis::PriceIndex { .. } => "1".into(),
            AdjustmentBasis::Unavailable { reason } => return Err(invalid(reason)),
        };
        Ok(AdjustmentAnchor {
            trade_date: date,
            factor,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record() -> AdjustmentRecord {
        AdjustmentRecord {
            schema_version: 3,
            instrument_id: "SH600000".into(),
            source_symbol: "SH600000".into(),
            basis: AdjustmentBasis::Explicit {
                source: FactorSource::SinaHfq,
                source_url: "https://finance.sina.com.cn/realstock/company/sh600000/hfq.js".into(),
                verified_through: "2020-01-10".parse().unwrap(),
                points: vec![
                    AdjustmentPoint {
                        effective_date: "2020-01-03".parse().unwrap(),
                        factor: "2.0000000000000000".into(),
                    },
                    AdjustmentPoint {
                        effective_date: "2020-01-06".parse().unwrap(),
                        factor: "3".into(),
                    },
                ],
            },
        }
    }
    #[test]
    fn coefficients_are_dated_and_cannot_fill_from_the_future() {
        let r = record();
        r.validate().unwrap();
        assert_eq!(r.on("2020-01-02".parse().unwrap()).unwrap(), None);
        assert!(r.anchor("2020-01-02".parse().unwrap()).is_err());
        assert_eq!(r.on("2020-01-04".parse().unwrap()).unwrap(), Some(2.));
        assert_eq!(r.on("2020-01-06".parse().unwrap()).unwrap(), Some(3.));
        assert_eq!(r.on("2020-01-11".parse().unwrap()).unwrap(), None);
        assert_eq!(
            r.anchor("2020-01-04".parse().unwrap()).unwrap().factor,
            "2.0000000000000000"
        );
    }
    #[test]
    fn invalid_values_dates_and_index_labels_fail() {
        for v in ["0", "-1", "NaN", "inf", "1e9999", " 1", "1 "] {
            assert!(coefficient(v).is_err());
        }
        let mut r = record();
        if let AdjustmentBasis::Explicit { points, .. } = &mut r.basis {
            points[1].effective_date = points[0].effective_date;
        }
        assert!(r.validate().is_err());
        r.basis = AdjustmentBasis::PriceIndex {
            source: "akshare.index_zh_a_hist".into(),
        };
        assert!(r.validate().is_err());
    }
}
