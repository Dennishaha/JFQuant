//! Whole-context Parquet factor artifacts. Receipts are verified on every reuse.
#![allow(missing_docs)]
use crate::research::pipeline::{Cancellation, PipelineError, Result, load_factor_cache};
use analytics_factor::{
    FactorEvent, FactorExecutor, FactorRegistry, FactorRunArtifact, FactorRunRequest,
    InMemoryFactorExecutor, Panel,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{atomic::Ordering, mpsc::Sender},
};
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub schema_version: u32,
    pub context_hash: String,
    pub artifact_path: PathBuf,
    pub sha256: String,
    pub manifest_sha256: String,
    pub cache_hit: bool,
}
fn hex(hash: Sha256) -> String {
    format!("sha256:{:x}", analytics_core::hash::Hex(hash.finalize()))
}
fn check(cancel: &Cancellation) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(PipelineError::Cancelled)
    } else {
        Ok(())
    }
}
fn file_hash(path: &Path, cancel: &Cancellation) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0; 65536];
    loop {
        check(cancel)?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Ok(hex(hasher))
}
struct HashWriter<'a>(&'a mut Sha256);
impl Write for HashWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
fn restore(
    root: &Path,
    signature: &str,
    panel: &Panel,
    request: &FactorRunRequest,
    cancel: &Cancellation,
) -> Result<(FactorRunArtifact, Receipt)> {
    let path = root.join("factors.parquet");
    let manifest_path = root.join("manifest.json");
    let mut receipt: Receipt = serde_json::from_slice(&std::fs::read(root.join("receipt.json"))?)?;
    if receipt.schema_version != 2
        || receipt.context_hash != signature
        || receipt.sha256 != file_hash(&path, cancel)?
        || receipt.manifest_sha256 != file_hash(&manifest_path, cancel)?
    {
        return Err(PipelineError::InvalidInput(
            "factor receipt mismatch".into(),
        ));
    }
    let cached = load_factor_cache(&path)?;
    if cached.factor_ids != request.factor_ids
        || cached.instrument_ids != panel.instrument_id
        || cached.trade_dates != panel.trade_date
        || cached.universe_id.as_deref() != Some(&request.universe_id)
    {
        return Err(PipelineError::InvalidInput(
            "factor context mismatch".into(),
        ));
    }
    let artifact = FactorRunArtifact {
        factor_ids: cached.factor_ids,
        rows: cached.instrument_ids.len(),
        values: cached.values,
        manifest_json: std::fs::read_to_string(manifest_path)?,
    };
    receipt.artifact_path = path;
    receipt.cache_hit = true;
    Ok((artifact, receipt))
}
pub fn compute(
    registry: FactorRegistry,
    panel: Panel,
    request: FactorRunRequest,
    context: &serde_json::Value,
    output: &Path,
    progress: Sender<FactorEvent>,
    cancel: Cancellation,
) -> Result<(FactorRunArtifact, Receipt)> {
    check(&cancel)?;
    panel.validate()?;
    let columns: BTreeMap<_, _> = panel.columns.iter().collect();
    let mut hasher = Sha256::new();
    serde_json::to_writer(
        HashWriter(&mut hasher),
        &(
            2,
            &request,
            context,
            registry.manifest_definitions(&request.factor_ids)?,
            &panel.instrument_id,
            &panel.trade_date,
            columns,
        ),
    )?;
    let context_hash = hex(hasher);
    let root = output
        .parent()
        .unwrap_or(output)
        .join(".factor-artifacts-v2")
        .join(context_hash.trim_start_matches("sha256:"));
    // Old, damaged or incomplete cache entries are misses. Readiness flags are never trusted.
    if let Ok((artifact, receipt)) = restore(&root, &context_hash, &panel, &request, &cancel) {
        check(&cancel)?;
        let _ = progress.send(FactorEvent::Finished {
            output_rows: artifact.rows,
        });
        return Ok((artifact, receipt));
    }
    check(&cancel)?;
    let instruments = panel.instrument_id.clone();
    let dates = panel.trade_date.clone();
    let universe = request.universe_id.clone();
    let artifact =
        InMemoryFactorExecutor { registry, panel }.compute(request, progress, cancel.clone())?;
    check(&cancel)?;
    let staging = root.with_extension(format!("{}.partial", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&staging)?;
    let result = (|| {
        artifact.write_parquet(
            staging.join("factors.parquet"),
            &instruments,
            &dates,
            &universe,
        )?;
        let manifest: serde_json::Value = serde_json::from_str(&artifact.manifest_json)?;
        crate::atomic_json(&staging.join("manifest.json"), &manifest)
            .map_err(|e| PipelineError::InvalidInput(e.to_string()))?;
        let receipt = Receipt {
            schema_version: 2,
            context_hash,
            artifact_path: root.join("factors.parquet"),
            sha256: file_hash(&staging.join("factors.parquet"), &cancel)?,
            manifest_sha256: file_hash(&staging.join("manifest.json"), &cancel)?,
            cache_hit: false,
        };
        crate::atomic_json(&staging.join("receipt.json"), &receipt)
            .map_err(|e| PipelineError::InvalidInput(e.to_string()))?;
        check(&cancel)?;
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        std::fs::rename(&staging, &root)?;
        Ok((artifact, receipt))
    })();
    if staging.exists() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}
