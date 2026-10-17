//! Explicit one-time conversion of a declared v1 artifact, outside all current readers.
use crate::{Result, Workspace, signature};
use analytics_storage::membership_history::{Request, document_path};
use serde_json::{Value, json};
use std::path::Path;
/// Recover an old frozen request after verifying its old content signature.
/// This never registers the old report or approves its membership; publish must revalidate v2.
pub(crate) fn artifact_request(w: &Workspace, path: &Path, value: &Value) -> Result<Request> {
    if value["schema_version"] != 1 {
        return Err("explicit artifact migration requires format 1".into());
    }
    let payload = json!([
        value["schema_version"],
        value["index"],
        value["market"],
        value["calendar"],
        value["source_map"],
        value["request"],
        value["report"],
        value["segment"]
    ]);
    let hash = signature(&payload)?;
    if value["signature"] != hash || value["id"] != format!("history-{}", &hash[7..]) {
        return Err("old artifact content signature failed".into());
    }
    let mut request =
        analytics_storage::membership_history_migration::migrate_v1(value["request"].clone())?;
    let source = document_path(&w.root, path)?;
    let parent = source
        .parent()
        .ok_or("artifact parent missing")?
        .strip_prefix(&w.root)?;
    for d in &mut request.documents {
        if d.path.is_absolute()
            || d.path
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err("invalid old artifact evidence path".into());
        }
        d.path = parent.join(&d.path);
        let bytes = std::fs::read(document_path(&w.root, &d.path)?)?;
        if analytics_storage::membership_history::bytes_hash(&bytes) != d.sha256 {
            return Err("old artifact evidence hash failed".into());
        }
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_old_artifact_conversion_rechecks_signature_and_never_trusts_ready() {
        let dir = tempfile::tempdir().unwrap();
        let w = Workspace::open(dir.path()).unwrap();
        let request = json!({"schema_version":1,"history":{"schema_version":1,"official_code":"746059","index_name":"Index","identity":[],"requested":{"start":"2021-09-17","end":"2021-09-17"},"anchors":[],"events":[],"reviews":[]},"documents":[]});
        let mut artifact = json!({"schema_version":1,"index":{},"market":{},"calendar":[],"source_map":{},"request":request,"report":{"ready":true},"segment":{},"signature":"","id":""});
        let hash = signature(&json!([
            artifact["schema_version"],
            artifact["index"],
            artifact["market"],
            artifact["calendar"],
            artifact["source_map"],
            artifact["request"],
            artifact["report"],
            artifact["segment"]
        ]))
        .unwrap();
        artifact["signature"] = json!(hash);
        artifact["id"] = json!(format!("history-{}", &hash[7..]));
        std::fs::write(
            w.resolve("artifact.json"),
            serde_json::to_vec(&artifact).unwrap(),
        )
        .unwrap();
        let migrated = artifact_request(&w, Path::new("artifact.json"), &artifact).unwrap();
        assert_eq!(migrated.schema_version, 2);
        assert!(migrated.history.anchors.is_empty());
        artifact["report"]["ready"] = json!(false);
        assert!(artifact_request(&w, Path::new("artifact.json"), &artifact).is_err());
    }
}
