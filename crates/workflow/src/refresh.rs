//! Data acquisition plans shared by the App and CLI. Policy is separate from research recipes.
#![allow(missing_docs)]
use crate::*;
use analytics_core::task::TaskContext;
use analytics_storage::{corporate_actions::ActionBundle, daily::DailyDataset};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
};
use uuid::Uuid;

use analytics_core::{Date, DateRange};
use analytics_storage::{
    adjustment::{AdjustmentRecord, AdjustmentSource},
    collector::{self, CollectionRequest, Provider},
    daily::{DailyBar, DailyManifest, read_bars, write_bars},
};
use chrono::Timelike;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum DataRole {
    Quotes,
    Actions,
    Execution,
    Members,
    Calendar,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SourcePolicy {
    pub schema_version: u32,
    pub priorities: BTreeMap<DataRole, Vec<Provider>>,
    pub overlap_sessions: usize,
}
impl Default for SourcePolicy {
    fn default() -> Self {
        Self {
            schema_version: 2,
            overlap_sessions: 5,
            priorities: [
                DataRole::Quotes,
                DataRole::Actions,
                DataRole::Execution,
                DataRole::Members,
                DataRole::Calendar,
            ]
            .into_iter()
            .map(|role| {
                (
                    role,
                    if role == DataRole::Execution {
                        vec![]
                    } else {
                        vec![Provider::Hithink, Provider::Tdxaidata]
                    },
                )
            })
            .collect(),
        }
    }
}
impl SourcePolicy {
    pub fn validate(&self) -> Result<()> {
        if self.schema_version != 2 || self.overlap_sessions > 60 || self.priorities.len() != 5 {
            return Err(
                "unsupported_source_policy: reconfigure sources using policy format 2".into(),
            );
        }
        for role in [
            DataRole::Quotes,
            DataRole::Actions,
            DataRole::Execution,
            DataRole::Members,
            DataRole::Calendar,
        ] {
            let list = self.priorities.get(&role).ok_or("source_role_missing")?;
            if (list.is_empty() && role != DataRole::Execution)
                || list.contains(&Provider::Cninfo)
                || list.len() > 4
                || list.iter().enumerate().any(|(i, p)| list[..i].contains(p))
            {
                return Err("invalid_source_priority".into());
            }
        }
        Ok(())
    }
    pub fn load(w: &Workspace) -> Result<Self> {
        let p = w.resolve(".jfquant/source-policy.json");
        let value = if p.exists() {
            let raw: Value = serde_json::from_slice(&fs::read(p)?)?;
            analytics_core::source_map::reject_removed_sources(&raw)?;
            serde_json::from_value(raw)?
        } else {
            Self::default()
        };
        value.validate()?;
        Ok(value)
    }
    pub fn save(&self, w: &Workspace) -> Result<()> {
        self.validate()?;
        atomic_json(&w.resolve(".jfquant/source-policy.json"), self)
    }
    pub fn routes(&self, role: DataRole, fixed: Option<Provider>) -> Vec<Provider> {
        fixed.map_or_else(|| self.priorities[&role].clone(), |p| vec![p])
    }
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum RefreshMode {
    #[default]
    Incremental,
    Repair,
    Full,
    /// Keep the immutable market version and fetch only company-action shards.
    Actions,
}
/// Explicit native acquisition source; paths are resolved against the destination workspace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(untagged)]
pub enum ShardSource {
    /// A task in the current workspace.
    Task(Uuid),
    /// A task in an existing local workspace; source files remain unchanged.
    Workspace {
        /// Existing workspace directory.
        workspace: PathBuf,
        /// Native refresh task UUID.
        task_id: Uuid,
    },
}
pub(crate) fn shard_workspace(w: &Workspace, source: &ShardSource) -> Result<(Workspace, Uuid)> {
    match source {
        ShardSource::Task(id) => Ok((w.clone(), *id)),
        ShardSource::Workspace { workspace, task_id } => {
            let root = w.resolve(workspace).canonicalize()?;
            if !root.join(".jfquant/workspace.json").is_file() {
                return Err("shard_source_workspace_missing".into());
            }
            Ok((Workspace::open(root)?, *task_id))
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RefreshRequest {
    pub template: Box<ResearchTemplate>,
    pub market: String,
    pub actions: Option<String>,
    pub mode: RefreshMode,
    pub provider: Option<Provider>,
    pub end: Option<Date>,
    pub preview: bool,
    pub resume: Option<Uuid>,
    /// Bounded security workers; every worker owns its collector process and context.
    #[serde(default = "default_workers")]
    pub workers: usize,
    /// Reuse completed, identity-compatible security checkpoints from these native tasks.
    #[serde(default)]
    pub reuse_shards_from: Vec<ShardSource>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityPlan {
    pub instrument: String,
    pub requested: DateRange,
    pub refresh_ranges: Vec<DateRange>,
    pub existing_rows: usize,
    pub quote_sources: Vec<Provider>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshPlan {
    pub schema_version: u32,
    pub source_policy: SourcePolicy,
    pub base_market: DataVersion,
    pub base_actions: Option<DataVersion>,
    pub history: DateRange,
    pub securities: Vec<SecurityPlan>,
    pub membership: Value,
    pub notes: Vec<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RefreshResult {
    pub schema_version: u32,
    pub market: Option<DataVersion>,
    pub actions: Option<DataVersion>,
    pub training: Value,
    pub account: Value,
    pub outcomes: BTreeMap<String, Value>,
    pub failures: BTreeMap<String, String>,
}

fn default_workers() -> usize {
    3
}
fn validate_workers(workers: usize) -> Result<()> {
    if !(1..=8).contains(&workers) {
        return Err("refresh_workers_out_of_range: expected 1..8".into());
    }
    Ok(())
}

// Batches bound memory, process count and outstanding HTTP calls. Native handles
// stay on their caller; children receive only plain requests and task contexts.
pub fn catalog(w: &Workspace) -> Result<Value> {
    Ok(json!({"policy":SourcePolicy::load(w)?,"providers":[
        {"provider":"tdxaidata","roles":["quotes","calendar","members_current","adjustment","actions_reference"],"account_dates":"requires_supplement","historical_members":"not_supported","historical_execution":"not_verified","credential_required":true},
        {"provider":"hithink","roles":["quotes","calendar","members_current","actions"],"training":"supplier_forward_ohlc_raw_shares_cny_vwap_null","calendar_window":"one_year","index_window":"five_years","historical_members":"not_supported","credential_required":true},
        {"provider":"cninfo","roles":["announcements"],"candidate_only":true,"credential_required":false}
    ]}))
}
fn collection(provider: Provider, id: &str, r: &DateRange, benchmark: &str) -> CollectionRequest {
    CollectionRequest {
        provider,
        adjustment_source: match provider {
            Provider::Tdxaidata => AdjustmentSource::TdxForward,
            Provider::Hithink => AdjustmentSource::HithinkForward,
            _ => AdjustmentSource::None,
        },
        source_bindings: Default::default(),
        start: r.start,
        end: r.end,
        instruments: [id.to_owned()].into(),
        benchmarks: if id == benchmark {
            [id.to_owned()].into()
        } else {
            Default::default()
        },
        universes: Default::default(),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reference_precision_is_compatible_not_independently_verified() {
        let mut a: analytics_core::execution::CorporateAction = serde_json::from_value(json!({
            "id":"x", "instrument":"SH600000", "announced":null,"record_date":null,
            "ex_date":"2021-01-04","pay_date":null,"listing_date":null,
            "cash_per_share":"0.3995","shares_per_share":"0","source":"fixture"
        }))
        .unwrap();
        let row = json!({"bonus_per_ten_shares":"3.99","share_bonus_native":"0.00"});
        assert!(reference_precision_compatible(&a, &row));
        a.cash_per_share = "0.39951".into();
        assert!(!reference_precision_compatible(&a, &row));
        a.cash_per_share = "0.3995".into();
        a.shares_per_share = "0.1".into();
        assert!(!reference_precision_compatible(&a, &row));
        a.shares_per_share = "0".into();
        assert!(!reference_precision_compatible(
            &a,
            &json!({"bonus_per_ten_shares":"3.990","share_bonus_native":"0.00"})
        ));
    }
    #[test]
    fn probe_compares_coefficient_shapes_not_independent_absolute_anchors() {
        let date = |s: &str| s.parse::<Date>().unwrap();
        let original = BTreeMap::from([
            (date("2020-01-02"), json!({"close":10.,"factor":2.})),
            (date("2020-01-03"), json!({"close":5.,"factor":4.})),
        ]);
        let mut reanchored = BTreeMap::from([
            (date("2020-01-02"), json!({"close":10.,"factor":1.})),
            (date("2020-01-03"), json!({"close":5.,"factor":2.})),
        ]);
        assert!(compare_probe_windows(std::slice::from_ref(&original), &reanchored).is_empty());
        reanchored.get_mut(&date("2020-01-03")).unwrap()["factor"] = json!(1.);
        assert_eq!(
            compare_probe_windows(&[original], &reanchored),
            vec![date("2020-01-03")]
        );
    }
    #[test]
    fn announcements_are_an_independent_public_request_with_a_fixed_source() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let scope = DateRange {
            start: "2016-05-01".parse().unwrap(),
            end: "2017-07-01".parse().unwrap(),
        };
        let request = WorkflowRequest::CollectAnnouncements {
            request: collection(Provider::Cninfo, "SH600000", &scope, ""),
            resume: None,
        };
        let snapshot = crate::snapshot(&w, &request).unwrap();
        assert_eq!(snapshot["request"]["operation"], "collect_announcements");
        assert!(w.datasets().unwrap().is_empty());
        let unsupported = WorkflowRequest::CollectAnnouncements {
            request: collection(Provider::Tdxaidata, "SH600000", &scope, ""),
            resume: None,
        };
        assert!(
            crate::snapshot(&w, &unsupported)
                .unwrap_err()
                .to_string()
                .contains("announcement_source_unsupported")
        );
        assert!(w.runs().unwrap().is_empty());
        assert_eq!(
            fs::read_dir(w.resolve(".jfquant/staging")).unwrap().count(),
            0
        );
    }
    #[test]
    fn defaults_use_hithink_for_supported_roles_and_policy_is_separate_from_templates() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let mut policy = SourcePolicy::load(&w).unwrap();
        assert_eq!(policy.priorities.len(), 5);
        for (role, order) in &policy.priorities {
            if *role == DataRole::Execution {
                assert!(order.is_empty());
            } else {
                assert_eq!(order[0], Provider::Hithink);
                assert_eq!(order[1], Provider::Tdxaidata);
            }
        }
        assert_eq!(
            policy.routes(DataRole::Quotes, Some(Provider::Hithink)),
            vec![Provider::Hithink]
        );
        let mut old = policy.clone();
        old.schema_version = 1;
        assert!(old.validate().is_err());
        let original = signature(&ResearchTemplate::builtin().unwrap()).unwrap();
        policy
            .priorities
            .get_mut(&DataRole::Actions)
            .unwrap()
            .swap(0, 1);
        policy.save(&w).unwrap();
        assert_eq!(SourcePolicy::load(&w).unwrap(), policy);
        assert_eq!(
            signature(&ResearchTemplate::builtin().unwrap()).unwrap(),
            original
        );
        policy
            .priorities
            .get_mut(&DataRole::Quotes)
            .unwrap()
            .push(Provider::Tdxaidata);
        assert!(policy.validate().is_err());
    }
    #[test]
    fn only_evidence_matched_rights_are_skipped_and_actions_mode_does_not_plan_quotes() {
        let mut issue = analytics_storage::execution_data::ExecutionIssue {
            id: "hithink-zero:SH600000:2021-01-04".into(),
            instrument: "SH600000".into(),
            announced: None,
            dates: DateRange {
                start: "2021-01-04".parse().unwrap(),
                end: "2021-01-04".parse().unwrap(),
            },
            code: "hithink_event_type_unknown".into(),
            event_category: "unknown".into(),
            evidence: vec!["primary.json".into()],
            message: String::new(),
        };
        let row = json!({"date":"2021-01-04","type":"1","allotment_native":"3.00","allot_price":"6.01","bonus_per_ten_shares":"0.00","share_bonus_native":"0.00"});
        let mut ambiguous = issue.clone();
        assert!(!classify_zero_rights(
            &mut ambiguous,
            &[row.clone(), row.clone()],
            "native.json"
        ));
        let mut unknown = row.clone();
        unknown["type"] = json!("15");
        assert!(!classify_zero_rights(&mut issue, &[unknown], "native.json"));
        assert!(classify_zero_rights(&mut issue, &[row], "native.json"));
        assert!(issue.skipped_by_engine());
        assert_eq!(issue.evidence.len(), 2);
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (m, _, template) = crate::demo::create_canonical(&w).unwrap();
        let request = RefreshRequest {
            template: Box::new(template),
            market: m.id,
            actions: None,
            mode: RefreshMode::Actions,
            provider: None,
            end: None,
            preview: false,
            resume: None,
            workers: 3,
            reuse_shards_from: vec![],
        };
        assert!(
            plan(&w, &request, &SourcePolicy::default())
                .unwrap()
                .securities
                .iter()
                .all(|s| s.refresh_ranges.is_empty())
        );
    }
    fn shard_fixtures() -> Vec<SecurityPlan> {
        (0..6)
            .map(|index| SecurityPlan {
                instrument: format!("SH{:06}", 600000 + index),
                requested: DateRange {
                    start: "2021-01-01".parse().unwrap(),
                    end: "2021-12-31".parse().unwrap(),
                },
                refresh_ranges: vec![],
                existing_rows: 0,
                quote_sources: vec![Provider::Hithink],
            })
            .collect()
    }
    #[test]
    fn bounded_shards_keep_task_context_and_deterministic_acceptance() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let current = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let context = TaskContext::new(Uuid::new_v4(), Default::default(), |_| {});
        let securities = shard_fixtures();
        let mut accepted = vec![];
        collect_shards(
            &securities,
            3,
            &context,
            |security| {
                assert_eq!(TaskContext::current().id, context.id);
                let count = current.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(10));
                current.fetch_sub(1, Ordering::SeqCst);
                Ok(security.instrument.clone())
            },
            |_, result| {
                accepted.push(result);
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(peak.load(Ordering::SeqCst), 3);
        assert_eq!(
            accepted,
            securities
                .iter()
                .map(|s| s.instrument.clone())
                .collect::<Vec<_>>()
        );
        assert!(collect_shards(&securities, 0, &context, |_| Ok(()), |_, _| Ok(())).is_err());
        assert!(collect_shards(&securities, 9, &context, |_| Ok(()), |_, _| Ok(())).is_err());
    }
    #[test]
    fn cancelling_a_batch_keeps_completed_shards_and_prevents_further_acceptance() {
        use std::sync::atomic::Ordering;
        let temp = tempfile::tempdir().unwrap();
        let context = TaskContext::new(Uuid::new_v4(), Default::default(), |_| {});
        let mut accepted = 0;
        let result = collect_shards(
            &shard_fixtures(),
            3,
            &context,
            |security| {
                fs::write(
                    temp.path().join(&security.instrument),
                    b"durable checked shard",
                )?;
                Ok(())
            },
            |_, _| {
                accepted += 1;
                context.cancel.store(true, Ordering::Relaxed);
                Ok(())
            },
        );
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert_eq!(accepted, 1);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 3);
    }
    #[test]
    fn gap_ranges_preserve_internal_missing_dates_and_merge_adjacent_sessions() {
        let calendar: Vec<Date> = [
            "2020-01-02",
            "2020-01-03",
            "2020-01-06",
            "2020-01-07",
            "2020-01-08",
        ]
        .map(|s| s.parse().unwrap())
        .into();
        let needed = [calendar[0], calendar[2], calendar[3]].into();
        let result = ranges(&calendar, &needed);
        assert_eq!(result.len(), 2);
        assert_eq!(
            result[1],
            DateRange {
                start: calendar[2],
                end: calendar[3]
            }
        );
    }
    #[test]
    fn preview_freezes_policy_and_never_needs_credentials_or_creates_a_download() {
        let temp = tempfile::tempdir().unwrap();
        let w = Workspace::open(temp.path()).unwrap();
        let (m, a, t) = crate::demo::create_canonical(&w).unwrap();
        let request = RefreshRequest {
            template: Box::new(t),
            market: m.id,
            actions: Some(a.id),
            mode: RefreshMode::Incremental,
            provider: None,
            end: None,
            preview: true,
            resume: None,
            workers: 3,
            reuse_shards_from: vec![],
        };
        let task = WorkflowRequest::RefreshData {
            request: Box::new(request),
        };
        let frozen = crate::snapshot(&w, &task).unwrap();
        assert_eq!(
            frozen["source_policy"]["priorities"]["actions"][0],
            "hithink"
        );
        let ctx = TaskContext::current();
        let run = w.begin(Uuid::new_v4(), "preview", frozen).unwrap();
        let result = crate::execute(&w, &task, &run, &ctx).unwrap();
        assert_eq!(result.details["account"]["status"], "not_run");
        assert!(
            result.details["plan"]["securities"]
                .as_array()
                .unwrap()
                .len()
                > 1
        );
        assert_eq!(
            fs::read_dir(w.resolve(".jfquant/staging")).unwrap().count(),
            0
        );
    }
}

#[cfg(test)]
mod retired_policy_tests {
    use super::*;
    #[test]
    fn retired_policy_is_rejected_and_preserved_for_reconfiguration() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace::open(temp.path()).unwrap();
        let path = workspace.resolve(".jfquant/source-policy.json");
        let bytes = br#"{"schema_version":2,"priorities":{"quotes":["eastmoney"]}}"#;
        fs::write(&path, bytes).unwrap();
        assert!(
            SourcePolicy::load(&workspace)
                .unwrap_err()
                .to_string()
                .contains("removed_provider")
        );
        assert_eq!(fs::read(path).unwrap(), bytes);
    }
}

mod quotes;
use quotes::*;
mod planning;
use planning::*;
mod publication;
use publication::*;
mod actions;
use actions::*;
mod sources;
pub use planning::{plan, ranges};
pub use publication::execute;
pub use sources::check_source;

#[cfg(test)]
use sources::compare_probe_windows;
