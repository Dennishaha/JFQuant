//! Lightweight file-change observation for display only; execution verifies content hashes.
use std::{
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};
/// Hash file names, sizes and modification times without reading market contents.
/// This is a stale-report hint, never an artifact reuse authorization.
pub fn source_stamp(market: &Path, universe: &Path) -> crate::Result<String> {
    fn collect(path: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        if path.is_dir() {
            for e in std::fs::read_dir(path)? {
                collect(&e?.path(), out)?;
            }
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("parquet"))
        {
            out.push(path.to_owned());
        }
        Ok(())
    }
    let mut paths = vec![];
    let three_factor = analytics_storage::three_factor::path(market);
    if three_factor.exists() {
        paths.push(three_factor);
    }
    for path in [market, universe] {
        if !path.exists() {
            return Err(format!("input missing: {}", path.display()).into());
        }
        collect(path, &mut paths)?;
    }
    paths.sort();
    let values = paths
        .iter()
        .map(|p| {
            let m = p.metadata()?;
            Ok((
                p,
                m.len(),
                m.modified()?
                    .duration_since(UNIX_EPOCH)
                    .map_err(std::io::Error::other)?
                    .as_nanos(),
            ))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    crate::signature(&values)
}
