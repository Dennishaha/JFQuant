//! Frozen local three-factor construction and immutable market publication.
use crate::{Result, RunRecord, WorkflowOutcome, Workspace, atomic_json, version};
use analytics_core::task::TaskContext;
use analytics_storage::{
    daily::DailyDataset,
    three_factor::{self, acquire, build},
};
use chrono::Datelike;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{fs, path::Path};

fn read(w: &Workspace, path: &Path) -> Result<(Vec<u8>, build::BuildRequest)> {
    let bytes = fs::read(w.resolve(path))?;
    let request: build::BuildRequest = serde_json::from_slice(&bytes)?;
    request.validate()?;
    if let Some(history) = &request.provider_history {
        acquire::verify_bundle(
            w.resolve(path).parent().ok_or("input parent missing")?,
            history,
        )?;
    }
    Ok((bytes, request))
}
fn identity(bytes: &[u8], request: &build::BuildRequest) -> Value {
    json!({"schema_version":1,"construction_version":build::VERSION,
        "inputs_sha256":format!("sha256:{:x}", analytics_core::hash::Hex(Sha256::digest(bytes))),
        "implementation_sha256":build::implementation_signature(),
        "range":request.range,"sources":request.sources})
}

impl Workspace {
    /// Replay a portable PIT size/value artifact and register its standard three-factor sidecar.
    pub fn publish_bigquant_size_value_factors(
        &self,
        market: &str,
        artifact: &Path,
        output: &Path,
    ) -> Result<crate::DataVersion> {
        self.publish_bigquant_artifact(
            market,
            artifact,
            output,
            "bigquant_size_value_factor",
            analytics_storage::bigquant::size_value_factors::verify_build,
        )
    }
    /// Replay a BigQuant full-universe MKT artifact and publish a new market with its standard sidecar.
    /// Takes the workspace lease and never rewrites the selected base or its source manifest.
    pub fn publish_bigquant_market_factors(
        &self,
        market: &str,
        artifact: &Path,
        output: &Path,
    ) -> Result<crate::DataVersion> {
        self.publish_bigquant_artifact(
            market,
            artifact,
            output,
            "bigquant_market_factor",
            analytics_storage::bigquant::market::verify_build,
        )
    }
    fn publish_bigquant_artifact(
        &self,
        market: &str,
        artifact: &Path,
        output: &Path,
        artifact_name: &str,
        verify: fn(&Path) -> std::io::Result<(three_factor::ThreeFactorReturns, Value)>,
    ) -> Result<crate::DataVersion> {
        let _lease = self.lease()?;
        let base = version(self, market, "market")?;
        let artifact = self.resolve(artifact);
        let (factors, manifest) = verify(&artifact)?;
        let dest = self.resolve(output);
        if dest.exists() {
            return Err("bigquant_market_output_exists".into());
        }
        let parent = dest.parent().ok_or("market output parent missing")?;
        fs::create_dir_all(parent)?;
        if !parent.canonicalize()?.starts_with(self.root()) {
            return Err("market output must be inside workspace".into());
        }
        let stage = parent.join(format!(".bigquant-publication-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&stage)?;
        let result = (|| -> Result<()> {
            let root = stage.join("market");
            analytics_storage::corporate_actions::evidence::copy_tree(
                &self.resolve(&base.path),
                &root,
            )?;
            let evidence = root.join(artifact_name);
            if evidence.exists() {
                fs::remove_dir_all(&evidence)?;
            }
            analytics_storage::corporate_actions::evidence::copy_tree(&artifact, &evidence)?;
            verify(&evidence)?;
            atomic_json(&root.join(three_factor::FILE_NAME), &factors)?;
            let dataset = DailyDataset::open(&root)?;
            let reread = three_factor::ThreeFactorReturns::read(&dataset.three_factor_path())?;
            if serde_json::to_value(reread)? != serde_json::to_value(&factors)? {
                return Err("published factor reader mismatch".into());
            }
            let mut source = base.details.clone();
            source["base_market"] =
                json!({"id":base.id,"signature":base.signature,"path":base.path});
            source[artifact_name] = json!({"manifest":manifest,
                "manifest_sha256":analytics_storage::market_view::file_hash(&evidence.join("manifest.json"))?,
                "artifact_path":artifact_name,"fields":three_factor::FIELDS,
                "full_three_factor_ready":manifest["full_three_factor_ready"]});
            atomic_json(&stage.join("jfquant_source.json"), &source)?;
            version(self, market, "market")?;
            fs::rename(&stage, &dest)?;
            Ok(())
        })();
        if result.is_err() && stage.exists() {
            fs::remove_dir_all(&stage)?;
        }
        result?;
        self.register(&dest.join("market"))
    }
}
pub(crate) fn freeze(w: &Workspace, path: &Path) -> Result<Value> {
    let (bytes, request) = read(w, path)?;
    Ok(identity(&bytes, &request))
}
pub(crate) fn freeze_acquisition(w: &Workspace, path: &Path) -> Result<Value> {
    let (bytes, request) = read(w, path)?;
    acquisition_identity(&bytes, &request)
}
fn acquisition_identity(bytes: &[u8], request: &build::BuildRequest) -> Result<Value> {
    if !request.shares.is_empty()
        || !request.book_equity.is_empty()
        || request.provider_history.is_some()
    {
        return Err("three_factor_acquisition_requires_empty_share_equity_template".into());
    }
    Ok(
        json!({"template":identity(bytes,request),"normalization":acquire::VERSION,
        "collector":analytics_storage::collector::implementation_identity()}),
    )
}
pub(crate) fn collect_inputs(
    w: &Workspace,
    market: &str,
    inputs: &Path,
    availability: acquire::Availability,
    resume: Option<uuid::Uuid>,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    let (bytes, mut request) = read(w, inputs)?;
    if run.request["three_factor_acquisition"] != acquisition_identity(&bytes, &request)? {
        return Err("three_factor_acquisition_inputs_or_implementation_changed".into());
    }
    let base = version(w, market, "market")?;
    let dataset = DailyDataset::open(w.resolve(&base.path))?;
    if !dataset.calendar.contains(&request.range.start)
        || !dataset.calendar.contains(&request.range.end)
    {
        return Err("three_factor_acquisition_range_not_in_market_calendar".into());
    }
    let first_year = request.range.start.year() - i32::from(request.range.start.month() <= 6);
    let start = analytics_core::Date::from_ymd_opt(first_year - 1, 12, 1)
        .ok_or("invalid formation year")?;
    let collection = analytics_storage::collector::CollectionRequest {
        index_inputs: Default::default(),
        source_bindings: Default::default(),
        provider: analytics_storage::collector::Provider::Tdxaidata,
        adjustment_source: analytics_storage::adjustment::AdjustmentSource::TdxForward,
        start,
        end: request.range.end,
        instruments: request.universe.intervals.keys().cloned().collect(),
        benchmarks: Default::default(),
        universes: Default::default(),
    };
    collection.validate()?;
    let staging = w.resolve(format!(
        ".jfquant/staging/three-factor-inputs-{}",
        resume.unwrap_or(run.id)
    ));
    fs::create_dir_all(&staging)?;
    let owner = json!({"frozen":run.request["three_factor_acquisition"],"market":run.request["inputs"],"availability":availability});
    let owner_path = staging.join("input-owner.json");
    if owner_path.exists() {
        if serde_json::from_slice::<Value>(&fs::read(&owner_path)?)? != owner {
            return Err("three_factor_acquisition_checkpoint_owner_mismatch".into());
        }
    } else {
        atomic_json(&owner_path, &owner)?;
    }
    context.report("采集通达信逐日股本与年报权益，保存原始回执")?;
    let file = analytics_storage::collector::collect_fundamentals(&collection, &staging, context)?;
    let bundle: Value = serde_json::from_slice(&fs::read(&file)?)?;
    let bundle_sha256 = analytics_storage::market_view::file_hash(&file)?;
    request.provider_history = Some(acquire::History {
        normalization: acquire::VERSION.into(),
        bundle_sha256: bundle_sha256.clone(),
        availability,
        instruments: serde_json::from_value(bundle["instruments"].clone())?,
    });
    for name in ["shares", "book_equity"] {
        request.sources.insert(name.into(),format!("tdxaidata/1.2.2; {}; {}; availability={availability:?}; revision_vintage_unverified",acquire::VERSION,bundle_sha256));
    }
    request.validate()?;
    acquire::verify_bundle(&staging, request.provider_history.as_ref().unwrap())?;
    fs::write(staging.join("template.json"), bytes)?;
    atomic_json(&staging.join("inputs.json"), &request)?;
    version(w, market, "market")?;
    context.check()?;
    let output = w.resolve(&run.output).join("three-factor-inputs");
    let counts:Vec<_>=request.provider_history.as_ref().unwrap().instruments.iter().map(|(id,r)|json!({
        "instrument":id,"observed_on":r.observed_on,"share_dates":r.shares.len(),
        "missing_share_values":r.shares.iter().filter(|r|r.total_shares.is_none()).count(),
        "annual_reports":r.annual.len(),"missing_annual_equity":r.annual.iter().filter(|r|r.book_equity_cny.is_none()).count(),
        "issues":r.issues})).collect();
    fs::rename(staging, &output)?;
    Ok(WorkflowOutcome {
        data: None,
        details: json!({"artifact":output.join("inputs.json"),"bundle":output.join("fundamentals.json"),
        "bundle_sha256":bundle_sha256,"availability":availability,"revision_vintage_verified":false,
        "market_wide_coverage_verified":false,"collection_range":{"start":collection.start,"end":collection.end},"securities":counts}),
    })
}
pub(crate) fn publish(
    w: &Workspace,
    market: &str,
    inputs: &Path,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    let (bytes, request) = read(w, inputs)?;
    let frozen = identity(&bytes, &request);
    if run.request["three_factor_build"] != frozen {
        return Err("three_factor_build_inputs_or_implementation_changed".into());
    }
    let base = version(w, market, "market")?;
    context.report("读取历史行情、股本和年度净资产，计算日频三因子")?;
    let dataset = DailyDataset::open(w.resolve(&base.path))?;
    let (mut factors, report) = build::calculate(&dataset, &request)?;
    factors.source.push_str(&format!(
        "; base={}; input={}; implementation={}",
        base.id,
        frozen["inputs_sha256"].as_str().unwrap(),
        frozen["implementation_sha256"].as_str().unwrap()
    ));
    // Copy to a new owned snapshot, preserving raw acquisition receipts and the old version.
    let staging = w.resolve(format!(".jfquant/staging/three-factors-{}", run.id));
    fs::create_dir(&staging)?;
    let root = staging.join("market");
    context.report("保存三因子、计算明细及独立行情副本")?;
    analytics_storage::corporate_actions::evidence::copy_tree(&w.resolve(&base.path), &root)?;
    let parent = w
        .resolve(&base.path)
        .parent()
        .ok_or("market parent missing")?
        .to_owned();
    for name in ["raw", "evidence"] {
        if parent.join(name).is_dir() {
            analytics_storage::corporate_actions::evidence::copy_tree(
                &parent.join(name),
                &staging.join(name),
            )?;
        }
    }
    let artifacts = root.join("three_factor_build");
    // An existing build remains recorded in the frozen base version, never mixed with this run.
    if artifacts.exists() {
        fs::remove_dir_all(&artifacts)?;
    }
    fs::create_dir(&artifacts)?;
    fs::write(artifacts.join("inputs.json"), &bytes)?;
    if let Some(history) = &request.provider_history {
        let source = w.resolve(inputs);
        let parent = source.parent().ok_or("input parent missing")?;
        fs::copy(
            parent.join("fundamentals.json"),
            artifacts.join("fundamentals.json"),
        )?;
        analytics_storage::corporate_actions::evidence::copy_tree(
            &parent.join("raw"),
            &artifacts.join("raw"),
        )?;
        acquire::verify_bundle(&artifacts, history)?;
    }
    atomic_json(&artifacts.join("report.json"), &report)?;
    atomic_json(&root.join(three_factor::FILE_NAME), &factors)?;
    let complete = factors
        .observations
        .iter()
        .filter(|r| r.mkt.is_some() && r.smb.is_some() && r.hml.is_some())
        .count();
    let manifest = json!({"schema_version":1,"identity":frozen,
        "provider_history":request.provider_history.as_ref().map(|h|json!({"availability":h.availability,"bundle_sha256":h.bundle_sha256,"normalization":h.normalization,"revision_vintage_verified":false})),
        "base_market":{"id":base.id,"signature":base.signature,"path":base.path,"content_sha256":base.details["content_sha256"]},
        "factor_fields":three_factor::FIELDS,"observations":factors.observations.len(),
        "ready":complete==factors.observations.len(),
        "complete_observations":complete,"incomplete_observations":factors.observations.len()-complete,
        "factor_file_sha256":analytics_storage::market_view::file_hash(&root.join(three_factor::FILE_NAME))?,
        "report_sha256":analytics_storage::market_view::file_hash(&artifacts.join("report.json"))?});
    atomic_json(&artifacts.join("manifest.json"), &manifest)?;
    let mut source = base.details.clone();
    source
        .as_object_mut()
        .ok_or("market source metadata invalid")?
        .remove("content_sha256");
    source["three_factor_build"] = manifest.clone();
    atomic_json(&staging.join("jfquant_source.json"), &source)?;
    // Recheck the original market after reading/copying and before publishing the derived version.
    version(w, market, "market")?;
    DailyDataset::open(&root)?;
    context.check()?;
    let output = w.resolve(format!("data/three-factors-{}", run.id));
    fs::rename(&staging, &output)?;
    let data = w.register(&output.join("market"))?;
    Ok(WorkflowOutcome {
        data: Some(data),
        details: manifest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{WorkflowRequest, snapshot};
    use uuid::Uuid;

    #[test]
    fn bigquant_market_artifact_is_replayed_published_and_read_without_changing_base() {
        use analytics_storage::bigquant::market::{self, SuspensionPolicy};
        let tmp = tempfile::tempdir().unwrap();
        let w = Workspace::open(tmp.path()).unwrap();
        let (base, _, _) = crate::demo::create(&w).unwrap();
        let original = crate::market_content_hash(&w.resolve(&base.path)).unwrap();
        let request = market::Request {
            start: "2009-01-05".parse().unwrap(),
            end: "2009-01-05".parse().unwrap(),
        };
        let metadata: serde_json::Map<String, Value> = [
            "cn_stock_instruments",
            "cn_stock_real_bar1d",
            "cn_stock_shares",
            "cn_stock_status",
            "cn_stock_basic_info",
        ]
        .into_iter()
        .map(|t| (t.into(), json!({"id":t,"synthetic_fixture":true})))
        .collect();
        let payload = json!({"projection_version":market::VERSION,"request":request,"sql":request.sql().unwrap(),"metadata":metadata,"arrow_schema":"synthetic fixture only","allowance":{},"rows":[{
            "date":"2009-01-05T00:00:00","universe_members":2,"a_share_members":2,"unclassified_members":0,"eligible_members":2,
            "strict_missing":0,"marked_missing":0,"strict_cap_sum":400.0,"strict_return_sum":4.0,"marked_cap_sum":400.0,"marked_return_sum":4.0,
            "suspended_marks":0,"prior_suspended_marks":0,"missing_shares":0,"duplicate_keys":0,"initial_reference_marks":0,"reported_reference_marks":0,"prior_reference_restores":0}]});
        let hash = format!(
            "{:x}",
            analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(&payload).unwrap()))
        );
        let evidence = json!({"schema_version":3,"provider":"bigquant","transport":"official_python_arrow_flight","observed_at":"2009-01-06T00:00:00Z","complete":true,"research_admitted":false,"sha256":hash,"payload":payload});
        atomic_json(&w.resolve("synthetic-evidence.json"), &evidence).unwrap();
        market::build(
            &w.resolve("synthetic-evidence.json"),
            &w.resolve("factor-artifact"),
            SuspensionPolicy::ConfirmedUnchangedFactor,
        )
        .unwrap();
        let published = w
            .publish_bigquant_market_factors(
                &base.id,
                Path::new("factor-artifact"),
                Path::new("data/published-fixture"),
            )
            .unwrap();
        assert_ne!(base.id, published.id);
        assert_eq!(
            crate::market_content_hash(&w.resolve(&base.path)).unwrap(),
            original
        );
        let data = DailyDataset::open(w.resolve(&published.path)).unwrap();
        let factors = three_factor::ThreeFactorReturns::read(&data.three_factor_path()).unwrap();
        assert_eq!(factors.observations[0].mkt, Some(0.01));
        assert!(factors.observations[0].smb.is_none() && factors.observations[0].hml.is_none());
        // Even after recomputing the outer file hash, publication must replay the source arithmetic.
        let path = w.resolve("factor-artifact/report.json");
        let mut report: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        report["mkt_nonnull_days"] = json!(99);
        atomic_json(&path, &report).unwrap();
        let path = w.resolve("factor-artifact/manifest.json");
        let mut m: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        m["report_sha256"] = json!(
            analytics_storage::market_view::file_hash(&w.resolve("factor-artifact/report.json"))
                .unwrap()
        );
        atomic_json(&path, &m).unwrap();
        assert!(market::verify_build(&w.resolve("factor-artifact")).is_err());
    }

    #[test]
    fn bigquant_market_sql_matches_independent_weights_and_confirmed_suspension_fixture() {
        use analytics_core::{PageRequest, ScalarValue};
        use analytics_engine::{DuckDbEngine, QueryEngine};
        use analytics_storage::bigquant::market::{Request, SuspensionPolicy, return_on};
        let request = Request {
            start: "2005-01-04".parse().unwrap(),
            end: "2005-01-14".parse().unwrap(),
        };
        // Hand-calculated first return: (100*10% + 300*(-2%))/400 = 1%.
        // CDR has a huge capitalization but must never contribute. Gap member has no consecutive pair.
        let fixture = r#"WITH raw_fixture AS (
          SELECT * FROM (VALUES
          (DATE '2005-01-04','600000.SH',10.0,1.0),(DATE '2005-01-05','600000.SH',11.0,1.0),
          (DATE '2005-01-06','600000.SH',NULL,1.0),(DATE '2005-01-07','600000.SH',NULL,2.0),
          (DATE '2005-01-10','600000.SH',NULL,1.0),(DATE '2005-01-11','600000.SH',12.0,1.0),
          (DATE '2005-01-12','600000.SH',13.2,1.0),(DATE '2005-01-13','600000.SH',13.2,1.0),
          (DATE '2005-01-14','600000.SH',13.2,1.0),
          (DATE '2005-01-04','600001.SH',30.0,1.0),(DATE '2005-01-05','600001.SH',29.4,1.0),
          (DATE '2005-01-06','600001.SH',29.4,1.0),(DATE '2005-01-07','600001.SH',29.4,1.0),
          (DATE '2005-01-10','600001.SH',29.4,1.0),(DATE '2005-01-11','600001.SH',29.4,1.0),
          (DATE '2005-01-12','600001.SH',29.4,1.0),(DATE '2005-01-13','600001.SH',NULL,1.0),
          (DATE '2005-01-14','600001.SH',NULL,1.0),
          (DATE '2005-01-04','000001.SZ',1.0,1.0),(DATE '2005-01-06','000001.SZ',100.0,1.0)
          ) AS v(date,instrument,close,adjust_factor)
          UNION ALL SELECT date,'689009.SH',1000000.0,1.0 FROM
          (VALUES (DATE '2005-01-04'),(DATE '2005-01-05'),(DATE '2005-01-06'),(DATE '2005-01-07'),
                  (DATE '2005-01-10'),(DATE '2005-01-11'),(DATE '2005-01-12'),(DATE '2005-01-13'),(DATE '2005-01-14')) d(date)
        ), cn_stock_real_bar1d AS (SELECT *,CASE WHEN instrument='600000.SH' THEN CASE WHEN date<=DATE '2005-01-05' THEN 10.0 ELSE 11.0 END ELSE close END AS pre_close,0.0 AS change_ratio FROM raw_fixture),
        cn_stock_instruments AS (SELECT date,instrument FROM cn_stock_real_bar1d),
        cn_stock_shares AS (SELECT date,instrument,CASE WHEN instrument='600000.SH' AND date=DATE '2005-01-05' THEN 100.0 ELSE 10.0 END total_shares FROM cn_stock_real_bar1d),
        cn_stock_status AS (SELECT date,instrument,CASE WHEN close IS NULL AND NOT(instrument='600001.SH' AND date=DATE '2005-01-13') THEN 1 ELSE 0 END suspended,0 AS exdr FROM cn_stock_real_bar1d),
        cn_stock_basic_info AS (SELECT *,DATE '2000-01-01' AS list_date FROM (VALUES ('600000.SH','上交所主板A股'),('600001.SH','上交所主板A股'),('000001.SZ','深交所主板A股'),('689009.SH','上交所科创板中国存托凭证')) v(instrument,security_type)),
        "#;
        let inner = format!(
            "{fixture}{}",
            request.sql().unwrap().strip_prefix("WITH").unwrap()
        );
        let names = [
            "universe_members",
            "a_share_members",
            "unclassified_members",
            "eligible_members",
            "strict_missing",
            "marked_missing",
            "strict_cap_sum",
            "strict_return_sum",
            "marked_cap_sum",
            "marked_return_sum",
            "suspended_marks",
            "prior_suspended_marks",
            "missing_shares",
            "duplicate_keys",
            "initial_reference_marks",
            "reported_reference_marks",
            "prior_reference_restores",
        ];
        let columns = names
            .iter()
            .map(|n| {
                format!(
                    "CAST({n} AS {}) AS {n}",
                    if n.ends_with("_sum") {
                        "DOUBLE"
                    } else {
                        "BIGINT"
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("SELECT date,{columns} FROM ({inner})");
        let mut engine = DuckDbEngine::open_in_memory().unwrap();
        let page = engine
            .query_page(PageRequest {
                sql,
                limit: 100,
                sort: vec![],
                after: None,
            })
            .unwrap();
        let rows: Vec<Value> = page
            .rows
            .iter()
            .map(|row| {
                Value::Object(
                    page.columns
                        .iter()
                        .zip(row)
                        .map(|(c, v)| {
                            let v = match v {
                                ScalarValue::Null => Value::Null,
                                ScalarValue::Float64(v) => json!(v),
                                ScalarValue::Int64(v) => json!(v),
                                ScalarValue::Date(d) => json!(d.to_string()),
                                ScalarValue::Utf8(v) => json!(v),
                                ScalarValue::Boolean(v) => json!(v),
                            };
                            (c.name.clone(), v)
                        })
                        .collect(),
                )
            })
            .collect();
        assert_eq!(rows.len(), 9);
        let marked = SuspensionPolicy::ConfirmedUnchangedFactor;
        assert!((return_on(&rows[1], marked).unwrap().unwrap() - 0.01).abs() < 1e-12);
        assert_eq!(return_on(&rows[2], SuspensionPolicy::Strict).unwrap(), None);
        assert_eq!(return_on(&rows[2], marked).unwrap(), Some(0.0));
        assert_eq!(rows[2]["eligible_members"], 2);
        assert_eq!(rows[2]["suspended_marks"], 1);
        // Factor changes during a suspension; even a reversal cannot silently revive stale marks.
        for r in &rows[3..5] {
            assert_eq!(return_on(r, marked).unwrap(), None);
        }
        // Once factors are stable, the provider's explicit prior-close reference recovers the previous suspended valuation.
        assert!((return_on(&rows[5], marked).unwrap().unwrap() - 5.0 / 202.0).abs() < 1e-12);
        assert!((return_on(&rows[6], marked).unwrap().unwrap() - 2.0 / 69.0).abs() < 1e-12);
        // Unknown quote at Jan 13 blocks subsequent carry despite a confirmed suspension at Jan 14.
        for r in &rows[7..] {
            assert_eq!(return_on(r, marked).unwrap(), None);
        }
    }

    #[test]
    fn bigquant_initial_reference_requires_confirmed_suspension_no_action_and_prior_listing() {
        use analytics_core::{PageRequest, ScalarValue};
        use analytics_engine::{DuckDbEngine, QueryEngine};
        let r = analytics_storage::bigquant::market::Request {
            start: "2005-01-04".parse().unwrap(),
            end: "2005-01-05".parse().unwrap(),
        };
        let mut engine = DuckDbEngine::open_in_memory().unwrap();
        for (exdr, listed, expect_mark, expect_missing) in [
            (0, "2000-01-01", 1, 0),
            (1, "2000-01-01", 0, 1),
            (0, "2005-01-04", 0, 1),
        ] {
            let fixture = format!(
                r#"WITH cn_stock_real_bar1d AS (
              SELECT * FROM (VALUES (DATE '2005-01-04','600000.SH',NULL,1.0,10.0,0.0),
              (DATE '2005-01-05','600000.SH',11.0,1.0,10.0,10.0),
              (DATE '2005-01-04','600001.SH',30.0,1.0,30.0,0.0),
              (DATE '2005-01-05','600001.SH',29.4,1.0,30.0,-2.0))v(date,instrument,close,adjust_factor,pre_close,change_ratio)
            ),cn_stock_instruments AS (SELECT date,instrument FROM cn_stock_real_bar1d),
            cn_stock_shares AS (SELECT date,instrument,10.0 AS total_shares FROM cn_stock_real_bar1d),
            cn_stock_status AS (SELECT date,instrument,CASE WHEN close IS NULL THEN 1 ELSE 0 END AS suspended,{exdr} AS exdr FROM cn_stock_real_bar1d),
            cn_stock_basic_info AS (SELECT DISTINCT instrument,'上交所主板A股' AS security_type,DATE '{listed}' AS list_date FROM cn_stock_real_bar1d),
            {}"#,
                r.sql().unwrap().strip_prefix("WITH").unwrap()
            );
            let sql = format!(
                "SELECT CAST(initial_reference_marks AS BIGINT) AS initial,CAST(marked_missing AS BIGINT) AS missing,CAST(marked_return_sum/marked_cap_sum AS DOUBLE) AS value FROM ({fixture}) ORDER BY date"
            );
            let page = engine
                .query_page(PageRequest {
                    sql,
                    limit: 10,
                    sort: vec![],
                    after: None,
                })
                .unwrap();
            assert_eq!(page.rows[0][0], ScalarValue::Int64(expect_mark));
            assert_eq!(page.rows[1][1], ScalarValue::Int64(expect_missing));
            if expect_missing == 0 {
                match page.rows[1][2] {
                    ScalarValue::Float64(v) => assert!((v - 0.01).abs() < 1e-12),
                    ref v => panic!("unexpected {v:?}"),
                }
            }
        }
    }
    fn fixture(w: &Workspace) -> (crate::DataVersion, WorkflowRequest) {
        let (base, _, _) = crate::demo::create(w).unwrap();
        let request = json!({"schema_version":build::SCHEMA_VERSION,"construction_version":build::VERSION,"provider_history":null,
            "sources":{"universe":"synthetic dated universe","shares":"synthetic shares","book_equity":"synthetic financials"},
            "market":"synthetic fixture","range":{"start":"2009-01-05","end":"2009-01-06"},
            "price_basis":"provider","market_basis":"market_return",
            "universe":{"coverage":{"start":"2007-01-02","end":"2010-01-12"},"intervals":{
                "SH600000":[{"start":"2007-01-02","end":"2010-01-12"}],
                "SH600001":[{"start":"2007-01-02","end":"2010-01-12"}]}},
            "shares":[{"instrument":"SH600000","effective_date":"2007-01-01","published_on":"2007-01-01","total_shares":1000000},
                {"instrument":"SH600001","effective_date":"2007-01-01","published_on":"2007-01-01","total_shares":2000000}],
            "book_equity":[],"risk_free":[]});
        atomic_json(&w.resolve("facts.json"), &request).unwrap();
        let job = WorkflowRequest::BuildThreeFactors {
            market: base.id.clone(),
            inputs: "facts.json".into(),
        };
        (base, job)
    }
    fn run(w: &Workspace, request: &WorkflowRequest) -> RunRecord {
        w.begin(
            Uuid::new_v4(),
            &request.label(),
            snapshot(w, request).unwrap(),
        )
        .unwrap()
    }
    #[test]
    fn acquisition_freezes_template_and_rejects_mutation_before_network() {
        let dir = tempfile::tempdir().unwrap();
        let w = Workspace::open(dir.path()).unwrap();
        let (base, _) = fixture(&w);
        // Existing explicitly dated manual facts cannot be overwritten by acquisition.
        assert!(freeze_acquisition(&w, Path::new("facts.json")).is_err());
        let mut template: Value =
            serde_json::from_slice(&fs::read(w.resolve("facts.json")).unwrap()).unwrap();
        template["shares"] = json!([]);
        atomic_json(&w.resolve("facts.json"), &template).unwrap();
        let job = WorkflowRequest::CollectThreeFactorInputs {
            market: base.id,
            inputs: "facts.json".into(),
            availability: acquire::Availability::ObservedOn,
            resume: None,
        };
        let run = run(&w, &job);
        assert_eq!(
            run.request["three_factor_acquisition"]["collector"],
            analytics_storage::collector::implementation_identity()
        );
        template["market"] = json!("modified after submission");
        atomic_json(&w.resolve("facts.json"), &template).unwrap();
        let error = crate::execute(&w, &job, &run, &TaskContext::current())
            .err()
            .unwrap();
        assert!(
            error
                .to_string()
                .contains("three_factor_acquisition_inputs_or_implementation_changed")
        );
        assert!(
            !w.resolve(format!(".jfquant/staging/three-factor-inputs-{}", run.id))
                .exists()
        );
        let mut wire = serde_json::to_value(job).unwrap();
        wire.as_object_mut().unwrap().remove("availability");
        assert!(serde_json::from_value::<WorkflowRequest>(wire).is_err());
    }
    #[test]
    fn old_input_files_are_rejected_before_freezing_or_publishing() {
        let dir = tempfile::tempdir().unwrap();
        let w = Workspace::open(dir.path()).unwrap();
        let (_, job) = fixture(&w);
        let mut old: Value =
            serde_json::from_slice(&fs::read(w.resolve("facts.json")).unwrap()).unwrap();
        old["schema_version"] = json!(1);
        atomic_json(&w.resolve("facts.json"), &old).unwrap();
        assert!(snapshot(&w, &job).is_err());
        old["schema_version"] = json!(build::SCHEMA_VERSION);
        old.as_object_mut().unwrap().remove("provider_history");
        atomic_json(&w.resolve("facts.json"), &old).unwrap();
        assert!(snapshot(&w, &job).is_err());
        assert!(
            fs::read_dir(w.resolve(".jfquant/staging"))
                .unwrap()
                .next()
                .is_none()
        );
    }
    #[test]
    fn three_factor_build_publishes_immutable_market_and_null_diagnostics() {
        let dir = tempfile::tempdir().unwrap();
        let w = Workspace::open(dir.path()).unwrap();
        let (base, request) = fixture(&w);
        let before = crate::market_content_hash(&w.resolve(&base.path)).unwrap();
        let run = run(&w, &request);
        let outcome = crate::execute(&w, &request, &run, &TaskContext::current()).unwrap();
        let data = outcome.data.unwrap();
        assert_ne!(base.id, data.id);
        assert_eq!(
            before,
            crate::market_content_hash(&w.resolve(&base.path)).unwrap()
        );
        assert_eq!(outcome.details["ready"], false);
        assert_eq!(outcome.details["complete_observations"], 0);
        assert_eq!(outcome.details["incomplete_observations"], 2);
        assert_eq!(data.details["synthetic_fixture"], true);
        let dataset = DailyDataset::open(w.resolve(&data.path)).unwrap();
        let i = dataset
            .calendar
            .binary_search(&"2009-01-05".parse().unwrap())
            .unwrap();
        assert_eq!(dataset.field("SH600000", "mkt").unwrap().get(i), Some(0.));
        assert_eq!(dataset.field("SH600000", "smb").unwrap().get(i), None);
        let saved = w.resolve(&data.path).join("three_factor_build/inputs.json");
        assert_eq!(
            fs::read(saved).unwrap(),
            fs::read(w.resolve("facts.json")).unwrap()
        );
        version(&w, &data.id, "market").unwrap();
        // Changed diagnostics are part of the registered market identity.
        fs::write(
            w.resolve(&data.path).join("three_factor_build/report.json"),
            b"{}",
        )
        .unwrap();
        assert!(version(&w, &data.id, "market").is_err());
    }
    #[test]
    fn three_factor_build_rejects_changed_inputs_after_submission() {
        let dir = tempfile::tempdir().unwrap();
        let w = Workspace::open(dir.path()).unwrap();
        let (_, request) = fixture(&w);
        let run = run(&w, &request);
        let mut bytes = fs::read(w.resolve("facts.json")).unwrap();
        bytes.push(b' ');
        fs::write(w.resolve("facts.json"), bytes).unwrap();
        let error = crate::execute(&w, &request, &run, &TaskContext::current()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("inputs_or_implementation_changed")
        );
        assert!(!w.resolve(format!("data/three-factors-{}", run.id)).exists());
    }
}
