//! Workspace-local immutable index configurations shared by App and CLI.
use crate::{Result, Workspace, atomic_json, signature};
use analytics_core::{Date, indices::IndexConfig};
use analytics_storage::collector::{CollectionRequest, Provider, validate_index};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs};
/// Immutable portable configuration version; metadata and computation signatures are distinct.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IndexVersion {
    /// Content-derived immutable version identifier.
    pub id: String,
    /// Full configuration integrity hash.
    pub signature: String,
    /// Hash of the interpretation contract, excluding presentation fields.
    pub calculation_signature: String,
    /// Explicit provenance, never supplied by imported configuration.
    pub builtin: bool,
    /// Portable user-maintainable configuration.
    pub config: IndexConfig,
}
/// Shared worker operations. Reads never trigger acquisition.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum IndexOperation {
    /// Shared history maintenance; never implies a paid MSCI API request.
    History {
        /// Explicit operation and frozen input facts.
        request: Box<crate::membership_history::Operation>,
    },
    /// Builtin seeds and all immutable workspace versions.
    List,
    /// Validate and save a new immutable version.
    Save {
        /// Portable definition to validate and publish.
        config: Box<IndexConfig>,
    },
    /// Read portable JSON in the worker and save a validated version.
    Import {
        /// Explicit source file.
        path: std::path::PathBuf,
    },
    /// Retrieve a validated portable configuration/version.
    Get {
        /// Immutable version or builtin seed.
        version: String,
    },
    /// Atomically export a portable configuration within the workspace.
    Export {
        /// Immutable version or builtin seed.
        version: String,
        /// Workspace-relative destination.
        path: std::path::PathBuf,
    },
    /// Explicit acquisition through the normal collector; success is scoped to these dates.
    Verify {
        /// Immutable version or builtin seed.
        version: String,
        /// Explicit adapter.
        provider: Provider,
        /// First requested observation.
        start: Date,
        /// Last requested observation.
        end: Date,
        /// Also acquire observation-date members, never historical backfill.
        current_members: bool,
    },
    /// Capture official MSCI CNY price closes using a frozen market's exchange calendar.
    /// This source supplies neither OHLC nor member history.
    CollectMsci {
        /// Immutable index configuration containing an exact `msci` mapping.
        version: String,
        /// Registered market supplying the task's exchange calendar.
        calendar_market: String,
        /// First requested date.
        start: Date,
        /// Last requested date.
        end: Date,
    },
}
fn locator_key(provider: &str, code: &str) -> String {
    if provider == "hithink" {
        analytics_storage::hithink::index_code(code).unwrap_or_else(|_| code.to_owned())
    } else {
        code.to_owned()
    }
}
fn item(config: IndexConfig, builtin: bool) -> Result<IndexVersion> {
    config.validate()?;
    let hash = signature(&config)?;
    Ok(IndexVersion {
        id: if builtin {
            format!("builtin:{}", config.definition.id)
        } else {
            format!("index-{}", &hash[7..])
        },
        calculation_signature: signature(&config.calculation_contract())?,
        signature: hash,
        builtin,
        config,
    })
}
impl Workspace {
    /// Save validated mappings without overwriting any existing configuration.
    pub fn save_index(&self, config: IndexConfig) -> Result<IndexVersion> {
        config.validate()?;
        for p in config.mappings.keys() {
            analytics_storage::collector::validate_index_mapping(&config, p)?;
        }
        for existing in self.indices()? {
            if existing.config.definition.id != config.definition.id
                && (existing.config.data_key == config.data_key
                    || existing.config.mappings.iter().any(|(p, c)| {
                        config
                            .mappings
                            .get(p)
                            .is_some_and(|candidate| locator_key(p, candidate) == locator_key(p, c))
                    }))
            {
                return Err(
                    "index identity conflict: key or provider locator already assigned".into(),
                );
            }
            if existing.config.definition.id == config.definition.id
                && (existing.config.definition.official_code != config.definition.official_code
                    || existing.config.data_key != config.data_key
                    || existing.config.definition.currency != config.definition.currency
                    || existing.config.definition.return_basis != config.definition.return_basis)
            {
                return Err("index identity changed: use a distinct index identity".into());
            }
        }
        let v = item(config, false)?;
        let path = self.resolve(format!(".jfquant/indices/{}.json", v.id));
        if path.exists() {
            return self.index(&v.id);
        }
        atomic_json(&path, &v)?;
        Ok(v)
    }
    /// Read and verify an immutable version, or an explicit builtin seed.
    pub fn index(&self, id: &str) -> Result<IndexVersion> {
        if let Some(name) = id.strip_prefix("builtin:") {
            return item(
                analytics_core::indices::builtin_config(name)
                    .ok_or("builtin index has no configured route")?,
                true,
            );
        }
        if id.len() != 70
            || !id.starts_with("index-")
            || !id[6..].bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err("invalid index version ID".into());
        }
        let v: IndexVersion = serde_json::from_slice(&fs::read(
            self.resolve(format!(".jfquant/indices/{id}.json")),
        )?)?;
        let expected = item(v.config.clone(), false)?;
        if v.id != id
            || v.signature != expected.signature
            || v.calculation_signature != expected.calculation_signature
            || v.builtin
        {
            return Err("index version changed".into());
        }
        Ok(v)
    }
    /// Resolve a template through the same immutable definition used by acquisition.
    pub fn index_template(&self, version: &str) -> Result<crate::ResearchTemplate> {
        let v = self.index(version)?;
        if v.builtin {
            return crate::ResearchTemplate::builtin_index(&v.config.definition.id);
        }
        let mut t = crate::ResearchTemplate::builtin()?;
        t.id = format!("index-{}", v.config.definition.id);
        t.name = format!("{} research", v.config.definition.name);
        t.benchmark = v.config.data_key;
        t.pool_version = format!("index:{}", v.config.definition.id);
        t.validate()?;
        Ok(t)
    }
    /// Offline list; builtin seeds and saved versions share the same definition and resolver.
    pub fn indices(&self) -> Result<Vec<IndexVersion>> {
        let mut out = analytics_core::indices::all()
            .iter()
            .filter_map(|i| analytics_core::indices::builtin_config(&i.id))
            .map(|c| item(c, true))
            .collect::<Result<Vec<_>>>()?;
        let root = self.resolve(".jfquant/indices");
        if root.exists() {
            for e in fs::read_dir(root)? {
                let p = e?.path();
                if p.extension().is_some_and(|e| e == "json") {
                    out.push(
                        self.index(
                            p.file_stem()
                                .and_then(|s| s.to_str())
                                .ok_or("invalid index version path")?,
                        )?,
                    );
                }
            }
        }
        out.sort_by(|a, b| {
            a.config
                .definition
                .id
                .cmp(&b.config.definition.id)
                .then(a.id.cmp(&b.id))
        });
        Ok(out)
    }
}
/// Construct the exact normal collection request from a frozen definition.
pub fn collection(
    config: IndexConfig,
    provider: Provider,
    start: Date,
    end: Date,
    current_members: bool,
) -> Result<CollectionRequest> {
    validate_index(&config, provider)?;
    let key = config.data_key.clone();
    let r = CollectionRequest {
        index_inputs: BTreeMap::from([(key.clone(), config.clone())]),
        source_bindings: BTreeMap::from([(key.clone(), config.mappings[provider.name()].clone())]),
        provider,
        adjustment_source: match provider {
            Provider::Hithink => analytics_storage::adjustment::AdjustmentSource::HithinkForward,
            Provider::Tdxaidata => analytics_storage::adjustment::AdjustmentSource::TdxForward,
            _ => return Err("index source unsupported".into()),
        },
        start,
        end,
        instruments: [key.clone()].into(),
        benchmarks: [key].into(),
        universes: if current_members {
            [config.definition.id].into()
        } else {
            Default::default()
        },
    };
    r.validate()?;
    Ok(r)
}

pub(crate) fn collect_msci(
    w: &Workspace,
    run: &crate::RunRecord,
    version: &str,
    calendar_market: &str,
    range: analytics_core::DateRange,
    context: &analytics_core::task::TaskContext,
) -> Result<crate::WorkflowOutcome> {
    let v = w.index(version)?;
    if serde_json::to_value(&v)? != run.request["index_version"]
        || run.request["index_adapter_signature"] != analytics_storage::msci::signature()
    {
        return Err("frozen MSCI index or adapter changed".into());
    }
    let reference = crate::usecases::version(w, calendar_market, "market")?;
    let ds = analytics_storage::daily::DailyDataset::open(w.resolve(&reference.path))?;
    let staging = w.resolve(format!(".jfquant/staging/msci-{}", run.id));
    let root =
        analytics_storage::msci::collect(&v.config, &range, &ds.calendar, &staging, context)?;
    let mut source: serde_json::Value =
        serde_json::from_slice(&fs::read(staging.join("jfquant_source.json"))?)?;
    source["calendar_reference"] = serde_json::json!({"market":reference.id,"signature":reference.signature,"calendar_sha256":crate::signature(&ds.calendar)?});
    atomic_json(&staging.join("jfquant_source.json"), &source)?;
    let report: serde_json::Value =
        serde_json::from_slice(&fs::read(staging.join("report.json"))?)?;
    let relative = root.strip_prefix(&staging)?.to_owned();
    context.check()?;
    let output = w.resolve(format!("data/msci-index-{}", run.id));
    fs::rename(staging, &output)?;
    let data = Some(w.register(&output.join(relative))?);
    let details = serde_json::json!({"schema_version":1,"index_version":v,"provider":"msci","adapter_signature":analytics_storage::msci::signature(),"requested":range,"calendar_market":calendar_market,"report":report,"historical_members_supported":false});
    Ok(crate::WorkflowOutcome { data, details })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn custom() -> IndexConfig {
        let mut c = analytics_core::indices::builtin_config("csi1000").unwrap();
        c.definition.id = "test_index".into();
        c.definition.name = "Fixture index".into();
        c.definition.official_code = "TEST001".into();
        c.definition.symbol = None;
        c.data_key = "IDX_test_index".into();
        c.mappings = BTreeMap::from([("hithink".into(), "TEST001.CSI".into())]);
        c
    }
    #[test]
    fn immutable_versions_preserve_identity_and_exclude_presentation_from_computation() {
        let tmp = tempfile::tempdir().unwrap();
        let w = Workspace::open(tmp.path()).unwrap();
        let c = custom();
        let first = w.save_index(c.clone()).unwrap();
        assert_eq!(first.id, w.save_index(c.clone()).unwrap().id);
        let mut display = c.clone();
        display.definition.name = "Renamed".into();
        display.definition.description = "Updated reference".into();
        let second = w.save_index(display).unwrap();
        assert_ne!(first.id, second.id);
        assert_eq!(first.calculation_signature, second.calculation_signature);
        let mut route = c.clone();
        route
            .mappings
            .insert("hithink".into(), "TEST002.CSI".into());
        let third = w.save_index(route).unwrap();
        assert_ne!(first.calculation_signature, third.calculation_signature);
        assert_eq!(w.index(&first.id).unwrap().config, c);
        let mut conflict = c.clone();
        conflict.definition.id = "another_index".into();
        assert!(w.save_index(conflict).is_err());
        let mut identity = c.clone();
        identity.definition.official_code = "OTHER".into();
        assert!(w.save_index(identity).is_err());
        let mut alias = c.clone();
        alias.definition.id = "another_index".into();
        alias.data_key = "IDX_another_index".into();
        alias.mappings.insert("hithink".into(), "000852.SH".into());
        assert!(w.save_index(alias).is_err());
        let p = w.resolve(format!(".jfquant/indices/{}.json", first.id));
        let mut damaged: serde_json::Value =
            serde_json::from_slice(&fs::read(&p).unwrap()).unwrap();
        damaged["config"]["definition"]["name"] = "tampered".into();
        fs::write(&p, serde_json::to_vec(&damaged).unwrap()).unwrap();
        assert!(w.index(&first.id).is_err());
    }
    #[test]
    fn frozen_collection_and_import_do_not_follow_later_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let w = Workspace::open(tmp.path()).unwrap();
        let c = custom();
        let first = w.save_index(c.clone()).unwrap();
        let request = crate::WorkflowRequest::Index {
            request: IndexOperation::Verify {
                version: first.id.clone(),
                provider: Provider::Hithink,
                start: "2026-10-12".parse().unwrap(),
                end: "2026-10-16".parse().unwrap(),
                current_members: false,
            },
        };
        let frozen = crate::snapshot(&w, &request).unwrap();
        assert_eq!(
            frozen["index_version"]["config"],
            serde_json::to_value(&c).unwrap()
        );
        let mut updated = c.clone();
        updated.mappings.insert("hithink".into(), "NEW.CSI".into());
        w.save_index(updated).unwrap();
        assert_eq!(
            frozen["index_version"]["config"]["mappings"]["hithink"],
            "TEST001.CSI"
        );
        let r = collection(
            c.clone(),
            Provider::Hithink,
            "2026-10-12".parse().unwrap(),
            "2026-10-16".parse().unwrap(),
            false,
        )
        .unwrap();
        assert!(r.universes.is_empty());
        assert_eq!(r.source_bindings[&c.data_key], "TEST001.CSI");
        let path = w.resolve("index.json");
        fs::write(&path, serde_json::to_vec(&c).unwrap()).unwrap();
        let imported = crate::snapshot(
            &w,
            &crate::WorkflowRequest::Index {
                request: IndexOperation::Import { path: path.clone() },
            },
        )
        .unwrap();
        fs::write(path, b"{}").unwrap();
        assert_eq!(imported["index_import"], serde_json::to_value(&c).unwrap());
        assert!(collection(c, Provider::Tdxaidata, r.start, r.end, false).is_err());
    }
    #[test]
    fn cancelled_save_publishes_nothing_and_portable_export_roundtrips() {
        let tmp = tempfile::tempdir().unwrap();
        let w = Workspace::open(tmp.path()).unwrap();
        let request = crate::WorkflowRequest::Index {
            request: IndexOperation::Save {
                config: Box::new(custom()),
            },
        };
        let record = w
            .begin(
                uuid::Uuid::new_v4(),
                "index",
                crate::snapshot(&w, &request).unwrap(),
            )
            .unwrap();
        let context = analytics_core::task::TaskContext::default();
        context
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(crate::usecases::execute(&w, &request, &record, &context).is_err());
        assert!(!w.resolve(".jfquant/indices").exists());
        let context = analytics_core::task::TaskContext::default();
        let result = crate::usecases::execute(&w, &request, &record, &context).unwrap();
        let id = result.details["index_version"]["id"]
            .as_str()
            .unwrap()
            .to_owned();
        let export = crate::WorkflowRequest::Index {
            request: IndexOperation::Export {
                version: id.clone(),
                path: "portable-index.json".into(),
            },
        };
        crate::usecases::execute(&w, &export, &record, &context).unwrap();
        let portable: IndexConfig =
            serde_json::from_slice(&fs::read(w.resolve("portable-index.json")).unwrap()).unwrap();
        assert_eq!(w.save_index(portable).unwrap().id, id);
    }
    #[test]
    fn msci_task_freezes_calendar_config_and_adapter_without_network_queries() {
        let tmp = tempfile::tempdir().unwrap();
        let w = Workspace::open(tmp.path()).unwrap();
        let (market, _, _) = crate::demo::create(&w).unwrap();
        let mut c = custom();
        c.definition.official_code = "746059".into();
        c.mappings = BTreeMap::from([("msci".into(), "746059".into())]);
        let index = w.save_index(c.clone()).unwrap();
        let request = crate::WorkflowRequest::Index {
            request: IndexOperation::CollectMsci {
                version: index.id.clone(),
                calendar_market: market.id.clone(),
                start: "2007-01-02".parse().unwrap(),
                end: "2007-01-12".parse().unwrap(),
            },
        };
        let frozen = crate::snapshot(&w, &request).unwrap();
        assert_eq!(frozen["inputs"][0]["signature"], market.signature);
        assert_eq!(
            frozen["index_version"]["config"],
            serde_json::to_value(&c).unwrap()
        );
        let mut edited = c;
        edited.definition.name = "Display edit".into();
        w.save_index(edited).unwrap();
        assert_eq!(frozen["index_version"]["id"], index.id);
        let mut record = w.begin(uuid::Uuid::new_v4(), "index", frozen).unwrap();
        record.request["index_adapter_signature"] = "stale".into();
        let error = crate::usecases::execute(
            &w,
            &request,
            &record,
            &analytics_core::task::TaskContext::default(),
        )
        .unwrap_err();
        assert!(error.to_string().contains("frozen MSCI"));
        assert!(!w.resolve(format!("data/msci-index-{}", record.id)).exists());
        let invalid = crate::WorkflowRequest::Index {
            request: IndexOperation::CollectMsci {
                version: index.id,
                calendar_market: market.id,
                start: "2000-01-01".parse().unwrap(),
                end: "2007-01-12".parse().unwrap(),
            },
        };
        assert!(crate::snapshot(&w, &invalid).is_err());
    }
}
