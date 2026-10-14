//! Owned derivation staging and per-page receipts; never borrow another project's cache.
use super::*;
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    url: String,
    sha256: String,
}
fn receipt(path: &Path) -> io::Result<std::path::PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid("page filename missing"))?;
    let root = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| invalid("page root missing"))?;
    Ok(root
        .join("receipts")
        .join(format!("{}.json", name.to_string_lossy())))
}
fn verified(path: &Path) -> bool {
    receipt(path)
        .ok()
        .and_then(|r| fs::read(r).ok())
        .and_then(|b| serde_json::from_slice::<Receipt>(&b).ok())
        .is_some_and(|r| {
            fs::read(path).is_ok_and(|b| format!("sha256:{:x}", Sha256::digest(b)) == r.sha256)
        })
}
pub(super) fn prepare(
    source: &ActionBundle,
    source_root: &Path,
    output: &Path,
    method: &str,
    range: Value,
) -> io::Result<()> {
    source.validate_identity()?;
    task_context_check()?;
    let owner = serde_json::json!({"schema_version":3,"downloader":"JFQuant analytics-storage derivation","source_sha256":format!("sha256:{:x}",Sha256::digest(serde_json::to_vec(source).map_err(invalid)?)),"method":method,"range":range});
    if output.exists() {
        let saved: Value =
            serde_json::from_slice(&fs::read(output.join("owner.json"))?).map_err(invalid)?;
        if saved != owner || output.join("actions.json").exists() {
            return Err(invalid(
                "derivation staging identity mismatch or already completed",
            ));
        }
    } else {
        fs::create_dir(output)?;
        fs::create_dir(output.join("raw"))?;
        fs::create_dir(output.join("receipts"))?;
        fs::write(
            output.join("owner.json"),
            serde_json::to_vec(&owner).map_err(invalid)?,
        )?;
    }
    for (name, hash) in &source.raw_sha256 {
        task_context_check()?;
        let bytes = fs::read(source_root.join("raw").join(name))?;
        if format!("sha256:{:x}", Sha256::digest(&bytes)) != *hash {
            return Err(invalid("source evidence hash mismatch"));
        }
        let path = output.join("raw").join(name);
        if !verified(&path) {
            fs::write(path, bytes)?;
        }
    }
    Ok(())
}
pub(super) fn page(url: &str, command: &mut Command, path: &Path) -> io::Result<Vec<u8>> {
    task_context_check()?;
    let proof = receipt(path)?;
    if let Ok(bytes) = fs::read(&proof)
        && let Ok(r) = serde_json::from_slice::<Receipt>(&bytes)
        && r.url == url
        && verified(path)
    {
        return fs::read(path);
    }
    let response = crate::transport::curl(command)?;
    if !response.status.success() {
        return Err(invalid(format!(
            "page download failed: {}",
            String::from_utf8_lossy(&response.stderr)
        )));
    }
    let part = path.with_extension("partial");
    fs::write(&part, &response.stdout)?;
    fs::rename(part, path)?;
    let r = Receipt {
        url: url.into(),
        sha256: format!("sha256:{:x}", Sha256::digest(&response.stdout)),
    };
    let part = proof.with_extension("partial");
    fs::write(&part, serde_json::to_vec(&r).map_err(invalid)?)?;
    fs::rename(part, proof)?;
    Ok(response.stdout)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn receipts_resume_only_the_same_url_source_and_date_range() {
        let temp = tempfile::tempdir().unwrap();
        let source_root = temp.path().join("source");
        fs::create_dir_all(source_root.join("raw")).unwrap();
        let source = ActionBundle {
            schema_version: 4,
            source_bindings: Default::default(),
            coverage: Default::default(),
            downloader: "JFQuant analytics-storage corporate-actions".into(),
            downloaded_at: "2026-10-13".into(),
            queried_symbols: Default::default(),
            actions: vec![],
            failures: Default::default(),
            raw_sha256: Default::default(),
        };
        let output = temp.path().join("stage");
        prepare(
            &source,
            &source_root,
            &output,
            "details",
            serde_json::json!(["2013", "2026"]),
        )
        .unwrap();
        let path = output.join("raw/event.html");
        fs::write(&path, "saved page").unwrap();
        let r = Receipt {
            url: "https://example.test/page".into(),
            sha256: format!("sha256:{:x}", Sha256::digest(b"saved page")),
        };
        fs::write(receipt(&path).unwrap(), serde_json::to_vec(&r).unwrap()).unwrap();
        assert!(verified(&path));
        let bytes = page(&r.url, &mut Command::new("unused"), &path).unwrap();
        assert_eq!(bytes, b"saved page");
        prepare(
            &source,
            &source_root,
            &output,
            "details",
            serde_json::json!(["2013", "2026"]),
        )
        .unwrap();
        assert!(
            prepare(
                &source,
                &source_root,
                &output,
                "details",
                serde_json::json!(["2014", "2026"])
            )
            .is_err()
        );
        fs::write(&path, "tampered").unwrap();
        assert!(!verified(&path));
        assert!(page(&r.url, &mut Command::new("unused"), &path).is_err());
    }
}
