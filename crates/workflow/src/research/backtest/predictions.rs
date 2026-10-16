//! Atomic, hash-verified OOS predictions; account inputs are deliberately excluded.
use super::*;
use analytics_core::schedule::ResolvedFold;
use arrow_array::{Array, Float64Array, StringArray};
type Restored = (Vec<(Date, String, f64)>, Vec<FoldReport>, serde_json::Value);
pub(super) fn training_source(source: &Path) -> Result<(PathBuf, Option<String>)> {
    let mut current = source.to_owned();
    for _ in 0..32 {
        if current.join("training.json").is_file() {
            return Ok((current, None));
        }
        let run: serde_json::Value = serde_json::from_slice(&fs::read(current.join("run.json"))?)?;
        let frozen = run["request"]
            .get("workflow_snapshot")
            .unwrap_or(&run["request"]);
        if let Some(training) = frozen["request"].get("training") {
            let id: uuid::Uuid = serde_json::from_value(training.clone())?;
            return Ok((
                current
                    .parent()
                    .ok_or("prediction task parent missing")?
                    .join(id.to_string()),
                Some(
                    frozen["training_artifact_sha256"]
                        .as_str()
                        .ok_or("training receipt missing")?
                        .into(),
                ),
            ));
        }
        let id: uuid::Uuid = serde_json::from_value(frozen["request"]["source"].clone())?;
        current = current
            .parent()
            .ok_or("prediction source parent missing")?
            .join(id.to_string());
    }
    Err("prediction provenance cycle or excessive depth".into())
}
fn identity(request: &ResearchRequest, factors: &str, folds: &[ResolvedFold]) -> Result<String> {
    let execution: serde_json::Value = fs::read(request.output.join("execution.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    if execution["independent"]["contract"] == "independent-v1" {
        return crate::signature(&(
            "independent-prediction-v1",
            factors,
            folds,
            &execution["independent"],
            &request.quality_policy,
            crate::research::worker::runtime_signature(),
        ));
    }
    if let Some(source) = &request.reuse_predictions {
        let (training_root, expected) = training_source(source)?;
        let bytes = fs::read(training_root.join("training.json"))?;
        if expected.is_some_and(|hash| {
            hash != format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            )
        }) {
            return Err("prediction training provenance damaged".into());
        }
        let training: serde_json::Value = serde_json::from_slice(&bytes)?;
        let mut unsigned = training.clone();
        let receipt = unsigned
            .as_object_mut()
            .ok_or("training descriptor invalid")?
            .remove("artifact_signature")
            .ok_or("training receipt missing")?;
        if receipt
            != format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&unsigned)?))
            )
            || training["schema_version"] != artifacts::TRAINING_SCHEMA_VERSION
            || training["status"] != "trained"
            || serde_json::from_slice::<serde_json::Value>(&fs::read(source.join("summary.json"))?)?
                ["factor_signature"]
                != factors
            || training["resolved_folds"] != serde_json::to_value(folds)?
            || training["feature_order"] != serde_json::to_value(&request.config.factor_ids)?
            || training["request"]["quality_policy"]
                != serde_json::to_value(&request.quality_policy)?
        {
            return Err("prediction feature/date/training identity mismatch".into());
        }
        let key = format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&(
                "prediction-v1",
                factors,
                folds,
                &training["models"],
                &training["feature_order"],
                &request.quality_policy,
                training["runtime_signature"]
                    .as_str()
                    .ok_or("prediction producer missing")?
            ))?))
        );
        let published: serde_json::Value =
            serde_json::from_slice(&fs::read(source.join("prediction_manifest.json"))?)?;
        if published["signature"] != key
            || published["status"] != "completed"
            || published["sha256"] != file_hash(&source.join("predictions.parquet"))?
        {
            return Err("published prediction damaged".into());
        }
        return Ok(key);
    }
    let source = request.reuse_training.as_ref().unwrap_or(&request.output);
    let training: serde_json::Value =
        serde_json::from_slice(&fs::read(source.join("training.json"))?)?;
    validate_training_artifact(&training)?;
    for (i, model) in training["models"]
        .as_array()
        .ok_or("model receipts missing")?
        .iter()
        .enumerate()
    {
        let fold = source.join(format!("fold-{i:04}"));
        if model["model_sha256"] != file_hash(&fold.join("model.txt"))?
            || model["manifest_sha256"] != file_hash(&fold.join("manifest.json"))?
        {
            return Err("prediction model receipt mismatch".into());
        }
    }
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&(
            "prediction-v1",
            factors,
            folds,
            &training["models"],
            &training["feature_order"],
            &request.quality_policy,
            crate::research::worker::runtime_signature()
        ))?))
    ))
}
fn root(request: &ResearchRequest, key: &str) -> PathBuf {
    request
        .reuse_predictions
        .as_ref()
        .unwrap_or(&request.output)
        .parent()
        .unwrap_or(&request.output)
        .join(".predictions-v1")
        .join(key.trim_start_matches("sha256:"))
}
pub(super) fn publish(
    request: &ResearchRequest,
    factors: &str,
    folds: &[ResolvedFold],
    signals: &[(Date, String, f64)],
    reports: &[FoldReport],
) -> Result<()> {
    if signals.iter().any(|(_, _, score)| !score.is_finite()) {
        return Err("nonfinite prediction cannot be published".into());
    }
    let key = identity(request, factors, folds)?;
    let dest = root(request, &key);
    if dest.join("manifest.json").exists() {
        restore(request, factors, folds)?;
        return Ok(());
    }
    let staging = dest.with_extension(format!("{}.partial", uuid::Uuid::new_v4()));
    fs::create_dir_all(staging.join("sample_masks"))?;
    super::akquant::table(
        &staging.join("predictions.parquet"),
        &["date", "symbol", "score"],
        vec![
            std::sync::Arc::new(StringArray::from(
                signals.iter().map(|x| x.0.to_string()).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(StringArray::from(
                signals.iter().map(|x| x.1.clone()).collect::<Vec<_>>(),
            )),
            std::sync::Arc::new(Float64Array::from(
                signals.iter().map(|x| x.2).collect::<Vec<_>>(),
            )),
        ],
    )?;
    save(staging.join("fold_reports.json"), &reports)?;
    fs::copy(
        request.output.join("sample_selection.json"),
        staging.join("sample_selection.json"),
    )?;
    let admission: analytics_core::admission::AdmissionReport =
        serde_json::from_slice(&fs::read(request.output.join("admission.json"))?)?;
    let decisions = admission
        .decisions
        .into_iter()
        .filter(|d| {
            matches!(
                d.code.as_str(),
                "label_missing_or_outside_split" | "insufficient_finite_features"
            )
        })
        .collect::<Vec<_>>();
    save(staging.join("sample_decisions.json"), &decisions)?;
    let mut files = vec![
        "predictions.parquet".to_owned(),
        "fold_reports.json".into(),
        "sample_selection.json".into(),
        "sample_decisions.json".into(),
    ];
    for i in 0..folds.len() {
        for split in ["train", "validation", "prediction"] {
            let name = format!("sample_masks/fold-{i:04}-{split}.json");
            if request.output.join(&name).exists() {
                fs::copy(request.output.join(&name), staging.join(&name))?;
                files.push(name);
            }
        }
    }
    if request
        .output
        .join("independent_predictions.json")
        .is_file()
    {
        fs::copy(
            request.output.join("independent_predictions.json"),
            staging.join("independent_predictions.json"),
        )?;
        files.push("independent_predictions.json".into());
    }
    let hashes = files
        .iter()
        .map(|name| Ok((name.clone(), file_hash(&staging.join(name))?)))
        .collect::<Result<BTreeMap<_, _>>>()?;
    save(
        staging.join("manifest.json"),
        &artifacts::PredictionCacheManifest {
            schema_version: 1,
            signature: key.clone(),
            rows: signals.len(),
            files: hashes,
        },
    )?;
    analytics_core::task::TaskContext::current().check()?;
    fs::rename(&staging, &dest)?;
    fs::copy(
        dest.join("predictions.parquet"),
        request.output.join("predictions.parquet"),
    )?;
    save(
        request.output.join("prediction_manifest.json"),
        &artifacts::PredictionReceipt {
            schema_version: 1,
            signature: key,
            sha256: file_hash(&dest.join("predictions.parquet"))?,
            rows: signals.len(),
            status: "completed".into(),
            cache_hit: false,
        },
    )?;
    Ok(())
}
pub(super) fn restore(
    request: &ResearchRequest,
    factors: &str,
    folds: &[ResolvedFold],
) -> Result<Option<Restored>> {
    let key = identity(request, factors, folds)?;
    let dir = root(request, &key);
    if !dir.join("manifest.json").is_file() {
        return Ok(None);
    }
    let m: artifacts::PredictionCacheManifest =
        serde_json::from_slice(&fs::read(dir.join("manifest.json"))?)?;
    if m.schema_version != 1 || m.signature != key {
        return Err("prediction cache identity mismatch".into());
    }
    let hashes = m.files;
    if let Some(source) = &request.reuse_predictions {
        let published: serde_json::Value =
            serde_json::from_slice(&fs::read(source.join("prediction_manifest.json"))?)?;
        if published["sha256"]
            != hashes
                .get("predictions.parquet")
                .map(String::as_str)
                .unwrap_or("")
        {
            return Err("published prediction differs from immutable cache".into());
        }
    }
    for required in [
        "predictions.parquet",
        "sample_selection.json",
        "sample_decisions.json",
        "fold_reports.json",
    ] {
        if !hashes.contains_key(required) {
            return Err("prediction cache incomplete".into());
        }
    }
    for (name, hash) in &hashes {
        if Path::new(name)
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
            || file_hash(&dir.join(name))? != *hash
        {
            return Err("prediction cache damaged".into());
        }
    }
    // Required split masks are explicit; an incomplete manifest cannot reuse a
    // prediction produced before the full phase was published.
    for (i, fold) in folds.iter().enumerate() {
        let independent = request.output.join("model_selection.json").is_file();
        for split in ["train", "prediction"]
            .into_iter()
            .filter(|s| !independent || *s == "prediction")
            .chain(
                fold.actual
                    .validation
                    .as_ref()
                    .filter(|_| !independent)
                    .map(|_| "validation"),
            )
        {
            if !hashes.contains_key(&format!("sample_masks/fold-{i:04}-{split}.json")) {
                return Err("prediction cache sample receipt missing".into());
            }
        }
    }
    let mut signals = Vec::<(Date, String, f64)>::new();
    let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        fs::File::open(dir.join("predictions.parquet"))?,
    )?;
    if builder
        .schema()
        .fields()
        .iter()
        .map(|f| f.name().as_str())
        .ne(["date", "symbol", "score"])
    {
        return Err("prediction cache schema mismatch".into());
    }
    for batch in builder.build()? {
        let b = batch?;
        let dates = b
            .column(0)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or("prediction dates type")?;
        let ids = b
            .column(1)
            .as_any()
            .downcast_ref::<StringArray>()
            .ok_or("prediction symbols type")?;
        let scores = b
            .column(2)
            .as_any()
            .downcast_ref::<Float64Array>()
            .ok_or("prediction score type")?;
        for i in 0..b.num_rows() {
            if dates.is_null(i)
                || ids.is_null(i)
                || scores.is_null(i)
                || !scores.value(i).is_finite()
            {
                return Err("prediction cache missing/nonfinite row".into());
            }
            signals.push((
                dates.value(i).parse()?,
                ids.value(i).into(),
                scores.value(i),
            ));
        }
    }
    if m.rows != signals.len() {
        return Err("prediction cache row count mismatch".into());
    }
    let mut seen = BTreeSet::new();
    for (day, id, _) in &signals {
        analytics_core::pool::validate_instrument(id)?;
        if !folds.iter().any(|f| f.actual.test.contains(*day)) || !seen.insert((*day, id)) {
            return Err("prediction date/security/duplicate identity invalid".into());
        }
    }
    let reports: Vec<FoldReport> =
        serde_json::from_slice(&fs::read(dir.join("fold_reports.json"))?)?;
    if reports.len() != folds.len() || reports.iter().any(|r| r.performance.is_some()) {
        return Err("prediction cache fold phase mismatch".into());
    }
    let selection = serde_json::from_slice(&fs::read(dir.join("sample_selection.json"))?)?;
    fs::create_dir_all(request.output.join("sample_masks"))?;
    for name in hashes.keys() {
        fs::copy(dir.join(name), request.output.join(name))?;
    }
    save(
        request.output.join("prediction_manifest.json"),
        &serde_json::json!({"schema_version":1,"signature":key,"sha256":hashes["predictions.parquet"],"rows":signals.len(),"status":"completed","cache_hit":true}),
    )?;
    Ok(Some((signals, reports, selection)))
}
