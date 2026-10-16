//! Data acquisition plans shared by the App and CLI. Policy is separate from research recipes.
#![allow(missing_docs)]
use crate::*;
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
fn collect_shards<T: Send>(
    securities: &[SecurityPlan],
    workers: usize,
    context: &TaskContext,
    fetch: impl Fn(&SecurityPlan) -> Result<T> + Sync,
    mut accept: impl FnMut(&SecurityPlan, T) -> Result<()>,
) -> Result<()> {
    validate_workers(workers)?;
    for batch in securities.chunks(workers) {
        context.check()?;
        let results = std::thread::scope(|scope| {
            let jobs: Vec<_> = batch
                .iter()
                .map(|security| {
                    let fetch = &fetch;
                    scope.spawn(move || {
                        context.execute(|| fetch(security).map_err(|e| e.to_string()))
                    })
                })
                .collect();
            jobs.into_iter()
                .map(|job| {
                    job.join()
                        .unwrap_or_else(|_| Err("acquisition_worker_panicked".into()))
                })
                .collect::<Vec<_>>()
        });
        for (security, result) in batch.iter().zip(results) {
            context.check()?;
            accept(security, result.map_err(invalid)?)?;
        }
    }
    Ok(())
}

type CollectedQuotes = (Vec<DailyBar>, AdjustmentRecord, PathBuf);
struct QuoteShard {
    attempts: Vec<Value>,
    selected: Option<(Provider, CollectedQuotes)>,
    calendar: Vec<Date>,
}
fn acquire_quotes(
    security: &SecurityPlan,
    request: &RefreshRequest,
    plan: &RefreshPlan,
    base_path: &Path,
    staging: &Path,
    calendar: (&[Date], bool),
    context: &TaskContext,
) -> Result<QuoteShard> {
    let (calendar, calendar_verified) = calendar;
    let id = &security.instrument;
    let mut attempts = vec![];
    let mut selected = None;
    let mut extra_calendar = vec![];
    for provider in &security.quote_sources {
        context.check()?;
        if !matches!(provider, Provider::Tdxaidata | Provider::Hithink) {
            attempts.push(json!({"provider":provider,"status":"not_run","code":"quotation_route_unsupported"}));
            continue;
        }
        let root = staging.join(format!("quotes-{id}-{provider:?}"));
        let same = native_provider(&plan.base_market, id) == Some(*provider)
            && base_path.join("daily_manifest.json").is_file();
        let result = (|| -> Result<_> {
            if *provider == Provider::Hithink {
                let req = collection(*provider, id, &plan.history, &request.template.benchmark);
                let market = analytics_storage::hithink::collect_with_calendar(
                    &req,
                    &root,
                    context,
                    "market",
                    Some(calendar),
                )?;
                return Ok((
                    read_bars(&market.join("bars").join(format!("{id}.parquet")))?,
                    AdjustmentRecord::read(&market, id)?,
                    root.clone(),
                ));
            }
            let mut rows = BTreeMap::new();
            if same && request.mode != RefreshMode::Full {
                let base = DailyDataset::open(base_path)?;
                for bar in raw_rows(&base, id, &plan.history)? {
                    rows.insert(bar.trade_date, bar);
                }
            }
            let intervals = if same && request.mode != RefreshMode::Full {
                security.refresh_ranges.clone()
            } else {
                vec![plan.history.clone()]
            };
            for (index, range) in intervals.iter().enumerate() {
                let req = collection(*provider, id, range, &request.template.benchmark);
                let market =
                    collector::collect(&req, &root.join(format!("part-{index}")), context)?;
                let daily = DailyDataset::open(&market)?;
                if calendar_verified && daily.calendar.iter().any(|day| !calendar.contains(day)) {
                    return Err("source_calendar_conflict".into());
                }
                if !calendar_verified {
                    extra_calendar.extend(daily.calendar.iter().copied());
                }
                for bar in read_bars(&market.join("bars").join(format!("{id}.parquet")))? {
                    rows.insert(bar.trade_date, bar);
                }
            }
            let full = root.join("coefficients");
            collector::collect_coefficients(
                &collection(*provider, id, &plan.history, &request.template.benchmark),
                &full,
                context,
            )?;
            if rows.is_empty() {
                return Err("empty_refresh_history".into());
            }
            Ok((
                rows.into_values().collect(),
                AdjustmentRecord::read(&full.join("market"), id)?,
                root.clone(),
            ))
        })();
        match result {
            Ok(value) => {
                selected = Some((*provider, value));
                attempts.push(json!({"provider":provider,"status":"passed"}));
                break;
            }
            Err(error) => {
                context.check()?;
                attempts.push(
                    json!({"provider":provider,"status":"failed","reason":error.to_string()}),
                );
            }
        }
    }
    Ok(QuoteShard {
        attempts,
        selected,
        calendar: extra_calendar,
    })
}

/// Consecutive requested trading dates are coalesced without hiding internal gaps.
pub fn ranges(calendar: &[Date], needed: &BTreeSet<Date>) -> Vec<DateRange> {
    let mut out: Vec<DateRange> = vec![];
    let mut adjacent = false;
    for d in calendar {
        if needed.contains(d) {
            if adjacent {
                out.last_mut().unwrap().end = *d;
            } else {
                out.push(DateRange { start: *d, end: *d });
            }
            adjacent = true;
        } else {
            adjacent = false;
        }
    }
    out
}
fn earliest_train(t: &ResearchTemplate) -> Result<Date> {
    use analytics_core::schedule::Schedule;
    Ok(match &t.schedule {
        Schedule::Single { fold } => fold.train.start,
        Schedule::Rolling { first, .. } => first.train.start,
        Schedule::Manual { folds } => folds
            .iter()
            .map(|f| f.train.start)
            .min()
            .ok_or("empty schedule")?,
    })
}
fn last_requested(t: &ResearchTemplate, calendar: &[Date]) -> Result<Date> {
    use analytics_core::schedule::Schedule;
    Ok(match &t.schedule {
        Schedule::Single { fold } => fold.test.end,
        Schedule::Manual { folds } => folds
            .iter()
            .map(|f| f.test.end)
            .max()
            .ok_or("empty schedule")?,
        Schedule::Rolling { until: Some(d), .. } => *d,
        Schedule::Rolling { until: None, .. } => {
            let now = chrono::Local::now();
            let complete = if now.hour() < 16 {
                now.date_naive().pred_opt().ok_or("date_overflow")?
            } else {
                now.date_naive()
            };
            complete.max(*calendar.last().ok_or("empty calendar")?)
        }
    })
}
fn native_provider(v: &DataVersion, id: &str) -> Option<Provider> {
    let value = v
        .details
        .get("provider")
        .cloned()
        .or_else(|| v.details["security_sources"][id].get("provider").cloned())?;
    serde_json::from_value(value).ok()
}
fn raw_rows(d: &DailyDataset, id: &str, requested: &DateRange) -> Result<Vec<DailyBar>> {
    let fields = ["open", "high", "low", "close"]
        .into_iter()
        .map(|name| d.research_raw_field(id, name, true))
        .collect::<std::io::Result<Vec<_>>>()?;
    let volume = d.research_raw_volume(id, true)?;
    let amount = d.research_field(id, "amount", true)?;
    Ok(d.calendar
        .iter()
        .enumerate()
        .filter(|(_, day)| requested.contains(**day))
        .map(|(i, day)| DailyBar {
            instrument_id: id.into(),
            trade_date: *day,
            open: fields[0].get(i),
            high: fields[1].get(i),
            low: fields[2].get(i),
            close: fields[3].get(i),
            volume: volume.get(i),
            amount: amount.get(i),
            adj_open: None,
            adj_high: None,
            adj_low: None,
            adj_close: None,
        })
        .collect())
}
pub fn plan(w: &Workspace, request: &RefreshRequest, policy: &SourcePolicy) -> Result<RefreshPlan> {
    policy.validate()?;
    validate_workers(request.workers)?;
    if request.reuse_shards_from.len() > 8
        || request
            .reuse_shards_from
            .iter()
            .collect::<BTreeSet<_>>()
            .len()
            != request.reuse_shards_from.len()
    {
        return Err("invalid_shard_reuse_tasks".into());
    }
    request.template.validate()?;
    let base = version(w, &request.market, "market")?;
    let d = DailyDataset::open(w.resolve(&base.path))?;
    let (pool, membership) = w.resolve_pool(&request.template.pool_version, &base.id)?;
    let first = earliest_train(&request.template)?;
    let pos = d.calendar.partition_point(|d| *d < first);
    let lo = *d
        .calendar
        .get(pos.saturating_sub(request.template.factor_set.warmup_sessions()))
        .ok_or("warmup_calendar_missing")?;
    let end = request
        .end
        .unwrap_or(last_requested(&request.template, &d.calendar)?);
    let history = DateRange { start: lo, end };
    history.validate()?;
    let mut ids: BTreeSet<_> = pool
        .intervals
        .iter()
        .filter(|(_, rs)| rs.iter().any(|r| r.start <= end && r.end >= lo))
        .map(|(id, _)| id.clone())
        .collect();
    ids.insert(request.template.benchmark.clone());
    let actions = request
        .actions
        .as_deref()
        .map(|id| version(w, id, "actions"))
        .transpose()?;
    if let Some(a) = &actions {
        let b = ActionBundle::read(&w.resolve(&a.path))?;
        ids.extend(b.required_symbols(&ids, first, end));
    }
    let calendar: Vec<_> = d
        .calendar
        .iter()
        .copied()
        .filter(|d| history.contains(*d))
        .collect();
    let priorities = policy.routes(DataRole::Quotes, request.provider);
    let mut securities = vec![];
    for id in ids {
        TaskContext::current().check()?;
        if request.mode == RefreshMode::Actions {
            securities.push(SecurityPlan {
                instrument: id,
                requested: history.clone(),
                refresh_ranges: vec![],
                existing_rows: 0,
                quote_sources: priorities.clone(),
            });
            continue;
        }
        let rows = raw_rows(&d, &id, &history)?;
        let present: BTreeSet<_> = rows
            .iter()
            .filter(|r| {
                r.open.is_some()
                    && r.high.is_some()
                    && r.low.is_some()
                    && r.close.is_some()
                    && r.volume.is_some()
                    && r.amount.is_some()
            })
            .map(|r| r.trade_date)
            .collect();
        let mut needed: BTreeSet<_> = calendar
            .iter()
            .filter(|d| !present.contains(d))
            .copied()
            .collect();
        if request.mode == RefreshMode::Full
            || native_provider(&base, &id) != priorities.first().copied()
        {
            needed.extend(&calendar);
        }
        if request.mode == RefreshMode::Incremental {
            needed.extend(calendar.iter().rev().take(policy.overlap_sessions));
        }
        let mut rs = ranges(&calendar, &needed);
        if end > *d.calendar.last().ok_or("empty_calendar")? {
            rs.push(DateRange {
                start: d
                    .calendar
                    .last()
                    .unwrap()
                    .succ_opt()
                    .ok_or("date_overflow")?,
                end,
            });
        }
        securities.push(SecurityPlan {
            instrument: id,
            requested: history.clone(),
            refresh_ranges: rs,
            existing_rows: present.len(),
            quote_sources: priorities.clone(),
        });
    }
    Ok(RefreshPlan {
        schema_version: 1,
        source_policy: policy.clone(),
        base_market: base,
        base_actions: actions,
        history,
        securities,
        membership,
        notes: vec![
            "训练价量、成员覆盖与实际账户依赖分别核验；下载成功不等于研究准入".into(),
            "按用途优先HiThink，通达信备用；不支持或覆盖不足记录后尝试下一来源".into(),
            "历史成员保持所选版本覆盖，当前成分及当前停牌/涨跌停不回填历史".into(),
            "切换行情来源重取该证券所需历史，禁止逐字段拼接".into(),
        ],
    })
}

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
fn copy_tree(source: &Path, dest: &Path) -> Result<()> {
    fs::create_dir_all(dest)?;
    for e in fs::read_dir(source)? {
        TaskContext::current().check()?;
        let e = e?;
        let to = dest.join(e.file_name());
        if e.file_type()?.is_symlink() {
            return Err("unsafe_acquisition_symlink".into());
        }
        if e.file_type()?.is_dir() {
            copy_tree(&e.path(), &to)?;
        } else {
            fs::copy(e.path(), to)?;
        }
    }
    Ok(())
}
fn reuse_shards(
    w: &Workspace,
    run: &RunRecord,
    request: &RefreshRequest,
    plan: &RefreshPlan,
    staging: &Path,
) -> Result<Vec<Value>> {
    let mut reused = vec![];
    for source_ref in &request.reuse_shards_from {
        let (source_workspace, source_id) = shard_workspace(w, source_ref)?;
        if source_id == run.id && source_workspace.root() == w.root() {
            return Err("shard_reuse_self_reference".into());
        }
        let report = crate::coverage::refresh_report(&source_workspace, source_id, false)?;
        if report["reason"] == "planning_pending" {
            return Err("shard_reuse_plan_pending".into());
        }
        let source_run = source_workspace.resolve(format!(".jfquant/runs/{source_id}"));
        let original: RunRecord = serde_json::from_slice(&fs::read(source_run.join("run.json"))?)?;
        let source: RefreshPlan =
            serde_json::from_slice(&fs::read(source_run.join("refresh-plan.json"))?)?;
        let original_request = &original.request["request"]["request"];
        if source.history != plan.history
            || (request.mode != RefreshMode::Actions
                && (source.base_market.id != plan.base_market.id
                    || original_request["mode"] != serde_json::to_value(request.mode)?))
            || original_request["template"]["benchmark"] != request.template.benchmark
        {
            return Err("shard_reuse_window_mismatch".into());
        }
        let owner = original_request["resume"]
            .as_str()
            .map(Uuid::parse_str)
            .transpose()?
            .unwrap_or(source_id);
        let source_root = source_workspace.resolve(format!(".jfquant/staging/refresh-{owner}"));
        let progress: Value = if source_run.join("refresh-progress.json").exists() {
            serde_json::from_slice(&fs::read(source_run.join("refresh-progress.json"))?)?
        } else {
            json!({"outcomes":{}})
        };
        for security in &plan.securities {
            analytics_core::pool::validate_instrument(&security.instrument)?;
            for (kind, role) in [("quotes", DataRole::Quotes), ("actions", DataRole::Actions)] {
                if request.mode == RefreshMode::Actions && kind == "quotes" {
                    continue;
                }
                for provider in plan.source_policy.routes(role, request.provider) {
                    let name = format!("{kind}-{}-{provider:?}", security.instrument);
                    let from = source_root.join(&name);
                    let to = staging.join(&name);
                    if to.exists() || !from.is_dir() {
                        continue;
                    }
                    if provider == Provider::Tdxaidata {
                        let owner: Value =
                            serde_json::from_slice(&fs::read(from.join("owner.json"))?)?;
                        if owner["collector_signature"] != collector::collector_signature() {
                            continue;
                        }
                    }
                    if provider == Provider::Hithink {
                        let owner: Value =
                            serde_json::from_slice(&fs::read(from.join("owner.json"))?)?;
                        if owner["collector_signature"]
                            != analytics_storage::hithink::producer_signature()
                        {
                            continue;
                        }
                    }
                    let completed = if kind == "quotes" {
                        progress["outcomes"][&security.instrument]["quotes"] == "downloaded"
                            && progress["outcomes"][&security.instrument]["attempts"]
                                .as_array()
                                .and_then(|a| a.last())
                                .is_some_and(|a| {
                                    a["provider"] == serde_json::to_value(provider).unwrap()
                                })
                    } else {
                        from.join("actions.json").is_file()
                            && (ActionBundle::read(&from.join("actions.json"))?
                                .failures
                                .is_empty()
                                || provider == Provider::Tdxaidata)
                    };
                    if !completed {
                        continue;
                    }
                    // Copy atomically. Acquisition below still verifies exact request,
                    // producer identity and every checkpoint/evidence hash before use.
                    let temporary = staging.join(format!(".reuse-{name}"));
                    if temporary.exists() {
                        fs::remove_dir_all(&temporary)?
                    }
                    copy_tree(&from, &temporary)?;
                    fs::rename(&temporary, &to)?;
                    reused.push(json!({"source_task":source_id,"instrument":security.instrument,"kind":kind,"provider":provider,"checkpoint_validation":"required_by_collector_before_use"}));
                }
            }
        }
    }
    Ok(reused)
}

/// Publish typed candidates only after every requested shard has a complete source reply.
pub fn execute(
    w: &Workspace,
    run: &RunRecord,
    request: &RefreshRequest,
    policy: &SourcePolicy,
    context: &TaskContext,
) -> Result<Value> {
    let p = plan(w, request, policy)?;
    atomic_json(&w.resolve(&run.output).join("refresh-plan.json"), &p)?;
    if request.preview {
        return Ok(
            json!({"plan":p,"training":{"status":"not_run","reason":"preview_only"},"account":{"status":"not_run","reason":"account_path_not_run"}}),
        );
    }
    let owner = request.resume.unwrap_or(run.id);
    let staging = w.resolve(format!(".jfquant/staging/refresh-{owner}"));
    fs::create_dir_all(&staging)?;
    let identity = json!({"schema_version":1,"plan_signature":signature(&p)?,"mode":request.mode,"collector_signature":collector::collector_signature()});
    let owner_path = staging.join("refresh-owner.json");
    if owner_path.exists() && serde_json::from_slice::<Value>(&fs::read(&owner_path)?)? != identity
    {
        return Err("refresh_checkpoint_owner_mismatch".into());
    }
    atomic_json(&owner_path, &identity)?;
    if request.mode == RefreshMode::Actions {
        let reused = reuse_shards(w, run, request, &p, &staging)?;
        atomic_json(&w.resolve(&run.output).join("reused-shards.json"), &reused)?;
        let mut result = RefreshResult {
            schema_version: 1,
            market: Some(p.base_market.clone()),
            actions: None,
            training: json!({"status":"not_run","reason":"market_unchanged"}),
            account: json!({"status":"not_run","reason":"account_path_not_run"}),
            outcomes: BTreeMap::new(),
            failures: BTreeMap::new(),
        };
        refresh_actions(w, run, request, &p, &staging, &mut result, context)?;
        atomic_json(&w.resolve(&run.output).join("refresh-result.json"), &result)?;
        return Ok(serde_json::to_value(result)?);
    }
    let base = DailyDataset::open(w.resolve(&p.base_market.path))?;
    let output = staging.join("published");
    fs::create_dir_all(output.join("market/bars"))?;
    fs::create_dir_all(output.join("market/adjustments"))?;
    let mut outcomes = BTreeMap::new();
    let mut failures = BTreeMap::new();
    let (calendar_reference, calendar_attempts) =
        reference_route(&p, request, &staging, context, DataRole::Calendar)?;
    let (member_reference, member_attempts) =
        reference_route(&p, request, &staging, context, DataRole::Members)?;
    let mut calendar: Vec<Date> = if let Some(value) = &calendar_reference {
        serde_json::from_value(value["data"]["calendar"].clone())?
    } else {
        base.calendar
            .iter()
            .copied()
            .filter(|d| p.history.contains(*d))
            .collect()
    };
    outcomes.insert("calendar".into(),json!({"attempts":calendar_attempts,"status":if calendar_reference.is_some(){"passed"}else{"failed"}}));
    outcomes.insert("members".into(),json!({"attempts":member_attempts,"historical_source":"selected_base_version","new_members":"current_observation_only"}));
    if calendar_reference.is_none() {
        failures.insert("calendar".into(), "all_calendar_sources_failed".into());
    }
    let mut anchors = BTreeMap::new();
    let mut sources = BTreeMap::new();
    let reused = reuse_shards(w, run, request, &p, &staging)?;
    atomic_json(&w.resolve(&run.output).join("reused-shards.json"), &reused)?;
    let source_calendar = calendar.clone();
    let base_path = w.resolve(&p.base_market.path);
    collect_shards(
        &p.securities,
        request.workers,
        context,
        |security| {
            acquire_quotes(
                security,
                request,
                &p,
                &base_path,
                &staging,
                (&source_calendar, calendar_reference.is_some()),
                context,
            )
        },
        |security, shard| {
            let id = &security.instrument;
            let QuoteShard {
                attempts,
                selected,
                calendar: extra_calendar,
            } = shard;
            calendar.extend(extra_calendar);
            if let Some((provider, (rows, adjustment, root))) = selected {
                let first = rows
                    .iter()
                    .find(|r| r.close.is_some())
                    .ok_or("refresh_anchor_missing")?;
                let baseline = raw_rows(&base, id, &p.history)?
                    .into_iter()
                    .map(|r| (r.trade_date, r))
                    .collect::<BTreeMap<_, _>>();
                let revisions: Vec<Value> = rows
                    .iter()
                    .filter_map(|row| {
                        baseline
                            .get(&row.trade_date)
                            .filter(|old| {
                                serde_json::to_value(old).ok() != serde_json::to_value(row).ok()
                            })
                            .map(|old| json!({"date":row.trade_date,"before":old,"after":row}))
                    })
                    .collect();
                let previous_adjustment =
                    AdjustmentRecord::read(&w.resolve(&p.base_market.path), id).ok();
                let revision_report = json!({"instrument":id,"base_market":p.base_market.id,"source_changed":native_provider(&p.base_market,id)!=Some(provider),"quote_revisions":revisions,"adjustment_before":previous_adjustment,"adjustment_after":adjustment});
                atomic_json(
                    &w.resolve(&run.output).join(format!("revisions-{id}.json")),
                    &revision_report,
                )?;
                anchors.insert(id.clone(), adjustment.anchor(first.trade_date)?);
                write_bars(
                    &output.join("market/bars").join(format!("{id}.parquet")),
                    &rows,
                )?;
                atomic_json(
                    &output.join("market/adjustments").join(format!("{id}.json")),
                    &adjustment,
                )?;
                copy_tree(&root, &output.join("evidence").join(id))?;
                sources.insert(id.clone(), json!({"provider":provider,"attempts":attempts}));
                outcomes.insert(
                    id.clone(),
                    json!({"quotes":"downloaded","rows":rows.len(),"attempts":attempts}),
                );
            } else {
                failures.insert(
                    id.clone(),
                    "all_quotation_sources_failed; see attempts".into(),
                );
                outcomes.insert(id.clone(), json!({"quotes":"failed","attempts":attempts}));
            }
            atomic_json(
                &w.resolve(&run.output).join("refresh-progress.json"),
                &json!({"outcomes":outcomes,"failures":failures}),
            )?;
            Ok(())
        },
    )?;
    calendar.sort();
    calendar.dedup();
    calendar.retain(|d| p.history.contains(*d));
    let mut result = RefreshResult {
        schema_version: 1,
        market: None,
        actions: p.base_actions.clone(),
        training: json!({"status":"not_run","reason":"research_check_required"}),
        account: json!({"status":"not_run","reason":"account_path_not_run"}),
        outcomes,
        failures,
    };
    if result.failures.is_empty() {
        let mut universes = BTreeMap::new();
        for name in ["csi300", "csi500"] {
            if let Ok(pool) = base.pool(name) {
                universes.insert(name.to_owned(), pool);
            }
        }
        if let Some(value) = &member_reference {
            let current: BTreeMap<String, analytics_core::pool::PoolMembership> =
                serde_json::from_value(value["data"]["universes"].clone())?;
            for (name, pool) in current {
                universes.insert(format!("{name}-current"), pool);
            }
        }
        let manifest = DailyManifest {
            schema_version: 6,
            calendar,
            universes,
            instruments: p.securities.iter().map(|s| s.instrument.clone()).collect(),
            price_basis: "unadjusted".into(),
            normalization: if sources.values().any(|s| s["provider"] == "hithink")
                && sources.values().any(|s| s["provider"] == "tdxaidata")
            {
                analytics_storage::adjustment::MIXED_NORMALIZATION
            } else if sources.values().any(|s| s["provider"] == "hithink") {
                analytics_storage::adjustment::HITHINK_NORMALIZATION
            } else {
                analytics_storage::adjustment::NORMALIZATION
            }
            .into(),
            adjustment_anchors: anchors,
        };
        atomic_json(&output.join("market/daily_manifest.json"), &manifest)?;
        atomic_json(&output.join("market/source_map.json"), base.source_map())?;
        for name in ["calendar", "members"] {
            let root = staging.join(format!("reference-{name}"));
            if root.exists() {
                copy_tree(&root, &output.join("evidence").join(name))?;
            }
        }
        atomic_json(
            &output.join("market/acquisition.json"),
            &json!({"schema_version":1,"plan":p,"security_sources":sources,"provider_training_verified":sources.values().all(|s|matches!(s["provider"].as_str(),Some("tdxaidata"|"hithink")))}),
        )?;
        atomic_json(
            &output.join("jfquant_source.json"),
            &json!({"downloader":"JFQuant analytics-storage","independent_download":true,"snapshot_verified":true,"security_sources":sources,"plan":p,"evidence_tree_sha256":market_content_hash(&output.join("evidence"))?}),
        )?;
        DailyDataset::open(output.join("market"))?;
        let dest = w.resolve(format!("data/refresh-{}", run.id));
        fs::rename(&output, &dest)?;
        result.market = Some(w.register(&dest.join("market"))?);
    }
    // Action/execution routes are independent: quote failures never discard valid events.
    refresh_actions(w, run, request, &p, &staging, &mut result, context)?;
    atomic_json(&w.resolve(&run.output).join("refresh-result.json"), &result)?;
    Ok(serde_json::to_value(result)?)
}

fn reference_route(
    p: &RefreshPlan,
    request: &RefreshRequest,
    staging: &Path,
    context: &TaskContext,
    role: DataRole,
) -> Result<(Option<Value>, Vec<Value>)> {
    let kind = if role == DataRole::Calendar {
        "calendar"
    } else {
        "members"
    };
    let mut attempts = vec![];
    for provider in p.source_policy.routes(role, request.provider) {
        if !matches!(provider, Provider::Tdxaidata | Provider::Hithink) {
            attempts.push(json!({"provider":provider,"status":"not_run","code":"reference_capability_unavailable"}));
            continue;
        }
        let mut r = collection(
            provider,
            &request.template.benchmark,
            &p.history,
            &request.template.benchmark,
        );
        if role == DataRole::Members {
            r.universes.insert(
                if request.template.benchmark == "SH000300" {
                    "csi300"
                } else {
                    "csi500"
                }
                .into(),
            );
        }
        let root = staging
            .join(format!("reference-{kind}"))
            .join(format!("{provider:?}"));
        match collector::collect_reference(&r, &root, context, kind) {
            Ok(path) => {
                let value = serde_json::from_slice(&fs::read(path)?)?;
                attempts.push(json!({"provider":provider,"status":"passed"}));
                return Ok((Some(value), attempts));
            }
            Err(e) => {
                context.check()?;
                attempts.push(json!({"provider":provider,"status":"failed","code":"reference_collection_failed","reason":e.to_string()}));
            }
        }
    }
    Ok((None, attempts))
}

fn action_histories_match(a: &ActionBundle, b: &ActionBundle) -> bool {
    a.actions.len() == b.actions.len()
        && a.actions.iter().all(|x| {
            b.actions.iter().any(|y| {
                x.instrument == y.instrument
                    && x.ex_date == y.ex_date
                    && x.record_date == y.record_date
                    && (x.cash() - y.cash()).abs() <= 1e-6
                    && (x.shares() - y.shares()).abs() <= 1e-6
                    && x.terms == y.terms
                    && (x.pay_date.is_none() || y.pay_date.is_none() || x.pay_date == y.pay_date)
                    && (x.listing_date.is_none()
                        || y.listing_date.is_none()
                        || x.listing_date == y.listing_date)
            })
        })
}

fn classify_zero_rights(
    issue: &mut analytics_storage::execution_data::ExecutionIssue,
    candidates: &[Value],
    evidence: &str,
) -> bool {
    if issue.code != "hithink_event_type_unknown" || issue.dates.start != issue.dates.end {
        return false;
    }
    let number = |row: &Value, key: &str| {
        row[key]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|n| n.is_finite())
    };
    let matches: Vec<_> = candidates
        .iter()
        .filter(|row| row["date"] == issue.dates.start.to_string())
        .collect();
    if matches.len() != 1 {
        return false;
    }
    let row = matches[0];
    if row["type"] != "1"
        || !number(row, "allotment_native").is_some_and(|n| n > 0.)
        || !number(row, "allot_price").is_some_and(|n| n > 0.)
        || number(row, "bonus_per_ten_shares") != Some(0.)
        || number(row, "share_bonus_native") != Some(0.)
    {
        return false;
    }
    issue.code = "execution_event_unsupported_skipped".into();
    issue.event_category = "rights_issue".into();
    issue.evidence.push(evidence.into());
    issue.message="HiThink零现金零送股事件与同日通达信配股参考匹配；无认购及交付条款，AKQuant跳过，不补造现金或股份".into();
    true
}
// Compatibility with the observed two-decimal per-ten reference is not proof
// of rounding or an independent verification. Never modify primary source terms.
fn reference_precision_compatible(
    action: &analytics_core::execution::CorporateAction,
    candidate: &Value,
) -> bool {
    let compatible = |field: &str, per_share: f64| {
        let Some(text) = candidate[field].as_str() else {
            return false;
        };
        let Some((_, fraction)) = text.split_once('.') else {
            return false;
        };
        fraction.len() == 2
            && fraction.bytes().all(|b| b.is_ascii_digit())
            && text.parse::<f64>().ok().is_some_and(|value| {
                value.is_finite()
                    && per_share.is_finite()
                    && (per_share * 10. - value).abs() <= 0.005 + 1e-10
            })
    };
    compatible("bonus_per_ten_shares", action.cash())
        && compatible("share_bonus_native", action.shares())
}

fn attach_reference_evidence(
    chosen: &mut (ActionBundle, PathBuf),
    reference: &ActionBundle,
    root: &Path,
) -> Result<String> {
    let mut structured = None;
    for (name, hash) in &reference.raw_sha256 {
        let target = if name.ends_with("-actions-structured.json")
            || name == "complete-sources-structured.json"
        {
            format!("tdx-reference-{name}")
        } else {
            name.clone()
        };
        if name.ends_with("-actions-structured.json") {
            structured = Some(target.clone());
        }
        if let Some(previous) = chosen.0.raw_sha256.get(&target) {
            if previous != hash {
                return Err("reference_evidence_collision".into());
            }
            continue;
        }
        fs::copy(
            root.join("raw").join(name),
            chosen.1.join("raw").join(&target),
        )?;
        chosen.0.raw_sha256.insert(target, hash.clone());
    }
    structured.ok_or_else(|| "reference_structured_evidence_missing".into())
}

struct ActionShard {
    selected: Option<(ActionBundle, PathBuf)>,
    diagnostics: Vec<Value>,
    action_attempts: Vec<Value>,
    execution_attempts: Vec<Value>,
}
fn acquire_actions(
    s: &SecurityPlan,
    request: &RefreshRequest,
    p: &RefreshPlan,
    staging: &Path,
    context: &TaskContext,
) -> Result<ActionShard> {
    let mut selected = vec![];
    let mut diagnostics = vec![];
    let mut action_attempts = vec![];
    let mut execution_attempts = vec![];
    let mut choices: Vec<(ActionBundle, PathBuf)> = vec![];
    let mut tdx_candidates = vec![];
    let mut tdx_reference = None;
    let action_routes = p.source_policy.routes(DataRole::Actions, request.provider);
    let execution_routes = p
        .source_policy
        .routes(DataRole::Execution, request.provider);
    let mut routes = action_routes.clone();
    for provider in &execution_routes {
        if !routes.contains(provider) {
            routes.push(*provider);
        }
    }
    for provider in routes {
        context.check()?;
        if !matches!(provider, Provider::Tdxaidata | Provider::Hithink) {
            action_attempts.push(
                json!({"provider":provider,"status":"not_run","code":"action_route_unsupported"}),
            );
            continue;
        }
        let path = staging.join(format!("actions-{}-{provider:?}", s.instrument));
        let r = collection(provider, &s.instrument, &p.history, "");
        match collector::collect_actions(&r, &path, context) {
            Ok(file) => {
                let b = ActionBundle::read(&file)?;
                if provider == Provider::Tdxaidata {
                    tdx_reference = Some((b.clone(), path.clone()));
                    let shard: Value = serde_json::from_slice(&fs::read(
                        path.join("raw")
                            .join(format!("{}-actions-structured.json", s.instrument)),
                    )?)?;
                    tdx_candidates = shard["evidence"]["candidates"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default();
                    diagnostics.push(json!({"instrument":s.instrument,"provider":provider,"candidates":tdx_candidates,"native_reference_only":true,"path":path}));
                }
                if b.failures.is_empty() && b.structured_coverage.contains_key(&s.instrument) {
                    action_attempts.push(json!({"provider":provider,"status":"passed"}));
                    choices.push((b, path));
                } else {
                    action_attempts.push(json!({"provider":provider,"status":"not_run","code":"account_fields_require_supplement","failures":b.failures}));
                }
            }
            Err(e) => {
                context.check()?;
                action_attempts
                    .push(json!({"provider":provider,"status":"failed","reason":e.to_string()}));
            }
        }
    }
    for provider in execution_routes {
        let status = "not_run";
        execution_attempts.push(json!({"provider":provider,"status":status,"code":if provider==Provider::Tdxaidata {"historical_execution_not_verified"}else if provider==Provider::Hithink {"historical_execution_route_unavailable"}else{"see_source_capabilities"}}));
    }
    if !choices.is_empty() {
        let mut chosen = choices.remove(0);
        for other in choices {
            let missing = |book: &ActionBundle| {
                book.actions
                    .iter()
                    .filter(|a| {
                        (a.cash() > 0. && a.pay_date.is_none())
                            || (a.shares() > 0. && a.listing_date.is_none())
                    })
                    .count()
            };
            // A complete preferred history does not need a second provider
            // to approve it. Match events only when joining missing dates;
            // other providers can still supply independent execution fields.
            if missing(&chosen.0) == 0 || action_histories_match(&chosen.0, &other.0) {
                if missing(&chosen.0) > 0 && missing(&other.0) < missing(&chosen.0) {
                    diagnostics.push(json!({"instrument":s.instrument,"code":"matched_event_history_source_supplement","selected":other.1}));
                    chosen = other;
                } else if let Some(aux) = other.0.execution_data {
                    let target = chosen.0.execution_data.get_or_insert_with(|| {
                        analytics_storage::execution_data::ExecutionData {
                            schema_version: 1,
                            ..Default::default()
                        }
                    });
                    target.suspensions.extend(aux.suspensions);
                    target.limits.extend(aux.limits);
                    target.capabilities.extend(aux.capabilities);
                    // Every auxiliary source byte remains authenticated, including its structured reply.
                    for (name, hash) in other.0.raw_sha256 {
                        let new = if name.ends_with("-actions-structured.json")
                            || name == "complete-sources-structured.json"
                        {
                            format!("supplement-{name}")
                        } else {
                            name.clone()
                        };
                        if !chosen.0.raw_sha256.contains_key(&new) {
                            fs::copy(
                                other.1.join("raw").join(name),
                                chosen.1.join("raw").join(&new),
                            )?;
                            chosen.0.raw_sha256.insert(new, hash);
                        }
                    }
                }
            } else {
                chosen
                    .0
                    .execution_issues
                    .push(analytics_storage::execution_data::ExecutionIssue {
                        id: String::new(),
                        event_category: String::new(),
                        evidence: Vec::new(),
                        instrument: s.instrument.clone(),
                        announced: None,
                        dates: p.history.clone(),
                        code: "source_event_conflict".into(),
                        message: "完整事件来源在金额、登记日或事件集合上不一致，未按优先级覆盖"
                            .into(),
                    });
                diagnostics.push(json!({"instrument":s.instrument,"code":"source_event_conflict","other":other.1}));
            }
        }
        let mut reference_evidence = Vec::new();
        if let Some((reference, root)) = &tdx_reference {
            let evidence = attach_reference_evidence(&mut chosen, reference, root)?;
            reference_evidence.push(evidence.clone());
            let shard: Value = serde_json::from_slice(&fs::read(
                root.join("raw")
                    .join(format!("{}-actions-structured.json", s.instrument)),
            )?)?;
            if shard["evidence"]["stock_status"]["status"] == "captured" {
                let source = reference.source_symbol(&s.instrument)?;
                let symbol = format!("{}.{}", &source[2..], &source[..2]);
                match analytics_storage::tdx_status::from_evidence(
                    root,
                    &shard["evidence"],
                    &symbol,
                    &p.history,
                ) {
                    Ok(suspended) => {
                let data = chosen.0.execution_data.get_or_insert_with(|| {
                    analytics_storage::execution_data::ExecutionData {
                        schema_version: 1,
                        ..Default::default()
                    }
                });
                data.schema_version = 2;
                data.suspensions.insert(s.instrument.clone(), suspended);
                data.capabilities.insert(format!("{}:{}", analytics_storage::tdx_status::RULE, s.instrument), json!({"status":"available","source_symbol":symbol,"range":p.history,"structured":evidence,"use":"retrospective-account-validation-only;not-a-signal","right_censored":true}));
                    }
                    Err(error) => diagnostics.push(json!({"instrument":s.instrument,"code":"historical_status_unavailable","reason":error.to_string(),"evidence":evidence})),
                }
            }
            for issue in &mut chosen.0.execution_issues {
                classify_zero_rights(issue, &tdx_candidates, &evidence);
            }
        }
        for candidate in &tdx_candidates {
            let date: Date = serde_json::from_value(candidate["date"].clone())?;
            let allot = candidate["allotment_native"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(0.);
            let ordinary = candidate["type"] == "1" && allot == 0.;
            if candidate["type"] == "1"
                && allot > 0.
                && chosen.0.execution_issues.iter().any(|issue| {
                    issue.dates.start == date
                        && issue.event_category == "rights_issue"
                        && issue.skipped_by_engine()
                })
            {
                continue;
            }
            let cash = candidate["bonus_per_ten_shares"]
                .as_str()
                .and_then(|s| s.parse::<f64>().ok())
                .map(|v| v / 10.);
            let matched = chosen.0.actions.iter().any(|a| {
                a.ex_date == date
                    && cash.is_some_and(|v| (v - a.cash()).abs() <= 1e-6)
                    && candidate["share_bonus_native"]
                        .as_str()
                        .and_then(|s| s.parse::<f64>().ok())
                        .is_some_and(|shares| (shares / 10. - a.shares()).abs() <= 1e-6)
            });
            let precision_compatible = ordinary
                && !matched
                && chosen
                    .0
                    .actions
                    .iter()
                    .any(|a| a.ex_date == date && reference_precision_compatible(a, candidate));
            if !ordinary || !matched {
                chosen
                    .0
                    .execution_issues
                    .push(analytics_storage::execution_data::ExecutionIssue {
                        id: String::new(),
                        event_category: String::new(),
                        evidence: reference_evidence.clone(),
                        instrument: s.instrument.clone(),
                        announced: None,
                        dates: DateRange {
                            start: date,
                            end: date,
                        },
                        code: if precision_compatible {
                            "tdx_reference_precision_difference"
                        } else if ordinary {
                            "source_event_conflict"
                        } else {
                            "tdx_special_action_requires_review"
                        }
                        .into(),
                        message: if precision_compatible {
                            "通达信两位小数每十股参考与供应商事件在半个显示单位内相容；保留差异，采用主源条款，未宣称独立核验".into()
                        } else {
                            "通达信事件参考与账户事件未完成匹配，保留原始证据".into()
                        },
                    });
            }
        }
        selected.push(chosen);
    }
    Ok(ActionShard {
        selected: selected.pop(),
        diagnostics,
        action_attempts,
        execution_attempts,
    })
}

fn refresh_actions(
    w: &Workspace,
    run: &RunRecord,
    request: &RefreshRequest,
    p: &RefreshPlan,
    staging: &Path,
    result: &mut RefreshResult,
    context: &TaskContext,
) -> Result<()> {
    let mut selected = vec![];
    let mut diagnostics = vec![];
    let securities: Vec<_> = p
        .securities
        .iter()
        .filter(|s| s.instrument != request.template.benchmark)
        .cloned()
        .collect();
    let mut completed = BTreeSet::new();
    collect_shards(
        &securities,
        request.workers,
        context,
        |security| acquire_actions(security, request, p, staging, context),
        |security, shard| {
            if let Some(chosen) = shard.selected {
                selected.push(chosen);
            }
            diagnostics.extend(shard.diagnostics);
            let outcome = result
                .outcomes
                .entry(security.instrument.clone())
                .or_insert_with(|| json!({}));
            outcome["action_attempts"] = json!(shard.action_attempts);
            outcome["execution_attempts"] = json!(shard.execution_attempts);
            completed.insert(security.instrument.clone());
            atomic_json(
                &w.resolve(&run.output).join("action-progress.json"),
                &json!({"completed":completed,"complete_securities":selected.len(),"requested":securities.len()}),
            )?;
            Ok(())
        },
    )?;
    atomic_json(
        &w.resolve(&run.output).join("source-diagnostics.json"),
        &diagnostics,
    )?;
    let expected = p
        .securities
        .iter()
        .filter(|s| s.instrument != request.template.benchmark)
        .count();
    if selected.len() != expected {
        result.account = json!({"status":"failed","code":"action_history_incomplete","complete_securities":selected.len(),"requested":expected,"account_path":"not_run"});
        return Ok(());
    }
    let dest = staging.join("published-actions");
    fs::create_dir_all(dest.join("raw"))?;
    let (mut book, _) = selected.first().cloned().ok_or("empty_action_scope")?;
    book.actions.clear();
    book.queried_symbols.clear();
    book.raw_sha256.clear();
    book.structured_coverage.clear();
    book.execution_issues.clear();
    book.execution_data = None;
    for (b, root) in selected {
        for (name, hash) in &b.raw_sha256 {
            let path = dest.join("raw").join(name);
            if path.exists()
                && format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(fs::read(&path)?))
                ) != *hash
            {
                // Per-collector completion receipts share a name; the action bundle's
                // structured shards already reference and authenticate native/HTTP bodies.
                if name == "complete-sources-structured.json" {
                    continue;
                }
                return Err("action_evidence_name_conflict".into());
            }
            fs::copy(root.join("raw").join(name), path)?;
            book.raw_sha256.insert(name.clone(), hash.clone());
        }
        book.actions.extend(b.actions);
        book.queried_symbols.extend(b.queried_symbols);
        book.structured_coverage.extend(b.structured_coverage);
        book.execution_issues.extend(b.execution_issues);
        book.source_bindings.extend(b.source_bindings);
        if let Some(aux) = b.execution_data {
            let all = book.execution_data.get_or_insert_with(|| {
                analytics_storage::execution_data::ExecutionData {
                    schema_version: 1,
                    ..Default::default()
                }
            });
            all.schema_version = all.schema_version.max(aux.schema_version);
            all.suspensions.extend(aux.suspensions);
            all.limits.extend(aux.limits);
            all.coefficients.extend(aux.coefficients);
            all.capabilities.extend(aux.capabilities);
        }
    }
    let all_evidence = dest.join("source-evidence");
    fs::create_dir_all(&all_evidence)?;
    for s in &p.securities {
        for provider in [Provider::Hithink, Provider::Tdxaidata] {
            let root = staging.join(format!("actions-{}-{provider:?}", s.instrument));
            if root.is_dir() {
                copy_tree(
                    &root,
                    &all_evidence.join(format!("{}-{provider:?}", s.instrument)),
                )?;
            }
        }
    }
    atomic_json(&dest.join("source-diagnostics.json"), &diagnostics)?;
    let proof = dest.join("raw/source-evidence-sha256.json");
    atomic_json(&proof, &market_content_hash(&all_evidence)?)?;
    book.raw_sha256.insert(
        "source-evidence-sha256.json".into(),
        format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(fs::read(&proof)?))
        ),
    );
    analytics_storage::corporate_actions::facts::refresh(&mut book, &dest)?;
    atomic_json(&dest.join("actions.json"), &book)?;
    ActionBundle::read(&dest.join("actions.json"))?;
    let published = w.resolve(format!("data/refresh-actions-{}", run.id));
    fs::rename(&dest, &published)?;
    result.actions = Some(w.register(&published.join("actions.json"))?);
    result.account = json!({"status":"not_run","code":"account_path_not_run","action_securities":book.queried_symbols.len(),"missing_cash_dates":book.actions.iter().filter(|a|a.cash()>0.&&a.pay_date.is_none()).count(),"missing_listing_dates":book.actions.iter().filter(|a|a.shares()>0.&&a.listing_date.is_none()).count(),"execution_issues":book.execution_issues});
    Ok(())
}

// Compare independent windows at a shared anchor. Absolute source coefficients
// are intentionally not compared: each valid request may use a different anchor.
fn compare_probe_windows(
    previous: &[BTreeMap<Date, Value>],
    current: &BTreeMap<Date, Value>,
) -> Vec<Date> {
    let mut conflicts = BTreeSet::new();
    for old in previous {
        let scale = current
            .iter()
            .find_map(|(day, value)| {
                old.get(day)?
                    .get("factor")?
                    .as_f64()
                    .zip(value["factor"].as_f64())
                    .filter(|(a, b)| *a > 0. && *b > 0.)
                    .map(|(a, b)| a / b)
            })
            .unwrap_or(1.);
        for (day, value) in current {
            let Some(prior) = old.get(day) else {
                continue;
            };
            let factor_matches = match (prior["factor"].as_f64(), value["factor"].as_f64()) {
                (Some(a), Some(b)) if a > 0. && b > 0. => (b * scale / a - 1.).abs() <= 1e-5,
                (None, None) => true,
                _ => false,
            };
            let mut a = prior.clone();
            let mut b = value.clone();
            a.as_object_mut().unwrap().remove("factor");
            b.as_object_mut().unwrap().remove("factor");
            if a != b || !factor_matches {
                conflicts.insert(*day);
            }
        }
    }
    conflicts.into_iter().collect()
}

/// Bounded three-round account probe. Never schedules the full research universe.
pub fn check_source(
    w: &Workspace,
    run: &RunRecord,
    provider: Provider,
    context: &TaskContext,
) -> Result<Value> {
    if matches!(provider, Provider::Tdxaidata | Provider::Hithink) {
        analytics_storage::credentials::load_source(if provider == Provider::Tdxaidata {
            "tdxaidata"
        } else {
            "hithink"
        })?;
    }
    let today = chrono::Local::now();
    let end = if today.hour() < 16 {
        today.date_naive().pred_opt().ok_or("date_overflow")?
    } else {
        today.date_naive()
    };
    let mut checks = vec![];
    let mut common: BTreeMap<String, Vec<BTreeMap<Date, Value>>> = BTreeMap::new();
    let mut native_requests = 0;
    let windows = if provider == Provider::Hithink {
        [
            DateRange {
                start: end - chrono::Duration::days(30),
                end,
            },
            DateRange {
                start: end - chrono::Duration::days(60),
                end,
            },
            DateRange {
                start: end - chrono::Duration::days(90),
                end,
            },
        ]
    } else {
        [
            DateRange {
                start: "2006-12-04".parse()?,
                end: "2006-12-08".parse()?,
            },
            DateRange {
                start: "2016-06-13".parse()?,
                end: "2016-06-24".parse()?,
            },
            DateRange {
                start: end - chrono::Duration::days(7),
                end,
            },
        ]
    };
    for round in 0..3 {
        for id in [
            "SH600000", "SH600018", "SZ000001", "SH000300", "SH600030", "SH600068",
        ] {
            for (i, range) in windows.iter().enumerate() {
                context.check()?;
                let r = DateRange {
                    start: range.start - chrono::Duration::days(round * 3),
                    end: range.end,
                };
                let r = if provider == Provider::Tdxaidata
                    && round == 0
                    && i == 1
                    && id == "SH600000"
                {
                    DateRange {
                        start: "2006-12-04".parse()?,
                        end,
                    }
                } else {
                    r
                };
                context.report(format!(
                    "来源小样本 第{}轮 · {} · {}—{}",
                    round + 1,
                    id,
                    r.start,
                    r.end
                ))?;
                let start = std::time::Instant::now();
                let root = w
                    .resolve(&run.output)
                    .join(format!("probe-{round}-{id}-{i}"));
                let req = collection(provider, id, &r, if id == "SH000300" { id } else { "" });
                let mut outcome = match collector::collect(&req, &root, context) {
                    Ok(market) => {
                        let data = DailyDataset::open(&market)?;
                        let rows = read_bars(&market.join("bars").join(format!("{id}.parquet")))?;
                        let record = AdjustmentRecord::read(&market, id)?;
                        let observed = common.entry(id.into()).or_default();
                        let mut current = BTreeMap::new();
                        for row in &rows {
                            let value = json!({"open":row.open,"high":row.high,"low":row.low,"close":row.close,"volume":row.volume,"amount":row.amount,"factor":record.on(row.trade_date)?});
                            current.insert(row.trade_date, value);
                        }
                        let mismatches = compare_probe_windows(observed, &current);
                        observed.push(current);
                        json!({"status":if mismatches.is_empty(){"passed"}else{"failed"},"code":if mismatches.is_empty(){"quotation_validated"}else{"cross_window_conflict"},"rows":rows.len(),"calendar_sessions":data.calendar.len(),"first":rows.first().map(|r|r.trade_date),"last":rows.last().map(|r|r.trade_date),"window_conflicts":mismatches})
                    }
                    Err(error) => {
                        context.check()?;
                        let failures = fs::read(root.join("acquisition_readiness.json"))
                            .ok()
                            .and_then(|b| serde_json::from_slice::<Value>(&b).ok());
                        json!({"status":"failed","reason":error.to_string(),"source_failures":failures})
                    }
                };
                // Count native data calls even when a shard failed validation;
                // successful-only counts would hide the actual probe usage.
                native_requests += fs::read_to_string(root.join("raw/sdk-requests.jsonl"))
                    .map(|journal| journal.lines().count())
                    .unwrap_or(0);
                outcome["round"] = json!(round + 1);
                outcome["instrument"] = json!(id);
                outcome["range"] = json!(r);
                outcome["elapsed_ms"] = json!(start.elapsed().as_millis());
                checks.push(outcome);
                atomic_json(
                    &w.resolve(&run.output).join("source-check.json"),
                    &json!({"checks":checks,"complete":false,"native_requests":native_requests,"points_cost":"unknown","full_download_authorized":false}),
                )?;
            }
        }
    }
    let report = json!({"schema_version":1,"provider":provider,"checks":checks,"native_requests":native_requests,"complete":true,"points_cost":"unknown","full_download_authorized":false,"account_dates":"requires_other_sources","historical_members":"not_supported","historical_execution":"not_verified","sdk_version":"1.2.2"});
    atomic_json(&w.resolve(&run.output).join("source-check.json"), &report)?;
    Ok(report)
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
