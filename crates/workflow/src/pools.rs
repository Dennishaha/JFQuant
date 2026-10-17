//! Immutable pool catalog and resolution shared by App, CLI and the research worker.
use crate::{Result, Workspace, atomic_json, signature, version};
use analytics_core::{
    Date,
    pool::{PoolDefinition, PoolMembership},
};
use analytics_storage::daily::DailyDataset;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
/// Frozen recipe version. IDs are content-derived; changing a recipe creates a new version.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PoolVersion {
    /// Content-derived pool ID.
    pub id: String,
    /// Full recipe content hash.
    pub signature: String,
    /// Portable definition with immutable source references.
    pub definition: PoolDefinition,
}
impl Workspace {
    /// Save a validated pool recipe without modifying existing versions.
    pub fn save_pool(&self, definition: PoolDefinition) -> Result<PoolVersion> {
        definition.validate()?;
        let signature = signature(&definition)?;
        let id = format!("pool-{}", &signature[7..23]);
        let item = PoolVersion {
            id: id.clone(),
            signature,
            definition,
        };
        let path = self.resolve(format!(".jfquant/pools/{id}.json"));
        if path.exists() {
            return self.pool(&id);
        }
        atomic_json(&path, &item)?;
        Ok(item)
    }
    /// Read a pool version, rejecting modified content and invalid identities.
    pub fn pool(&self, id: &str) -> Result<PoolVersion> {
        if !id.starts_with("pool-")
            || id.len() != 21
            || !id[5..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("invalid pool version ID".into());
        }
        let p: PoolVersion = serde_json::from_slice(&fs::read(
            self.resolve(format!(".jfquant/pools/{id}.json")),
        )?)?;
        p.definition.validate()?;
        let hash = signature(&p.definition)?;
        if p.id != id || p.signature != hash || format!("pool-{}", &hash[7..23]) != id {
            return Err("pool version changed".into());
        }
        Ok(p)
    }
    /// List independently validated pool versions.
    pub fn pools(&self) -> Result<Vec<PoolVersion>> {
        let root = self.resolve(".jfquant/pools");
        if !root.exists() {
            return Ok(vec![]);
        }
        let mut out = vec![];
        for e in fs::read_dir(root)? {
            let p = e?.path();
            if p.extension().is_some_and(|e| e == "json") {
                let id = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .ok_or("invalid pool path")?;
                out.push(self.pool(id)?);
            }
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }
    /// Import a current pool definition; no migration or script evaluation.
    pub fn import_pool(&self, path: &Path) -> Result<PoolVersion> {
        self.save_pool(serde_json::from_slice(&fs::read(path)?)?)
    }
    /// Resolve a builtin universe against the selected data version, or a saved composed pool.
    pub fn resolve_pool(
        &self,
        id: &str,
        market: &str,
    ) -> Result<(PoolMembership, serde_json::Value)> {
        let m = version(self, market, "market")?;
        let dataset = DailyDataset::open(self.resolve(&m.path))?;
        if let Some(universe) = id.strip_prefix("index:") {
            let config = dataset.index_config(universe)?;
            let membership = dataset.pool(universe)?;
            let frozen = serde_json::json!({"pool_version":id,"market_version":market,"market_signature":m.signature,"index_contract":config.calculation_contract(),"membership":membership});
            return Ok((membership, frozen));
        }
        if let Some(universe) = id.strip_prefix("builtin:") {
            if !analytics_core::indices::find(universe).is_some_and(|i| i.symbol.is_some()) {
                return Err("unknown builtin pool".into());
            }
            let membership = dataset.pool(universe)?;
            let frozen = serde_json::json!({"pool_version":id,"market_version":market,"market_signature":m.signature,"membership":membership});
            return Ok((membership, frozen));
        }
        let p = self.pool(id)?;
        let inputs = std::cell::RefCell::new(std::collections::BTreeMap::new());
        let histories = std::cell::RefCell::new(std::collections::BTreeMap::new());
        let membership = p
            .definition
            .evaluate(&dataset.calendar, |version_id, universe| {
                if universe.is_empty() {
                    let a = self.history(version_id).map_err(|e| e.to_string())?;
                    let membership = a.segment.membership.clone();
                    histories.borrow_mut().insert(a.id.clone(), a);
                    return Ok(membership);
                }
                let v = version(self, version_id, "market").map_err(|e| e.to_string())?;
                inputs.borrow_mut().insert(v.id.clone(), v.clone());
                DailyDataset::open(self.resolve(v.path))
                    .and_then(|d| d.pool(universe))
                    .map_err(|e| e.to_string())
            })?;
        let frozen = serde_json::json!({"resolution_schema_version":1,"version":p,"membership":membership,"inputs":inputs.into_inner().into_values().collect::<Vec<_>>(),"histories":histories.into_inner().into_values().collect::<Vec<_>>()});
        Ok((membership, frozen))
    }
    /// Date preview using the same resolver as research.
    pub fn preview_pool(&self, id: &str, market: &str, day: Date) -> Result<serde_json::Value> {
        let (m, frozen) = self.resolve_pool(id, market)?;
        if day < m.coverage.start || day > m.coverage.end {
            return Err("pool preview date outside verified coverage".into());
        }
        Ok(
            serde_json::json!({"pool_version":id,"date":day,"instruments":m.on(day),"signature":signature(&frozen)?}),
        )
    }
}
