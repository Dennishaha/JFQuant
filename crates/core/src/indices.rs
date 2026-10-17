//! Shared index identity catalogue. Catalogue membership never proves data coverage.
use serde::{Deserialize, Serialize};
use std::sync::OnceLock;
/// Explicit catalogue metadata version, independent of market manifests.
pub const SCHEMA_VERSION: u32 = 1;
/// Date of verification against the linked publishers; no constituent lists are embedded.
pub const VERIFIED_ON: &str = "2026-10-17";
/// A named index and its optional reviewed domestic quote route.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IndexDefinition {
    /// Stable application ID, also the builtin universe name when a route exists.
    pub id: String,
    /// Publisher-qualified display name.
    pub name: String,
    /// Publisher code, never automatically treated as an exchange symbol.
    pub official_code: String,
    /// Reviewed SH/SZ price-index locator; None means catalogue-only.
    pub symbol: Option<String>,
    /// Broad research role.
    pub category: String,
    /// Scope and outstanding integration limitations.
    pub description: String,
    /// Official publisher reference.
    pub source_url: String,
    /// Currency of the selected variant, or an explicit unresolved description.
    pub currency: String,
    /// Price/return basis of the route, or an explicit unresolved description.
    pub return_basis: String,
}
/// Immutable shared catalogue used by storage, CLI and App, without network or workspace reads.
pub fn all() -> &'static [IndexDefinition] {
    static ITEMS: OnceLock<Vec<IndexDefinition>> = OnceLock::new();
    ITEMS.get_or_init(|| {
        serde_json::from_str(include_str!("indices.json")).expect("builtin index catalogue")
    })
}
/// Resolve an exact application ID; unknown IDs never fall back to another index.
pub fn find(id: &str) -> Option<&'static IndexDefinition> {
    all().iter().find(|i| i.id == id)
}
/// Resolve a reviewed domestic price-index locator; ETF/stock codes are not aliases.
pub fn by_symbol(symbol: &str) -> Option<&'static IndexDefinition> {
    all().iter().find(|i| i.symbol.as_deref() == Some(symbol))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalogue_preserves_distinct_identities_and_only_routes_reviewed_symbols() {
        let mut ids = std::collections::BTreeSet::new();
        let mut symbols = std::collections::BTreeSet::new();
        for index in all() {
            assert!(ids.insert(&index.id));
            assert!(index.source_url.starts_with("https://"));
            if let Some(symbol) = &index.symbol {
                crate::pool::validate_exchange_symbol(symbol).unwrap();
                assert!(symbols.insert(symbol));
                assert_eq!(index.return_basis, "price");
                assert_eq!(index.currency, "CNY");
            }
        }
        assert_eq!(find("msci_a50").unwrap().official_code, "746059");
        assert_eq!(find("csia50").unwrap().official_code, "930050");
        assert!(find("msci_a50").unwrap().symbol.is_none());
        assert!(by_symbol("SH000001").is_some());
        assert!(by_symbol("SZ000001").is_none());
        assert!(by_symbol("SH560050").is_none());
        assert!(find("unknown").is_none());
    }
}

/// Portable runtime definition. No credentials, URLs to execute, or local paths.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct IndexConfig {
    /// Explicit portable format.
    pub schema_version: u32,
    /// Publisher identity and presentation metadata.
    pub definition: IndexDefinition,
    /// Stable snapshot key, distinct from provider codes and stock order symbols.
    pub data_key: String,
    /// Provider name to exact code accepted by that adapter.
    pub mappings: std::collections::BTreeMap<String, String>,
}
impl IndexConfig {
    /// Structural validation only; adapter capability is checked in storage.
    pub fn validate(&self) -> Result<(), String> {
        let d = &self.definition;
        if self.schema_version != 1
            || d.id.is_empty()
            || d.id.len() > 64
            || !d
                .id
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
            || d.name.trim().is_empty()
            || d.official_code.trim().is_empty()
            || d.currency.len() != 3
            || !d.currency.bytes().all(|b| b.is_ascii_uppercase())
            || !matches!(
                d.return_basis.as_str(),
                "price" | "net_return" | "gross_return"
            )
        {
            return Err("invalid index definition, currency or return basis".into());
        }
        crate::source_map::validate_key(&self.data_key)?;
        if !self.data_key.starts_with("IDX_") && d.symbol.as_deref() != Some(&self.data_key) {
            return Err("custom index requires an IDX_ data key".into());
        }
        if let Some(b) = find(&d.id) {
            if b.official_code != d.official_code
                || b.currency != d.currency
                || b.return_basis != d.return_basis
                || b.symbol.as_ref().is_some_and(|s| s != &self.data_key)
            {
                return Err("builtin index identity cannot be redefined".into());
            }
        } else if by_symbol(&self.data_key).is_some() || !self.data_key.starts_with("IDX_") {
            return Err("custom index cannot occupy a builtin or stock key".into());
        }
        for (provider, code) in &self.mappings {
            if !matches!(provider.as_str(), "hithink" | "tdxaidata" | "msci") {
                return Err("unsupported index provider".into());
            }
            validate_locator(code)?;
        }
        Ok(())
    }
    /// Only inputs relevant to price interpretation; presentation edits do not change this value.
    pub fn calculation_contract(&self) -> serde_json::Value {
        serde_json::json!({"schema_version":1,"id":self.definition.id,"official_code":self.definition.official_code,"data_key":self.data_key,"currency":self.definition.currency,"return_basis":self.definition.return_basis,"mappings":self.mappings})
    }
}
/// Safe opaque locator grammar; provider-specific restrictions belong to adapters.
pub fn validate_locator(code: &str) -> Result<(), String> {
    if code.is_empty()
        || code.len() > 64
        || !code
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    {
        return Err("invalid index provider code".into());
    }
    Ok(())
}
/// Seed configuration for an existing reviewed native route.
pub fn builtin_config(id: &str) -> Option<IndexConfig> {
    let definition = find(id)?.clone();
    let data_key = definition.symbol.clone()?;
    Some(IndexConfig {
        schema_version: 1,
        definition,
        data_key: data_key.clone(),
        mappings: std::collections::BTreeMap::from([
            ("hithink".into(), data_key.clone()),
            ("tdxaidata".into(), data_key),
        ]),
    })
}
