//! Immutable explicit composition: restore Qlib raw values with its ORIGINAL scale,
//! then use independent coefficients for research. Source binaries are never overwritten.
pub mod supplement;
use crate::adjustment::AdjustmentAnchor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};
fn invalid(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}
/// Current lazy market view; paths resolve inside the selected workspace.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MarketView {
    /// Format 1, never automatically migrated.
    pub schema_version: u32,
    /// Base Qlib version's workspace-relative directory.
    pub base_path: PathBuf,
    /// Exact full base content digest.
    pub base_sha256: String,
    /// Frozen registered coefficient manifest, workspace-relative.
    pub coefficients_path: PathBuf,
    /// Coefficient manifest hash; its reader also verifies all evidence.
    pub coefficients_sha256: String,
    /// Original Qlib recovery contract, distinct from new research normalization.
    pub recovery: String,
    /// All copied coefficient files and hashes.
    pub records: BTreeMap<String, String>,
    /// First actual raw closing observations, never future-filled.
    pub anchors: BTreeMap<String, AdjustmentAnchor>,
}
/// Digest all regular files in deterministic relative-path order, rejecting links.
pub fn tree_hash(root: &Path) -> io::Result<String> {
    crate::evidence_store::tree_hash(root)
}
/// Exact digest of one frozen file.
pub fn file_hash(p: &Path) -> io::Result<String> {
    let mut input = crate::evidence_store::open(p)?;
    let mut hash = Sha256::new();
    let mut buffer = [0u8; 65536];
    loop {
        analytics_core::task::TaskContext::current().check()?;
        let n = std::io::Read::read(&mut input, &mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
    }
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(hash.finalize())
    ))
}
impl MarketView {
    /// Validate all dependencies before reading any research or account data.
    pub fn read(root: &Path) -> io::Result<(Self, PathBuf)> {
        let v: Self =
            serde_json::from_slice(&fs::read(root.join("market_view.json"))?).map_err(invalid)?;
        if v.schema_version != 1
            || v.recovery != "qlib-normalized-price-over-original-factor;cent-tick;shares-v1"
            || v.records.is_empty()
            || v.records.len() != v.anchors.len()
        {
            return Err(invalid("unsupported_data_format: market view"));
        }
        let workspace = root
            .ancestors()
            .find(|p| p.join(".jfquant/workspace.json").is_file())
            .ok_or_else(|| invalid("market view requires its selected workspace"))?
            .canonicalize()?;
        let resolve = |p: &Path| -> io::Result<PathBuf> {
            if p.is_absolute()
                || p.components()
                    .any(|c| !matches!(c, std::path::Component::Normal(_)))
            {
                return Err(invalid("invalid view dependency path"));
            }
            let p = workspace.join(p).canonicalize()?;
            if !p.starts_with(&workspace) {
                return Err(invalid("view dependency outside workspace"));
            }
            Ok(p)
        };
        let base = resolve(&v.base_path)?;
        if base.join("market_view.json").exists()
            || !base.join("calendars/day.txt").is_file()
            || tree_hash(&base)? != v.base_sha256
        {
            return Err(invalid("market_view_base_damaged"));
        }
        let coefficients = resolve(&v.coefficients_path)?;
        if file_hash(&coefficients)? != v.coefficients_sha256 {
            return Err(invalid("coefficient_version_damaged"));
        }
        let bundle = crate::collector::CoefficientBundle::read(&coefficients)?;
        if bundle.records != v.records || v.anchors.keys().ne(v.records.keys()) {
            return Err(invalid("market_view_coefficient_binding_mismatch"));
        }
        for (id, receipt) in &v.records {
            analytics_core::pool::validate_instrument(id).map_err(invalid)?;
            if file_hash(&root.join("adjustments").join(format!("{id}.json")))? != *receipt {
                return Err(invalid("market_view_coefficient_damaged"));
            }
            let record = crate::adjustment::AdjustmentRecord::read(root, id)?;
            let anchor = &v.anchors[id];
            let q = crate::qlib::QlibDataset::open(&base)?;
            let close = q.field(id, "close")?;
            let original = q.field(id, "factor")?;
            let first = q
                .calendar
                .iter()
                .enumerate()
                .find_map(|(i, d)| {
                    crate::qlib::raw_price(close.get(i), original.get(i)).map(|_| *d)
                })
                .ok_or_else(|| invalid("market_view_raw_anchor_missing"))?;
            if first != anchor.trade_date {
                return Err(invalid("market_view_first_close_mismatch"));
            }
            if record.anchor(anchor.trade_date)? != *anchor {
                return Err(invalid("market_view_anchor_mismatch"));
            }
        }
        Ok((v, base))
    }
}

#[cfg(test)]
mod hash_compatibility_tests {
    use super::*;
    #[test]
    fn sha256_vectors_and_frozen_tree_keep_legacy_digests() {
        // NIST vectors and a tree digest captured before the sha2 upgrade.
        for (input, expected) in [
            (
                b"".as_slice(),
                "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            ),
            (
                b"abc".as_slice(),
                "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            ),
        ] {
            assert_eq!(
                format!("{:x}", analytics_core::hash::Hex(Sha256::digest(input))),
                expected
            );
        }
        let tmp = tempfile::tempdir().unwrap();
        fs::write(tmp.path().join("a"), b"abc").unwrap();
        assert_eq!(
            file_hash(&tmp.path().join("a")).unwrap(),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            tree_hash(tmp.path()).unwrap(),
            "sha256:55726b7abfbe6468f6d7e95966cc9cc24af9f62d1111ad00ba21b0fc293ff277"
        );
    }
}
