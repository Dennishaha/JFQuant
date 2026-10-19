//! Fixed historical projections. Run encoding preserves NULL and gaps on the source date axis.
use super::{cached, flight, invalid, schema};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{io, path::Path};
/// Identity of the SQL projection, included in evidence and cache verification.
pub const VERSION: &str = "bigquant-historical-projections-v1";
/// Purpose-specific projections, with a fixed field list and no caller-supplied SQL.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Every December 31 balance-sheet vintage, with parent equity and capital deductions.
    AnnualEquity,
    /// Constant total-share runs, split on value changes and source-calendar gaps.
    ShareRuns,
    /// Explicit observed dates for decoding share runs; never inferred from weekdays.
    ShareCalendar,
    /// Security-membership runs, split on missing source-calendar dates.
    UniverseRuns,
    /// Explicit source dates for decoding membership runs.
    UniverseCalendar,
    /// Unadjusted close and the explicitly returned provider adjustment field.
    PriceFactors,
    /// Raw OHLC, actual volume/amount and the provider adjustment field.
    PriceBars,
    /// Explicit full-day suspension flags; a missing quote is never assumed to be a suspension.
    Suspensions,
}
impl Kind {
    fn table(self) -> &'static str {
        match self {
            Self::AnnualEquity => "cn_stock_financial_balance_general_pit",
            Self::ShareRuns | Self::ShareCalendar => "cn_stock_shares",
            Self::UniverseRuns | Self::UniverseCalendar => "cn_stock_instruments",
            Self::PriceFactors | Self::PriceBars => "cn_stock_real_bar1d",
            Self::Suspensions => "cn_stock_status",
        }
    }
    fn source_columns(self) -> &'static [&'static str] {
        match self {
            Self::AnnualEquity => &[
                "date",
                "instrument",
                "report_date",
                "change_type",
                "total_equity_to_parent_shareholders",
                "other_equity_instruments",
                "preference_of_other_equity_instruments",
                "preference_shares",
                "perpetual_bonds",
            ],
            Self::ShareRuns => &["date", "instrument", "total_shares"],
            Self::UniverseRuns => &["date", "instrument"],
            Self::ShareCalendar | Self::UniverseCalendar => &["date"],
            Self::PriceFactors => &["date", "instrument", "close", "adjust_factor"],
            Self::PriceBars => &[
                "date",
                "instrument",
                "open",
                "high",
                "low",
                "close",
                "volume",
                "amount",
                "adjust_factor",
            ],
            Self::Suspensions => &["date", "instrument", "suspended"],
        }
    }
    fn output_columns(self) -> &'static [&'static str] {
        match self {
            Self::ShareRuns => &[
                "instrument",
                "start_date",
                "end_date",
                "observations",
                "distinct_dates",
                "total_shares",
            ],
            Self::UniverseRuns => &[
                "instrument",
                "start_date",
                "end_date",
                "observations",
                "distinct_dates",
            ],
            _ => self.source_columns(),
        }
    }
}
/// Explicit range and optional exact symbols. A page is limited to 50,000 returned rows.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    /// Fixed projection name.
    pub kind: Kind,
    /// Inclusive source date lower bound.
    pub start: String,
    /// Inclusive upper bound.
    pub end: String,
    /// BigQuant symbols; empty requests the complete source table within the date range.
    pub instruments: Vec<String>,
    /// Returned row bound; one extra row detects truncation.
    pub limit: usize,
}
impl Request {
    fn sql(&self) -> io::Result<String> {
        let start = NaiveDate::parse_from_str(&self.start, "%Y-%m-%d")
            .map_err(|_| invalid("bigquant_date_invalid"))?;
        let end = NaiveDate::parse_from_str(&self.end, "%Y-%m-%d")
            .map_err(|_| invalid("bigquant_date_invalid"))?;
        if start > end
            || start.to_string() != self.start
            || end.to_string() != self.end
            || !(1..=50000).contains(&self.limit)
            || self.instruments.len() > 1000
            || self
                .instruments
                .iter()
                .any(|s| s.is_empty() || !s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.'))
            || (matches!(self.kind, Kind::ShareCalendar | Kind::UniverseCalendar)
                && !self.instruments.is_empty())
        {
            return Err(invalid("bigquant_history_request_invalid"));
        }
        let scope = if self.instruments.is_empty() {
            String::new()
        } else {
            format!(
                " WHERE instrument IN ({})",
                self.instruments
                    .iter()
                    .map(|s| format!("'{s}'"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        let base = format!(
            "SELECT {} FROM {} WHERE date >= '{}' AND date < DATE '{}' + INTERVAL 1 DAY",
            self.kind.source_columns().join(","),
            self.kind.table(),
            self.start,
            self.end
        );
        let limit = self.limit + 1;
        let sql = match self.kind {
            Kind::AnnualEquity => format!(
                "WITH source AS ({base}) SELECT * FROM source{} EXTRACT(MONTH FROM report_date)=12 AND EXTRACT(DAY FROM report_date)=31 ORDER BY date,instrument,report_date,change_type LIMIT {limit}",
                if scope.is_empty() {
                    " WHERE ".into()
                } else {
                    format!("{scope} AND ")
                }
            ),
            Kind::ShareCalendar | Kind::UniverseCalendar => format!(
                "WITH source AS ({base}) SELECT DISTINCT date FROM source ORDER BY date LIMIT {limit}"
            ),
            Kind::PriceFactors | Kind::PriceBars => format!(
                "WITH source AS ({base}) SELECT * FROM source{scope} ORDER BY instrument,date LIMIT {limit}"
            ),
            Kind::Suspensions => format!(
                "WITH source AS ({base}) SELECT * FROM source{} suspended=1 ORDER BY instrument,date LIMIT {limit}",
                if scope.is_empty() {
                    " WHERE ".into()
                } else {
                    format!("{scope} AND ")
                }
            ),
            Kind::ShareRuns | Kind::UniverseRuns => {
                let change = if self.kind == Kind::ShareRuns {
                    "total_shares IS DISTINCT FROM lag(total_shares) OVER w OR "
                } else {
                    ""
                };
                let value = if self.kind == Kind::ShareRuns {
                    ",min(total_shares) AS total_shares"
                } else {
                    ""
                };
                // Rank all source dates before symbol filtering, so a security-specific gap splits its run.
                format!(
                    "WITH source AS ({base}), dated AS (SELECT *,dense_rank() OVER (ORDER BY date) AS market_day FROM source), scoped AS (SELECT * FROM dated{scope}), marked AS (SELECT *,CASE WHEN {change}market_day IS DISTINCT FROM lag(market_day) OVER w + 1 THEN 1 ELSE 0 END AS new_run FROM scoped WINDOW w AS (PARTITION BY instrument ORDER BY date)), grouped AS (SELECT *,sum(new_run) OVER (PARTITION BY instrument ORDER BY date ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS run FROM marked) SELECT instrument,min(date) AS start_date,max(date) AS end_date,count(*) AS observations,count(DISTINCT date) AS distinct_dates{value} FROM grouped GROUP BY instrument,run ORDER BY instrument,start_date,end_date LIMIT {limit}"
                )
            }
        };
        Ok(sql)
    }
}
fn hash(value: &Value) -> io::Result<String> {
    Ok(Sha256::digest(serde_json::to_vec(value)?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}
fn read(python: &Path, request: &Request) -> io::Result<Value> {
    let sql = request.sql()?;
    let metadata = schema(request.kind.table())?;
    let fields = metadata["metadata"]["schema"]
        .as_object()
        .ok_or_else(|| invalid("bigquant_schema_missing"))?;
    if request
        .kind
        .source_columns()
        .iter()
        .any(|c| !fields.contains_key(*c))
    {
        return Err(invalid("bigquant_column_missing"));
    }
    let result = flight(
        python,
        json!({"operation":"read","table":request.kind.table(),"column_count":request.kind.output_columns().len(),"max_rows":request.limit+1,"sql":sql,"filters":{"date":[request.start,request.end]}}),
    )?;
    let mut rows = result["rows"]
        .as_array()
        .ok_or_else(|| invalid("bigquant_rows_invalid"))?
        .clone();
    let complete = rows.len() <= request.limit;
    rows.truncate(request.limit);
    let payload = json!({"projection_version":VERSION,"request":request,"sql":sql,"metadata":metadata,"arrow_schema":result["arrow_schema"],"allowance":result["allowance"],"rows":rows});
    Ok(
        json!({"schema_version":2,"provider":"bigquant","transport":"official_python_arrow_flight","sdk":{"bigquant":"0.1.11","bigquant_core":"0.1.14"},"observed_at":chrono::Utc::now().to_rfc3339(),"complete":complete,"research_admitted":false,"sha256":hash(&payload)?,"payload":payload}),
    )
}
/// Verify the exact query, version and hash; this does not certify accounting NULL semantics.
pub fn verify(value: &Value, request: &Request) -> io::Result<()> {
    let sql = request.sql()?;
    let payload = &value["payload"];
    let rows = payload["rows"]
        .as_array()
        .ok_or_else(|| invalid("bigquant_history_cache_invalid"))?;
    if value["schema_version"] != 2
        || value["provider"] != "bigquant"
        || value["transport"] != "official_python_arrow_flight"
        || value["sdk"] != json!({"bigquant":"0.1.11","bigquant_core":"0.1.14"})
        || !value["complete"].is_boolean()
        || value["research_admitted"] != false
        || value["observed_at"]
            .as_str()
            .is_none_or(|s| chrono::DateTime::parse_from_rfc3339(s).is_err())
        || value["sha256"] != hash(payload)?
        || payload["projection_version"] != VERSION
        || payload["request"] != serde_json::to_value(request)?
        || payload["sql"] != sql
        || payload["metadata"]["id"] != request.kind.table()
        || !payload["arrow_schema"].is_string()
        || rows.len() > request.limit
        || rows.iter().any(|r| {
            r.as_object().is_none_or(|o| {
                o.len() != request.kind.output_columns().len()
                    || request
                        .kind
                        .output_columns()
                        .iter()
                        .any(|c| !o.contains_key(*c))
            })
        })
    {
        return Err(invalid("bigquant_history_cache_mismatch"));
    }
    Ok(())
}
/// Save or reuse a hashed immutable extract, without registering it as research-ready input.
pub fn fetch(python: &Path, request: &Request, output: &Path) -> io::Result<(Value, bool)> {
    request.sql()?;
    cached(output, || read(python, request), |v| verify(v, request))
}
/// Decode source calendar positions for a run, rejecting duplicate days and hidden holes.
pub fn run_indices(
    calendar: &[NaiveDate],
    row: &Value,
) -> io::Result<std::ops::RangeInclusive<usize>> {
    if calendar.windows(2).any(|p| p[0] >= p[1]) {
        return Err(invalid("bigquant_calendar_invalid"));
    }
    let date = |name: &str| -> io::Result<NaiveDate> {
        let s = row[name]
            .as_str()
            .ok_or_else(|| invalid("bigquant_run_date_invalid"))?;
        if s.len() != 19 || !s.ends_with("T00:00:00") {
            return Err(invalid("bigquant_run_date_invalid"));
        }
        NaiveDate::parse_from_str(&s[..10], "%Y-%m-%d")
            .map_err(|_| invalid("bigquant_run_date_invalid"))
    };
    let start = calendar
        .binary_search(&date("start_date")?)
        .map_err(|_| invalid("bigquant_run_off_calendar"))?;
    let end = calendar
        .binary_search(&date("end_date")?)
        .map_err(|_| invalid("bigquant_run_off_calendar"))?;
    if start > end
        || row["observations"].as_u64() != Some((end - start + 1) as u64)
        || row["distinct_dates"] != row["observations"]
    {
        return Err(invalid("bigquant_run_gap_or_duplicate"));
    }
    Ok(start..=end)
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn fixed_projection_and_lossless_run_validation() {
        let r = Request {
            kind: Kind::AnnualEquity,
            start: "2020-01-01".into(),
            end: "2021-01-01".into(),
            instruments: vec![],
            limit: 50000,
        };
        assert!(
            r.sql()
                .unwrap()
                .contains("EXTRACT(MONTH FROM report_date)=12")
        );
        let suspension = Request {
            kind: Kind::Suspensions,
            ..r.clone()
        };
        assert!(
            suspension
                .sql()
                .unwrap()
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
                .contains("WHERE suspended=1 ORDER BY instrument,date LIMIT 50001")
        );
        let calendar = ["2020-01-02", "2020-01-03", "2020-01-06"]
            .map(|s| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap());
        let mut row = json!({"start_date":"2020-01-02T00:00:00","end_date":"2020-01-06T00:00:00","observations":3,"distinct_dates":3,"total_shares":null});
        assert_eq!(run_indices(&calendar, &row).unwrap(), 0..=2);
        assert!(row["total_shares"].is_null());
        row["observations"] = json!(2);
        row["distinct_dates"] = json!(2);
        assert!(run_indices(&calendar, &row).is_err());
        row["observations"] = json!(3);
        row["distinct_dates"] = json!(2);
        assert!(run_indices(&calendar, &row).is_err());
        let mut bad = r;
        bad.instruments = vec!["x');delete".into()];
        assert!(bad.sql().is_err());
    }
}
