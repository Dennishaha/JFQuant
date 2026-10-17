//! Acquisition use cases.
use super::*;
pub(super) fn execute(
    w: &Workspace,
    request: &WorkflowRequest,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    let mut data = None;
    let mut details = Value::Null;
    match request {
        WorkflowRequest::CollectAnnouncements { request, resume } => {
            if request.provider != analytics_storage::collector::Provider::Cninfo {
                return Err(
                    "announcement_source_unsupported: select cninfo for CNINFO evidence".into(),
                );
            }
            let staging = w.resolve(format!(
                ".jfquant/staging/announcements-{}",
                resume.unwrap_or(run.id)
            ));
            let manifest =
                analytics_storage::collector::collect_announcements(request, &staging, context)?;
            details = serde_json::from_slice(&fs::read(&manifest)?)?;
            let output = w.resolve(&run.output).join("announcements");
            fs::rename(staging, &output)?;
            details["artifact"] = json!(output.join("announcements.json"));
            details["candidate_only"] = json!(true);
        }
        WorkflowRequest::CollectReference {
            request,
            kind,
            resume,
        } => {
            let staging = w.resolve(format!(
                ".jfquant/staging/reference-{}",
                resume.unwrap_or(run.id)
            ));
            let file =
                analytics_storage::collector::collect_reference(request, &staging, context, kind)?;
            details = serde_json::from_slice(&fs::read(&file)?)?;
            let output = w.resolve(&run.output).join("reference");
            fs::rename(&staging, &output)?;
            details["artifact"] = json!(output.join("reference.json"));
        }
        WorkflowRequest::SourceStatus {} => details = refresh::catalog(w)?,
        WorkflowRequest::SaveSourcePolicy { policy } => {
            policy.save(w)?;
            details = refresh::catalog(w)?;
        }
        WorkflowRequest::CheckSource { provider } => {
            details = refresh::check_source(w, run, *provider, context)?
        }
        WorkflowRequest::RefreshData { request } => {
            let policy = serde_json::from_value(run.request["source_policy"].clone())?;
            details = refresh::execute(w, run, request, &policy, context)?;
            data = details
                .get("market")
                .filter(|v| !v.is_null())
                .map(|v| serde_json::from_value(v.clone()))
                .transpose()?;
        }
        WorkflowRequest::CheckUpdates {} => {
            context.report("检查公开行情版本")?;
            let value = analytics_storage::transport::json(
                "https://api.github.com/repos/chenditc/investment_data/releases/latest",
            )?;
            let tag = value["tag_name"].as_str().ok_or("release tag missing")?;
            tag.parse::<analytics_core::Date>()?;
            let assets = value["assets"].as_array().ok_or("release assets missing")?;
            for name in ["qlib_bin.tar.gz", "qlib_bin.manifest.json"] {
                if !assets.iter().any(|a| a["name"] == name) {
                    return Err("release lacks paired immutable assets".into());
                }
            }
            details =
                json!({"tag":tag,"published_at":value["published_at"],"url":value["html_url"]});
        }
        WorkflowRequest::DownloadMarket { tag, resume } => {
            tag.parse::<analytics_core::Date>()?;
            context.report("下载并验证行情")?;
            let owner = resume.unwrap_or(run.id);
            let staging = w.root.join(format!(".jfquant/staging/{owner}"));
            let output = w.root.join(format!("data/market-{tag}-{}", run.id));
            analytics_storage::acquisition::fetch_release_resumable(tag, &staging, &output)?;
            data = Some(w.register(&output.join("qlib_bin"))?);
        }
        WorkflowRequest::CollectCoefficients { request, resume } => {
            let owner = resume.unwrap_or(run.id);
            let staging = w
                .root
                .join(format!(".jfquant/staging/python-coefficients-{owner}"));
            let manifest =
                analytics_storage::collector::collect_coefficients(request, &staging, context)?;
            let relative = manifest.strip_prefix(&staging)?.to_owned();
            let output = w.root.join(format!("data/python-coefficients-{}", run.id));
            fs::rename(&staging, &output)?;
            data = Some(w.register(&output.join(relative))?);
        }
        WorkflowRequest::ComposeMarket {
            market,
            coefficients,
        } => {
            let base = version(w, market, "market")?;
            let c = version(w, coefficients, "coefficients")?;
            let d = DailyDataset::open(w.resolve(&base.path))?;
            if !d.is_original_qlib() {
                return Err("composition requires an original Qlib snapshot".into());
            }
            let bundle =
                analytics_storage::collector::CoefficientBundle::read(&w.resolve(&c.path))?;
            let staging = w
                .root
                .join(format!(".jfquant/staging/market-view-{}", run.id));
            let root = staging.join("market");
            fs::create_dir_all(root.join("adjustments"))?;
            let mut anchors = BTreeMap::new();
            for id in bundle.records.keys() {
                context.check()?;
                let close = d.raw_field(id, "close")?;
                let first = (0..d.calendar.len())
                    .find(|i| close.get(*i).is_some())
                    .ok_or("raw close anchor missing")?;
                let record = analytics_storage::adjustment::AdjustmentRecord::read(
                    &w.resolve(&c.path)
                        .parent()
                        .ok_or("coefficient parent")?
                        .join("market"),
                    id,
                )?;
                anchors.insert(id.clone(), record.anchor(d.calendar[first])?);
                fs::copy(
                    w.resolve(&c.path)
                        .parent()
                        .unwrap()
                        .join("market/adjustments")
                        .join(format!("{id}.json")),
                    root.join("adjustments").join(format!("{id}.json")),
                )?;
            }
            let view = analytics_storage::market_view::MarketView {
                schema_version: 1,
                base_path: base.path.clone(),
                base_sha256: base.details["content_sha256"]
                    .as_str()
                    .ok_or("base digest missing")?
                    .into(),
                coefficients_path: c.path.clone(),
                coefficients_sha256: analytics_storage::market_view::file_hash(
                    &w.resolve(&c.path),
                )?,
                recovery: "qlib-normalized-price-over-original-factor;cent-tick;shares-v1".into(),
                records: bundle.records,
                anchors,
            };
            atomic_json(&root.join("market_view.json"), &view)?;
            atomic_json(
                &staging.join("jfquant_source.json"),
                &json!({"downloader":"JFQuant analytics-storage","snapshot_verified":true,"independent_download":true,"derived_view":true,"raw_source":"Qlib reconstructed raw quotations","base_version":base,"coefficient_version":c,"normalization":analytics_storage::adjustment::NORMALIZATION}),
            )?;
            DailyDataset::open(&root)?;
            let output = w.root.join(format!("data/market-view-{}", run.id));
            fs::rename(&staging, &output)?;
            data = Some(w.register(&output.join("market"))?);
        }
        WorkflowRequest::CollectActions { request, resume } => {
            let owner = resume.unwrap_or(run.id);
            let staging = w
                .root
                .join(format!(".jfquant/staging/python-actions-{owner}"));
            analytics_storage::collector::collect_actions(request, &staging, context)?;
            let output = w.root.join(format!("data/python-actions-{}", run.id));
            fs::rename(staging, &output)?;
            data = Some(w.register(&output.join("actions.json"))?);
        }
        WorkflowRequest::CollectQuotes { request, resume } => {
            let owner = resume.unwrap_or(run.id);
            let staging = w
                .root
                .join(format!(".jfquant/staging/python-quotes-{owner}"));
            analytics_storage::collector::collect_quotes(request, &staging, context)?;
            let output = w.root.join(format!("data/python-quotes-{}", run.id));
            fs::rename(staging, &output)?;
            data = Some(w.register(&output.join("market"))?);
        }
        WorkflowRequest::CollectMarket { request, resume } => {
            let owner = resume.unwrap_or(run.id);
            let staging = w.root.join(format!(".jfquant/staging/collector-{owner}"));
            analytics_storage::collector::collect(request, &staging, context)?;
            let output = w.root.join(format!("data/collector-{}", run.id));
            fs::rename(staging, &output)?;
            data = Some(w.register(&output.join("market"))?);
        }
        WorkflowRequest::DownloadActions {
            market,
            resume,
            pool_version,
        } => {
            let market_version = version(w, market, "market")?;
            let (pool, _) = w.resolve_pool(pool_version, market)?;
            let symbols = pool.intervals.into_keys().collect::<Vec<_>>();
            let path = w.root.join(format!("data/actions-{}", run.id));
            let owner = resume.unwrap_or(run.id);
            let staging = w.root.join(format!(".jfquant/staging/actions-{owner}"));
            let dataset = DailyDataset::open(w.resolve(&market_version.path))?;
            let request = analytics_storage::collector::CollectionRequest {
                index_inputs: Default::default(),
                source_bindings: Default::default(),
                provider: analytics_storage::collector::Provider::Hithink,
                adjustment_source: analytics_storage::adjustment::AdjustmentSource::HithinkForward,
                start: *dataset.calendar.first().ok_or("empty market calendar")?,
                end: *dataset.calendar.last().ok_or("empty market calendar")?,
                instruments: symbols
                    .iter()
                    .filter(|s| analytics_core::pool::validate_exchange_symbol(s).is_ok())
                    .cloned()
                    .collect(),
                benchmarks: Default::default(),
                universes: Default::default(),
            };
            context.report("HiThink 公司行动事件；缺失日期保持 null")?;
            let file = analytics_storage::collector::collect_actions(&request, &staging, context)?;
            let mut book = ActionBundle::read(&file)?;
            for id in symbols.iter().filter(|s| !request.instruments.contains(*s)) {
                book.failures.insert(
                    id.clone(),
                    "provider_locator_missing: historical key has no formal source code".into(),
                );
            }
            fs::write(&file, serde_json::to_vec_pretty(&book)?)?;
            fs::rename(staging, &path)?;
            data = Some(w.register(&path.join("actions.json"))?);
        }
        _ => unreachable!("domain dispatcher mismatch"),
    }
    Ok(WorkflowOutcome { data, details })
}
