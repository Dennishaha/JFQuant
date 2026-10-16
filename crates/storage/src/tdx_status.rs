//! Historical status from authenticated GP29 and exchange-calendar native replies.
use crate::corporate_actions::ActionBundle;
use analytics_core::{Date, DateRange};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
};

/// Interpretation rule recorded beside the authenticated native receipts.
pub const RULE: &str = "tdxaidata.gp29-status-days-v2";
const CAPTURE_RULE: &str = "tdxaidata.gp29-resumption-v1";
fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}
fn date(value: &Value) -> io::Result<Date> {
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string());
    Date::parse_from_str(&text, "%Y%m%d").map_err(io::Error::other)
}
fn data(value: &Value) -> io::Result<&Value> {
    if value.get("ErrorId").is_some()
        && !matches!(value["ErrorId"].as_str(), Some("0"))
        && value["ErrorId"] != 0
    {
        return Err(invalid("status native business error"));
    }
    Ok(if value["Value"].is_object() {
        &value["Value"]
    } else {
        value
    })
}
/// Strictly interpret resumption counts against an independent ordered market calendar.
/// A future resumption proves a historical interval retrospectively, never a trading signal.
pub fn normalize(
    calendar: &Value,
    events: &Value,
    range: &DateRange,
) -> io::Result<BTreeMap<Date, String>> {
    let calendar = data(calendar)?["Date"]
        .as_array()
        .ok_or_else(|| invalid("status calendar missing"))?
        .iter()
        .map(date)
        .collect::<io::Result<Vec<_>>>()?;
    if calendar.is_empty() || calendar.windows(2).any(|w| w[0] >= w[1]) {
        return Err(invalid("status calendar unordered or duplicate"));
    }
    let rows = data(events)?["GP29"]
        .as_array()
        .ok_or_else(|| invalid("GP29 field missing"))?;
    let mut seen = BTreeSet::new();
    let mut suspended = BTreeMap::new();
    for row in rows {
        let resumed = date(&row["Date"])?;
        if !range.contains(resumed) || !seen.insert(resumed) {
            return Err(invalid("GP29 date outside query or duplicate"));
        }
        let values = row["Value"]
            .as_array()
            .filter(|v| v.len() == 2)
            .ok_or_else(|| invalid("GP29 fields truncated"))?;
        let count = values[0]
            .as_str()
            .and_then(|v| v.parse::<f64>().ok())
            .ok_or_else(|| invalid("GP29 suspension count missing"))?;
        if !count.is_finite() || count < 0. || count.fract() != 0. || count > 10000. {
            return Err(invalid("GP29 suspension count invalid"));
        }
        if count == 0. {
            continue;
        }
        let position = calendar
            .binary_search(&resumed)
            .map_err(|_| invalid("GP29 resumption outside calendar"))?;
        let first = position
            .checked_sub(count as usize)
            .ok_or_else(|| invalid("GP29 preceding calendar insufficient"))?;
        for day in &calendar[first..position] {
            if range.contains(*day) {
                suspended.insert(*day, "suspension_day".into());
            }
        }
    }
    Ok(suspended)
}
fn reply(root: &Path, receipt: &Value) -> io::Result<Value> {
    let name = receipt["filename"]
        .as_str()
        .ok_or_else(|| invalid("status receipt filename missing"))?;
    if Path::new(name).file_name().and_then(|v| v.to_str()) != Some(name) {
        return Err(invalid("unsafe status receipt"));
    }
    let bytes = fs::read(root.join("raw").join(name))?;
    let (text, _, bad) = encoding_rs::GBK.decode(&bytes);
    if bad {
        return Err(invalid("status reply encoding invalid"));
    }
    serde_json::from_str(&text).map_err(io::Error::other)
}
/// Validate native request identity, response hashes and derived suspension dates.
pub fn from_evidence(
    root: &Path,
    evidence: &Value,
    symbol: &str,
    range: &DateRange,
) -> io::Result<BTreeMap<Date, String>> {
    if evidence["stock_status"]["status"] != "captured" {
        return Ok(BTreeMap::new());
    }
    if evidence["stock_status"]["rule"] != CAPTURE_RULE {
        return Err(invalid("unsupported stock status interpretation"));
    }
    crate::collector::verify_http_receipts(root, evidence)?;
    let receipts = evidence["sdk_receipts"]
        .as_array()
        .ok_or_else(|| invalid("status receipts missing"))?;
    let begin = format!("{} 00:00:00", range.start);
    let end = format!("{} 00:00:00", range.end);
    let calendar_start = evidence["stock_status"]["calendar_start"]
        .as_str()
        .ok_or_else(|| invalid("status calendar start missing"))?;
    let expected_start = range
        .start
        .checked_sub_signed(chrono::Duration::days(3660))
        .ok_or_else(|| invalid("status date overflow"))?;
    if calendar_start != expected_start.to_string() {
        return Err(invalid("status calendar query mismatch"));
    }
    let calendar = receipts
        .iter()
        .find(|r| {
            r["function"] == "GetTradeCalendarInStr"
                && r["arguments"] == json!([format!("{calendar_start} 00:00:00"), end, "SH", -1])
        })
        .ok_or_else(|| invalid("status calendar receipt missing"))?;
    let events = receipts
        .iter()
        .find(|r| {
            r["function"] == "GetProDataInStr"
                && r["arguments"][0]
                    .as_str()
                    .and_then(|v| serde_json::from_str::<Value>(v).ok())
                    .is_some_and(|v| {
                        v["type"] == 3
                            && v["code"] == symbol
                            && v["table_list"] == json!(["GP29"])
                            && v["start_time"] == begin
                            && v["end_time"] == end
                    })
        })
        .ok_or_else(|| invalid("GP29 request identity mismatch"))?;
    let calendar = reply(root, calendar)?;
    let events = reply(root, events)?;
    let mut days = normalize(&calendar, &events, range)?;
    if evidence["stock_status"]["current_snapshot"] == "captured" {
        let snapshot = receipts
            .iter()
            .find(|r| r["function"] == "GetMoreInfoInStr" && r["arguments"] == json!([symbol]))
            .ok_or_else(|| invalid("current status identity mismatch"))?;
        ongoing_episode(
            &calendar,
            &events,
            &reply(root, snapshot)?,
            range,
            &mut days,
        )?;
    }
    Ok(days)
}
/// A current flag alone has no historical meaning. A dated flag and most recent
/// suspension start prove only that latest ongoing episode, never prior episodes.
fn ongoing_episode(
    calendar: &Value,
    events: &Value,
    snapshot: &Value,
    range: &DateRange,
    days: &mut BTreeMap<Date, String>,
) -> io::Result<()> {
    let snapshot = data(snapshot)?;
    if snapshot["TPFlag"] != "1" {
        return Ok(());
    }
    let as_of = date(&snapshot["HqDate"])?;
    let start = date(&snapshot["StopJYDate_Recent"])?;
    if start > as_of {
        return Err(invalid("current suspension start after snapshot date"));
    }
    if as_of < range.end || start > range.end {
        return Ok(());
    }
    for event in data(events)?["GP29"]
        .as_array()
        .ok_or_else(|| invalid("GP29 field missing"))?
    {
        let resumed = date(&event["Date"])?;
        let count = event["Value"][0]
            .as_str()
            .and_then(|v| v.parse::<f64>().ok())
            .unwrap_or(0.);
        if count > 0. && resumed >= start {
            return Err(invalid(
                "current suspension conflicts with historical resumption",
            ));
        }
    }
    let calendar = data(calendar)?["Date"]
        .as_array()
        .ok_or_else(|| invalid("status calendar missing"))?;
    let calendar = calendar.iter().map(date).collect::<io::Result<Vec<_>>>()?;
    // The authenticated query already ends at range.end. Its final returned
    // trading day can precede that date on weekends/holidays. Never invent dates
    // to fill the gap: account valuation still requires every actual session.
    for day in calendar {
        if range.contains(day) && day >= start {
            days.insert(day, "suspension_day".into());
        }
    }
    Ok(())
}
/// A verified native interval confirms suspension days; raw bars resolve intraday trading.
pub fn suspension_day(book: &ActionBundle, id: &str, date: Date) -> bool {
    book.execution_data.as_ref().is_some_and(|d| {
        d.capabilities
            .get(&format!("{RULE}:{id}"))
            .is_some_and(|v| v["status"] == "available")
            && d.suspensions
                .get(id)
                .and_then(|v| v.get(&date))
                .map(String::as_str)
                == Some("suspension_day")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ongoing_status_requires_dated_flag_and_consistent_history() {
        let calendar = json!({"Date":[20260914,20260915,20260916,20261012]});
        let events = json!({"ErrorId":"0","Value":{"GP29":[]}});
        let range = DateRange {
            start: "2026-09-14".parse().unwrap(),
            end: "2026-10-12".parse().unwrap(),
        };
        let snapshot = json!({"ErrorId":"0","Value":{"TPFlag":"1","HqDate":"20261015","StopJYDate_Recent":"20260915"}});
        let mut days = BTreeMap::new();
        ongoing_episode(&calendar, &events, &snapshot, &range, &mut days).unwrap();
        assert_eq!(
            days.keys().map(ToString::to_string).collect::<Vec<_>>(),
            ["2026-09-15", "2026-09-16", "2026-10-12"]
        );
        for (field, value) in [
            ("TPFlag", "0"),
            ("HqDate", "20261009"),
            ("StopJYDate_Recent", "20261013"),
        ] {
            let mut other = snapshot.clone();
            other["Value"][field] = json!(value);
            let mut no_days = BTreeMap::new();
            ongoing_episode(&calendar, &events, &other, &range, &mut no_days).unwrap();
            assert!(no_days.is_empty());
        }
        let mut missing = snapshot.clone();
        missing["Value"].as_object_mut().unwrap().remove("HqDate");
        assert!(
            ongoing_episode(&calendar, &events, &missing, &range, &mut BTreeMap::new()).is_err()
        );
        let conflict = json!({"Value":{"GP29":[{"Date":"20260916","Value":["1.00","0.00"]}]}});
        assert!(
            ongoing_episode(
                &calendar,
                &conflict,
                &snapshot,
                &range,
                &mut BTreeMap::new()
            )
            .is_err()
        );
        let weekend = DateRange {
            start: "2026-10-15".parse().unwrap(),
            end: "2026-10-18".parse().unwrap(),
        };
        let dated =
            json!({"Value":{"TPFlag":"1","HqDate":"20261019","StopJYDate_Recent":"20261015"}});
        let mut days = BTreeMap::new();
        ongoing_episode(
            &json!({"Date":[20261015,20261016]}),
            &events,
            &dated,
            &weekend,
            &mut days,
        )
        .unwrap();
        assert_eq!(days.len(), 2);
        assert!(!days.contains_key(&weekend.end));
    }
    #[test]
    fn native_receipts_bind_status_to_identity_dates_and_bytes() {
        use sha2::{Digest, Sha256};
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("raw")).unwrap();
        let range = DateRange {
            start: "2021-01-01".parse().unwrap(),
            end: "2021-03-01".parse().unwrap(),
        };
        let mut receipts = Vec::new();
        for (function, arguments, body) in [
            ("GetTradeCalendarInStr", json!(["2010-12-25 00:00:00","2021-03-01 00:00:00","SH",-1]), json!({"Date":["20210128","20210129","20210201","20210202"]})),
            ("GetProDataInStr", json!([json!({"type":3,"code":"600340.SH","table_list":["GP29"],"start_time":"2021-01-01 00:00:00","end_time":"2021-03-01 00:00:00"}).to_string()]), json!({"ErrorId":"0","Value":{"GP29":[{"Date":"20210202","Value":["2.00","0.00"]}]}})),
            ("GetMoreInfoInStr",json!(["600340.SH"]),json!({"ErrorId":"0","Value":{"TPFlag":"0"}})),
        ] {
            let bytes = serde_json::to_vec(&body).unwrap();
            let name = format!("sdk-{function}.body");
            fs::write(root.path().join("raw").join(&name), &bytes).unwrap();
            receipts.push(json!({"kind":"sdk_native_response","sdk":"tdxaidata","version":"1.2.2","function":function,"arguments":arguments,"filename":name,"sha256":format!("sha256:{:x}",Sha256::digest(&bytes))}));
        }
        let evidence = json!({"stock_status":{"status":"captured","rule":CAPTURE_RULE,"calendar_start":"2010-12-25","current_snapshot":"captured"},"sdk_receipts":receipts});
        assert_eq!(
            from_evidence(root.path(), &evidence, "600340.SH", &range)
                .unwrap()
                .len(),
            2
        );
        assert!(from_evidence(root.path(), &evidence, "600341.SH", &range).is_err());
        let mut wrong = evidence.clone();
        wrong["sdk_receipts"][2]["arguments"] = json!(["600341.SH"]);
        assert!(from_evidence(root.path(), &wrong, "600340.SH", &range).is_err());
        fs::write(root.path().join("raw/sdk-GetProDataInStr.body"), b"changed").unwrap();
        assert!(from_evidence(root.path(), &evidence, "600340.SH", &range).is_err());
    }
    #[test]
    fn historical_resumption_uses_trading_days_not_calendar_days() {
        let calendar = json!({"Date":[20210128,20210129,20210201,20210202]});
        let events =
            json!({"ErrorId":"0","Value":{"GP29":[{"Date":"20210202","Value":["2.00","0.00"]}]}});
        let range = DateRange {
            start: "2021-01-28".parse().unwrap(),
            end: "2021-02-02".parse().unwrap(),
        };
        let status = normalize(&calendar, &events, &range).unwrap();
        assert!(status.values().all(|v| v == "suspension_day"));
        let mut auxiliary = crate::execution_data::ExecutionData {
            schema_version: 2,
            ..Default::default()
        };
        auxiliary
            .suspensions
            .insert("SH600340".into(), status.clone());
        assert!(auxiliary.validate().is_ok());
        auxiliary.schema_version = 1;
        assert!(auxiliary.validate().is_err());
        assert_eq!(
            status.keys().map(ToString::to_string).collect::<Vec<_>>(),
            ["2021-01-29", "2021-02-01"]
        );
        let mut invalid = events.clone();
        invalid["Value"]["GP29"][0]["Value"][0] = json!("4.00");
        assert!(normalize(&calendar, &invalid, &range).is_err());
        invalid["Value"]["GP29"][0]["Value"][0] = json!("1.50");
        assert!(normalize(&calendar, &invalid, &range).is_err());
        assert!(normalize(&calendar, &json!({"ErrorId":"0","Value":{}}), &range).is_err());
    }
}
