//! Snapshot-local exceptional source bindings. Ordinary market codes need no registration.
use crate::{Date, DateRange};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Inclusive source locator; used by acquisition and optional exceptional bindings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourceLocator {
    /// qlib, exchange, hithink, tdxaidata or cninfo.
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
                if removed_source(&l.provider) {
                    return Err("removed_provider: reconfigure source map".into());
                }
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

/// Whether provenance identifies a source removed from the supported acquisition system.
pub fn removed_source(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "akshare",
        "yfinance",
        "tushare",
        "eastmoney",
        "sina",
        "aastocks",
        "新浪",
        "东方财富",
        "东财",
    ]
    .iter()
    .any(|source| value.contains(source))
}
/// Reject removed providers anywhere in structured provenance, including mixed evidence.
pub fn reject_removed_sources(value: &serde_json::Value) -> Result<(), String> {
    fn retired(value: &serde_json::Value, provenance: bool) -> bool {
        match value {
            serde_json::Value::String(s) => {
                (provenance || s.starts_with("https://") || s.starts_with("http://"))
                    && removed_source(s)
            }
            serde_json::Value::Array(v) => v.iter().any(|v| retired(v, provenance)),
            serde_json::Value::Object(v) => v.iter().any(|(k, v)| {
                removed_source(k)
                    || retired(
                        v,
                        matches!(
                            k.as_str(),
                            "provider"
                                | "providers"
                                | "source"
                                | "sources"
                                | "source_url"
                                | "source_rule"
                                | "url"
                                | "urls"
                                | "sdk"
                                | "downloader"
                                | "normalization"
                                | "filename"
                                | "function"
                                | "quotes"
                                | "actions"
                                | "execution"
                                | "members"
                                | "calendar"
                        ),
                    )
            }),
            _ => false,
        }
    }
    if retired(value, false) {
        Err("removed_provider: reacquire data or reconfigure sources; legacy evidence and checkpoints cannot be restored".into())
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod removed_sources_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn issuer_names_and_document_quotes_are_business_data_not_provider_identities() {
        assert!(reject_removed_sources(&json!({"provider":"cninfo","stockList":[{"code":"300059","zwjc":"东方财富"}],"quote":"东方财富股份有限公司"})).is_ok());
        assert!(
            reject_removed_sources(
                &json!({"provider":"hithink","priorities":{"quotes":["eastmoney"]}})
            )
            .is_err()
        );
    }
    #[test]
    fn nested_mixed_provenance_rejects_every_retired_source() {
        for source in [
            "akshare",
            "yfinance",
            "tushare",
            "eastmoney",
            "sina",
            "aastocks",
        ] {
            let value = json!({"provider":"hithink","evidence":[{"provider":"tdxaidata"},{"url":format!("https://{source}.example/history")}]});
            assert!(
                reject_removed_sources(&value)
                    .unwrap_err()
                    .contains("removed_provider")
            );
        }
        assert!(
            reject_removed_sources(
                &json!({"providers":["hithink","tdxaidata","cninfo"],"null":null})
            )
            .is_ok()
        );
    }
}
