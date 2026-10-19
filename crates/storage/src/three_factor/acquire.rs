//! Native-evidence normalization for dated capital snapshots and annual common equity.
use super::{build::BuildRequest, invalid};
use crate::collector::CollectionRequest;
use analytics_core::Date;
use chrono::Datelike;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
};

/// Independent normalization identity, included in queued jobs and output provenance.
pub const VERSION: &str = "tdx-native-capital-common-equity-v1";
const FIELDS: [&str; 5] = ["FN004", "FN238", "FN271", "FN428", "FN429"];

/// Treatment of provider history whose original revision vintage is unavailable.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    /// Conservative default: observations become available after the actual capture date.
    #[default]
    ObservedOn,
    /// Explicit research assumption: use provider announcement dates / historical closing shares.
    /// This does not certify that today's financial revisions existed on those dates.
    ProviderDates,
}
/// One exact-date capital snapshot. It is never carried across missing dates.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShareAtClose {
    /// Date of the capital structure returned by the source.
    pub trade_date: Date,
    /// Actual shares, not ten-thousand shares.
    pub total_shares: Option<f64>,
}
/// One annual common-equity candidate, consistent with the source's per-share equity.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Annual {
    /// December 31 report period end.
    pub period_end: Date,
    /// Provider-reported announcement date; revision vintage remains unverified.
    pub reported_on: Date,
    /// Parent equity less explicitly reported preferred/perpetual equity, in CNY.
    pub book_equity_cny: Option<f64>,
}
/// Data-quality issue retained even when a numeric candidate is null.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Issue {
    /// Affected share date or annual report end.
    pub date: Date,
    /// Stable normalization diagnostic.
    pub code: String,
}
/// One security's captured history; no claim of historical revision completeness.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Captured {
    /// Actual local collection date, shared by the observations in this shard.
    pub observed_on: Date,
    /// Sorted exact-date snapshots, with missing values retained.
    pub shares: Vec<ShareAtClose>,
    /// Sorted annual observations; duplicate period/announcement keys are rejected.
    pub annual: Vec<Annual>,
    /// Missing and inconsistent accounting fields.
    pub issues: Vec<Issue>,
}
/// Explicit provider-history branch of the portable builder input.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct History {
    /// Must match the independent normalization version.
    pub normalization: String,
    /// Exact native-evidence bundle identity, retained in the acquisition task.
    pub bundle_sha256: String,
    /// Explicit temporal assumption; default selection is conservative at acquisition time.
    pub availability: Availability,
    /// Exact exchange identifiers in the caller's dated historical universe.
    pub instruments: BTreeMap<String, Captured>,
}
impl History {
    pub(super) fn validate(&self, request: &BuildRequest) -> io::Result<()> {
        if self.normalization != VERSION
            || self
                .bundle_sha256
                .strip_prefix("sha256:")
                .is_none_or(|s| s.len() != 64 || !s.bytes().all(|b| b.is_ascii_hexdigit()))
            || !request.shares.is_empty()
            || !request.book_equity.is_empty()
            || self
                .instruments
                .keys()
                .ne(request.universe.intervals.keys())
        {
            return Err(invalid(
                "three_factor_provider_history_identity_or_mixed_facts",
            ));
        }
        for id in self.instruments.keys() {
            analytics_core::pool::validate_exchange_symbol(id).map_err(invalid)?;
        }
        for rows in self.instruments.values() {
            if rows
                .shares
                .windows(2)
                .any(|p| p[0].trade_date >= p[1].trade_date)
                || rows.shares.iter().any(|r| {
                    r.trade_date > rows.observed_on
                        || r.total_shares.is_some_and(|v| !v.is_finite() || v <= 0.)
                })
                || rows.annual.windows(2).any(|p| {
                    (p[0].period_end, p[0].reported_on) >= (p[1].period_end, p[1].reported_on)
                })
                || rows.annual.iter().any(|r| {
                    r.period_end.month() != 12
                        || r.period_end.day() != 31
                        || r.reported_on <= r.period_end
                        || r.reported_on > rows.observed_on
                        || r.book_equity_cny.is_some_and(|v| !v.is_finite())
                })
            {
                return Err(invalid("three_factor_provider_history_dates_or_values"));
            }
        }
        Ok(())
    }
    pub(super) fn shares(&self, id: &str, date: Date) -> Option<f64> {
        let rows = self.instruments.get(id)?;
        if self.availability == Availability::ObservedOn && rows.observed_on >= date {
            return None;
        }
        rows.shares
            .get(
                rows.shares
                    .binary_search_by_key(&date, |r| r.trade_date)
                    .ok()?,
            )?
            .total_shares
    }
    pub(super) fn book(&self, id: &str, year: i32, date: Date) -> Option<f64> {
        let rows = self.instruments.get(id)?;
        if self.availability == Availability::ObservedOn && rows.observed_on >= date {
            return None;
        }
        rows.annual
            .iter()
            .filter(|r| r.period_end.year() == year && r.reported_on < date)
            .max_by_key(|r| r.reported_on)?
            .book_equity_cny
    }
}

fn number(value: &Value) -> io::Result<Option<f64>> {
    if value.is_null() || matches!(value.as_str(), Some("" | "--")) {
        return Ok(None);
    }
    let value = match value {
        Value::Number(v) => v.as_f64(),
        Value::String(v) => v.parse::<f64>().ok(),
        _ => None,
    }
    .ok_or_else(|| invalid("fundamentals_malformed_number"))?;
    if !value.is_finite() || value.abs() > 1e25 {
        return Err(invalid("fundamentals_invalid_numeric_or_sentinel"));
    }
    Ok(Some(value))
}
fn date(value: &Value) -> io::Result<Date> {
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    if text.len() != 8 || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid("fundamentals_invalid_native_date"));
    }
    Date::parse_from_str(&text, "%Y%m%d").map_err(invalid)
}
fn body(root: &Path, receipt: &Value) -> io::Result<Value> {
    let name = receipt["filename"]
        .as_str()
        .ok_or_else(|| invalid("fundamentals_receipt_missing"))?;
    if Path::new(name).file_name().and_then(|s| s.to_str()) != Some(name)
        || !name.starts_with("sdk-")
        || receipt["kind"] != "sdk_native_response"
        || receipt["sdk"] != "tdxaidata"
        || receipt["version"] != "1.2.2"
        || crate::market_view::file_hash(&root.join("raw").join(name))? != receipt["sha256"]
    {
        return Err(invalid("fundamentals_receipt_identity_or_hash"));
    }
    let bytes = fs::read(root.join("raw").join(name))?;
    let (text, _, errors) = encoding_rs::GB18030.decode(&bytes);
    if errors {
        return Err(invalid("fundamentals_native_encoding"));
    }
    let value: Value = serde_json::from_str(&text).map_err(invalid)?;
    if value.get("ErrorId").is_some_and(|v| v != "0" && v != 0)
        || (value.get("ErrorId").is_none() && receipt["function"] != "GetHISDATsInStr")
        || value.get("error").is_some()
    {
        return Err(invalid("fundamentals_native_error"));
    }
    Ok(value.get("Value").cloned().unwrap_or(value))
}
fn annual(value: &Value, start: Date, end: Date, output: &mut Captured) -> io::Result<()> {
    let fields: BTreeMap<_, _> = FIELDS
        .into_iter()
        .chain(["announce_time", "tag_time"])
        .map(|k| {
            Ok((
                k,
                value[k]
                    .as_array()
                    .ok_or_else(|| invalid("fundamentals_financial_field_missing"))?,
            ))
        })
        .collect::<io::Result<_>>()?;
    let count = fields["tag_time"].len();
    if fields.values().any(|v| v.len() != count) {
        return Err(invalid("fundamentals_financial_array_alignment"));
    }
    let mut seen = BTreeSet::new();
    for (i, period) in fields["tag_time"].iter().enumerate() {
        let period_end = date(period)?;
        let reported_on = date(&fields["announce_time"][i])?;
        if reported_on <= period_end
            || reported_on < start
            || reported_on > end
            || !seen.insert((period_end, reported_on))
        {
            return Err(invalid("fundamentals_financial_date_or_duplicate"));
        }
        if period_end.month() != 12 || period_end.day() != 31 {
            continue;
        }
        let values = FIELDS
            .map(|k| number(&fields[k][i]))
            .into_iter()
            .collect::<io::Result<Vec<_>>>()?;
        // FN004 is rounded to cents/share. Require consistency, never substitute
        // its rounded product for exact common equity. Native FN238/FN271 are shares/CNY;
        // FN428/FN429 are explicitly denominated in ten-thousand CNY by TDX.
        let candidate = match values.as_slice() {
            [
                Some(per_share),
                Some(shares),
                Some(parent),
                Some(preferred),
                Some(perpetual),
            ] if *shares > 0. && *preferred >= 0. && *perpetual >= 0. => {
                let common = parent - (preferred + perpetual) * 10000.;
                let reference = per_share * shares;
                ((common - reference).abs() <= 0.0051 * shares + parent.abs() * 2e-7)
                    .then_some(common)
            }
            _ => None,
        };
        if candidate.is_none() {
            output.issues.push(Issue {
                date: period_end,
                code: "annual_common_equity_missing_or_inconsistent".into(),
            });
        }
        output.annual.push(Annual {
            period_end,
            reported_on,
            book_equity_cny: candidate,
        });
    }
    output.annual.sort_by_key(|r| (r.period_end, r.reported_on));
    Ok(())
}
/// Verify a retained acquisition package before a workflow uses or copies its input file.
/// Replays normalization from native bytes instead of trusting derived JSON alone.
pub fn verify_bundle(root: &Path, history: &History) -> io::Result<()> {
    if crate::market_view::file_hash(&root.join("fundamentals.json"))? != history.bundle_sha256 {
        return Err(invalid("three_factor_native_bundle_changed"));
    }
    let bundle: Value =
        serde_json::from_slice(&fs::read(root.join("fundamentals.json"))?).map_err(invalid)?;
    let request: CollectionRequest =
        serde_json::from_value(bundle["request"].clone()).map_err(invalid)?;
    request.validate()?;
    if bundle["normalization"] != VERSION
        || bundle["schema_version"] != 1
        || bundle["instruments"] != serde_json::to_value(&history.instruments).map_err(invalid)?
        || request.provider != crate::collector::Provider::Tdxaidata
        || !request.source_bindings.is_empty()
        || request.instruments != history.instruments.keys().cloned().collect()
    {
        return Err(invalid("three_factor_native_bundle_identity"));
    }
    for (name, sha) in bundle["evidence_sha256"]
        .as_object()
        .ok_or_else(|| invalid("fundamentals_bundle_evidence_missing"))?
    {
        if Path::new(name).file_name().and_then(|s| s.to_str()) != Some(name)
            || crate::market_view::file_hash(&root.join("raw").join(name))? != *sha
        {
            return Err(invalid("fundamentals_bundle_evidence_changed"));
        }
    }
    for (id, expected) in &history.instruments {
        let msg: Value = serde_json::from_slice(&fs::read(
            root.join("raw")
                .join(format!("{id}-actions-structured.json")),
        )?)
        .map_err(invalid)?;
        if serde_json::to_value(normalize(&request, root, id, &msg)?).map_err(invalid)?
            != serde_json::to_value(expected).map_err(invalid)?
        {
            return Err(invalid("fundamentals_bundle_normalization_mismatch"));
        }
    }
    Ok(())
}
/// Rebuild one captured shard exclusively from hash-checked native responses and exact arguments.
pub(crate) fn normalize(
    request: &CollectionRequest,
    root: &Path,
    id: &str,
    msg: &Value,
) -> io::Result<Captured> {
    let observed_on: Date = serde_json::from_value(msg["observed_on"].clone()).map_err(invalid)?;
    let today = (chrono::Utc::now() + chrono::Duration::hours(8)).date_naive();
    if msg["instrument"] != id || observed_on < request.end || observed_on > today {
        return Err(invalid("fundamentals_shard_identity_or_observation_date"));
    }
    let symbol = format!("{}.{}", &id[2..], &id[..2]);
    let time = |d: Date| format!("{d} 00:00:00");
    let receipts = msg["evidence"]["sdk_receipts"]
        .as_array()
        .ok_or_else(|| invalid("fundamentals_receipts_missing"))?;
    let mut output = Captured {
        observed_on,
        shares: vec![],
        annual: vec![],
        issues: vec![],
    };
    let mut financial = 0;
    let mut share_windows = BTreeSet::new();
    let mut quote_windows = BTreeSet::new();
    for receipt in receipts {
        let value = body(root, receipt)?;
        let args = &receipt["arguments"];
        match receipt["function"].as_str() {
            Some("GetProDataInStr") => {
                if args.as_array().is_none_or(|v| v.len() != 1) {
                    return Err(invalid("fundamentals_financial_arguments"));
                }
                let args: Value = serde_json::from_str(
                    args[0]
                        .as_str()
                        .ok_or_else(|| invalid("fundamentals_financial_arguments"))?,
                )
                .map_err(invalid)?;
                if args
                    != json!({"type":1,"code":symbol,"table_list":FIELDS,"start_time":time(request.start),"end_time":time(request.end),"report_type":"announce_time"})
                {
                    return Err(invalid("fundamentals_financial_request_mismatch"));
                }
                financial += 1;
                annual(&value, request.start, request.end, &mut output)?;
            }
            Some("GetGbInfoInStr" | "GetHISDATsInStr") => {
                let gb = receipt["function"] == "GetGbInfoInStr";
                let (first, last) = if gb {
                    (&args[4], &args[5])
                } else {
                    (&args[1], &args[2])
                };
                let parse = |v: &Value| {
                    v.as_str()
                        .and_then(|s| Date::parse_from_str(s, "%Y-%m-%d 00:00:00").ok())
                        .ok_or_else(|| invalid("fundamentals_interval_argument"))
                };
                let start = parse(first)?;
                let end = parse(last)?;
                if start < request.start
                    || end > request.end
                    || start > end
                    || start.year() != end.year()
                    || args
                        != &if gb {
                            json!([symbol, 1, 0, "", time(start), time(end)])
                        } else {
                            json!([symbol, time(start), time(end), "1d", "none", -1])
                        }
                {
                    return Err(invalid("fundamentals_interval_request_mismatch"));
                }
                let windows = if gb {
                    &mut share_windows
                } else {
                    &mut quote_windows
                };
                if !windows.insert((start, end)) {
                    return Err(invalid("fundamentals_duplicate_interval"));
                }
                if gb {
                    for row in value
                        .as_array()
                        .ok_or_else(|| invalid("fundamentals_shares_array_missing"))?
                    {
                        let trade_date = date(&row["Date"])?;
                        if trade_date < start || trade_date > end {
                            return Err(invalid("fundamentals_share_date_outside_request"));
                        }
                        let mut shares = number(&row["Zgb"])?;
                        if shares == Some(0.) {
                            shares = None;
                        }
                        if shares.is_some_and(|v| v < 0.) {
                            return Err(invalid("fundamentals_negative_shares"));
                        }
                        if shares.is_none() {
                            output.issues.push(Issue {
                                date: trade_date,
                                code: "total_shares_missing".into(),
                            });
                        }
                        output.shares.push(ShareAtClose {
                            trade_date,
                            total_shares: shares,
                        });
                    }
                }
            }
            _ => return Err(invalid("fundamentals_unexpected_native_function")),
        }
    }
    let expected: BTreeSet<_> = (request.start.year()..=request.end.year())
        .map(|year| {
            (
                request.start.max(Date::from_ymd_opt(year, 1, 1).unwrap()),
                request.end.min(Date::from_ymd_opt(year, 12, 31).unwrap()),
            )
        })
        .collect();
    if financial != 1 || share_windows != expected || quote_windows != expected {
        return Err(invalid("fundamentals_native_windows_incomplete"));
    }
    if output
        .shares
        .windows(2)
        .any(|p| p[0].trade_date >= p[1].trade_date)
    {
        return Err(invalid("fundamentals_shares_unsorted_or_duplicate"));
    }
    Ok(output)
}

#[cfg(test)]
mod tests;
