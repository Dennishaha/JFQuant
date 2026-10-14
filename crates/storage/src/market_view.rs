//! Immutable explicit composition: restore Qlib raw values with its ORIGINAL scale,
//! then use independent coefficients for research. Source binaries are never overwritten.
use crate::adjustment::AdjustmentAnchor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    io::{self, Read},
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
    fn files(root: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
        for e in fs::read_dir(root)? {
            let e = e?;
            let ty = e.file_type()?;
            if ty.is_dir() {
                files(&e.path(), out)?;
            } else if ty.is_file() {
                out.push(e.path());
            } else {
                return Err(invalid("data versions cannot contain links"));
            }
        }
        Ok(())
    }
    let mut paths = vec![];
    files(root, &mut paths)?;
    paths.sort();
    let mut h = Sha256::new();
    let mut buf = vec![0; 1024 * 1024];
    for p in paths {
        analytics_core::task::TaskContext::current().check()?;
        let r = p
            .strip_prefix(root)
            .map_err(invalid)?
            .to_str()
            .ok_or_else(|| invalid("non-UTF8 data path"))?;
        h.update((r.len() as u64).to_le_bytes());
        h.update(r.as_bytes());
        let mut f = fs::File::open(p)?;
        h.update(f.metadata()?.len().to_le_bytes());
        loop {
            analytics_core::task::TaskContext::current().check()?;
            let n = f.read(&mut buf)?;
            if n == 0 {
                break;
            }
            h.update(&buf[..n]);
        }
    }
    Ok(format!("sha256:{:x}", h.finalize()))
}
/// Exact digest of one frozen file.
pub fn file_hash(p: &Path) -> io::Result<String> {
    Ok(format!("sha256:{:x}", Sha256::digest(fs::read(p)?)))
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
