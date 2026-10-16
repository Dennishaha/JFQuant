//! Immutable custom revisions, shared by service clients and frozen task requests.
#![allow(missing_docs)]
use crate::{Result, Workspace, atomic_json};
use analytics_factor::{
    operators::{CompiledExpression, compile_formula},
    selection::CustomFactorDefinition,
};
use serde::{Deserialize, Serialize};
use std::{fs, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct FactorDraft {
    pub id: Option<String>,
    pub expected_revision: Option<u32>,
    pub name: String,
    pub description: String,
    pub formula: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactorEntry {
    pub definition: CustomFactorDefinition,
    pub archived: bool,
}
fn directory(w: &Workspace, id: &str) -> Result<PathBuf> {
    let raw = id
        .strip_prefix("custom_")
        .ok_or("invalid_custom_factor_id")?;
    let uuid = uuid::Uuid::parse_str(raw)?;
    if id != format!("custom_{}", uuid.simple()) {
        return Err("invalid_custom_factor_id".into());
    }
    Ok(w.resolve(format!(".jfquant/factors/{id}")))
}
pub fn validate(draft: &FactorDraft) -> Result<CompiledExpression> {
    if draft.name.trim().is_empty() {
        return Err("因子名称不能为空".into());
    }
    Ok(compile_formula(&draft.formula)?)
}
pub fn list(w: &Workspace, include_archived: bool) -> Result<Vec<FactorEntry>> {
    let root = w.resolve(".jfquant/factors");
    if !root.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for e in fs::read_dir(root)? {
        let dir = e?.path();
        if !dir.is_dir() {
            continue;
        }
        let mut revisions = Vec::new();
        for f in fs::read_dir(&dir)? {
            let p = f?.path();
            if p.extension().is_some_and(|s| s == "json")
                && p.file_stem()
                    .and_then(|s| s.to_str())
                    .is_some_and(|s| s.parse::<u32>().is_ok())
            {
                let d: CustomFactorDefinition = serde_json::from_slice(&fs::read(&p)?)?;
                d.validate()?;
                if directory(w, &d.id)? != dir
                    || p.file_stem().unwrap().to_str() != Some(d.revision.to_string().as_str())
                {
                    return Err("custom_factor_revision_identity_mismatch".into());
                }
                revisions.push(d);
            }
        }
        revisions.sort_by_key(|d| d.revision);
        if let Some(definition) = revisions.pop() {
            let archived = dir.join("archived.json").exists();
            if include_archived || !archived {
                result.push(FactorEntry {
                    definition,
                    archived,
                });
            }
        }
    }
    result.sort_by(|a, b| a.definition.id.cmp(&b.definition.id));
    Ok(result)
}
pub fn save(w: &Workspace, draft: FactorDraft) -> Result<CustomFactorDefinition> {
    validate(&draft)?;
    let id = draft
        .id
        .unwrap_or_else(|| format!("custom_{}", uuid::Uuid::new_v4().simple()));
    let dir = directory(w, &id)?;
    let existing = list(w, true)?.into_iter().find(|e| e.definition.id == id);
    let revision = if let Some(e) = existing {
        if e.archived {
            return Err("归档因子不能直接编辑；请复制为新因子".into());
        }
        if draft.expected_revision != Some(e.definition.revision) {
            return Err("因子已被修改，请刷新后重试".into());
        }
        e.definition
            .revision
            .checked_add(1)
            .ok_or("revision overflow")?
    } else {
        if draft.expected_revision.is_some() {
            return Err("custom_factor_not_found".into());
        }
        1
    };
    let definition =
        CustomFactorDefinition::new(id, draft.name, draft.description, draft.formula, revision)?;
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{revision}.json"));
    // Immutable revision publication. Service serializes writers; create_new prevents accidental replacement.
    let staging = dir.join(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)?;
    use std::io::Write;
    file.write_all(&serde_json::to_vec_pretty(&definition)?)?;
    file.sync_all()?;
    let published = fs::hard_link(&staging, &path);
    let _ = fs::remove_file(staging);
    published?;
    Ok(definition)
}
pub fn archive(w: &Workspace, id: &str) -> Result<()> {
    let dir = directory(w, id)?;
    if !dir.exists() {
        return Err("custom_factor_not_found".into());
    }
    atomic_json(
        &dir.join("archived.json"),
        &serde_json::json!({"schema_version":1,"archived":true}),
    )
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FormulaPreview {
    pub formula: String,
    pub compiled: CompiledExpression,
    pub rows: Vec<(analytics_core::Date, String, Option<f64>)>,
    pub label: String,
}
/// Deterministic synthetic preview, explicitly separate from real-market evaluation.
pub fn preview(
    draft: &FactorDraft,
    cancel: crate::research::pipeline::Cancellation,
) -> Result<FormulaPreview> {
    use analytics_factor::{
        FactorExecutor, FactorRegistry, FactorRunRequest, InMemoryFactorExecutor, Panel,
        selection::FactorRef,
    };
    let compiled = validate(draft)?;
    let definition = CustomFactorDefinition::new(
        "custom_preview".into(),
        draft.name.clone(),
        draft.description.clone(),
        draft.formula.clone(),
        1,
    )?;
    let refs = vec![FactorRef::Custom {
        definition: Box::new(definition),
    }];
    let ids = vec!["custom_preview".to_string()];
    let registry = FactorRegistry::resolve(Default::default(), &refs, &ids)?;
    let days = (compiled.lookback as usize + 5).clamp(32, 1024);
    let mut panel = Panel {
        instrument_id: Vec::new(),
        trade_date: Vec::new(),
        columns: Default::default(),
    };
    for t in 0..days {
        for stock in 0..3 {
            panel.instrument_id.push(format!("SYNTHETIC_{stock}"));
            panel.trade_date.push(
                "2020-01-01"
                    .parse::<analytics_core::Date>()?
                    .checked_add_signed(chrono::Duration::days(t as i64))
                    .ok_or("preview date overflow")?,
            );
            let close = 20. + stock as f64 + t as f64 * 0.1;
            for (key, value) in [
                ("open", close - 0.1),
                ("high", close + 0.3),
                ("low", close - 0.2),
                ("close", close),
                ("volume", 1000. + t as f64),
                ("amount", (1000. + t as f64) * close),
                ("vwap", close),
                ("ret", 0.1 / (close - 0.1)),
            ] {
                panel
                    .columns
                    .entry(key.into())
                    .or_default()
                    .push(Some(value));
            }
        }
    }
    let version = registry.selection_version(&ids)?;
    let (tx, _) = std::sync::mpsc::channel();
    let result = InMemoryFactorExecutor {
        registry,
        panel: panel.clone(),
    }
    .compute(
        FactorRunRequest {
            date_range: analytics_core::DateRange {
                start: panel.trade_date[0],
                end: *panel.trade_date.last().unwrap(),
            },
            universe_id: "synthetic-preview".into(),
            factor_ids: ids,
            formula_version: version,
        },
        tx,
        cancel,
    )?;
    let values = &result.values["custom_preview"];
    let rows = (panel.trade_date.len().saturating_sub(15)..panel.trade_date.len())
        .map(|i| {
            (
                panel.trade_date[i],
                panel.instrument_id[i].clone(),
                values[i],
            )
        })
        .collect();
    Ok(FormulaPreview {
        formula: draft.formula.clone(),
        compiled,
        rows,
        label: "合成样本预览（最多1024行/证券，非市场有效性结果）".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use analytics_factor::{FactorRegistry, selection::FactorRef};
    fn draft() -> FactorDraft {
        FactorDraft {
            name: "20日动量".into(),
            formula: "CLOSE / GTJA_DELAY(CLOSE,20) - 1".into(),
            ..Default::default()
        }
    }
    #[test]
    fn immutable_revisions_archive_conflicts_and_tamper_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let w = Workspace::open(dir.path()).unwrap();
        let one = save(&w, draft()).unwrap();
        let refs = vec![FactorRef::Custom {
            definition: Box::new(one.clone()),
        }];
        let ids = vec![one.id.clone()];
        let signature = FactorRegistry::resolve(Default::default(), &refs, &ids)
            .unwrap()
            .selection_version(&ids)
            .unwrap();
        let mut next = draft();
        next.id = Some(one.id.clone());
        next.expected_revision = Some(1);
        next.formula = "CLOSE / GTJA_DELAY(CLOSE,10) - 1".into();
        let two = save(&w, next.clone()).unwrap();
        assert_eq!(two.revision, 2);
        assert_ne!(one.definition_hash, two.definition_hash);
        assert!(save(&w, next).is_err());
        archive(&w, &one.id).unwrap();
        assert!(list(&w, false).unwrap().is_empty());
        assert!(list(&w, true).unwrap()[0].archived);
        assert_eq!(
            FactorRegistry::resolve(Default::default(), &refs, &ids)
                .unwrap()
                .selection_version(&ids)
                .unwrap(),
            signature
        );
        let mut damaged = one.clone();
        damaged.formula = "CLOSE".into();
        assert!(damaged.validate().is_err());
        let mut damaged = one;
        damaged.compiled.operator_versions.clear();
        assert!(damaged.validate().is_err());
    }
    #[test]
    fn frozen_mixed_research_trains_and_old_model_survives_library_edits() {
        use analytics_core::{
            research_quality::FactorSet,
            schedule::{Fold, Schedule},
            task::TaskContext,
        };
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (market, actions, mut t) = crate::demo::create_canonical(&w).unwrap();
        let d = save(&w, draft()).unwrap();
        t.schedule = Schedule::Single {
            fold: Fold {
                train: analytics_core::DateRange {
                    start: "2009-01-02".parse().unwrap(),
                    end: "2009-01-12".parse().unwrap(),
                },
                validation: None,
                test: analytics_core::DateRange {
                    start: "2010-01-02".parse().unwrap(),
                    end: "2010-01-12".parse().unwrap(),
                },
            },
        };
        t.config.factor_refs = vec![
            FactorRef::Custom {
                definition: Box::new(d.clone()),
            },
            FactorRef::builtin(FactorSet::Alpha158, "alpha158_001_KMID"),
            FactorRef::builtin(FactorSet::Gtja191, "alpha_002"),
        ];
        t.config.factor_ids = t
            .config
            .factor_refs
            .iter()
            .map(|r| r.id().to_owned())
            .collect();
        t.formula_version = t.resolved_formula_version().unwrap();
        t.validate().unwrap();
        let request = crate::WorkflowRequest::TrainResearch {
            template: Box::new(t.clone()),
            market: market.id,
            actions: Some(actions.id.clone()),
        };
        let context = TaskContext::default();
        let run = w
            .begin(
                context.id,
                "mixed training",
                crate::snapshot(&w, &request).unwrap(),
            )
            .unwrap();
        crate::execute(&w, &request, &run, &context).unwrap();
        let artifact: serde_json::Value = serde_json::from_slice(
            &fs::read(w.resolve(&run.output).join("training.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(artifact["schema_version"], 4);
        assert_eq!(
            artifact["feature_order"],
            serde_json::json!(t.config.factor_ids)
        );
        crate::research::backtest::validate_training_artifact(&artifact).unwrap();
        let model = w.resolve(&run.output).join("fold-0000/manifest.json");
        assert_eq!(
            crate::research::load_manifest(&model)
                .unwrap()
                .schema_version,
            3
        );
        save(
            &w,
            FactorDraft {
                id: Some(d.id.clone()),
                expected_revision: Some(1),
                formula: "CLOSE / GTJA_DELAY(CLOSE,10) - 1".into(),
                ..draft()
            },
        )
        .unwrap();
        archive(&w, &d.id).unwrap();
        crate::research::backtest::validate_training_artifact(&artifact).unwrap();
        crate::research::load_manifest(&model).unwrap();
        let backtest = crate::WorkflowRequest::BacktestResearch {
            training: run.id,
            actions: actions.id,
            execution: None,
            event_check_policy: Default::default(),
        };
        let c = TaskContext::default();
        let run = w
            .begin(
                c.id,
                "reuse frozen model",
                crate::snapshot(&w, &backtest).unwrap(),
            )
            .unwrap();
        let result = crate::execute(&w, &backtest, &run, &c).unwrap();
        assert_eq!(
            result.details["schema_version"],
            crate::research::backtest::RESEARCH_SCHEMA_VERSION
        );
    }
}
