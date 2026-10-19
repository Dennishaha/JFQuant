//! Frozen June 2×3 portfolios, sufficient-statistic acquisition, and ordinary factor sidecar publication.
use super::{cached, flight, invalid, market, schema, size_value};
use crate::three_factor::{ThreeFactorReturns, build::PORTFOLIOS};
use chrono::{Datelike, NaiveDate};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
};

/// Independent formula/formation version, with the approved dated-report eligibility convention.
pub const VERSION: &str = "bigquant-cn-ff3-june-bps-v1";
type Assignment = (i32, String, String);
const TABLES: [&str; 5] = [
    "cn_stock_instruments",
    "cn_stock_real_bar1d",
    "cn_stock_shares",
    "cn_stock_status",
    "cn_stock_basic_info",
];
const RETURN_COLUMNS: [&str; 10] = [
    "date",
    "formation_year",
    "portfolio",
    "members",
    "missing",
    "cap_sum",
    "return_sum",
    "suspended_marks",
    "prior_suspended_marks",
    "prior_reference_restores",
];
fn fields() -> Value {
    json!([{"name":"mkt","type":"nullable_float64","unit":"decimal"},{"name":"smb","type":"nullable_float64","unit":"decimal"},{"name":"hml","type":"nullable_float64","unit":"decimal"}])
}
/// All evidence paths resolve relative to the plan file. No latest-value financial backfill is permitted.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    /// Current construction-plan format.
    pub schema_version: u32,
    /// Must match VERSION.
    pub construction_version: String,
    /// Inclusive output dates; supporting quote history starts in 2005.
    pub range: market::Request,
    /// Non-overlapping format4 June formation extracts.
    pub formation_inputs: Vec<PathBuf>,
    /// Format1 raw balances proving equivalent financial vintages where necessary.
    pub balance_supplements: Vec<PathBuf>,
    /// Verified format3 MKT sufficient statistics from the same price/valuation method.
    pub market_evidence: PathBuf,
    /// Explicitly selected available_before_formation eligibility.
    pub report_policy: String,
    /// Explicit provider_dates share assumption.
    pub shares_availability: String,
    /// Must remain true.
    pub shares_revision_risk: bool,
}
fn num(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .filter(|n| n.is_finite())
}
fn hash(v: &Value) -> io::Result<String> {
    Ok(format!(
        "{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(v)?))
    ))
}
fn read_plan(path: &Path) -> io::Result<Plan> {
    let mut p: Plan = serde_json::from_slice(&fs::read(path)?)?;
    if p.schema_version != 1
        || p.construction_version != VERSION
        || p.report_policy != "available_before_formation"
        || p.shares_availability != "provider_dates"
        || !p.shares_revision_risk
        || p.formation_inputs.is_empty()
    {
        return Err(invalid("size_value_plan_invalid"));
    }
    p.range.sql()?;
    let parent = path
        .parent()
        .ok_or_else(|| invalid("size_value_plan_parent_missing"))?;
    for f in p
        .formation_inputs
        .iter_mut()
        .chain(&mut p.balance_supplements)
        .chain(std::iter::once(&mut p.market_evidence))
    {
        if f.is_relative() {
            *f = parent.join(&*f);
        }
    }
    Ok(p)
}
fn quantile(mut values: Vec<f64>, p: f64) -> f64 {
    values.sort_by(f64::total_cmp);
    let index = (values.len() - 1) as f64 * p;
    let a = index.floor() as usize;
    let b = index.ceil() as usize;
    values[a] + (values[b] - values[a]) * (index - a as f64)
}
fn assign(audit: &mut Value) -> io::Result<Vec<(i32, String, String)>> {
    let mut assignments = Vec::new();
    for f in audit["formations"]
        .as_array_mut()
        .ok_or_else(|| invalid("formations_missing"))?
    {
        let y = f["year"]
            .as_i64()
            .ok_or_else(|| invalid("formation_year_invalid"))? as i32;
        let mut members = Vec::new();
        let mut blockers = Vec::new();
        let mut unavailable = Vec::new();
        for r in f["members"].as_array().unwrap() {
            match r["issue"].as_str(){
                None if r["issue"].is_null()=>{
                    if ["basic_rows","june_universe_rows","december_universe_rows","june_share_rows","december_share_rows"].iter().any(|k|num(&r[k])!=Some(1.)){blockers.push(json!({"instrument":r["instrument"],"issue":"formation_source_keys_not_unique"}));continue;}
                    let symbol=r["instrument"].as_str().ok_or_else(||invalid("formation_symbol_invalid"))?;
                    if symbol.len()!=9 || !symbol[..6].bytes().all(|b|b.is_ascii_digit()) || ![".SH",".SZ",".BJ"].contains(&&symbol[6..]){return Err(invalid("formation_symbol_invalid"));}
                    let size=num(&r["june_close"]).unwrap()*num(&r["june_shares"]).unwrap();
                    let bm=num(&r["common_equity_cny"]).unwrap()/(num(&r["december_close"]).unwrap()*num(&r["december_shares"]).unwrap());
                    if size<=0. || bm<=0. || !size.is_finite() || !bm.is_finite(){return Err(invalid("formation_metric_invalid"));}
                    members.push((symbol.to_string(),size,bm));
                },
                Some("non_a_share_excluded"|"not_listed_at_prior_december"|"nonpositive_common_equity")=>{},
                Some("parent_equity_unknown") if r["raw_date"].is_null() && r["xps_date"].is_null()=>unavailable.push(json!({"instrument":r["instrument"],"reason":"no_annual_report_available_in_provider_pit_before_formation","actual_nonpublication_independently_verified":false})),
                _=>blockers.push(json!({"instrument":r["instrument"],"issue":r["issue"]})),
            }
        }
        f["report_policy"] = json!("available_before_formation");
        f["report_unavailable_exclusions"] = json!(unavailable);
        f["blockers"] = json!(blockers);
        f["ready"] = json!(false);
        if !blockers.is_empty() || members.len() < 6 {
            continue;
        }
        let size = quantile(members.iter().map(|m| m.1).collect(), 0.5);
        let low = quantile(members.iter().map(|m| m.2).collect(), 0.3);
        let high = quantile(members.iter().map(|m| m.2).collect(), 0.7);
        let mut groups = BTreeMap::<String, Vec<String>>::new();
        for p in PORTFOLIOS {
            groups.insert(p.into(), Vec::new());
        }
        for (symbol, s, bm) in members {
            let group = format!(
                "{}{}",
                if s <= size { "S" } else { "B" },
                if bm <= low {
                    "L"
                } else if bm <= high {
                    "N"
                } else {
                    "H"
                }
            );
            groups.get_mut(&group).unwrap().push(symbol);
        }
        f["thresholds"] = json!({"size_median":size,"bm_p30":low,"bm_p70":high,"quantile":"R7"});
        f["portfolios"] = json!(groups);
        if groups.values().any(Vec::is_empty) {
            f["blockers"] = json!([{"issue":"empty_formation_portfolio"}]);
            continue;
        }
        f["ready"] = json!(true);
        for (p, symbols) in groups {
            for s in symbols {
                assignments.push((y, s, p.clone()));
            }
        }
    }
    assignments.sort();
    Ok(assignments)
}
fn form(plan: &Plan) -> io::Result<(Value, Vec<Assignment>)> {
    let mut audit =
        size_value::audit_supplemented(&plan.formation_inputs, &plan.balance_supplements)?;
    let groups = assign(&mut audit)?;
    Ok((audit, groups))
}
fn sql(plan: &Plan, groups: &[(i32, String, String)]) -> io::Result<String> {
    let source = plan.range.sql()?;
    let (prefix, _) = source
        .rsplit_once("SELECT date,\n  count(*) AS universe_members")
        .ok_or_else(|| invalid("market_query_identity"))?;
    let mut buckets = BTreeMap::<(i32, &str), Vec<&str>>::new();
    for (y, s, p) in groups {
        buckets.entry((*y, p)).or_default().push(s);
    }
    let clauses=buckets.into_iter().map(|((y,p),symbols)|format!("WHEN year(date)-CASE WHEN month(date)<=6 THEN 1 ELSE 0 END={y} AND instrument IN ({}) THEN '{p}'",symbols.into_iter().map(|s|format!("'{s}'")).collect::<Vec<_>>().join(","))).collect::<Vec<_>>().join("\n");
    let classification = if clauses.is_empty() {
        "CAST(NULL AS VARCHAR)".into()
    } else {
        format!("CASE {clauses} END")
    };
    let grid=PORTFOLIOS.into_iter().map(|p|format!("SELECT date,year(date)-CASE WHEN month(date)<=6 THEN 1 ELSE 0 END AS formation_year,'{p}' AS portfolio FROM calendar")).collect::<Vec<_>>().join(" UNION ALL ");
    Ok(format!(
        r#"{prefix},
context_terms AS (SELECT * FROM terms QUALIFY row_number() OVER ()>0),
classified AS (SELECT *,year(date)-CASE WHEN month(date)<=6 THEN 1 ELSE 0 END AS formation_year,{classification} AS portfolio FROM context_terms),
stats AS (
 SELECT date,formation_year,portfolio,count(*) AS members,
 sum(CASE WHEN marked_cap IS NULL THEN 1 ELSE 0 END) AS missing,
 sum(marked_cap) AS cap_sum,sum(marked_cap*marked_return) AS return_sum,
 sum(CASE WHEN actual_close IS NULL AND marked_close IS NOT NULL THEN 1 ELSE 0 END) AS suspended_marks,
 sum(CASE WHEN prior_close IS NULL AND prior_mark IS NOT NULL THEN 1 ELSE 0 END) AS prior_suspended_marks,
 sum(CASE WHEN prior_mark IS NULL AND previous_mark IS NOT NULL THEN 1 ELSE 0 END) AS prior_reference_restores
 FROM classified
 WHERE previous_day=market_day-1 AND portfolio IS NOT NULL GROUP BY date,formation_year,portfolio
),
grid AS ({grid})
SELECT g.date,g.formation_year,g.portfolio,coalesce(s.members,0) AS members,coalesce(s.missing,0) AS missing,s.cap_sum,s.return_sum,
coalesce(s.suspended_marks,0) AS suspended_marks,coalesce(s.prior_suspended_marks,0) AS prior_suspended_marks,coalesce(s.prior_reference_restores,0) AS prior_reference_restores
FROM grid g LEFT JOIN stats s USING(date,formation_year,portfolio) ORDER BY g.date,g.portfolio LIMIT 50001"#
    ))
}
fn verify_returns(v: &Value, plan: &Plan, groups: &[(i32, String, String)]) -> io::Result<()> {
    let p = &v["payload"];
    let rows = p["rows"]
        .as_array()
        .ok_or_else(|| invalid("size_value_return_rows_missing"))?;
    if v["schema_version"] != 5
        || v["provider"] != "bigquant"
        || v["transport"] != "official_python_arrow_flight"
        || rows.len() > 50001
        || !p["arrow_schema"].is_string()
        || v["observed_at"]
            .as_str()
            .is_none_or(|s| chrono::DateTime::parse_from_rfc3339(s).is_err())
        || v["research_admitted"] != false
        || v["complete"] != json!(rows.len() <= 50000)
        || v["sha256"] != hash(p)?
        || p["projection_version"] != VERSION
        || p["range"] != serde_json::to_value(&plan.range)?
        || p["groups_sha256"] != hash(&json!(groups))?
        || p["sql"] != sql(plan, groups)?
        || TABLES.iter().any(|t| p["metadata"][t]["id"] != *t)
        || rows.iter().any(|r| {
            r.as_object().is_none_or(|o| {
                o.len() != RETURN_COLUMNS.len()
                    || RETURN_COLUMNS.iter().any(|c| !o.contains_key(*c))
            })
        })
    {
        return Err(invalid("size_value_return_evidence_mismatch"));
    }
    Ok(())
}
/// Fetch six portfolio sufficient statistics on every source day, from locally frozen formations.
pub fn fetch(python: &Path, plan_path: &Path, output: &Path) -> io::Result<(Value, bool)> {
    let plan = read_plan(plan_path)?;
    let (_, groups) = form(&plan)?;
    let query = sql(&plan, &groups)?;
    cached(
        output,
        || {
            let mut metadata = BTreeMap::new();
            for t in TABLES {
                metadata.insert(t, schema(t)?);
            }
            let result = flight(
                python,
                json!({"operation":"read","table":TABLES[0],"tables":TABLES,"column_count":RETURN_COLUMNS.len(),"max_rows":50001,"sql":query,"filters":{"date":["2005-01-04",plan.range.end.to_string()]}}),
            )?;
            let rows = result["rows"]
                .as_array()
                .ok_or_else(|| invalid("portfolio_rows_missing"))?;
            let payload = json!({"projection_version":VERSION,"range":plan.range,"groups_sha256":hash(&json!(groups))?,"sql":query,"metadata":metadata,"rows":rows,"arrow_schema":result["arrow_schema"],"allowance":result["allowance"]});
            Ok(
                json!({"schema_version":5,"provider":"bigquant","transport":"official_python_arrow_flight","observed_at":chrono::Utc::now().to_rfc3339(),"research_admitted":false,"complete":rows.len()<=50000,"sha256":hash(&payload)?,"payload":payload}),
            )
        },
        |v| verify_returns(v, &plan, &groups),
    )
}
fn portfolio_return(r: &Value) -> io::Result<Option<f64>> {
    let members = num(&r["members"]).ok_or_else(|| invalid("portfolio_members_invalid"))?;
    let missing = num(&r["missing"]).ok_or_else(|| invalid("portfolio_missing_invalid"))?;
    if members < 0. || missing < 0. || missing > members {
        return Err(invalid("portfolio_counts_invalid"));
    }
    if members == 0. || missing > 0. {
        return Ok(None);
    }
    match (num(&r["cap_sum"]), num(&r["return_sum"])) {
        (Some(c), Some(n)) if c > 0. && (n / c).is_finite() => Ok(Some(n / c)),
        _ => Err(invalid("portfolio_aggregate_invalid")),
    }
}
fn combine(p: &BTreeMap<String, Option<f64>>) -> (Option<f64>, Option<f64>) {
    let smb = p["SL"]
        .zip(p["SN"])
        .zip(p["SH"])
        .zip(p["BL"])
        .zip(p["BN"])
        .zip(p["BH"])
        .map(|(((((sl, sn), sh), bl), bn), bh)| (sl + sn + sh - bl - bn - bh) / 3.);
    let hml = p["SH"]
        .zip(p["BH"])
        .zip(p["SL"])
        .zip(p["BL"])
        .map(|(((sh, bh), sl), bl)| (sh + bh - sl - bl) / 2.);
    (smb, hml)
}
fn calculate(plan: &Plan, evidence: &Value) -> io::Result<(ThreeFactorReturns, Value)> {
    let (mut formations, groups) = form(plan)?;
    verify_returns(evidence, plan, &groups)?;
    if evidence["complete"] != true {
        return Err(invalid("portfolio_extract_truncated"));
    }
    let m: Value = serde_json::from_slice(&fs::read(&plan.market_evidence)?)?;
    let req: market::Request = serde_json::from_value(m["payload"]["request"].clone())?;
    if req.start != plan.range.start || req.end != plan.range.end {
        return Err(invalid("market_portfolio_range_mismatch"));
    }
    let (mut factors, market_report) =
        market::calculate(&m, &req, market::SuspensionPolicy::ConfirmedUnchangedFactor)?;
    let mut daily = BTreeMap::<NaiveDate, BTreeMap<String, Option<f64>>>::new();
    let mut seen = BTreeSet::new();
    for row in evidence["payload"]["rows"].as_array().unwrap() {
        let s = row["date"]
            .as_str()
            .ok_or_else(|| invalid("portfolio_date_invalid"))?;
        if s.len() != 19 || !s.ends_with("T00:00:00") {
            return Err(invalid("portfolio_date_invalid"));
        }
        let date = NaiveDate::parse_from_str(&s[..10], "%Y-%m-%d")
            .map_err(|_| invalid("portfolio_date_invalid"))?;
        let p = row["portfolio"]
            .as_str()
            .ok_or_else(|| invalid("portfolio_name_missing"))?;
        let y = date.year() - i32::from(date.month() <= 6);
        if row["formation_year"].as_i64() != Some(y as i64)
            || !PORTFOLIOS.contains(&p)
            || !seen.insert((date, p.to_string()))
            || date > plan.range.end
        {
            return Err(invalid("portfolio_date_or_key_invalid"));
        }
        daily
            .entry(date)
            .or_default()
            .insert(p.to_string(), portfolio_return(row)?);
    }
    let (mut smb_count, mut hml_count, mut complete) = (0, 0, 0);
    for o in &mut factors.observations {
        let p = daily
            .get(&o.trade_date)
            .ok_or_else(|| invalid("portfolio_source_date_missing"))?;
        if p.len() != 6 {
            return Err(invalid("portfolio_grid_incomplete"));
        }
        (o.smb, o.hml) = combine(p);
        smb_count += usize::from(o.smb.is_some());
        hml_count += usize::from(o.hml.is_some());
        complete += usize::from(o.mkt.is_some() && o.smb.is_some() && o.hml.is_some());
    }
    if daily.len() != factors.observations.len() {
        return Err(invalid("portfolio_calendar_mismatch"));
    }
    factors.source = format!(
        "{VERSION}; portfolio_evidence={}; PIT provider BPS; shares=provider_dates; shares_revision_risk=true",
        evidence["sha256"]
    );
    factors.return_basis.push_str("; June 2x3 annual portfolios, R7 50% size and 30/70% book/December-market; available-before-formation reports; nonpositive BE excluded; missing financial fields block formation; SMB=(SL+SN+SH-BL-BN-BH)/3; HML=(SH+BH-SL-BL)/2");
    factors.validate()?;
    // Paths differ after portable bundling; content identities are authoritative.
    for s in formations["sources"].as_array_mut().unwrap() {
        s.as_object_mut().unwrap().remove("path");
    }
    Ok((
        factors,
        json!({"schema_version":1,"construction_version":VERSION,"days":market_report["days"],"mkt_nonnull_days":market_report["mkt_nonnull_days"],"smb_nonnull_days":smb_count,"hml_nonnull_days":hml_count,"complete_three_factor_days":complete,"shares_revision_risk":true,"rf_subtracted":false,"formations":formations,"market_diagnostics":market_report,"portfolio_diagnostics":evidence["payload"]["rows"]}),
    ))
}
/// Freeze every participating constructor and reader in the artifact manifest.
pub fn implementation_signature() -> String {
    let mut h = Sha256::new();
    for b in [
        include_bytes!("size_value_factors.rs").as_slice(),
        include_bytes!("size_value.rs").as_slice(),
        include_bytes!("size_value_formation.sql").as_slice(),
        include_bytes!("market.rs").as_slice(),
        include_bytes!("market.sql").as_slice(),
        include_bytes!("../three_factor.rs").as_slice(),
    ] {
        h.update(b);
        h.update([0]);
    }
    format!("sha256:{:x}", analytics_core::hash::Hex(h.finalize()))
}
/// Build a portable immutable artifact. Existing outputs are never overwritten.
pub fn build(plan_path: &Path, returns: &Path, output: &Path) -> io::Result<Value> {
    let plan = read_plan(plan_path)?;
    let evidence: Value = serde_json::from_slice(&fs::read(returns)?)?;
    calculate(&plan, &evidence)?;
    if output.exists() {
        return Err(invalid("size_value_output_exists"));
    }
    let parent = output
        .parent()
        .ok_or_else(|| invalid("size_value_parent_missing"))?;
    fs::create_dir_all(parent)?;
    let lock = output.with_extension("build-lock");
    let _lock = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)?;
    let stage = parent.join(format!(
        ".size-value-build-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let result = (|| -> io::Result<Value> {
        fs::create_dir_all(stage.join("inputs"))?;
        let mut portable = plan.clone();
        for (i, p) in portable.formation_inputs.iter_mut().enumerate() {
            let name = PathBuf::from(format!("inputs/formation-{i}.json"));
            fs::copy(&*p, stage.join(&name))?;
            *p = name;
        }
        for (i, p) in portable.balance_supplements.iter_mut().enumerate() {
            let name = PathBuf::from(format!("inputs/balance-{i}.json"));
            fs::copy(&*p, stage.join(&name))?;
            *p = name;
        }
        fs::copy(&portable.market_evidence, stage.join("inputs/market.json"))?;
        portable.market_evidence = "inputs/market.json".into();
        fs::write(
            stage.join("plan.json"),
            serde_json::to_vec_pretty(&portable)?,
        )?;
        fs::copy(returns, stage.join("returns.json"))?;
        let bundled = read_plan(&stage.join("plan.json"))?;
        let (factors, report) = calculate(&bundled, &evidence)?;
        fs::write(
            stage.join(crate::three_factor::FILE_NAME),
            serde_json::to_vec_pretty(&factors)?,
        )?;
        fs::write(
            stage.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        let mut hashes = BTreeMap::new();
        for p in portable
            .formation_inputs
            .iter()
            .chain(&portable.balance_supplements)
            .chain([
                &portable.market_evidence,
                &PathBuf::from("plan.json"),
                &PathBuf::from("returns.json"),
                &PathBuf::from("report.json"),
                &PathBuf::from(crate::three_factor::FILE_NAME),
            ])
        {
            hashes.insert(
                p.to_string_lossy().into_owned(),
                crate::market_view::file_hash(&stage.join(p))?,
            );
        }
        let fields = fields();
        let manifest = json!({"schema_version":1,"construction_version":VERSION,"implementation_sha256":implementation_signature(),"fields":fields,"schema_sha256":hash(&fields)?,"files":hashes,"days":report["days"],"mkt_nonnull_days":report["mkt_nonnull_days"],"smb_nonnull_days":report["smb_nonnull_days"],"hml_nonnull_days":report["hml_nonnull_days"],"complete_three_factor_days":report["complete_three_factor_days"],"full_three_factor_ready":report["complete_three_factor_days"]==report["days"],"shares_revision_risk":true,"rf_subtracted":false,"report_policy":"available_before_formation","suspension_policy":"confirmed_unchanged_factor","sdk":{"bigquant":"0.1.11","bigquant_core":"0.1.14"}});
        fs::write(
            stage.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        verify_build(&stage)?;
        fs::rename(&stage, output)?;
        Ok(manifest)
    })();
    if result.is_err() && stage.exists() {
        fs::remove_dir_all(stage)?;
    }
    fs::remove_file(lock)?;
    result
}
/// Validate source bytes and replay formation, portfolio math, and all report diagnostics.
pub fn verify_build(root: &Path) -> io::Result<(ThreeFactorReturns, Value)> {
    let manifest: Value = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
    if manifest["schema_version"] != 1
        || manifest["construction_version"] != VERSION
        || manifest["implementation_sha256"] != implementation_signature()
        || manifest["schema_sha256"] != hash(&manifest["fields"])?
        || manifest["fields"] != fields()
        || manifest["sdk"] != json!({"bigquant":"0.1.11","bigquant_core":"0.1.14"})
        || manifest["shares_revision_risk"] != true
        || manifest["rf_subtracted"] != false
        || manifest["report_policy"] != "available_before_formation"
        || manifest["suspension_policy"] != "confirmed_unchanged_factor"
    {
        return Err(invalid("size_value_manifest_identity_invalid"));
    }
    for (name, h) in manifest["files"]
        .as_object()
        .ok_or_else(|| invalid("size_value_file_hashes_missing"))?
    {
        let p = Path::new(name);
        if p.is_absolute()
            || p.components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            || *h != crate::market_view::file_hash(&root.join(p))?
        {
            return Err(invalid("size_value_file_hash_invalid"));
        }
    }
    let plan = read_plan(&root.join("plan.json"))?;
    let expected: BTreeSet<String> = plan
        .formation_inputs
        .iter()
        .chain(&plan.balance_supplements)
        .chain(std::iter::once(&plan.market_evidence))
        .map(|p| {
            p.strip_prefix(root)
                .map(|p| p.to_string_lossy().into_owned())
                .map_err(|_| invalid("size_value_bundle_path_outside_artifact"))
        })
        .chain(
            [
                "plan.json",
                "returns.json",
                "report.json",
                crate::three_factor::FILE_NAME,
            ]
            .into_iter()
            .map(|s| Ok(s.to_string())),
        )
        .collect::<io::Result<_>>()?;
    if manifest["files"]
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<BTreeSet<_>>()
        != expected
    {
        return Err(invalid("size_value_manifest_file_set_mismatch"));
    }
    let evidence: Value = serde_json::from_slice(&fs::read(root.join("returns.json"))?)?;
    let (factors, report) = calculate(&plan, &evidence)?;
    if serde_json::from_slice::<Value>(&fs::read(root.join(crate::three_factor::FILE_NAME))?)?
        != serde_json::to_value(&factors)?
        || serde_json::from_slice::<Value>(&fs::read(root.join("report.json"))?)? != report
    {
        return Err(invalid("size_value_artifact_replay_mismatch"));
    }
    for k in [
        "days",
        "mkt_nonnull_days",
        "smb_nonnull_days",
        "hml_nonnull_days",
        "complete_three_factor_days",
    ] {
        if manifest[k] != report[k] {
            return Err(invalid("size_value_manifest_count_mismatch"));
        }
    }
    if manifest["full_three_factor_ready"]
        != json!(report["complete_three_factor_days"] == report["days"])
    {
        return Err(invalid("size_value_manifest_readiness_mismatch"));
    }
    Ok((factors, manifest))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture_metadata(financial: bool) -> Value {
        let mut names = TABLES.to_vec();
        if financial {
            names.extend([
                "cn_stock_financial_balance_general_pit",
                "cn_stock_financial_xps",
            ]);
        }
        names
            .into_iter()
            .map(|n| (n.to_string(), json!({"id":n,"synthetic_fixture":true})))
            .collect::<serde_json::Map<_, _>>()
            .into()
    }
    fn fixture_evidence(version: u32, payload: Value) -> Value {
        json!({"schema_version":version,"provider":"bigquant","transport":"official_python_arrow_flight","observed_at":"2012-07-04T00:00:00Z","research_admitted":false,"complete":true,"sha256":hash(&payload).unwrap(),"payload":payload})
    }
    #[test]
    fn portable_artifact_replays_annual_groups_six_returns_and_rejects_report_tampering() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let request = size_value::Request {
            start_year: 2012,
            end_year: 2012,
        };
        let mut rows = Vec::new();
        for i in 0..12 {
            let bm = (i % 3 + 1) as f64;
            let equity = 100. * bm;
            rows.push(json!({"formation_year":2012,"formation_date":"2012-06-29T00:00:00","december_date":"2011-12-30T00:00:00","instrument":format!("{:06}.SH",600000+i),"security_type":"上交所主板A股","basic_rows":1,"june_universe_rows":1,"december_universe_rows":1,"june_close":if i<6 {10.}else{100.},"june_shares":10.,"june_share_rows":1,"december_close":10.,"december_shares":10.,"december_share_rows":1,"raw_date":"2012-04-20T00:00:00","report_date":"2011-12-31T00:00:00","raw_keys":1,"parent_equity":equity+20.,"other_equity":20.,"capital_reserves":20.,"surplus_reserve":40.,"undistributed_profit":10.,"xps_date":"2012-04-20T00:00:00","xps_keys":1,"bps_lf":equity/10.,"capital_reserves_ps_lf":2.,"surplus_reserve_ps_lf":4.,"undistributed_profit_ps_lf":1.}));
        }
        let e = fixture_evidence(
            4,
            json!({"projection_version":size_value::VERSION,"request":request,"sql":request.sql().unwrap(),"metadata":fixture_metadata(true),"arrow_schema":"synthetic fixture","allowance":{},"rows":rows}),
        );
        fs::write(root.join("formation.json"), serde_json::to_vec(&e).unwrap()).unwrap();
        let range = market::Request {
            start: "2012-07-02".parse().unwrap(),
            end: "2012-07-03".parse().unwrap(),
        };
        let market_rows:Vec<Value>=["2012-07-02","2012-07-03"].into_iter().map(|d|json!({"date":format!("{d}T00:00:00"),"universe_members":12,"a_share_members":12,"unclassified_members":0,"eligible_members":12,"strict_missing":0,"marked_missing":0,"strict_cap_sum":400.,"strict_return_sum":4.,"marked_cap_sum":400.,"marked_return_sum":4.,"suspended_marks":0,"prior_suspended_marks":0,"missing_shares":0,"duplicate_keys":0,"initial_reference_marks":0,"reported_reference_marks":0,"prior_reference_restores":0})).collect();
        let m = fixture_evidence(
            3,
            json!({"projection_version":market::VERSION,"request":range,"sql":range.sql().unwrap(),"metadata":fixture_metadata(false),"arrow_schema":"synthetic fixture","allowance":{},"rows":market_rows}),
        );
        fs::write(root.join("market.json"), serde_json::to_vec(&m).unwrap()).unwrap();
        let plan = Plan {
            schema_version: 1,
            construction_version: VERSION.into(),
            range,
            formation_inputs: vec!["formation.json".into()],
            balance_supplements: vec![],
            market_evidence: "market.json".into(),
            report_policy: "available_before_formation".into(),
            shares_availability: "provider_dates".into(),
            shares_revision_risk: true,
        };
        fs::write(root.join("plan.json"), serde_json::to_vec(&plan).unwrap()).unwrap();
        let plan = read_plan(&root.join("plan.json")).unwrap();
        let (audit, groups) = form(&plan).unwrap();
        assert_eq!(groups.len(), 12);
        assert_eq!(audit["formations"][0]["ready"], true);
        assert_eq!(audit["formations"][0]["thresholds"]["size_median"], 550.);
        let mut returns = Vec::new();
        for d in ["2012-07-02", "2012-07-03"] {
            for (p, r) in PORTFOLIOS
                .into_iter()
                .zip([0.01, 0.02, 0.03, 0.04, 0.05, 0.06])
            {
                returns.push(json!({"date":format!("{d}T00:00:00"),"formation_year":2012,"portfolio":p,"members":2,"missing":0,"cap_sum":200.,"return_sum":200.*r,"suspended_marks":0,"prior_suspended_marks":0,"prior_reference_restores":0}));
            }
        }
        let e = fixture_evidence(
            5,
            json!({"projection_version":VERSION,"range":plan.range,"groups_sha256":hash(&json!(groups)).unwrap(),"sql":sql(&plan,&groups).unwrap(),"metadata":fixture_metadata(false),"arrow_schema":"synthetic fixture","allowance":{},"rows":returns}),
        );
        fs::write(root.join("returns.json"), serde_json::to_vec(&e).unwrap()).unwrap();
        build(
            &root.join("plan.json"),
            &root.join("returns.json"),
            &root.join("artifact"),
        )
        .unwrap();
        let (factors, manifest) = verify_build(&root.join("artifact")).unwrap();
        assert_eq!(manifest["complete_three_factor_days"], 2);
        for o in factors.observations {
            assert_eq!(o.mkt, Some(0.01));
            assert!((o.smb.unwrap() + 0.03).abs() < 1e-14);
            assert!((o.hml.unwrap() - 0.02).abs() < 1e-14);
        }
        let mut report: Value =
            serde_json::from_slice(&fs::read(root.join("artifact/report.json")).unwrap()).unwrap();
        report["smb_nonnull_days"] = json!(999);
        fs::write(
            root.join("artifact/report.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        let mut m = manifest;
        m["files"]["report.json"] =
            json!(crate::market_view::file_hash(&root.join("artifact/report.json")).unwrap());
        fs::write(
            root.join("artifact/manifest.json"),
            serde_json::to_vec(&m).unwrap(),
        )
        .unwrap();
        assert!(verify_build(&root.join("artifact")).is_err());
    }
    #[test]
    fn independent_six_portfolio_returns_neutral_missing_only_blocks_smb() {
        let mut p = BTreeMap::new();
        for (k, v) in PORTFOLIOS
            .into_iter()
            .zip([0.01, 0.02, 0.03, 0.04, 0.05, 0.06])
        {
            p.insert(k.to_string(), Some(v));
        }
        let (s, h) = combine(&p);
        assert!((s.unwrap() + 0.03).abs() < 1e-14);
        assert!((h.unwrap() - 0.02).abs() < 1e-14);
        p.insert("SN".into(), None);
        let (s, h) = combine(&p);
        assert!(s.is_none());
        assert!((h.unwrap() - 0.02).abs() < 1e-14);
        assert_eq!(
            portfolio_return(&json!({"members":2,"missing":1,"cap_sum":100.,"return_sum":1.}))
                .unwrap(),
            None
        );
        assert!((quantile(vec![1., 2., 3., 4.], 0.3) - 1.9).abs() < 1e-14);
    }
}
