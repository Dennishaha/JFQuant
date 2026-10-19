//! Fixed full-universe market projection. Policy choices are explicit at local construction.
use super::{cached, flight, invalid, schema};
use crate::three_factor::{Observation, ThreeFactorReturns};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io, path::Path};

/// Algorithm identity: provider-universe A shares, lagged total capitalization, adjusted simple returns.
pub const VERSION: &str = "bigquant-cn-market-lagcap-v4";
/// Exact constructor/reader implementation frozen in published factor artifacts.
pub fn implementation_signature() -> String {
    let mut h = Sha256::new();
    for b in [
        include_bytes!("market.rs").as_slice(),
        include_bytes!("market.sql").as_slice(),
        include_bytes!("../three_factor.rs").as_slice(),
    ] {
        h.update(b);
        h.update([0]);
    }
    format!("sha256:{:x}", analytics_core::hash::Hex(h.finalize()))
}
const TABLES: [(&str, &[&str]); 5] = [
    ("cn_stock_instruments", &["date", "instrument"]),
    (
        "cn_stock_real_bar1d",
        &[
            "date",
            "instrument",
            "close",
            "adjust_factor",
            "pre_close",
            "change_ratio",
        ],
    ),
    ("cn_stock_shares", &["date", "instrument", "total_shares"]),
    (
        "cn_stock_status",
        &["date", "instrument", "suspended", "exdr"],
    ),
    (
        "cn_stock_basic_info",
        &["instrument", "security_type", "list_date"],
    ),
];
const COLUMNS: [&str; 18] = [
    "date",
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
const ISSUE_COLUMNS: [&str; 31] = [
    "date",
    "instrument",
    "close",
    "factor",
    "shares",
    "suspended",
    "previous_day",
    "market_day",
    "observed_close",
    "observed_factor",
    "observed_day",
    "blocked_day",
    "marked_close",
    "prior_close",
    "prior_mark",
    "prior_factor",
    "prior_shares",
    "strict_cap",
    "marked_cap",
    "basic_rows",
    "universe_rows",
    "price_rows",
    "share_rows",
    "status_rows",
    "security_type",
    "starting_close",
    "pre_close",
    "change_ratio",
    "exdr",
    "reported_close",
    "previous_mark",
];
fn query(request: &Request, issues: bool) -> io::Result<String> {
    let sql = request.sql()?;
    if !issues {
        return Ok(sql);
    }
    let (prefix, _) = sql
        .rsplit_once("SELECT date,\n  count(*) AS universe_members")
        .ok_or_else(|| invalid("bigquant_market_query_identity"))?;
    Ok(format!(
        "{prefix}SELECT {} FROM terms QUALIFY row_number() OVER ()>0 AND date=max(date) OVER () ORDER BY date,instrument LIMIT 10001",
        ISSUE_COLUMNS.join(",")
    ))
}
/// Inclusive output dates. Supporting observations are read only from 2005-01-04 onward.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Inclusive first output date.
    pub start: NaiveDate,
    /// Inclusive last output date.
    pub end: NaiveDate,
}
impl Request {
    /// Fixed SQL for independent fixture verification; no caller-supplied SQL reaches the SDK.
    pub fn sql(&self) -> io::Result<String> {
        if self.start < NaiveDate::from_ymd_opt(2005, 1, 4).unwrap() || self.start > self.end {
            return Err(invalid("bigquant_market_date_range_invalid"));
        }
        Ok(include_str!("market.sql")
            // Keep the window context intact; filter the requested output dates in Rust.
            .replace("{{start}}", "2005-01-04")
            .replace("{{end}}", &self.end.to_string()))
    }
}
fn hash(v: &Value) -> io::Result<String> {
    Ok(format!(
        "{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(v)?))
    ))
}
fn read(python: &Path, request: &Request, issues: bool) -> io::Result<Value> {
    let sql = query(request, issues)?;
    let columns = if issues {
        ISSUE_COLUMNS.len()
    } else {
        COLUMNS.len()
    };
    let mut metadata = BTreeMap::new();
    for (table, required) in TABLES {
        let m = schema(table)?;
        let fields = m["metadata"]["schema"]
            .as_object()
            .ok_or_else(|| invalid("bigquant_schema_missing"))?;
        if required.iter().any(|c| !fields.contains_key(*c)) {
            return Err(invalid("bigquant_market_column_missing"));
        }
        metadata.insert(table, m);
    }
    let result = flight(
        python,
        json!({"operation":"read","table":TABLES[0].0,
        "tables":TABLES.iter().map(|t|t.0).collect::<Vec<_>>(),"column_count":columns,"max_rows":10001,
        "sql":sql,"filters":{"date":["2005-01-04",request.end.to_string()]}}),
    )?;
    let rows = result["rows"]
        .as_array()
        .ok_or_else(|| invalid("bigquant_market_rows_invalid"))?;
    let payload = json!({"projection_version":VERSION,"request":request,"sql":sql,"metadata":metadata,
        "rows":rows,"arrow_schema":result["arrow_schema"],"allowance":result["allowance"]});
    Ok(
        json!({"schema_version":3,"provider":"bigquant","transport":"official_python_arrow_flight",
        "observed_at":chrono::Utc::now().to_rfc3339(),"complete":rows.len()<=10000,
        "research_admitted":false,"sha256":hash(&payload)?,"payload":payload}),
    )
}
/// Validate evidence identity and shape without querying the provider.
pub fn verify(value: &Value, request: &Request) -> io::Result<()> {
    verify_query(value, request, false)
}
fn verify_query(value: &Value, request: &Request, issues: bool) -> io::Result<()> {
    let payload = &value["payload"];
    let rows = payload["rows"]
        .as_array()
        .ok_or_else(|| invalid("bigquant_market_rows_invalid"))?;
    if value["schema_version"] != 3
        || value["provider"] != "bigquant"
        || value["transport"] != "official_python_arrow_flight"
        || value["research_admitted"] != false
        || !value["complete"].is_boolean()
        || rows.len() > 10001
        || value["observed_at"]
            .as_str()
            .is_none_or(|s| chrono::DateTime::parse_from_rfc3339(s).is_err())
        || value["sha256"] != hash(payload)?
        || payload["projection_version"] != VERSION
        || payload["request"] != serde_json::to_value(request)?
        || payload["sql"] != query(request, issues)?
        || !payload["arrow_schema"].is_string()
        || TABLES.iter().any(|t| payload["metadata"][t.0]["id"] != t.0)
        || rows.iter().any(|r| {
            r.as_object().is_none_or(|o| {
                let columns: &[&str] = if issues { &ISSUE_COLUMNS } else { &COLUMNS };
                o.len() != columns.len() || columns.iter().any(|c| !o.contains_key(*c))
            })
        })
    {
        return Err(invalid("bigquant_market_evidence_mismatch"));
    }
    Ok(())
}
/// Save immutable full-market sufficient statistics, with both valuation diagnostics.
pub fn fetch(python: &Path, request: &Request, output: &Path) -> io::Result<(Value, bool)> {
    request.sql()?;
    cached(
        output,
        || read(python, request, false),
        |v| verify(v, request),
    )
}
/// Capture all members on the last source date, retaining window context for diagnostics.
pub fn issues(python: &Path, request: &Request, output: &Path) -> io::Result<(Value, bool)> {
    query(request, true)?;
    cached(
        output,
        || read(python, request, true),
        |v| verify_query(v, request, true),
    )
}
/// Treatment of confirmed suspensions. Unknown quotes always remain missing.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspensionPolicy {
    /// Every actual quote must be present; no valuation carry.
    Strict,
    /// Carry a prior observed raw close only through confirmed suspensions and unchanged factors.
    ConfirmedUnchangedFactor,
}
fn number(v: &Value) -> io::Result<Option<f64>> {
    if v.is_null() {
        return Ok(None);
    }
    let n = v
        .as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| invalid("bigquant_market_number_invalid"))?;
    if !n.is_finite() {
        return Err(invalid("bigquant_market_number_nonfinite"));
    }
    Ok(Some(n))
}
fn count(row: &Value, name: &str) -> io::Result<u64> {
    row[name]
        .as_u64()
        .or_else(|| row[name].as_str().and_then(|s| s.parse().ok()))
        .ok_or_else(|| invalid("bigquant_market_count_invalid"))
}
/// Calculate a nullable market return without reweighting around any missing constituent.
pub fn return_on(row: &Value, policy: SuspensionPolicy) -> io::Result<Option<f64>> {
    let key = match policy {
        SuspensionPolicy::Strict => "strict",
        SuspensionPolicy::ConfirmedUnchangedFactor => "marked",
    };
    let eligible = count(row, "eligible_members")?;
    let missing = count(row, &format!("{key}_missing"))?;
    if missing > eligible
        || count(row, "a_share_members")? > count(row, "universe_members")?
        || eligible > count(row, "a_share_members")?
    {
        return Err(invalid("bigquant_market_counts_inconsistent"));
    }
    let cap = number(&row[format!("{key}_cap_sum")])?;
    let numerator = number(&row[format!("{key}_return_sum")])?;
    if eligible == 0
        || missing > 0
        || count(row, "unclassified_members")? > 0
        || count(row, "duplicate_keys")? > 0
    {
        return Ok(None);
    }
    match (cap, numerator) {
        (Some(c), Some(n)) if c > 0. && (n / c).is_finite() => Ok(Some(n / c)),
        _ => Err(invalid("bigquant_market_aggregate_missing_or_invalid")),
    }
}
/// Construct the normal three-factor sidecar; SMB/HML stay null until separately supported.
pub fn calculate(
    evidence: &Value,
    request: &Request,
    policy: SuspensionPolicy,
) -> io::Result<(ThreeFactorReturns, Value)> {
    verify(evidence, request)?;
    if evidence["complete"] != true {
        return Err(invalid("bigquant_market_extract_truncated"));
    }
    let mut observations = Vec::new();
    let mut prior = None;
    let mut nonnull = 0;
    let mut context_initial_references = 0u64;
    let mut marked_member_days = 0u64;
    let mut prior_marked_member_days = 0u64;
    let mut marked_days = 0u64;
    let mut reported_reference_marks = 0u64;
    let mut prior_reference_restores = 0u64;
    for row in evidence["payload"]["rows"].as_array().unwrap() {
        let s = row["date"]
            .as_str()
            .ok_or_else(|| invalid("bigquant_market_date_invalid"))?;
        if s.len() != 19 || !s.ends_with("T00:00:00") {
            return Err(invalid("bigquant_market_date_invalid"));
        }
        let date = NaiveDate::parse_from_str(&s[..10], "%Y-%m-%d")
            .map_err(|_| invalid("bigquant_market_date_invalid"))?;
        if date < NaiveDate::from_ymd_opt(2005, 1, 4).unwrap()
            || date > request.end
            || prior.is_some_and(|d| date <= d)
        {
            return Err(invalid("bigquant_market_date_sequence_invalid"));
        }
        context_initial_references += count(row, "initial_reference_marks")?;
        prior = Some(date);
        if date < request.start {
            continue;
        }
        let mkt = return_on(row, policy)?;
        reported_reference_marks += count(row, "reported_reference_marks")?;
        prior_reference_restores += count(row, "prior_reference_restores")?;
        let marks = count(row, "suspended_marks")?;
        let prior_marks = count(row, "prior_suspended_marks")?;
        marked_member_days += marks;
        prior_marked_member_days += prior_marks;
        marked_days +=
            u64::from(marks > 0 || prior_marks > 0 || count(row, "prior_reference_restores")? > 0);
        nonnull += usize::from(mkt.is_some());
        observations.push(Observation {
            trade_date: date,
            mkt,
            smb: None,
            hml: None,
        });
    }
    if observations.is_empty() {
        return Err(invalid("bigquant_market_empty"));
    }
    let factors=ThreeFactorReturns{schema_version:1,source:format!("{VERSION}; BigQuant evidence={}; shares=provider_dates; share_revision_vintage_verified=false",evidence["sha256"]),
        market:"BigQuant dated cn_stock_instruments A-share records; CDR excluded by provider security_type; historical code/revision risk retained".into(),
        return_basis:format!("market_return (RF not subtracted); adjusted-close simple return; prior-session raw valuation * total shares; suspension_policy={policy:?}; confirmed suspension carry requires unchanged adjustment factor; provider pre_close references require exdr=0, with change_ratio=0 for current suspended marks; initial seed and prior-reference restoration require prior listing; factor-changing suspended dates and unexplained gaps remain null; all eligible members required"),
        unit:"decimal".into(),observations};
    factors.validate()?;
    let report = json!({"schema_version":1,"construction_version":VERSION,"evidence_sha256":evidence["sha256"],"suspension_policy":policy,
        "shares_availability":"provider_dates","shares_revision_risk":true,"market_basis":"market_return","rf_subtracted":false,
        "mkt_nonnull_days":nonnull,"days":factors.observations.len(),"complete_three_factor_days":0,
        "context_initial_reference_members":context_initial_references,
        "current_suspension_mark_member_days":marked_member_days,"prior_suspension_mark_member_days":prior_marked_member_days,
        "dates_with_suspension_marks":marked_days,
        "reported_reference_marks":reported_reference_marks,"prior_reference_restores":prior_reference_restores,
        "smb_hml_issue":"annual_common_equity_scope_or_null_semantics_unresolved","diagnostics":evidence["payload"]["rows"]});
    Ok((factors, report))
}
/// Immutable offline construction; output contains standard factors, diagnostics and source evidence.
pub fn build(input: &Path, output: &Path, policy: SuspensionPolicy) -> io::Result<Value> {
    let bytes = fs::read(input)?;
    let evidence: Value = serde_json::from_slice(&bytes)?;
    let request: Request = serde_json::from_value(evidence["payload"]["request"].clone())?;
    let (factors, report) = calculate(&evidence, &request, policy)?;
    if output.exists() {
        return Err(invalid("bigquant_factor_output_exists"));
    }
    let parent = output
        .parent()
        .ok_or_else(|| invalid("bigquant_factor_parent_missing"))?;
    fs::create_dir_all(parent)?;
    let lock = output.with_extension("build-lock");
    let _lock = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&lock)?;
    let stage = parent.join(format!(
        ".bigquant-build-{}-{}",
        std::process::id(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    let result = (|| -> io::Result<()> {
        fs::create_dir(&stage)?;
        fs::write(stage.join("evidence.json"), bytes)?;
        fs::write(
            stage.join(crate::three_factor::FILE_NAME),
            serde_json::to_vec_pretty(&factors)?,
        )?;
        fs::write(
            stage.join("report.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        let fields = json!([{"name":"mkt","type":"nullable_float64","unit":"decimal"},{"name":"smb","type":"nullable_float64","unit":"decimal"},{"name":"hml","type":"nullable_float64","unit":"decimal"}]);
        let manifest = json!({"schema_version":1,"construction_version":VERSION,"implementation_sha256":implementation_signature(),
            "sdk":{"bigquant":"0.1.11","bigquant_core":"0.1.14"},"suspension_policy":policy,"range":request,
            "fields":fields,"schema_sha256":hash(&fields)?,"evidence_file_sha256":crate::market_view::file_hash(&stage.join("evidence.json"))?,
            "factor_file_sha256":crate::market_view::file_hash(&stage.join(crate::three_factor::FILE_NAME))?,
            "report_sha256":crate::market_view::file_hash(&stage.join("report.json"))?,"mkt_nonnull_days":report["mkt_nonnull_days"],"days":report["days"],
            "shares_revision_risk":true,"full_three_factor_ready":false});
        fs::write(
            stage.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;
        verify_build(&stage)?;
        if output.exists() {
            return Err(invalid("bigquant_factor_output_exists"));
        }
        fs::rename(&stage, output)
    })();
    if result.is_err() && stage.exists() {
        fs::remove_dir_all(&stage)?;
    }
    fs::remove_file(lock)?;
    result?;
    Ok(
        json!({"output":output,"mkt_nonnull_days":report["mkt_nonnull_days"],"days":report["days"],"smb_hml_ready":false}),
    )
}
/// Recheck every artifact hash and replay construction before workflow publication.
pub fn verify_build(root: &Path) -> io::Result<(ThreeFactorReturns, Value)> {
    let manifest: Value = serde_json::from_slice(&fs::read(root.join("manifest.json"))?)?;
    let evidence: Value = serde_json::from_slice(&fs::read(root.join("evidence.json"))?)?;
    let request: Request = serde_json::from_value(evidence["payload"]["request"].clone())?;
    let policy: SuspensionPolicy = serde_json::from_value(manifest["suspension_policy"].clone())?;
    if manifest["schema_version"] != 1
        || manifest["construction_version"] != VERSION
        || manifest["implementation_sha256"] != implementation_signature()
        || manifest["schema_sha256"] != hash(&manifest["fields"])?
        || manifest["fields"]
            != json!([{"name":"mkt","type":"nullable_float64","unit":"decimal"},{"name":"smb","type":"nullable_float64","unit":"decimal"},{"name":"hml","type":"nullable_float64","unit":"decimal"}])
        || manifest["shares_revision_risk"] != true
        || manifest["full_three_factor_ready"] != false
        || manifest["sdk"] != json!({"bigquant":"0.1.11","bigquant_core":"0.1.14"})
        || manifest["range"] != serde_json::to_value(&request)?
        || manifest["evidence_file_sha256"]
            != crate::market_view::file_hash(&root.join("evidence.json"))?
        || manifest["factor_file_sha256"]
            != crate::market_view::file_hash(&root.join(crate::three_factor::FILE_NAME))?
        || manifest["report_sha256"] != crate::market_view::file_hash(&root.join("report.json"))?
    {
        return Err(invalid("bigquant_factor_artifact_identity_or_hash"));
    }
    let (factors, report) = calculate(&evidence, &request, policy)?;
    let actual: Value =
        serde_json::from_slice(&fs::read(root.join(crate::three_factor::FILE_NAME))?)?;
    let actual_report: Value = serde_json::from_slice(&fs::read(root.join("report.json"))?)?;
    if actual != serde_json::to_value(&factors)?
        || report != actual_report
        || manifest["mkt_nonnull_days"] != report["mkt_nonnull_days"]
        || manifest["days"] != report["days"]
    {
        return Err(invalid("bigquant_factor_artifact_replay_mismatch"));
    }
    Ok((factors, manifest))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_cap_weighted_fixture_and_missing_propagation() {
        // Capitalizations 100 and 300, returns +10% and -2%: (10-6)/400 = 1%.
        let mut row = json!({"eligible_members":2,"a_share_members":2,"universe_members":2,"strict_missing":0,
            "marked_missing":0,"strict_cap_sum":400.,"strict_return_sum":4.,"marked_cap_sum":400.,"marked_return_sum":4.,
            "unclassified_members":0,"duplicate_keys":0});
        assert_eq!(
            return_on(&row, SuspensionPolicy::Strict).unwrap(),
            Some(0.01)
        );
        row["strict_missing"] = json!(1);
        assert_eq!(return_on(&row, SuspensionPolicy::Strict).unwrap(), None);
        assert_eq!(
            return_on(&row, SuspensionPolicy::ConfirmedUnchangedFactor).unwrap(),
            Some(0.01)
        );
        row["marked_missing"] = json!(1);
        assert_eq!(
            return_on(&row, SuspensionPolicy::ConfirmedUnchangedFactor).unwrap(),
            None
        );
        row["marked_missing"] = json!(3);
        assert!(return_on(&row, SuspensionPolicy::ConfirmedUnchangedFactor).is_err());
    }
}
