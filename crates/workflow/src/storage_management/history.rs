use super::*;
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    io::{BufRead, BufReader},
};

pub(crate) fn released(w: &Workspace) -> Result<Vec<ReleasedResource>> {
    let root = owned_path(w, Path::new(".jfquant/storage/transactions"))?;
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut values = std::collections::BTreeMap::new();
    for entry in fs::read_dir(root)? {
        let path = entry?.path().join("journal.json");
        let path = owned_path(w, path.strip_prefix(w.root())?)?;
        if !path.is_file() {
            continue;
        }
        let journal: super::cleanup::Journal = read(&path)?;
        super::cleanup::validate(w, &journal)?;
        if journal.result.committed {
            for resource in journal.plan.resources {
                values.insert(
                    resource.id.clone(),
                    ReleasedResource {
                        plan: journal.plan.id,
                        resource,
                    },
                );
            }
        }
    }
    Ok(values.into_values().collect())
}
pub(crate) fn dataset_released(w: &Workspace, id: &str) -> Result<bool> {
    Ok(released(w)?
        .iter()
        .any(|r| r.resource.name == id && r.resource.task.is_none()))
}
pub(crate) fn released_dataset_ids(w: &Workspace) -> Result<BTreeSet<String>> {
    Ok(released(w)?
        .into_iter()
        .filter(|r| r.resource.task.is_none())
        .map(|r| r.resource.name)
        .collect())
}
pub(crate) fn capture(w: &Workspace, task: Uuid) -> Result<()> {
    let path = PathBuf::from(format!(".jfquant/storage/history/{task}.json"));
    let target = owned_path(w, &path)?;
    if target.exists() {
        return Ok(());
    }
    let record = w.runs()?.into_iter().find(|r| r.id == task).or_else(|| {
        // Archived task aliases remain readable even though active enumeration omits them.
        read::<RunRecord>(&w.resolve(format!(".jfquant/runs/{task}/run.json"))).ok()
    });
    let Some(record) = record else {
        return Err(format!("storage_history_record_missing:{task}").into());
    };
    let root = w.resolve(&record.output);
    let result = w.results(&record).unwrap_or_else(|e| {
        let mut value = read::<Value>(&root.join("summary.json"))
            .or_else(|_| read::<Value>(&root.join("training.json")))
            .unwrap_or_else(|_| json!({}));
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "_storage_history_warning".into(),
                json!(format!("清理前的原产物已无法完整核验：{e}")),
            );
        }
        value
    });
    let nav = read_nav(&root.join("nav.csv"))?;
    super::cleanup::durable_write(
        &target,
        &HistoricalView {
            schema_version: 1,
            task,
            parameters: record.request,
            result,
            nav,
        },
    )
}
pub(crate) fn read_nav(path: &Path) -> Result<Vec<(String, f64, f64)>> {
    let file = match fs::File::open(path) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
        Err(e) => return Err(e.into()),
    };
    let mut out = vec![];
    for line in BufReader::new(file).lines().skip(1) {
        let line = line?;
        let cells: Vec<_> = line.split(',').collect();
        if cells.len() == 3
            && let (Ok(a), Ok(b)) = (cells[1].parse::<f64>(), cells[2].parse::<f64>())
            && a.is_finite()
            && b.is_finite()
        {
            out.push((cells[0].into(), a, b));
        }
    }
    Ok(out)
}
pub fn historical_view(w: &Workspace, task: Uuid) -> Result<Option<HistoricalDisplay>> {
    if !released(w)?.iter().any(|r| r.resource.task == Some(task)) {
        return Ok(None);
    }
    let path = owned_path(
        w,
        &PathBuf::from(format!(".jfquant/storage/history/{task}.json")),
    )?;
    let saved: HistoricalView = read(&path)?;
    if saved.schema_version != 1 || saved.task != task {
        return Err("storage_history_identity_mismatch".into());
    }
    Ok(Some((saved.result, saved.nav)))
}
fn input_paths(value: &Value, out: &mut Vec<(Option<String>, PathBuf)>) {
    match value {
        Value::Object(o) => {
            if o.contains_key("signature")
                && o.contains_key("kind")
                && let Some(path) = o.get("path").and_then(Value::as_str)
            {
                out.push((
                    o.get("id").and_then(Value::as_str).map(str::to_owned),
                    path.into(),
                ));
            }
            for (k, v) in o {
                if matches!(
                    k.as_str(),
                    "data_root" | "market_path" | "corporate_actions"
                ) && let Some(path) = v.as_str()
                {
                    out.push((None, path.into()));
                }
                input_paths(v, out);
            }
        }
        Value::Array(a) => {
            for v in a {
                input_paths(v, out)
            }
        }
        _ => {}
    }
}
pub fn availability(w: &Workspace, task: Uuid) -> Result<ResourceAvailability> {
    let items = released(w)?;
    let root = w.resolve(format!(".jfquant/runs/{task}"));
    let record: RunRecord = read(&root.join("run.json"))?;
    let workbench = read::<Value>(&root.join("artifacts.json"))
        .ok()
        .and_then(|v| {
            serde_json::from_value::<crate::research::pipeline::PipelineArtifact>(
                v["artifact"].clone(),
            )
            .ok()
        });
    let mut result = ResourceAvailability {
        historical_view: root.join("summary.json").is_file()
            || root.join("training.json").is_file(),
        models_reusable: root.join("training.json").is_file()
            || workbench.as_ref().is_some_and(|a| {
                w.resolve(&a.training.model_path).is_file()
                    && w.resolve(&a.training.manifest_path).is_file()
            }),
        predictions_replayable: root.join("prediction_manifest.json").is_file()
            && root.join("predictions.parquet").is_file(),
        original_inputs_available: true,
        full_verification_available: root.join("summary.json").is_file(),
        ..Default::default()
    };
    for item in items.iter().filter(|r| r.resource.task == Some(task)) {
        result.released_resources.push(item.resource.id.clone());
        result.reasons.push(format!(
            "{} 已永久释放（计划 {}）",
            item.resource.name, item.plan
        ));
        match item.resource.category {
            ResourceCategory::Models => result.models_reusable = false,
            ResourceCategory::Predictions => result.predictions_replayable = false,
            ResourceCategory::Backtest => result.full_verification_available = false,
            _ => {}
        }
    }
    let mut paths = vec![];
    input_paths(&record.request, &mut paths);
    if paths.iter().any(|(id, p)| {
        !w.resolve(p).exists()
            || items.iter().any(|item| {
                item.resource.task.is_none()
                    && id.as_ref().map_or_else(
                        || w.resolve(p).starts_with(w.resolve(&item.resource.path)),
                        |id| item.resource.name == *id,
                    )
            })
    }) {
        result.original_inputs_available = false;
        result.full_verification_available = false;
        result.predictions_replayable = false;
        result
            .reasons
            .push("原始输入已缺失，完整复现不可用；独立模型复用按模型包另行判断".into());
    }
    if let Ok(manifest) = read::<Value>(&root.join("prediction_manifest.json"))
        && let Some(key) = manifest["signature"].as_str()
    {
        let cache = root
            .parent()
            .unwrap()
            .join(".predictions-v1")
            .join(key.trim_start_matches("sha256:"));
        if !cache.join("manifest.json").is_file() {
            result.predictions_replayable = false;
            result.reasons.push("共享预测回放材料不可用".into());
        }
    }
    if !result.released_resources.is_empty() {
        result.historical_view = w
            .resolve(format!(".jfquant/storage/history/{task}.json"))
            .is_file();
    }
    Ok(result)
}
pub fn require_available(w: &Workspace, task: Uuid, capability: &str) -> Result<()> {
    let state = availability(w, task)?;
    let available = match capability {
        "models" => state.models_reusable,
        "predictions" => state.predictions_replayable,
        "verification" => state.full_verification_available,
        _ => return Err("unknown storage capability".into()),
    };
    if !available && (!state.released_resources.is_empty() || !state.original_inputs_available) {
        return Err(format!(
            "storage_artifacts_released:{capability}:{}",
            state.reasons.join("; ")
        )
        .into());
    }
    Ok(())
}
pub(crate) fn require_path_available(root: &Path, capability: &str) -> Result<()> {
    let Some(task) = root
        .file_name()
        .and_then(|s| s.to_str())
        .and_then(|s| s.parse::<Uuid>().ok())
    else {
        return Ok(());
    };
    if let Some(workspace) = root
        .ancestors()
        .find(|p| p.join(".jfquant/workspace.json").is_file())
    {
        require_available(
            &Workspace {
                root: workspace.canonicalize()?,
            },
            task,
            capability,
        )?;
    }
    Ok(())
}
