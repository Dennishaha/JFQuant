//! Securities are identities; exchange codes and provider symbols are dated locators.
use crate::{Date, DateRange};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

/// Recognize opaque security keys, independent of exchange or provider codes.
pub fn is_security_id(id: &str) -> bool {
    id.strip_prefix("SEC-").is_some_and(|s| {
        s.len() == 32
            && s.bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    })
}
/// One captured original and its separately hashed readable extraction.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IdentityEvidence {
    /// Original bytes, relative to the evidence package.
    pub original: PathBuf,
    /// SHA256 of original bytes.
    pub original_sha256: String,
    /// Extracted text path, relative to the same package.
    pub text: PathBuf,
    /// SHA256 of extracted text.
    pub text_sha256: String,
    /// Auditable upstream URL, not executable content.
    pub source_url: String,
    /// Exact identifying passage.
    pub quote: String,
    /// supplier_identity, implementation, listing, or synthetic_fixture.
    pub kind: String,
    /// Extraction implementation version.
    pub parser_version: String,
}
/// An inclusive dated locator, never itself a security identity.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecurityLocator {
    /// exchange, qlib, akshare, yfinance, sina, or cninfo.
    pub provider: String,
    /// Exact symbol accepted by the named source.
    pub symbol: String,
    /// Explicitly evidenced dates.
    pub coverage: DateRange,
    /// Evidence keys in the directory.
    pub evidence: Vec<String>,
}
/// One independently identifiable security, including terminated securities.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecurityRecord {
    /// Immutable opaque key. Revisions must retain it for the same security.
    pub security_id: String,
    /// Legal issuer identity, independent of the trading code.
    pub issuer: String,
    /// Human-readable security name.
    pub name: String,
    /// a_share or price_index.
    pub kind: String,
    /// Evidenced identity interval; its end may be an observation cutoff, never an automatic delisting.
    pub lifetime: DateRange,
    /// Dated exchange and vendor locators.
    pub locators: Vec<SecurityLocator>,
    /// Issuer/lifecycle evidence keys.
    pub evidence: Vec<String>,
}
/// Immutable, reviewed identity directory; content hashes belong to the storage layer.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecurityRegistry {
    /// Evidence-backed succession facts; account execution still needs a complete action instance.
    pub successions: Vec<SecuritySuccession>,
    /// Current directory format, 1.
    pub schema_version: u32,
    /// Why this revision was made.
    pub reason: String,
    /// Synthetic fixtures cannot be mixed with public data.
    pub synthetic_fixture: bool,
    /// Reviewed records keyed by opaque ID.
    pub securities: BTreeMap<String, SecurityRecord>,
    /// Original/extracted evidence referenced by records and locators.
    pub evidence: BTreeMap<String, IdentityEvidence>,
}
/// Confirmed relationship without inventing missing entitlement/delivery dates.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SecuritySuccession {
    /// Original independently identified security.
    pub predecessor: String,
    /// Receiving independently identified security.
    pub successor: String,
    /// merger, reduction, reform, or reorganization.
    pub kind: String,
    /// Exact publicly documented shares per old share; not yet an executable entitlement.
    pub ratio: String,
    /// Optional reviewed event key once all required account dates are known.
    pub event_id: Option<String>,
    /// Matching implementation/listing evidence keys.
    pub evidence: Vec<String>,
    /// Why the two identities are connected and what remains unverified.
    pub reason: String,
}
impl SecurityRegistry {
    /// Validate identity and dated locator uniqueness without filesystem access.
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != 1 || self.reason.trim().is_empty() || self.securities.is_empty() {
            return Err("invalid security directory/version/reason".into());
        }
        let mut bindings = BTreeMap::<(&str, &str), Vec<(&str, &DateRange)>>::new();
        for link in &self.successions {
            if link.predecessor == link.successor
                || !self.securities.contains_key(&link.predecessor)
                || !self.securities.contains_key(&link.successor)
                || !matches!(
                    link.kind.as_str(),
                    "merger" | "reduction" | "reform" | "reorganization"
                )
                || link.reason.trim().is_empty()
                || crate::actions::decimal(&link.ratio)? <= 0.
            {
                return Err("invalid security succession".into());
            }
            if !self.synthetic_fixture
                && (link.evidence.is_empty()
                    || link.evidence.iter().any(|e| !self.evidence.contains_key(e)))
            {
                return Err("succession evidence missing".into());
            }
        }
        for (id, s) in &self.securities {
            if !is_security_id(id)
                || id != &s.security_id
                || s.issuer.trim().is_empty()
                || s.name.trim().is_empty()
                || !matches!(s.kind.as_str(), "a_share" | "price_index")
            {
                return Err("invalid security identity".into());
            }
            s.lifetime.validate().map_err(|e| e.to_string())?;
            if !self.synthetic_fixture
                && (s.evidence.is_empty() || s.locators.iter().any(|l| l.evidence.is_empty()))
            {
                return Err(format!("{id}: identity/lifecycle evidence missing"));
            }
            for key in s
                .evidence
                .iter()
                .chain(s.locators.iter().flat_map(|l| &l.evidence))
            {
                if !self.evidence.contains_key(key) {
                    return Err(format!("missing identity evidence {key}"));
                }
            }
            for l in &s.locators {
                l.coverage.validate().map_err(|e| e.to_string())?;
                if !matches!(
                    l.provider.as_str(),
                    "exchange" | "qlib" | "akshare" | "yfinance" | "sina" | "cninfo"
                ) || l.symbol.is_empty()
                    || !l.symbol.is_ascii()
                    || !l
                        .symbol
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
                    || l.coverage.start < s.lifetime.start
                    || l.coverage.end > s.lifetime.end
                {
                    return Err(format!("{id}: invalid locator/date coverage"));
                }
                if l.provider == "exchange" {
                    crate::pool::validate_exchange_symbol(&l.symbol)?;
                }
                let existing = bindings.entry((&l.provider, &l.symbol)).or_default();
                if existing
                    .iter()
                    .any(|(_, r)| r.start <= l.coverage.end && l.coverage.start <= r.end)
                {
                    return Err(format!(
                        "ambiguous dated locator {}:{}",
                        l.provider, l.symbol
                    ));
                }
                existing.push((id, &l.coverage));
            }
            for (i, a) in s.locators.iter().enumerate() {
                for b in s.locators.iter().skip(i + 1) {
                    if a.provider == b.provider
                        && matches!(a.provider.as_str(), "exchange" | "qlib")
                        && a.coverage.start <= b.coverage.end
                        && b.coverage.start <= a.coverage.end
                    {
                        return Err("security has overlapping exchange/source locators".into());
                    }
                }
            }
        }
        Ok(())
    }
    /// Resolve a dated source symbol; undated reused codes deliberately remain ambiguous.
    pub fn resolve(
        &self,
        provider: &str,
        symbol: &str,
        date: Option<Date>,
    ) -> Result<&str, String> {
        if is_security_id(symbol) {
            let s = self
                .securities
                .get(symbol)
                .ok_or("unknown security identity")?;
            if date.is_some_and(|d| d < s.lifetime.start || d > s.lifetime.end) {
                return Err("security outside evidenced lifetime".into());
            }
            return Ok(&s.security_id);
        }
        let matches: Vec<_> = self
            .securities
            .values()
            .filter(|s| {
                s.locators.iter().any(|l| {
                    l.provider == provider
                        && l.symbol == symbol
                        && date.is_none_or(|d| l.coverage.start <= d && d <= l.coverage.end)
                })
            })
            .collect();
        if matches.len() != 1 {
            return Err(format!("unresolved/ambiguous identity {provider}:{symbol}"));
        }
        Ok(&matches[0].security_id)
    }
    /// Exact exchange code for fees/trading, never the opaque account key.
    pub fn exchange_symbol(&self, id: &str, date: Date) -> Result<&str, String> {
        let s = self.securities.get(id).ok_or("unknown security identity")?;
        let locators: Vec<_> = s
            .locators
            .iter()
            .filter(|l| {
                l.provider == "exchange" && l.coverage.start <= date && date <= l.coverage.end
            })
            .collect();
        if locators.len() != 1 {
            return Err(format!("{id} {date}: exchange locator missing/ambiguous"));
        }
        Ok(&locators[0].symbol)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(id: u128, start: &str, end: &str) -> SecurityRecord {
        let range = DateRange {
            start: start.parse().unwrap(),
            end: end.parse().unwrap(),
        };
        SecurityRecord {
            security_id: format!("SEC-{id:032x}"),
            issuer: format!("issuer-{id}"),
            name: "fixture".into(),
            kind: "a_share".into(),
            lifetime: range.clone(),
            locators: vec![SecurityLocator {
                provider: "exchange".into(),
                symbol: "SH600018".into(),
                coverage: range,
                evidence: vec![],
            }],
            evidence: vec![],
        }
    }
    #[test]
    fn reused_code_is_two_identities_and_undated_lookup_fails() {
        let a = record(1, "2000-07-19", "2006-10-20");
        let b = record(2, "2006-10-26", "2026-10-12");
        let r = SecurityRegistry {
            successions: Vec::new(),
            schema_version: 1,
            reason: "fixture".into(),
            synthetic_fixture: true,
            securities: [(a.security_id.clone(), a), (b.security_id.clone(), b)].into(),
            evidence: BTreeMap::new(),
        };
        r.validate().unwrap();
        assert!(r.resolve("exchange", "SH600018", None).is_err());
        assert_eq!(
            r.resolve("exchange", "SH600018", Some("2005-01-01".parse().unwrap()))
                .unwrap(),
            format!("SEC-{:032x}", 1)
        );
        assert!(
            r.resolve("exchange", "SH600018", Some("2006-10-23".parse().unwrap()))
                .is_err()
        );
        let mut bad = r.clone();
        bad.securities.values_mut().last().unwrap().locators[0]
            .coverage
            .start = "2006-10-20".parse().unwrap();
        assert!(bad.validate().is_err());
    }
    #[test]
    fn public_identity_cannot_be_saved_without_evidence() {
        let a = record(1, "2000-01-01", "2026-01-01");
        let r = SecurityRegistry {
            successions: Vec::new(),
            schema_version: 1,
            reason: "review".into(),
            synthetic_fixture: false,
            securities: [(a.security_id.clone(), a)].into(),
            evidence: BTreeMap::new(),
        };
        assert!(r.validate().is_err());
    }
    #[test]
    fn code_change_preserves_positions_and_routes_fees_by_actual_execution_date() {
        use crate::{
            execution::ShareLedger,
            market_rules::{MarketRulesPreset, OrderSide},
        };
        let mut s = record(3, "2010-01-01", "2010-01-10");
        let id = s.security_id.clone();
        s.locators[0].coverage.end = "2010-01-03".parse().unwrap();
        let mut next = s.locators[0].clone();
        next.symbol = "SZ000018".into();
        next.coverage.start = "2010-01-04".parse().unwrap();
        next.coverage.end = s.lifetime.end;
        s.locators.push(next);
        let r = SecurityRegistry {
            successions: Vec::new(),
            schema_version: 1,
            reason: "same-security code change fixture".into(),
            synthetic_fixture: true,
            securities: [(id.clone(), s)].into(),
            evidence: BTreeMap::new(),
        };
        r.validate().unwrap();
        let rules = MarketRulesPreset::a_share_latest();
        let mut ledger = ShareLedger::new(10000., 0.).unwrap();
        ledger.security_registry = Some(r);
        ledger.begin_day("2010-01-03".parse().unwrap()).unwrap();
        let buy = ledger.fill(&id, OrderSide::Buy, 100, 10., &rules).unwrap();
        assert_eq!(
            buy.total(),
            rules
                .fees("SH600018", OrderSide::Buy, 1000.)
                .unwrap()
                .total()
        );
        ledger.end_day(&[]).unwrap();
        ledger.begin_day("2010-01-04".parse().unwrap()).unwrap();
        assert_eq!(ledger.positions[&id].shares, 100);
        let sell = ledger.fill(&id, OrderSide::Sell, 100, 10., &rules).unwrap();
        assert_eq!(
            sell.total(),
            rules
                .fees("SZ000018", OrderSide::Sell, 1000.)
                .unwrap()
                .total()
        );
        assert!(!ledger.positions.contains_key(&id));
    }
}
