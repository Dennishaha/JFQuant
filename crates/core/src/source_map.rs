//! Snapshot-local exceptional source bindings. Ordinary market codes need no registration.
use crate::{Date, DateRange};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Inclusive source locator; used by acquisition and optional exceptional bindings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceLocator {
    /// qlib, exchange, akshare, yfinance or cninfo.
    pub provider: String,
    /// Exact upstream code; never substituted with a different historical issue.
    pub symbol: String,
    /// Dates for which this locator applies.
    pub coverage: DateRange,
    /// Existing source references, without a separate review workflow.
    pub evidence: Vec<String>,
}
/// An exception to ordinary direct-code reading, embedded in a data snapshot.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceException {
    /// Market code used for order rules and fees.
    pub exchange_symbol: String,
    /// a_share or price_index.
    pub kind: String,
    /// Known scope; outside this scope the exception cannot answer a query.
    pub coverage: DateRange,
    /// Provider-specific paths, with dated boundaries.
    pub locators: Vec<SourceLocator>,
}
/// Minimal mapping format 1. Empty means direct-code access, not an unreviewed directory.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceMap {
    /// Current format, 1.
    pub schema_version: u32,
    /// Only special aliases, code changes or reused codes need entries.
    pub exceptions: BTreeMap<String, SourceException>,
}
impl Default for SourceMap {
    fn default() -> Self {
        Self {
            schema_version: 1,
            exceptions: BTreeMap::new(),
        }
    }
}
/// Safe data key, distinct from the exchange-code grammar. No old SEC directory IDs.
pub fn validate_key(id: &str) -> Result<(), String> {
    if id.len() < 8
        || id.len() > 80
        || id.starts_with("SEC-")
        || !id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'_' | b'-'))
    {
        return Err(format!("invalid data key {id}"));
    }
    Ok(())
}
impl SourceMap {
    /// Validate only actual exceptions, without requiring records for ordinary securities.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 {
            return Err("unsupported source-map format".into());
        }
        let mut assigned: BTreeMap<(&str, &str), Vec<(&str, &DateRange)>> = BTreeMap::new();
        for (id, e) in &self.exceptions {
            validate_key(id)?;
            crate::pool::validate_exchange_symbol(&e.exchange_symbol)?;
            e.coverage.validate().map_err(|e| e.to_string())?;
            if !matches!(e.kind.as_str(), "a_share" | "price_index") {
                return Err("invalid source-map asset kind".into());
            }
            for (i, l) in e.locators.iter().enumerate() {
                validate_key(&l.symbol)?;
                l.coverage.validate().map_err(|e| e.to_string())?;
                if l.coverage.start < e.coverage.start || l.coverage.end > e.coverage.end {
                    return Err("source interval outside exception scope".into());
                }
                if e.locators[..i].iter().any(|p| {
                    p.provider == l.provider
                        && p.coverage.start <= l.coverage.end
                        && l.coverage.start <= p.coverage.end
                }) {
                    return Err("source_binding_ambiguous: overlapping dated locators".into());
                }
                if l.provider != "exchange" {
                    let ranges = assigned.entry((&l.provider, &l.symbol)).or_default();
                    if ranges.iter().any(|(other, range)| {
                        *other != id
                            && range.start <= l.coverage.end
                            && l.coverage.start <= range.end
                    }) {
                        return Err(
                            "source_binding_ambiguous: one source assigned to two data keys".into(),
                        );
                    }
                    ranges.push((id, &l.coverage));
                }
            }
        }
        Ok(())
    }
    /// Resolve market rules for this date; ordinary codes are accepted directly.
    pub fn exchange_symbol<'a>(&'a self, id: &'a str, date: Date) -> Result<&'a str, String> {
        if let Some(e) = self.exceptions.get(id) {
            if !e.coverage.contains(date) {
                return Err("source_interval_invalid".into());
            }
            let matching: Vec<_> = e
                .locators
                .iter()
                .filter(|l| l.provider == "exchange" && l.coverage.contains(date))
                .collect();
            return match matching.as_slice() {
                [] => Ok(&e.exchange_symbol),
                [l] => Ok(&l.symbol),
                _ => Err("source_binding_ambiguous".into()),
            };
        }
        crate::pool::validate_exchange_symbol(id)?;
        Ok(id)
    }
    /// Explicit asset kind, with verified built-in price-index codes as direct defaults.
    pub fn is_price_index(&self, id: &str) -> bool {
        self.exceptions
            .get(id)
            .map_or(matches!(id, "SH000300" | "SH000905"), |e| {
                e.kind == "price_index"
            })
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ordinary_codes_need_no_directory_and_special_aliases_do_not_join() {
        let mut m = SourceMap::default();
        assert_eq!(
            m.exchange_symbol("SH600018", "2016-01-01".parse().unwrap())
                .unwrap(),
            "SH600018"
        );
        m.exceptions.insert(
            "SHT00018".into(),
            SourceException {
                exchange_symbol: "SH600018".into(),
                kind: "a_share".into(),
                coverage: DateRange {
                    start: "2000-07-19".parse().unwrap(),
                    end: "2006-10-20".parse().unwrap(),
                },
                locators: vec![],
            },
        );
        m.validate().unwrap();
        assert!(
            m.exchange_symbol("SHT00018", "2016-01-01".parse().unwrap())
                .is_err()
        );
        assert!(!m.exceptions.contains_key("SH600018"));
    }
    #[test]
    fn overlapping_provider_intervals_are_rejected() {
        let range = DateRange {
            start: "2000-01-01".parse().unwrap(),
            end: "2001-01-01".parse().unwrap(),
        };
        let l = SourceLocator {
            provider: "qlib".into(),
            symbol: "SHT00018".into(),
            coverage: range.clone(),
            evidence: vec![],
        };
        let m = SourceMap {
            schema_version: 1,
            exceptions: BTreeMap::from([(
                "SHT00018".into(),
                SourceException {
                    exchange_symbol: "SH600018".into(),
                    kind: "a_share".into(),
                    coverage: range,
                    locators: vec![l.clone(), l],
                },
            )]),
        };
        assert!(m.validate().is_err());
    }
}
