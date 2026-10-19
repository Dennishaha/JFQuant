use super::*;
use analytics_storage::filesystem;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Registry<'a> {
    pub w: &'a Workspace,
    pub context: &'a TaskContext,
    pub active_root: PathBuf,
    pub files: BTreeMap<PathBuf, StorageFile>,
    pub resources: BTreeMap<String, StorageResource>,
    pub runs: BTreeMap<PathBuf, RunRecord>,
    pub metadata: BTreeMap<PathBuf, String>,
    pub references: Vec<ResourceReference>,
    pub external: BTreeSet<PathBuf>,
    pub diagnostics: Vec<String>,
    pub names: BTreeMap<String, Vec<String>>,
    pub roots: BTreeMap<PathBuf, Vec<String>>,
}
impl Registry<'_> {
    pub fn add(
        &mut self,
        category: ResourceCategory,
        path: &Path,
        name: String,
        task: Option<Uuid>,
        paths: Vec<PathBuf>,
    ) -> Result<String> {
        let id = crate::signature(&(category, path, &name))?;
        let archived = path.starts_with(".jfquant/archive");
        let temporary =
            path.starts_with(".jfquant/staging") || path.to_string_lossy().contains(".partial");
        let mut modified = 0;
        for p in paths {
            self.context.check()?;
            if let Some(file) = self.files.get_mut(&p) {
                if !file.owners.contains(&id) {
                    file.owners.push(id.clone());
                }
                file.category = category;
                modified = modified.max(file.stamp.modified.0);
            }
        }
        if !self.resources.contains_key(&id) {
            self.names.entry(name.clone()).or_default().push(id.clone());
            self.roots
                .entry(path.to_owned())
                .or_default()
                .push(id.clone());
        }
        self.resources.entry(id.clone()).or_insert(StorageResource {
            id: id.clone(),
            name,
            category,
            path: path.to_owned(),
            task,
            start: None,
            end: None,
            modified_at: chrono::DateTime::from_timestamp(modified, 0)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default(),
            archived,
            temporary,
            released: false,
            deletable: true,
            reasons: vec![],
            usage: SpaceUsage::default(),
        });
        Ok(id)
    }
    pub fn under(&self, root: &Path) -> Vec<PathBuf> {
        self.files
            .range(root.to_owned()..)
            .take_while(|(p, _)| p.starts_with(root))
            .map(|(p, _)| p.clone())
            .collect()
    }
    pub fn protect(&mut self, id: &str, reason: impl Into<String>) {
        if let Some(r) = self.resources.get_mut(id) {
            r.deletable = false;
            let reason = reason.into();
            if !r.reasons.contains(&reason) {
                r.reasons.push(reason);
            }
        }
    }
    pub fn metadata(&mut self, path: &Path) -> Result<Value> {
        let file = self
            .files
            .get(path)
            .ok_or("storage_manifest_not_in_inventory")?;
        if file.stamp.bytes > 16 * 1024 * 1024 {
            return Err("storage_manifest_too_large".into());
        }
        self.context.check()?;
        let bytes = fs::read(owned_path(self.w, path)?)?;
        if filesystem::FileStamp::read(&self.w.resolve(path))? != file.stamp {
            return Err("storage_manifest_changed_during_scan".into());
        }
        self.metadata.insert(
            path.to_owned(),
            format!(
                "sha256:{}",
                <sha2::Sha256 as sha2::Digest>::digest(&bytes)
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            ),
        );
        Ok(serde_json::from_slice(&bytes)?)
    }
    pub fn target(&self, path: &Path) -> Option<String> {
        self.files
            .get(path)
            .and_then(|f| f.owners.first())
            .cloned()
            .or_else(|| {
                path.ancestors()
                    .find_map(|root| self.roots.get(root).and_then(|ids| ids.first()).cloned())
            })
    }
    pub fn reference(&mut self, source: &str, target: &str, kind: ReferenceKind, reason: &str) {
        if source != target
            && !self
                .references
                .iter()
                .any(|r| r.source == source && r.target == target && r.kind == kind)
        {
            self.references.push(ResourceReference {
                source: source.into(),
                target: target.into(),
                kind,
                reason: reason.into(),
            });
        }
    }
}
pub(crate) fn management_run(record: &RunRecord) -> bool {
    record.request["request"]["operation"] == "storage"
}
fn load_runs(r: &mut Registry<'_>) -> Result<()> {
    let paths: Vec<_> = r
        .files
        .keys()
        .filter(|p| {
            p.file_name().is_some_and(|n| n == "run.json")
                && (p.starts_with(".jfquant/runs") || p.starts_with(".jfquant/archive"))
        })
        .cloned()
        .collect();
    for p in paths {
        if p.starts_with(&r.active_root) {
            continue;
        }
        let Ok(value) = r.metadata(&p) else { continue };
        let Ok(run) = serde_json::from_value::<RunRecord>(value) else {
            continue;
        };
        if run.schema_version != 1
            || p.parent()
                .and_then(|p| p.file_name())
                .and_then(|s| s.to_str())
                != Some(run.id.to_string().as_str())
        {
            continue;
        }
        if management_run(&run) {
            r.metadata.remove(&p);
        }
        r.runs.insert(p.parent().unwrap().to_owned(), run);
    }
    Ok(())
}
fn data_packages(r: &mut Registry<'_>) -> Result<()> {
    let catalog_paths: Vec<_> = r
        .files
        .keys()
        .filter(|p| p.starts_with(".jfquant/catalog") && p.extension().is_some_and(|e| e == "json"))
        .cloned()
        .collect();
    for cp in catalog_paths {
        let value = match r.metadata(&cp) {
            Ok(v) => v,
            Err(e) => {
                r.diagnostics.push(format!("{}: {e}", cp.display()));
                continue;
            }
        };
        let d: crate::DataVersion = match serde_json::from_value(value) {
            Ok(d) => d,
            Err(e) => {
                r.diagnostics.push(format!("{}: {e}", cp.display()));
                continue;
            }
        };
        if !safe_relative(&d.path) {
            r.diagnostics.push(format!("{}: unsafe catalog path", d.id));
            continue;
        }
        if super::history::dataset_released(r.w, &d.id)? {
            continue;
        }
        let parent = d.path.parent().ok_or("dataset parent missing")?;
        let mut paths = vec![];
        let category = if d.kind == "actions" {
            ResourceCategory::Evidence
        } else {
            ResourceCategory::Market
        };
        let mut valid = true;
        if d.kind == "market" {
            match super::members::market(r, &d.path) {
                Ok(members) => paths.extend(members),
                Err(_) => valid = false,
            }
            let source = parent.join("jfquant_source.json");
            match r.metadata(&source) {
                Ok(v)
                    if v["downloader"] == "JFQuant analytics-storage"
                        || v["synthetic_fixture"] == true =>
                {
                    paths.push(source);
                    for field in ["evidence_sha256"] {
                        if let Some(entries) = v[field].as_object() {
                            for name in entries.keys() {
                                if safe_relative(Path::new(name)) {
                                    paths.push(parent.join("raw").join(name));
                                } else {
                                    valid = false;
                                }
                            }
                        }
                    }
                    if v["evidence_tree_sha256"].is_string() {
                        // A tree hash cannot establish membership without reading all evidence.
                        valid = false;
                    }
                }
                _ => valid = false,
            }
        } else if matches!(d.kind.as_str(), "actions" | "coefficients") {
            paths.push(d.path.clone());
            match r.metadata(&d.path) {
                Ok(v) => {
                    if d.kind == "actions" && !matches!(v["schema_version"].as_u64(), Some(5..=10))
                    {
                        valid = false;
                    }
                    if let Some(raw) = v["raw_sha256"].as_object() {
                        for name in raw.keys() {
                            let name = Path::new(name);
                            if safe_relative(name) {
                                paths.push(parent.join("raw").join(name));
                            } else {
                                valid = false;
                            }
                        }
                    }
                    if v["raw_sha256"].get("source-evidence-sha256.json").is_some() {
                        valid = false;
                    }
                    if d.kind == "coefficients" {
                        if v["schema_version"] != 1
                            || !v["evidence"].is_object()
                            || !v["records"].is_object()
                        {
                            valid = false;
                        }
                        if let Some(entries) = v["evidence"].as_object() {
                            for name in entries.keys() {
                                if safe_relative(Path::new(name)) {
                                    paths.push(parent.join("raw").join(name));
                                } else {
                                    valid = false;
                                }
                            }
                        }
                        if let Some(entries) = v["records"].as_object() {
                            for name in entries.keys() {
                                if safe_relative(Path::new(name))
                                    && Path::new(name).components().count() == 1
                                {
                                    paths.push(
                                        parent
                                            .join("market/adjustments")
                                            .join(format!("{name}.json")),
                                    );
                                } else {
                                    valid = false;
                                }
                            }
                        }
                    }
                }
                Err(_) => valid = false,
            }
        } else {
            valid = false;
        }
        let missing = paths.is_empty() || paths.iter().any(|p| !r.files.contains_key(p));
        let id = r.add(category, &d.path, d.id.clone(), None, paths)?;
        if let Some(resource) = r.resources.get_mut(&id) {
            resource.start = d.start;
            resource.end = d.end;
        }
        if !valid || missing {
            r.protect(&id, "生产者清单不完整或文件缺失，不能确认可清理范围");
        }
        let archive = parent.join("qlib_bin.tar.gz");
        let archive_manifest = parent.join("qlib_bin.manifest.json");
        if valid
            && r.files.contains_key(&archive)
            && let Ok(manifest) = r.metadata(&archive_manifest)
        {
            let verified = manifest["archive_size_bytes"].as_u64()
                == r.files.get(&archive).map(|f| f.stamp.bytes)
                && manifest["archive_sha256"].as_str().is_some();
            if verified {
                let mut paths = vec![archive.clone(), archive_manifest];
                if r.files.contains_key(&parent.join("owner.json")) {
                    paths.push(parent.join("owner.json"));
                }
                r.add(
                    ResourceCategory::Market,
                    &archive,
                    format!("已下载压缩包 · {}", archive.display()),
                    None,
                    paths,
                )?;
            }
        }
    }
    Ok(())
}
fn cached_packages(r: &mut Registry<'_>) -> Result<()> {
    let paths: Vec<_> = r
        .files
        .keys()
        .filter(|p| {
            p.file_name().is_some_and(|n| n == "manifest.json")
                && p.components().any(|c| {
                    matches!(
                        c.as_os_str().to_str(),
                        Some(".factor-shards-v1" | ".factor-artifacts-v2" | ".predictions-v1")
                    )
                })
        })
        .cloned()
        .collect();
    for p in paths {
        let root = p.parent().unwrap();
        let Ok(v) = r.metadata(&p) else { continue };
        let prediction = p.components().any(|c| c.as_os_str() == ".predictions-v1");
        let artifact = p
            .components()
            .any(|c| c.as_os_str() == ".factor-artifacts-v2");
        let mut members = vec![p.clone()];
        let valid = if prediction {
            if v["schema_version"] != 1 || v["signature"].as_str().is_none() {
                false
            } else if let Some(files) = v["files"].as_object() {
                for name in files.keys() {
                    if safe_relative(Path::new(name)) {
                        members.push(root.join(name));
                    }
                }
                !files.is_empty() && members.len() == files.len() + 1
            } else {
                false
            }
        } else if artifact {
            members.extend([root.join("factors.parquet"), root.join("receipt.json")]);
            r.metadata(&root.join("receipt.json"))
                .is_ok_and(|v| v["schema_version"] == 2 && v["context_hash"].as_str().is_some())
        } else {
            members.push(root.join("features.parquet"));
            v["schema_version"] == 1 && v["features"].is_array() && v["sha256"].as_str().is_some()
        };
        let complete = members.iter().all(|p| r.files.contains_key(p));
        let id = r.add(
            if prediction {
                ResourceCategory::Predictions
            } else {
                ResourceCategory::FactorCache
            },
            root,
            format!(
                "{} · {}",
                if prediction {
                    "共享预测"
                } else {
                    "共享因子"
                },
                root.file_name().unwrap().to_string_lossy()
            ),
            None,
            members,
        )?;
        if !valid || !complete {
            r.protect(&id, "缓存清单不完整，需检查原始内容");
        }
    }
    Ok(())
}
fn run_packages(r: &mut Registry<'_>) -> Result<()> {
    for (root, run) in r.runs.clone() {
        if management_run(&run) || run.read_only {
            continue;
        }
        workbench_packages(r, &root, &run)?;
        let model = root.join("training.json");
        if r.files.contains_key(&model) {
            let mut paths = vec![model.clone()];
            for name in [
                "fit_calendar.json",
                "admission.json",
                "sample_selection.json",
                "sample_masks",
                "factor-cache",
            ] {
                paths.extend(super::members::training(r, &root, name));
            }
            paths.extend(
                r.under(&root)
                    .into_iter()
                    .filter(|p| super::members::fold_file(p.strip_prefix(&root).unwrap())),
            );
            let valid = r
                .metadata(&model)
                .is_ok_and(|v| matches!(v["schema_version"].as_u64(), Some(4 | 5)));
            let id = r.add(
                ResourceCategory::Models,
                &model,
                format!("训练模型 · {}", run.id),
                Some(run.id),
                paths,
            )?;
            if !valid {
                r.protect(&id, "未知训练产物格式");
            }
        }
        let prediction = root.join("prediction_manifest.json");
        if r.files.contains_key(&prediction) {
            let mut paths = vec![prediction.clone()];
            for name in [
                "predictions.parquet",
                "sample_selection.json",
                "sample_decisions.json",
                "sample_masks",
                "fold_reports.json",
                "independent_predictions.json",
            ] {
                paths.extend(super::members::training(r, &root, name));
            }
            let valid = r
                .metadata(&prediction)
                .is_ok_and(|v| v["schema_version"] == 1 && v["signature"].as_str().is_some());
            let id = r.add(
                ResourceCategory::Predictions,
                &prediction,
                format!("任务预测 · {}", run.id),
                Some(run.id),
                paths,
            )?;
            if !valid {
                r.protect(&id, "未知预测产物格式");
            }
        }
        for (name, manifest) in [
            ("announcements", "announcements.json"),
            ("captured", "announcements.json"),
            ("combined", "announcements.json"),
            ("reference", "reference.json"),
            ("three-factor-inputs", "inputs.json"),
        ] {
            let dir = root.join(name);
            if r.files.contains_key(&dir.join(manifest))
                && let Ok(value) = r.metadata(&dir.join(manifest))
            {
                let paths = evidence_members(r, &dir, manifest, &value)?;
                if paths.is_empty() {
                    continue;
                }
                let incomplete = paths.iter().any(|p| !r.files.contains_key(p));
                let id = r.add(
                    ResourceCategory::Evidence,
                    &dir,
                    format!("采集证据 · {}", run.id),
                    Some(run.id),
                    paths,
                )?;
                if incomplete {
                    r.protect(&id, "证据清单指向缺失文件，不能确认完整清理范围");
                }
            }
        }
        let known = [
            "summary.json",
            "nav.csv",
            "outcome.json",
            "events.jsonl",
            "payload.json",
            "artifacts.json",
            "account.json",
            "account.csv",
            "positions.csv",
            "trades.csv",
            "orders.csv",
            "order_events.jsonl",
            "execution.json",
            "execution_readiness.json",
            "data_audit.json",
            "allocation-input.json",
            "allocation_report.json",
            "allocation-manifest.json",
            "reference-targets.json",
            "reference-input.json",
            "reference-account.json",
            "reference-event-checks.json",
            "rights_input.json",
            "rights_report.json",
            "account-event-checks.json",
            "event-checks.json",
            "adjustment-dependencies.json",
            "execution-dependencies.json",
            "input-check.json",
            "admission.json",
            "sample_selection.json",
            "sample_decisions.json",
            "sample_masks",
            "fold_reports.json",
            "model_selection.json",
            "independent_request.json",
            "backtest_plan.json",
            "cross_pool.json",
            "annual_reports.json",
            "native_account_daily.json",
            "daily.json",
            "portfolio_snapshots.json",
            "execution_actions.json",
            "engine_curve_differences.json",
            "execution_checks.json",
            "event_exposure_checks.json",
            "suspension_valuation_marks.json",
            "suspension_traded_days.json",
            "rights_state.json",
            "rights_failed_state.json",
            "rights_gap_report.json",
            "corporate_event_report.json",
            "execution_quote_diagnostics.json",
            "execution_warnings.json",
            "execution_valuation_gaps.json",
            "corporate_event_preview.json",
            "combined-input.json",
            "research-request.json",
            "cross_pool_report.json",
            "stages.json",
            "report.md",
            "allocation-eligibility.json",
            "allocation-readiness.json",
            "input-check-plan.json",
            "input_issues.json",
            "quality.json",
            "factor_audit.json",
            "factor-receipt.json",
            ".worker-bars.parquet",
        ];
        let mut paths = vec![];
        for name in known {
            paths.extend(
                super::members::training(r, &root, name)
                    .into_iter()
                    .filter(|p| {
                        r.files[p].owners.is_empty()
                            || (r.files.contains_key(&root.join("summary.json"))
                                && matches!(name, "fold_reports.json" | "annual_reports.json"))
                    }),
            );
        }
        if !paths.is_empty() {
            r.add(
                ResourceCategory::Backtest,
                &root,
                format!("回测明细与日志 · {}", run.id),
                Some(run.id),
                paths,
            )?;
        }
    }
    Ok(())
}
pub(super) fn evidence_members(
    r: &mut Registry<'_>,
    root: &Path,
    manifest: &str,
    value: &Value,
) -> Result<Vec<PathBuf>> {
    let mut paths = vec![root.join(manifest)];
    let raw = |name: &str| -> Option<PathBuf> {
        (safe_relative(Path::new(name)) && Path::new(name).components().count() == 1)
            .then(|| root.join("raw").join(name))
    };
    if manifest == "announcements.json" {
        if value["schema_version"] != 2 {
            return Ok(vec![]);
        }
        if let Some(docs) = value["documents"].as_array() {
            for doc in docs {
                for field in ["pdf", "text"] {
                    if let Some(path) = doc[field].as_str().and_then(raw) {
                        paths.push(path);
                    }
                }
            }
        }
        if let Some(receipts) = value["receipts"].as_object() {
            for id in receipts.keys() {
                if let Some(path) = raw(&format!("{id}-actions-structured.json")) {
                    paths.push(path);
                }
            }
        }
    } else {
        let mut evidence = value.clone();
        if manifest == "inputs.json" {
            for name in ["fundamentals.json", "template.json"] {
                if r.files.contains_key(&root.join(name)) {
                    paths.push(root.join(name));
                }
            }
            if r.files.contains_key(&root.join("fundamentals.json")) {
                evidence = r.metadata(&root.join("fundamentals.json"))?;
            }
        }
        for field in ["evidence_sha256", "evidence"] {
            if let Some(entries) = evidence[field].as_object() {
                for name in entries.keys() {
                    if let Some(path) = raw(name)
                        && r.files.contains_key(&path)
                    {
                        paths.push(path);
                    }
                }
            }
        }
    }
    Ok(paths)
}
fn workbench_packages(r: &mut Registry<'_>, root: &Path, run: &RunRecord) -> Result<()> {
    let descriptor = root.join("artifacts.json");
    if !r.files.contains_key(&descriptor) {
        return Ok(());
    }
    let Ok(value) = r.metadata(&descriptor) else {
        return Ok(());
    };
    let Ok(artifact) = serde_json::from_value::<crate::research::pipeline::PipelineArtifact>(
        value["artifact"].clone(),
    ) else {
        return Ok(());
    };
    let own = |path: PathBuf| -> Option<PathBuf> {
        let path = path
            .strip_prefix(r.w.root())
            .map(Path::to_owned)
            .unwrap_or(path);
        (safe_relative(&path) && path.starts_with(root)).then_some(path)
    };
    let paths = vec![
        artifact.training.model_path,
        artifact.training.manifest_path,
        artifact.training.metrics_path,
        artifact.training.feature_importance_path,
    ]
    .into_iter()
    .chain(artifact.factor_cache_path)
    .chain(artifact.factor_manifest_path)
    .filter_map(&own)
    .collect::<Vec<_>>();
    let prediction = own(artifact.predictions_path);
    if !paths.is_empty() {
        r.add(
            ResourceCategory::Models,
            &root.join("model-package"),
            format!("流水线模型 · {}", run.id),
            Some(run.id),
            paths,
        )?;
    }
    if let Some(path) = prediction
        && r.files.contains_key(&path)
    {
        r.add(
            ResourceCategory::Predictions,
            &path,
            format!("流水线预测 · {}", run.id),
            Some(run.id),
            vec![path.clone()],
        )?;
    }
    Ok(())
}
fn staging_packages(r: &mut Registry<'_>) -> Result<()> {
    let roots: BTreeSet<_> = r
        .files
        .keys()
        .filter_map(|p| {
            p.strip_prefix(".jfquant/staging")
                .ok()?
                .components()
                .next()
                .map(|c| PathBuf::from(".jfquant/staging").join(c))
        })
        .collect();
    for root in roots {
        let name = root.file_name().unwrap().to_string_lossy();
        let owners: Vec<_> = r
            .runs
            .values()
            .filter(|run| !management_run(run))
            .filter(|run| {
                let request = &run.request["request"];
                let resume = request["resume"]
                    .as_str()
                    .or_else(|| request["request"]["resume"].as_str());
                name.ends_with(resume.unwrap_or(&run.id.to_string()))
            })
            .cloned()
            .collect();
        if owners.is_empty() {
            continue;
        }
        let marker = r.files.keys().any(|p| {
            p.starts_with(&root)
                && matches!(
                    p.file_name().and_then(|n| n.to_str()),
                    Some(
                        "owner.json"
                            | "refresh-owner.json"
                            | "download-owner.json"
                            | "checkpoint.json"
                            | "qlib_bin.manifest.json"
                    )
                )
        });
        if !marker {
            continue;
        }
        let paths = r.under(&root);
        let id = r.add(
            ResourceCategory::Market,
            &root,
            format!("临时检查点 · {name}"),
            owners.first().map(|r| r.id),
            paths,
        )?;
        r.protect(&id, "检查点没有完整成员清单，仅统计不清理");
        if owners.iter().any(|r| r.status == crate::RunStatus::Running) {
            r.protect(&id, "仍有运行任务使用该检查点");
        }
    }
    Ok(())
}
fn protect_and_account(
    r: &mut Registry<'_>,
    symlinks: &[PathBuf],
) -> Result<(SpaceUsage, BTreeMap<ResourceCategory, SpaceUsage>)> {
    let mut unassigned: BTreeMap<(ResourceCategory, PathBuf), Vec<PathBuf>> = BTreeMap::new();
    for file in r.files.values().filter(|f| f.owners.is_empty()) {
        let management = file.path.starts_with(".jfquant")
            && !file.path.starts_with(".jfquant/archive")
            && !file.path.starts_with(".jfquant/staging");
        let category = if management {
            ResourceCategory::Metadata
        } else {
            ResourceCategory::Other
        };
        let prefix = if management {
            file.path.components().take(3).collect()
        } else {
            file.path.components().take(2).collect()
        };
        unassigned
            .entry((category, prefix))
            .or_default()
            .push(file.path.clone());
    }
    for ((category, path), files) in unassigned {
        let id = r.add(category, &path, path.display().to_string(), None, files)?;
        r.protect(&id, "配置、管理记录或尚未识别的文件，仅统计不清理");
    }
    for (root, run) in r.runs.clone() {
        if run.status == crate::RunStatus::Running && !management_run(&run) {
            let ids: Vec<_> = r
                .resources
                .values()
                .filter(|v| v.path.starts_with(&root) || v.task == Some(run.id))
                .map(|v| v.id.clone())
                .collect();
            for id in ids {
                r.protect(&id, "任务仍在运行");
            }
        }
    }
    for link in symlinks {
        let absolute = r.w.resolve(link);
        let archive_alias = link.parent() == Some(Path::new(".jfquant/runs"))
            && absolute
                .canonicalize()
                .is_ok_and(|p| p.starts_with(r.w.root().join(".jfquant/archive")));
        if !archive_alias {
            let ids: Vec<_> = r
                .resources
                .values()
                .filter(|v| link.starts_with(&v.path) || v.path.starts_with(link))
                .map(|v| v.id.clone())
                .collect();
            for id in ids {
                r.protect(&id, "资源包含未经确认的符号链接");
            }
            if let Ok(target) = absolute.canonicalize()
                && !target.starts_with(r.w.root())
            {
                r.external.insert(target);
            }
        }
    }
    let mut seen = BTreeSet::new();
    let mut total = SpaceUsage::default();
    let mut categories = BTreeMap::<_, SpaceUsage>::new();
    for file in r.files.values_mut() {
        r.context.check()?;
        file.owners.sort();
        file.owners.dedup();
        if let Some(owner) = file.owners.first() {
            file.category = r.resources[owner].category;
        }
        let unique = seen.insert((file.stamp.device, file.stamp.inode));
        let usage = SpaceUsage {
            logical_bytes: if unique { file.stamp.bytes } else { 0 },
            allocated_bytes: if unique { file.stamp.allocated } else { 0 },
            files: 1,
        };
        add_usage(&mut total, &usage);
        add_usage(categories.entry(file.category).or_default(), &usage);
        // Shared paths are assigned once in the aggregate; resource details show their full membership.
        for owner in &file.owners {
            let v = r.resources.get_mut(owner).unwrap();
            v.usage.logical_bytes += file.stamp.bytes;
            v.usage.allocated_bytes += file.stamp.allocated;
            v.usage.files += 1;
        }
    }
    Ok((total, categories))
}
pub(crate) fn add_usage(total: &mut SpaceUsage, more: &SpaceUsage) {
    total.logical_bytes = total.logical_bytes.saturating_add(more.logical_bytes);
    total.allocated_bytes = total.allocated_bytes.saturating_add(more.allocated_bytes);
    total.files = total.files.saturating_add(more.files);
}
pub fn scan(w: &Workspace, context: &TaskContext) -> Result<StorageInventory> {
    scan_with(
        w,
        context,
        ScanLevel::Resources,
        default_threads(),
        true,
        None,
    )
}
pub(crate) fn scan_with(
    w: &Workspace,
    context: &TaskContext,
    level: ScanLevel,
    threads: usize,
    verify: bool,
    reporter: Option<&super::progress::Reporter>,
) -> Result<StorageInventory> {
    phase(context, reporter, "遍历文件元数据")?;
    let physical = filesystem::walk(
        w.root(),
        threads,
        level == ScanLevel::Overview,
        |progress, diagnostics| {
            context.check()?;
            if let Some(reporter) = reporter {
                reporter.update(progress, diagnostics)?;
            }
            Ok(())
        },
    )?;
    let directories = physical.progress.directories;
    let physical = physical.inventory;
    if level == ScanLevel::Overview {
        let mut total = SpaceUsage::default();
        for usage in directories.values() {
            total.logical_bytes += usage.logical_bytes;
            total.allocated_bytes += usage.allocated_bytes;
            total.files += usage.files;
        }
        return Ok(StorageInventory {
            schema_version: 2,
            scan_id: context.id,
            level,
            dependencies_checked: false,
            stale: false,
            scanned_at: crate::now(),
            workspace: w.root().to_owned(),
            complete: physical.errors.is_empty(),
            diagnostics: physical.errors,
            available_bytes: filesystem::available_bytes(w.root()).ok(),
            total,
            categories: BTreeMap::new(),
            reclaimable_allocated_bytes_estimate: None,
            resources: vec![],
            references: vec![],
            external_references: vec![],
            files: vec![],
            metadata_fingerprint: String::new(),
            directories,
        });
    }
    phase(context, reporter, "识别资源与生产者清单")?;
    let mut r = Registry {
        w,
        context,
        active_root: PathBuf::from(format!(".jfquant/runs/{}", context.id)),
        files: physical
            .files
            .into_iter()
            .map(|f| {
                (
                    f.path.clone(),
                    StorageFile {
                        path: f.path,
                        stamp: f.stamp,
                        owners: vec![],
                        category: ResourceCategory::Other,
                    },
                )
            })
            .collect(),
        resources: BTreeMap::new(),
        runs: BTreeMap::new(),
        metadata: BTreeMap::new(),
        references: vec![],
        external: BTreeSet::new(),
        diagnostics: physical.errors,
        names: BTreeMap::new(),
        roots: BTreeMap::new(),
    };
    load_runs(&mut r)?;
    data_packages(&mut r)?;
    cached_packages(&mut r)?;
    run_packages(&mut r)?;
    staging_packages(&mut r)?;
    if verify {
        phase(context, reporter, "解析资源引用与清理依赖")?;
        super::dependencies::discover(&mut r, context)?;
    }
    let released = super::history::released(w)?;
    for item in &released {
        if r.resources
            .get(&item.resource.id)
            .is_some_and(|v| v.task.is_some())
        {
            r.resources.remove(&item.resource.id);
            for file in r.files.values_mut() {
                file.owners.retain(|id| id != &item.resource.id);
            }
        }
    }
    phase(context, reporter, "汇总空间与保护状态")?;
    let (total, categories) = protect_and_account(&mut r, &physical.symlinks)?;
    if let Err(e) = crate::maintenance::require_complete(w.root()) {
        r.diagnostics.push(e.to_string());
    }
    // Detect concurrent metadata changes instead of publishing a falsely complete inventory.
    phase(context, reporter, "重新校验文件元数据")?;
    let mut changed = vec![];
    for file in r.files.values().filter(|f| !volatile(&r, &f.path)) {
        context.check()?;
        if filesystem::FileStamp::read(&w.resolve(&file.path))
            .ok()
            .as_ref()
            != Some(&file.stamp)
        {
            changed.push(format!("{}: changed during scan", file.path.display()));
        }
    }
    r.diagnostics.extend(changed);
    let metadata_fingerprint = format!("storage-v2:{}", crate::signature(&r.metadata)?);
    let mut resources: Vec<_> = r.resources.into_values().collect();
    for item in released {
        if resources.iter().any(|r| r.id == item.resource.id) {
            continue;
        }
        let mut resource = item.resource;
        resource.released = true;
        resource.deletable = false;
        resource.usage = SpaceUsage::default();
        resources.push(resource);
    }
    resources.sort_by(|a, b| {
        b.usage
            .allocated_bytes
            .cmp(&a.usage.allocated_bytes)
            .then(a.id.cmp(&b.id))
    });
    let mut eligible: BTreeSet<_> = resources
        .iter()
        .filter(|v| v.deletable && !v.released)
        .map(|v| v.id.clone())
        .collect();
    loop {
        let blocked: Vec<_> = r
            .references
            .iter()
            .filter(|edge| {
                edge.kind != ReferenceKind::Provenance
                    && !eligible.contains(&edge.source)
                    && eligible.contains(&edge.target)
            })
            .map(|edge| edge.target.clone())
            .collect();
        if blocked.is_empty() {
            break;
        }
        for id in blocked {
            eligible.remove(&id);
        }
    }
    let files: Vec<_> = r.files.into_values().collect();
    let reclaimable = if verify && r.diagnostics.is_empty() {
        super::dependencies::release_usage(&files, &eligible).allocated_bytes
    } else {
        0
    };
    Ok(StorageInventory {
        schema_version: 2,
        scan_id: context.id,
        level,
        dependencies_checked: verify,
        stale: false,
        directories,
        scanned_at: crate::now(),
        workspace: w.root().to_owned(),
        complete: r.diagnostics.is_empty(),
        diagnostics: r.diagnostics,
        available_bytes: filesystem::available_bytes(w.root()).ok(),
        total,
        categories,
        reclaimable_allocated_bytes_estimate: verify.then_some(reclaimable),
        resources,
        references: r.references,
        external_references: r.external.into_iter().collect(),
        files,
        metadata_fingerprint,
    })
}
pub(crate) fn volatile(r: &Registry<'_>, path: &Path) -> bool {
    path.starts_with(".jfquant/storage")
        || path.starts_with(&r.active_root)
        || path == Path::new(".jfquant/app.lock")
        || path
            .ancestors()
            .any(|root| r.runs.get(root).is_some_and(management_run))
}

fn phase(
    context: &TaskContext,
    reporter: Option<&super::progress::Reporter>,
    phase: &str,
) -> Result<()> {
    context.report(phase)?;
    if let Some(reporter) = reporter {
        reporter.phase(phase);
    }
    Ok(())
}
