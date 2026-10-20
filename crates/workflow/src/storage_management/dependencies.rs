use super::{inventory::Registry, *};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

fn path_reference(
    r: &mut Registry<'_>,
    source: &str,
    path: &Path,
    kind: ReferenceKind,
    reason: &str,
) {
    let relative = if path.is_absolute() {
        match path.strip_prefix(r.w.root()) {
            Ok(p) => p.to_owned(),
            Err(_) => {
                r.external.insert(path.to_owned());
                return;
            }
        }
    } else {
        path.to_owned()
    };
    let resolved =
        r.w.resolve(&relative)
            .canonicalize()
            .ok()
            .and_then(|p| p.strip_prefix(r.w.root()).ok().map(Path::to_owned))
            .unwrap_or(relative);
    if let Some(targets) = r.logical_owners.get(&resolved).cloned() {
        for target in targets {
            r.reference(source, &target, kind, reason);
        }
    } else if let Some(target) = r.target(&resolved) {
        r.reference(source, &target, kind, reason);
    } else if kind == ReferenceKind::Integrity {
        r.protect(
            source,
            format!("必要依赖未被识别或已缺失：{}", path.display()),
        );
    }
}
fn strings(value: &Value, out: &mut Vec<String>) {
    match value {
        Value::String(s) => out.push(s.clone()),
        Value::Array(a) => {
            for v in a {
                strings(v, out)
            }
        }
        Value::Object(o) => {
            for v in o.values() {
                strings(v, out)
            }
        }
        _ => {}
    }
}
pub(crate) fn discover(r: &mut Registry<'_>, context: &TaskContext) -> Result<()> {
    let resources: Vec<_> = r.resources.values().cloned().collect();
    for resource in &resources {
        context.check()?;
        if resource.category == ResourceCategory::Market && !resource.temporary {
            for name in ["market_view.json", "market_supplement.json"] {
                let p = resource.path.join(name);
                if !r.contains(&p) {
                    continue;
                }
                let v = match r.metadata(&p) {
                    Ok(v) => v,
                    Err(e) => {
                        r.protect(&resource.id, e.to_string());
                        continue;
                    }
                };
                let paths = if name == "market_view.json" {
                    match serde_json::from_value::<analytics_storage::market_view::MarketView>(v) {
                        Ok(v) => vec![v.base_path, v.coefficients_path],
                        Err(e) => {
                            r.protect(&resource.id, e.to_string());
                            continue;
                        }
                    }
                } else {
                    match serde_json::from_value::<
                        analytics_storage::market_view::supplement::SupplementView,
                    >(v)
                    {
                        Ok(v) => std::iter::once(v.base_path)
                            .chain(v.selections.into_iter().map(|s| s.source_path))
                            .collect(),
                        Err(e) => {
                            r.protect(&resource.id, e.to_string());
                            continue;
                        }
                    }
                };
                for path in paths {
                    path_reference(
                        r,
                        &resource.id,
                        &path,
                        ReferenceKind::Integrity,
                        "保留的数据视图仍需读取此输入",
                    );
                }
            }
        }
        if resource.category == ResourceCategory::Predictions && resource.task.is_some() {
            if let Ok(v) = r.metadata(&resource.path)
                && let Some(key) = v["signature"].as_str()
                && let Some(root) = resource.path.parent().and_then(Path::parent)
            {
                let path = root
                    .join(".predictions-v1")
                    .join(key.trim_start_matches("sha256:"));
                path_reference(
                    r,
                    &resource.id,
                    &path,
                    ReferenceKind::Integrity,
                    "保留的预测回放依赖该共享预测包",
                );
            }
            if let Some(task) = resource.task {
                let task_root = r
                    .runs
                    .iter()
                    .find(|(_, run)| run.id == task)
                    .map(|(root, _)| root.clone());
                if let Some(root) = task_root {
                    if r.contains(&root.join("summary.json"))
                        && let Some(backtest) = r
                            .resources
                            .values()
                            .find(|v| {
                                v.task == Some(task) && v.category == ResourceCategory::Backtest
                            })
                            .map(|v| v.id.clone())
                    {
                        r.reference(
                            &resource.id,
                            &backtest,
                            ReferenceKind::Integrity,
                            "保留的预测回放需要来源回测的核验材料",
                        );
                    }
                    // Prediction identity may need the signed descriptor without opening model bytes.
                    // Give that descriptor shared ownership so model release does not break replay.
                    let mut current = root;
                    for _ in 0..32 {
                        let descriptor = current.join("training.json");
                        if let Some(mut file) = r.files.get_mut(&descriptor) {
                            if !file.owners.contains(&resource.id) {
                                file.owners.push(resource.id.clone());
                            }
                            break;
                        }
                        let Some(run) = r.runs.get(&current) else {
                            break;
                        };
                        let request = &run.request["request"];
                        let next = request["training"]
                            .as_str()
                            .or_else(|| request["source"].as_str())
                            .and_then(|s| s.parse::<Uuid>().ok());
                        let Some(next) = next else {
                            break;
                        };
                        let Some(root) = r
                            .runs
                            .iter()
                            .find(|(_, v)| v.id == next)
                            .map(|(root, _)| root.clone())
                        else {
                            break;
                        };
                        current = root;
                    }
                }
            }
        }
    }
    let mut by_task = BTreeMap::<String, Vec<String>>::new();
    for resource in r.resources.values() {
        if let Some(task) = resource.task {
            by_task
                .entry(task.to_string())
                .or_default()
                .push(resource.id.clone());
        }
    }
    // Completed request snapshots are provenance; active requests pin their actual inputs.
    for (root, run) in r.runs.clone() {
        if super::inventory::management_run(&run) {
            continue;
        }
        let sources: Vec<_> = r
            .resources
            .values()
            .filter(|v| v.task == Some(run.id))
            .map(|v| v.id.clone())
            .collect();
        let mut values = vec![];
        strings(&run.request, &mut values);
        for value in values {
            let mut targets = r.names.get(&value).cloned().unwrap_or_default();
            targets.extend(by_task.get(&value).into_iter().flatten().cloned());
            targets.sort();
            targets.dedup();
            for source in &sources {
                for target in &targets {
                    r.reference(
                        source,
                        target,
                        ReferenceKind::Provenance,
                        "已完成任务的冻结来源",
                    );
                }
                if value.contains('/') {
                    path_reference(
                        r,
                        source,
                        Path::new(&value),
                        ReferenceKind::Provenance,
                        "任务保留来源路径与身份",
                    );
                }
            }
            if run.status == crate::RunStatus::Running {
                for target in targets {
                    r.protect(&target, format!("运行任务 {} 正在使用", run.id));
                }
                if let Some(target) = r.target(Path::new(&value)) {
                    r.protect(&target, "运行任务正在使用此输入");
                }
            }
        }
        // Imported records refer to an external directory; they never claim ownership of it.
        if run.read_only
            && let Some(path) = run.request["source"].as_str()
        {
            if Path::new(path).is_absolute() && !Path::new(path).starts_with(r.w.root()) {
                r.external.insert(path.into());
            } else {
                let source = root.join("run.json").display().to_string();
                if let Some(target) = r.target(Path::new(path)) {
                    r.reference(
                        &source,
                        &target,
                        ReferenceKind::Unresolved,
                        "只读导入记录仍引用此目录",
                    );
                }
            }
        }
    }
    unresolved_metadata(r, context)
}
fn unresolved_metadata(r: &mut Registry<'_>, context: &TaskContext) -> Result<()> {
    // Unknown JSON outside established packages can contain user-maintained references.
    // Only metadata/configuration is read: raw replies, factor arrays and large datasets are excluded.
    let unknown: Vec<_> = r
        .files
        .json_values()
        .filter(|f| {
            f.owners.is_empty()
                && !r.metadata.contains_key(&f.path)
                && f.path.extension().is_some_and(|e| e == "json")
                && !super::inventory::volatile(r, &f.path)
                && !f.path.ancestors().any(|root| {
                    r.runs
                        .get(root)
                        .is_some_and(super::inventory::management_run)
                })
        })
        .map(|f| f.path.clone())
        .collect();
    for path in unknown {
        context.check()?;
        let value = match r.metadata(&path) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let mut values = vec![];
        strings(&value, &mut values);
        for value in values {
            let mut targets = r.names.get(&value).cloned().unwrap_or_default();
            if value.contains('/') {
                let p = Path::new(&value);
                let p = p.strip_prefix(r.w.root()).unwrap_or(p);
                if let Some(owners) = r.logical_owners.get(p) {
                    targets.extend(owners.clone());
                } else if let Some(target) = r.target(p) {
                    targets.push(target);
                } else if p.is_absolute() {
                    r.external.insert(p.to_owned());
                }
            }
            for target in targets {
                r.reference(
                    &path.display().to_string(),
                    &target,
                    ReferenceKind::Unresolved,
                    "未解析的元数据引用，需要先核实",
                );
            }
        }
    }
    Ok(())
}
pub(crate) fn release_usage(files: &[StorageFile], selected: &BTreeSet<String>) -> SpaceUsage {
    let mut inodes = BTreeMap::<(u64, u64), (u64, &StorageFile)>::new();
    for file in files
        .iter()
        .filter(|f| !f.owners.is_empty() && f.owners.iter().all(|o| selected.contains(o)))
    {
        let entry = inodes
            .entry((file.stamp.device, file.stamp.inode))
            .or_insert((0, file));
        entry.0 += 1;
    }
    let mut usage = SpaceUsage::default();
    for (links, file) in inodes.into_values() {
        usage.files += links;
        usage.logical_bytes += file.stamp.bytes;
        if links == file.stamp.links {
            usage.allocated_bytes += file.stamp.allocated;
        }
    }
    usage
}
pub(crate) fn build_plan(
    w: &Workspace,
    inventory: &StorageInventory,
    ids: &[String],
    id: Uuid,
) -> Result<CleanupPlan> {
    let selected: BTreeSet<_> = ids.iter().cloned().collect();
    let mut blockers = vec![];
    let mut impacts = vec![];
    if selected.is_empty() {
        blockers.push("请选择要释放的资源".into());
    }
    if !inventory.complete {
        blockers.push("扫描不完整，需先处理扫描诊断".into());
        blockers.extend(
            inventory
                .diagnostics
                .iter()
                .map(|d| format!("扫描诊断：{d}")),
        );
    }
    let mut resources = vec![];
    for id in &selected {
        match inventory.resources.iter().find(|r| &r.id == id) {
            Some(r) => {
                if !r.deletable || r.released {
                    blockers.push(format!("{}：{}", r.name, r.reasons.join("；")));
                }
                resources.push(r.clone());
            }
            None => blockers.push(format!("资源不存在：{id}")),
        }
    }
    for edge in &inventory.references {
        if selected.contains(&edge.target) && !selected.contains(&edge.source) {
            match edge.kind {
                ReferenceKind::Integrity | ReferenceKind::Unresolved => blockers.push(format!(
                    "{} → {}：{}",
                    edge.source, edge.target, edge.reason
                )),
                ReferenceKind::Provenance => impacts.push(format!(
                    "{} 的原始来源将被释放；历史摘要保留，完整复现可能不可用",
                    edge.source
                )),
            }
        }
    }
    for r in &resources {
        if r.task.is_some() {
            impacts.push(format!(
                "{}：相关产物不可再用，任务参数、摘要和收益曲线保留",
                r.name
            ));
        }
        if r.temporary {
            impacts.push(format!("{}：该检查点不能继续恢复", r.name));
        }
    }
    // Shared ownership is a retained-file rule, not an implicit cascading deletion.
    let index = analytics_storage::index::Index::open(&owned_path(
        w,
        Path::new(".jfquant/storage/index.sqlite"),
    )?)?;
    let candidate_files: Vec<StorageFile> =
        if index.generation_exists(&inventory.scan_id.to_string())? {
            index.selected(&inventory.scan_id.to_string(), ids)?
        } else {
            blockers.push("扫描索引已过期，请重新预览".into());
            vec![]
        };
    let files: Vec<_> = candidate_files
        .iter()
        .filter(|f| !f.owners.is_empty() && f.owners.iter().all(|o| selected.contains(o)))
        .cloned()
        .collect();
    if files.is_empty() {
        blockers.push("选择中没有可独立释放的文件；共享材料仍有保留的所有者".into());
    }
    let estimated_release = release_usage(&candidate_files, &selected);
    blockers.sort();
    blockers.dedup();
    impacts.sort();
    impacts.dedup();
    let fingerprint = crate::signature(&(&resources, &files, &inventory.metadata_fingerprint))?;
    Ok(CleanupPlan {
        schema_version: 1,
        id,
        created_at: crate::now(),
        workspace: w.root().to_owned(),
        resources,
        files,
        metadata_fingerprint: inventory.metadata_fingerprint.clone(),
        fingerprint,
        blockers,
        impacts,
        estimated_release,
    })
}
