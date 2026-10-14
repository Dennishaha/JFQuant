//! Immutable reviewed identity packages; source codes never replace security identity.
use crate::*;
use analytics_core::{Date, DateRange, pool::PoolMembership, security::SecurityRegistry};
use std::collections::BTreeMap;
/// Content-addressed portable evidence directory.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityVersion {
    /// Immutable directory ID.
    pub id: String,
    /// Full content hash.
    pub signature: String,
    /// Relative registry path.
    pub path: PathBuf,
}
impl Workspace {
    /// Verify/copy a current package without altering an existing version.
    pub fn import_identities(&self, path: &Path) -> Result<IdentityVersion> {
        let r = analytics_storage::security::read(path)?;
        let catalog = self.identity_versions()?;
        for old in catalog["versions"].as_array().into_iter().flatten() {
            let old_id = old["id"].as_str().ok_or("missing prior identity ID")?;
            let prior = self.identities(old_id)?;
            for (id, record) in &r.securities {
                if prior
                    .securities
                    .get(id)
                    .is_some_and(|old| old.issuer != record.issuer || old.kind != record.kind)
                {
                    return Err(
                        "immutable security ID cannot be reassigned to another issuer/type".into(),
                    );
                }
                if prior.securities.iter().any(|(old_id, old)| {
                    old_id != id
                        && old.issuer == record.issuer
                        && old.kind == record.kind
                        && old.lifetime.start == record.lifetime.start
                }) {
                    return Err(
                        "same issuer/issue must retain its previously registered security ID"
                            .into(),
                    );
                }
            }
        }
        let hash = signature(&r)?;
        let id = format!("identity-{}", &hash[7..23]);
        let dest = self.resolve(format!(".jfquant/identities/{id}"));
        if dest.exists() {
            return self.identity_version(&id);
        }
        let stage = self.resolve(format!(".jfquant/staging/identity-{}", Uuid::new_v4()));
        fs::create_dir_all(&stage)?;
        for e in r.evidence.values() {
            for file in [&e.original, &e.text] {
                let out = stage.join(file);
                fs::create_dir_all(out.parent().ok_or("evidence parent missing")?)?;
                fs::copy(path.parent().ok_or("package root missing")?.join(file), out)?;
            }
        }
        atomic_json(&stage.join("registry.json"), &r)?;
        analytics_storage::security::read(&stage.join("registry.json"))?;
        fs::create_dir_all(dest.parent().unwrap())?;
        fs::rename(stage, dest)?;
        self.identity_version(&id)
    }
    /// Validate directory content and evidence again on use.
    pub fn identity_version(&self, id: &str) -> Result<IdentityVersion> {
        if id.len() != 25
            || !id.starts_with("identity-")
            || !id[9..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("invalid identity version".into());
        }
        let path = PathBuf::from(format!(".jfquant/identities/{id}/registry.json"));
        let r = analytics_storage::security::read(&self.resolve(&path))?;
        let hash = signature(&r)?;
        if id != format!("identity-{}", &hash[7..23]) {
            return Err("identity directory changed".into());
        }
        Ok(IdentityVersion {
            id: id.into(),
            signature: hash,
            path,
        })
    }
    /// Verified selected directory.
    pub fn identities(&self, id: &str) -> Result<SecurityRegistry> {
        let v = self.identity_version(id)?;
        Ok(analytics_storage::security::read(&self.resolve(v.path))?)
    }
    /// List directories independently so one corrupt item cannot hide others.
    pub fn identity_versions(&self) -> Result<Value> {
        let root = self.resolve(".jfquant/identities");
        let mut versions = vec![];
        let mut errors = vec![];
        if root.exists() {
            for e in fs::read_dir(root)? {
                let path = e?.path();
                let Some(id) = path.file_name().and_then(|s| s.to_str()) else {
                    continue;
                };
                match self.identity_version(id) {
                    Ok(v) => versions.push(v),
                    Err(e) => errors.push(json!({"id":id,"error":e.to_string()})),
                }
            }
        }
        versions.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(json!({"versions":versions,"errors":errors}))
    }
    /// Export portable evidence, refusing an existing destination.
    pub fn export_identities(&self, id: &str, destination: &Path) -> Result<Value> {
        let v = self.identity_version(id)?;
        let r = self.identities(id)?;
        if destination.exists() {
            return Err("identity export destination already exists".into());
        }
        fs::create_dir_all(destination)?;
        let source = self.resolve(v.path).parent().unwrap().to_path_buf();
        for e in r.evidence.values() {
            for file in [&e.original, &e.text] {
                let out = destination.join(file);
                fs::create_dir_all(out.parent().unwrap())?;
                fs::copy(source.join(file), out)?;
            }
        }
        atomic_json(&destination.join("registry.json"), &r)?;
        Ok(json!({"id":id,"path":destination.join("registry.json")}))
    }
}
/// Split source membership at evidenced identity boundaries, retaining unresolved pieces.
pub fn resolve_pool(r: &SecurityRegistry, pool: &mut PoolMembership) -> Value {
    let mut out = BTreeMap::<String, Vec<DateRange>>::new();
    let mut issues = vec![];
    for (symbol, ranges) in &pool.intervals {
        for range in ranges {
            if analytics_core::security::is_security_id(symbol) {
                if !r.securities.contains_key(symbol) {
                    issues.push(
                        json!({"symbol":symbol,"coverage":range,"code":"unresolved_identity"}),
                    );
                }
                out.entry(symbol.clone()).or_default().push(range.clone());
                continue;
            }
            let mut boundaries = vec![range.start];
            if let Some(d) = range.end.succ_opt() {
                boundaries.push(d);
            }
            for s in r.securities.values() {
                for l in s
                    .locators
                    .iter()
                    .filter(|l| l.provider == "qlib" && l.symbol == *symbol)
                {
                    if l.coverage.start > range.start && l.coverage.start <= range.end {
                        boundaries.push(l.coverage.start);
                    }
                    if let Some(d) = l
                        .coverage
                        .end
                        .succ_opt()
                        .filter(|d| *d > range.start && *d <= range.end)
                    {
                        boundaries.push(d);
                    }
                }
            }
            boundaries.sort();
            boundaries.dedup();
            for start in boundaries.iter().filter(|d| **d <= range.end) {
                let end = boundaries
                    .iter()
                    .find(|d| **d > *start)
                    .and_then(|d| d.pred_opt())
                    .unwrap_or(range.end)
                    .min(range.end);
                let part = DateRange { start: *start, end };
                match r.resolve("qlib", symbol, Some(*start)) {
                    Ok(id) => out.entry(id.into()).or_default().push(part),
                    Err(e) => {
                        issues.push(json!({"symbol":symbol,"coverage":part,"code":"unresolved_identity","message":e}));
                        out.entry(symbol.clone()).or_default().push(part);
                    }
                }
            }
        }
    }
    pool.intervals = out;
    json!(issues)
}
/// Resolve a selector over a complete range without crossing different securities.
pub fn resolve_selector(r: &SecurityRegistry, symbol: &str, range: &DateRange) -> Result<String> {
    if analytics_core::security::is_security_id(symbol) {
        let record = r
            .securities
            .get(symbol)
            .ok_or("unknown selected security")?;
        if record.lifetime.start > range.end || record.lifetime.end < range.start {
            return Err("selected security has no required history overlap".into());
        }
        return Ok(symbol.into());
    }
    if let Ok(id) = r.resolve("exchange", symbol, None) {
        let record = &r.securities[id];
        if record.kind == "price_index"
            && record.lifetime.start <= range.end
            && record.lifetime.end >= range.end
        {
            return Ok(id.into());
        }
    }
    let a = r.resolve("exchange", symbol, Some(range.start))?;
    let b = r.resolve("exchange", symbol, Some(range.end))?;
    if a != b {
        return Err("code crosses securities; select an explicit security ID".into());
    }
    Ok(a.into())
}
/// Dated preview for both clients.
pub fn lookup(
    r: &SecurityRegistry,
    provider: &str,
    symbol: &str,
    date: Option<Date>,
) -> Result<Value> {
    let id = r.resolve(provider, symbol, date)?;
    Ok(json!({"security_id":id,"record":r.securities[id]}))
}
