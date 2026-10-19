//! PIT ordinary-equity recovery from explicit vendor per-share metrics; unknown raw deductions stay null.
use super::{cached, flight, invalid, market, schema};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fs, io, path::Path};

/// June 2×3 construction with PIT vendor BPS and cancellation of its common share denominator.
pub const VERSION: &str = "bigquant-cn-ff3-june-bps-v1";
const TABLES: [&str; 7] = [
    "cn_stock_instruments",
    "cn_stock_real_bar1d",
    "cn_stock_shares",
    "cn_stock_status",
    "cn_stock_basic_info",
    "cn_stock_financial_balance_general_pit",
    "cn_stock_financial_xps",
];
const COLUMNS: [&str; 28] = [
    "formation_year",
    "formation_date",
    "december_date",
    "instrument",
    "security_type",
    "basic_rows",
    "june_universe_rows",
    "december_universe_rows",
    "june_close",
    "june_shares",
    "june_share_rows",
    "december_close",
    "december_shares",
    "december_share_rows",
    "raw_date",
    "report_date",
    "raw_keys",
    "parent_equity",
    "other_equity",
    "capital_reserves",
    "surplus_reserve",
    "undistributed_profit",
    "xps_date",
    "xps_keys",
    "bps_lf",
    "capital_reserves_ps_lf",
    "surplus_reserve_ps_lf",
    "undistributed_profit_ps_lf",
];

/// At most five consecutive June formations per immutable extraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Inclusive first formation year; supporting quotes begin in 2005 only.
    pub start_year: i32,
    /// Inclusive last formation year.
    pub end_year: i32,
}
impl Request {
    /// Fixed seven-table query. Window context is protected before formation-date joins.
    pub fn sql(&self) -> io::Result<String> {
        if self.start_year < 2005
            || self.end_year < self.start_year
            || self.end_year > 2100
            || self.end_year - self.start_year >= 5
        {
            return Err(invalid("bigquant_formation_year_range_invalid"));
        }
        let sql = market::Request {
            start: NaiveDate::from_ymd_opt(2005, 1, 4).unwrap(),
            end: NaiveDate::from_ymd_opt(self.end_year, 6, 30).unwrap(),
        }
        .sql()?;
        let (prefix, _) = sql
            .rsplit_once("SELECT date,\n  count(*) AS universe_members")
            .ok_or_else(|| invalid("bigquant_market_query_identity"))?;
        Ok(format!(
            "{prefix}{}",
            include_str!("size_value_formation.sql")
                .replace("{{start_year}}", &self.start_year.to_string())
                .replace("{{end_year}}", &self.end_year.to_string())
        ))
    }
}
fn hash(v: &Value) -> io::Result<String> {
    Ok(format!(
        "{:x}",
        analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(v)?))
    ))
}
fn verify(value: &Value, request: &Request) -> io::Result<()> {
    let p = &value["payload"];
    let rows = p["rows"]
        .as_array()
        .ok_or_else(|| invalid("bigquant_formation_rows_invalid"))?;
    if value["schema_version"] != 4
        || value["provider"] != "bigquant"
        || value["transport"] != "official_python_arrow_flight"
        || value["research_admitted"] != false
        || value["complete"] != json!(rows.len() <= 50000)
        || value["sha256"] != hash(p)?
        || p["projection_version"] != VERSION
        || p["request"] != serde_json::to_value(request)?
        || p["sql"] != request.sql()?
        || TABLES.iter().any(|t| p["metadata"][t]["id"] != *t)
        || rows.iter().any(|r| {
            r.as_object().is_none_or(|o| {
                o.len() != COLUMNS.len() || COLUMNS.iter().any(|c| !o.contains_key(*c))
            })
        })
    {
        return Err(invalid("bigquant_formation_evidence_mismatch"));
    }
    for row in rows {
        let year = row["formation_year"]
            .as_i64()
            .ok_or_else(|| invalid("formation_year_invalid"))? as i32;
        let formation = row["formation_date"]
            .as_str()
            .ok_or_else(|| invalid("formation_date_invalid"))?;
        let date = NaiveDate::parse_from_str(formation, "%Y-%m-%dT00:00:00")
            .map_err(|_| invalid("formation_date_invalid"))?;
        if year < request.start_year
            || year > request.end_year
            || date.format("%Y-%m").to_string() != format!("{year}-06")
        {
            return Err(invalid("formation_date_invalid"));
        }
        for key in ["raw_date", "xps_date"] {
            if row[key].as_str().is_some_and(|d| {
                d >= formation || NaiveDate::parse_from_str(d, "%Y-%m-%dT00:00:00").is_err()
            }) {
                return Err(invalid("formation_financial_vintage_invalid"));
            }
        }
        if !row["report_date"].is_null()
            && row["report_date"] != format!("{}-12-31T00:00:00", year - 1)
        {
            return Err(invalid("formation_report_period_invalid"));
        }
    }
    Ok(())
}
/// Save dated financial anchors and June/December valuation references, with explicit source NULLs.
pub fn fetch(python: &Path, request: &Request, output: &Path) -> io::Result<(Value, bool)> {
    let sql = request.sql()?;
    cached(
        output,
        || {
            let mut metadata = BTreeMap::new();
            for table in TABLES {
                metadata.insert(table, schema(table)?);
            }
            for (table, fields) in [
                (
                    "cn_stock_financial_balance_general_pit",
                    vec![
                        "date",
                        "instrument",
                        "report_date",
                        "total_equity_to_parent_shareholders",
                        "other_equity_instruments",
                        "capital_reserves",
                        "surplus_reserve",
                        "undistributed_profit",
                    ],
                ),
                (
                    "cn_stock_financial_xps",
                    vec![
                        "date",
                        "instrument",
                        "report_date",
                        "bps_lf",
                        "capital_reserves_ps_lf",
                        "surplus_reserve_ps_lf",
                        "undistributed_profit_ps_lf",
                    ],
                ),
            ] {
                if fields
                    .iter()
                    .any(|f| metadata[table]["metadata"]["schema"].get(*f).is_none())
                {
                    return Err(invalid("bigquant_formation_column_missing"));
                }
            }
            let result = flight(
                python,
                json!({"operation":"read","table":TABLES[0],"tables":TABLES,"column_count":COLUMNS.len(),"max_rows":50001,"sql":sql,"filters":{"date":["2005-01-04",format!("{}-06-30",request.end_year)]}}),
            )?;
            let rows = result["rows"]
                .as_array()
                .ok_or_else(|| invalid("bigquant_formation_rows_invalid"))?;
            let payload = json!({"projection_version":VERSION,"request":request,"sql":sql,"metadata":metadata,"rows":rows,"arrow_schema":result["arrow_schema"],"allowance":result["allowance"]});
            Ok(
                json!({"schema_version":4,"provider":"bigquant","transport":"official_python_arrow_flight","observed_at":chrono::Utc::now().to_rfc3339(),"complete":rows.len()<=50000,"research_admitted":false,"sha256":hash(&payload)?,"payload":payload}),
            )
        },
        |v| verify(v, request),
    )
}
fn num(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .filter(|n| n.is_finite())
}
fn near(a: f64, b: f64) -> bool {
    (a - b).abs() <= 1e-4f64.max(a.abs().max(b.abs()) * 1e-7)
}
/// Recover the explicitly reported BPS numerator using another metric with the same denominator.
/// No unknown financial item is assigned zero. Units follow the source monetary anchors (CNY).
pub fn common_equity(row: &Value) -> Result<(f64, Value), &'static str> {
    recover_equity(row, false)
}
fn recover_equity(row: &Value, equivalent_vintage: bool) -> Result<(f64, Value), &'static str> {
    let parent = num(&row["parent_equity"]).ok_or("parent_equity_unknown")?;
    let bps = num(&row["bps_lf"]).ok_or("provider_bps_unknown")?;
    if num(&row["raw_keys"]) != Some(1.) || num(&row["xps_keys"]) != Some(1.) {
        return Err("financial_source_keys_not_unique");
    }
    let raw = row["raw_date"].as_str().ok_or("raw_date_unknown")?;
    let xps = row["xps_date"].as_str().ok_or("xps_date_unknown")?;
    if xps < raw && !equivalent_vintage {
        return Err("bps_older_than_latest_balance_vintage");
    }
    let mut anchors = Vec::new();
    for (amount, per_share) in [
        ("capital_reserves", "capital_reserves_ps_lf"),
        ("surplus_reserve", "surplus_reserve_ps_lf"),
        ("undistributed_profit", "undistributed_profit_ps_lf"),
    ] {
        if let (Some(a), Some(p)) = (num(&row[amount]), num(&row[per_share])) {
            if a != 0. && p != 0. {
                let denominator = a / p;
                if denominator.is_finite() && denominator > 0. {
                    anchors.push((amount, denominator));
                }
            } else if a != p {
                return Err("per_share_anchor_zero_mismatch");
            }
        }
    }
    let (_, denominator) = *anchors
        .first()
        .ok_or("common_share_denominator_unresolved")?;
    if anchors.iter().any(|(_, d)| !near(denominator, *d)) {
        return Err("per_share_anchors_disagree");
    }
    let equity = bps * denominator;
    if !equity.is_finite() {
        return Err("common_equity_nonfinite");
    }
    if let Some(other) = num(&row["other_equity"])
        && !near(equity, parent - other)
    {
        return Err("provider_bps_disagrees_with_explicit_equity");
    }
    Ok((
        equity,
        json!({"source":"provider_bps_common_denominator_cancellation","bps":bps,"denominator":denominator,"anchors":anchors,"raw_other_equity_is_null":row["other_equity"].is_null(),"raw_date":raw,"xps_date":xps,"equivalent_vintage_verified":equivalent_vintage}),
    ))
}
/// Offline coverage analysis of one or more immutable formation extracts.
pub fn audit(inputs: &[impl AsRef<Path>]) -> io::Result<Value> {
    audit_supplemented(inputs, &[] as &[&Path])
}
/// Additional raw PIT balance evidence may prove that a later version leaves all BPS numerator/anchor fields unchanged.
pub fn audit_supplemented(
    inputs: &[impl AsRef<Path>],
    supplements: &[impl AsRef<Path>],
) -> io::Result<Value> {
    let mut years: BTreeMap<i32, Value> = BTreeMap::new();
    let mut keys = std::collections::BTreeSet::new();
    let mut evidence = Vec::new();
    let mut balances = Vec::new();
    for path in supplements {
        let v: Value = serde_json::from_slice(&fs::read(path.as_ref())?)?;
        let request: super::ReadRequest = serde_json::from_value(v["payload"]["request"].clone())?;
        super::verify_snapshot(&v, &request)?;
        if v["complete"] != true || request.table != "cn_stock_financial_balance_general_pit" {
            return Err(invalid("balance_supplement_invalid"));
        }
        for name in [
            "date",
            "instrument",
            "report_date",
            "total_equity_to_parent_shareholders",
            "other_equity_instruments",
            "capital_reserves",
            "surplus_reserve",
            "undistributed_profit",
        ] {
            if !request.columns.iter().any(|c| c == name) {
                return Err(invalid("balance_supplement_column_missing"));
            }
        }
        evidence
            .push(json!({"path":path.as_ref(),"sha256":v["sha256"],"kind":"balance_supplement"}));
        balances.extend(v["payload"]["rows"].as_array().unwrap().iter().cloned());
    }
    for input in inputs {
        let v: Value = serde_json::from_slice(&fs::read(input.as_ref())?)?;
        let request: Request = serde_json::from_value(v["payload"]["request"].clone())?;
        verify(&v, &request)?;
        if v["complete"] != true {
            return Err(invalid("bigquant_formation_truncated"));
        }
        evidence.push(json!({"path":input.as_ref(),"sha256":v["sha256"]}));
        for row in v["payload"]["rows"].as_array().unwrap() {
            let year = row["formation_year"]
                .as_i64()
                .ok_or_else(|| invalid("formation_year_invalid"))? as i32;
            let symbol = row["instrument"]
                .as_str()
                .ok_or_else(|| invalid("formation_symbol_invalid"))?;
            if !keys.insert((year, symbol.to_string())) {
                return Err(invalid("overlapping_formation_evidence"));
            }
            let item=years.entry(year).or_insert_with(||json!({"year":year,"formation_date":row["formation_date"],"december_date":row["december_date"],"counts":{},"members":[]}));
            let issue = if row["security_type"].is_null() || num(&row["basic_rows"]) != Some(1.) {
                Some("unclassified_member")
            } else if !row["security_type"]
                .as_str()
                .is_some_and(|s| s.ends_with("A股"))
            {
                Some("non_a_share_excluded")
            } else if row["december_date"].is_null() {
                Some("prior_december_calendar_missing")
            } else if row["december_universe_rows"].is_null() {
                Some("not_listed_at_prior_december")
            } else {
                None
            };
            let mut output = row.clone();
            let category = if let Some(issue) = issue {
                output["issue"] = json!(issue);
                issue
            } else {
                let prior_balance = balances
                    .iter()
                    .filter(|b| {
                        b["instrument"] == row["instrument"]
                            && b["report_date"] == row["report_date"]
                            && b["date"]
                                .as_str()
                                .zip(row["xps_date"].as_str())
                                .is_some_and(|(a, b)| a <= b)
                    })
                    .max_by_key(|b| b["date"].as_str().unwrap());
                let equivalent = prior_balance.is_some_and(|b| {
                    [
                        ("parent_equity", "total_equity_to_parent_shareholders"),
                        ("other_equity", "other_equity_instruments"),
                        ("capital_reserves", "capital_reserves"),
                        ("surplus_reserve", "surplus_reserve"),
                        ("undistributed_profit", "undistributed_profit"),
                    ]
                    .iter()
                    .all(|(a, c)| row[a] == b[c])
                });
                if equivalent {
                    output["equivalent_balance_vintage"] = prior_balance.cloned().unwrap();
                }
                match recover_equity(row, equivalent) {
                    Ok((equity, provenance)) => {
                        output["common_equity_cny"] = json!(equity);
                        output["common_equity_provenance"] = provenance;
                        if equity <= 0. {
                            output["issue"] = json!("nonpositive_common_equity");
                            "nonpositive_common_equity"
                        } else if [
                            "june_close",
                            "june_shares",
                            "december_close",
                            "december_shares",
                        ]
                        .iter()
                        .any(|k| num(&row[k]).is_none_or(|v| v <= 0.))
                        {
                            output["issue"] = json!("formation_valuation_missing");
                            "formation_valuation_missing"
                        } else {
                            output["issue"] = Value::Null;
                            "positive_common_equity_and_caps"
                        }
                    }
                    Err(issue) => {
                        output["issue"] = json!(issue);
                        issue
                    }
                }
            };
            let counts = item["counts"].as_object_mut().unwrap();
            let n = counts.get(category).and_then(Value::as_u64).unwrap_or(0);
            counts.insert(category.into(), json!(n + 1));
            item["members"].as_array_mut().unwrap().push(output);
        }
    }
    Ok(
        json!({"schema_version":1,"construction_version":VERSION,"shares_revision_risk":true,"unknown_equity_replaced_with_zero":false,"sources":evidence,"formations":years.into_values().collect::<Vec<_>>()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn independent_common_denominator_cancels_without_imputing_unknown_deductions() {
        let mut row = json!({"parent_equity":100.,"other_equity":null,"bps_lf":8.,"capital_reserves":20.,"capital_reserves_ps_lf":2.,"surplus_reserve":40.,"surplus_reserve_ps_lf":4.,"undistributed_profit":-10.,"undistributed_profit_ps_lf":-1.,"raw_keys":1,"xps_keys":1,"raw_date":"2026-04-20T00:00:00","xps_date":"2026-04-20T00:00:00"});
        // 20 / 2 = 40 / 4 = -10 / -1 = 10 shares; 8 * 10 = 80 CNY.
        assert_eq!(common_equity(&row).unwrap().0, 80.);
        assert!(row["other_equity"].is_null());
        row["other_equity"] = json!(20.);
        assert_eq!(common_equity(&row).unwrap().0, 80.);
        row["other_equity"] = json!(10.);
        assert_eq!(
            common_equity(&row).unwrap_err(),
            "provider_bps_disagrees_with_explicit_equity"
        );
        row["other_equity"] = Value::Null;
        row["surplus_reserve_ps_lf"] = json!(2.);
        assert_eq!(
            common_equity(&row).unwrap_err(),
            "per_share_anchors_disagree"
        );
        row["surplus_reserve_ps_lf"] = json!(4.);
        row["xps_date"] = json!("2026-04-19T00:00:00");
        assert_eq!(
            common_equity(&row).unwrap_err(),
            "bps_older_than_latest_balance_vintage"
        );
        row["xps_date"] = json!("2026-04-20T00:00:00");
        row["bps_lf"] = Value::Null;
        assert_eq!(common_equity(&row).unwrap_err(), "provider_bps_unknown");
    }
}
