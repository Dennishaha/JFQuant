//! BigQuant read-only HTTP metadata and bounded official Arrow Flight SDK acquisition.
//! The SDK bridge is isolated; credentials travel through private stdin only.
/// Fixed, quota-bounded historical extracts and temporal run encoding.
pub mod history;
/// Full-provider-universe market terms and local, source-verified market-factor construction.
pub mod market;
/// PIT ordinary-equity recovery and annual size/value formation evidence.
pub mod size_value;
/// Frozen annual portfolios, daily sufficient statistics, and complete factor artifact construction.
pub mod size_value_factors;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Write},
    path::Path,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

fn invalid(message: &str) -> io::Error {
    io::Error::other(message)
}
fn key_parts(key: &str) -> io::Result<(&str, &str)> {
    let (ak, sk) = key
        .split_once('.')
        .ok_or_else(|| invalid("bigquant_key_invalid"))?;
    if ak.is_empty()
        || sk.is_empty()
        || !ak
            .bytes()
            .chain(sk.bytes())
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
    {
        return Err(invalid("bigquant_key_invalid"));
    }
    Ok((ak, sk))
}
fn signature(secret: &str, path: &str, timestamp: &str) -> String {
    // RFC 2104 HMAC; same path + body + millisecond timestamp as official SDK.
    let mut key = [0u8; 64];
    if secret.len() > 64 {
        key[..32].copy_from_slice(&Sha256::digest(secret.as_bytes()));
    } else {
        key[..secret.len()].copy_from_slice(secret.as_bytes());
    }
    let inner_pad = key.map(|b| b ^ 0x36);
    let outer_pad = key.map(|b| b ^ 0x5c);
    let mut inner = Sha256::new();
    inner.update(inner_pad);
    inner.update(path);
    inner.update(timestamp);
    let mut outer = Sha256::new();
    outer.update(outer_pad);
    outer.update(inner.finalize());
    outer
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn get(path: &str) -> io::Result<Value> {
    let key = crate::credentials::load_source("bigquant")?;
    let (ak, sk) = key_parts(&key)?;
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| invalid("bigquant_clock_invalid"))?
        .as_millis()
        .to_string();
    let config = format!(
        "header = \"X-BigQuant-Access-Key: {ak}\"\nheader = \"X-BigQuant-Timestamp: {timestamp}\"\nheader = \"X-BigQuant-Signature: {}\"\n",
        signature(sk, path, &timestamp)
    );
    let output = crate::transport::curl_input(
        Command::new("/usr/bin/curl").args([
            "--disable",
            "--silent",
            "--show-error",
            "--config",
            "-",
            "--proto",
            "=https",
            "--connect-timeout",
            "10",
            "--max-time",
            "30",
            "--max-filesize",
            "8388608",
            "--write-out",
            "\n%{http_code}",
            &format!("https://bigquant.com{path}"),
        ]),
        Some(config.as_bytes()),
    )?;
    if !output.status.success() {
        return Err(invalid("bigquant_http_transport_failed"));
    }
    let split = output
        .stdout
        .iter()
        .rposition(|b| *b == b'\n')
        .ok_or_else(|| invalid("bigquant_http_invalid"))?;
    if &output.stdout[split + 1..] != b"200" {
        return Err(invalid("bigquant_http_denied_or_unavailable"));
    }
    let value: Value = serde_json::from_slice(&output.stdout[..split])
        .map_err(|_| invalid("bigquant_json_invalid"))?;
    if value["code"] != 0 {
        return Err(invalid("bigquant_api_denied"));
    }
    Ok(value["data"].clone())
}
/// Validate auth without returning personal account information.
pub fn check() -> io::Result<Value> {
    get("/bigapis/auth/v1/users/me")?;
    Ok(
        json!({"provider":"bigquant","authenticated":true,"transport":"rust_http","data_permission_verified":false}),
    )
}
fn identifier(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
/// Read public table schema/docs using the official SDK's HTTP metadata route.
pub fn schema(table: &str) -> io::Result<Value> {
    if !identifier(table) {
        return Err(invalid("bigquant_table_invalid"));
    }
    get(&format!(
        "/bigapis/data/v1/spacedatasources/spaces/00000000-0000-0000-0000-000000000000/datasources/{table}"
    ))
}
/// One bounded page; all fields and dates are explicit. No arbitrary SQL or automatic full scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReadRequest {
    /// BigQuant table name.
    pub table: String,
    /// Columns to preserve, including nullable financial values.
    pub columns: Vec<String>,
    /// Actual table date column (e.g. publish_date or date).
    pub date_column: String,
    /// Inclusive partition/date lower bound.
    pub start: String,
    /// Inclusive upper bound.
    pub end: String,
    /// BigQuant identifiers such as 600519.SH; empty means all securities in this date range.
    pub instruments: Vec<String>,
    /// 1..10000; query fetches one extra row to detect incomplete results.
    pub limit: usize,
}
impl ReadRequest {
    fn sql(&self) -> io::Result<String> {
        if !identifier(&self.table)
            || !identifier(&self.date_column)
            || self.columns.is_empty()
            || self.columns.len() > 64
            || self.columns.iter().any(|x| !identifier(x))
            || self
                .columns
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != self.columns.len()
            || !(1..=10000).contains(&self.limit)
            || self.instruments.len() > 1000
            || self
                .instruments
                .iter()
                .any(|x| x.is_empty() || !x.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.'))
        {
            return Err(invalid("bigquant_request_invalid"));
        }
        let start = NaiveDate::parse_from_str(&self.start, "%Y-%m-%d")
            .map_err(|_| invalid("bigquant_date_invalid"))?;
        let end = NaiveDate::parse_from_str(&self.end, "%Y-%m-%d")
            .map_err(|_| invalid("bigquant_date_invalid"))?;
        if start > end || start.to_string() != self.start || end.to_string() != self.end {
            return Err(invalid("bigquant_date_invalid"));
        }
        let instruments = if self.instruments.is_empty() {
            String::new()
        } else {
            format!(
                " AND instrument IN ({})",
                self.instruments
                    .iter()
                    .map(|s| format!("'{s}'"))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        };
        Ok(format!(
            "SELECT {} FROM {} WHERE {} >= '{}' AND {} < DATE '{}' + INTERVAL 1 DAY{} ORDER BY {}, {} LIMIT {}",
            self.columns.join(","),
            self.table,
            self.date_column,
            self.start,
            self.date_column,
            self.end,
            instruments,
            self.date_column,
            self.columns.join(","),
            self.limit + 1
        ))
    }
}
fn flight(python: &Path, mut request: Value) -> io::Result<Value> {
    let key = crate::credentials::load_source("bigquant")?;
    key_parts(&key)?;
    request["key"] = Value::String(key);
    let input = serde_json::to_vec(&request).map_err(|_| invalid("bigquant_request_invalid"))?;
    // Existing transport supervises cancellation and drains output. Python is never a shell.
    let output = crate::transport::curl_input(
        Command::new(python).args(["-I", "-c", include_str!("bigquant/flight_bridge.py")]),
        Some(&input),
    )?;
    if !output.status.success() {
        return Err(invalid("bigquant_sdk_process_failed"));
    }
    let mut result: Value = serde_json::from_slice(&output.stdout)
        .map_err(|_| invalid("bigquant_sdk_response_invalid"))?;
    if result["ok"] != true {
        let error = result["error"].as_str().unwrap_or("bigquant_flight_failed");
        if error.starts_with("bigquant_sql_binding_")
            && error.len() < 160
            && error
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        {
            return Err(invalid(error));
        }
        return Err(invalid(match error {
            "bigquant_sql_ambiguous" => "bigquant_sql_ambiguous",
            "bigquant_sql_binding" => "bigquant_sql_binding",
            "bigquant_sql_parse" => "bigquant_sql_parse",
            "bigquant_sql_expression_limit" => "bigquant_sql_expression_limit",
            "bigquant_query_too_large" => "bigquant_query_too_large",
            "bigquant_query_memory_limit" => "bigquant_query_memory_limit",
            "bigquant_sdk_missing" => "bigquant_sdk_missing",
            "bigquant_sdk_permission_required" => "bigquant_sdk_permission_required",
            "bigquant_flight_timeout" => "bigquant_flight_timeout",
            "bigquant_sdk_version_mismatch" => "bigquant_sdk_version_mismatch",
            "bigquant_permission_denied" => "bigquant_permission_denied",
            "bigquant_table_not_entitled" => "bigquant_table_not_entitled",
            "bigquant_quota_denied" => "bigquant_quota_denied",
            "bigquant_authentication_failed" => "bigquant_authentication_failed",
            _ => "bigquant_flight_failed",
        }));
    }
    result.as_object_mut().unwrap().remove("ok");
    Ok(result)
}
/// Inspect SDK/Flight data quota. This does not prove access to any particular table.
pub fn quota(python: &Path) -> io::Result<Value> {
    flight(python, json!({"operation":"quota"}))
}
/// Acquire one bounded raw page with schema, query, timestamp and content hash.
/// `complete=false` requires smaller date/security partitions; never research-registers candidates.
pub fn read(python: &Path, request: &ReadRequest) -> io::Result<Value> {
    let sql = request.sql()?;
    let metadata = schema(&request.table)?;
    let fields = metadata["metadata"]["schema"]
        .as_object()
        .ok_or_else(|| invalid("bigquant_schema_missing"))?;
    if !fields.contains_key(&request.date_column)
        || request.columns.iter().any(|x| !fields.contains_key(x))
    {
        return Err(invalid("bigquant_column_missing"));
    }
    let mut filters = json!({});
    filters[&request.date_column] = json!([request.start, request.end]);
    let result = flight(
        python,
        json!({"operation":"read","sql":sql,"filters":filters,"table":request.table,"column_count":request.columns.len(),"max_rows":request.limit+1}),
    )?;
    let mut rows = result["rows"]
        .as_array()
        .ok_or_else(|| invalid("bigquant_rows_invalid"))?
        .clone();
    let complete = rows.len() <= request.limit;
    rows.truncate(request.limit);
    let raw = json!({"request":request,"sql":sql,"metadata":metadata,"arrow_schema":result["arrow_schema"],"allowance":result["allowance"],"rows":rows});
    let hash = Sha256::digest(serde_json::to_vec(&raw)?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    Ok(
        json!({"schema_version":1,"provider":"bigquant","transport":"official_python_arrow_flight","sdk":{"bigquant":"0.1.11","bigquant_core":"0.1.14"},"observed_at":chrono::Utc::now().to_rfc3339(),"complete":complete,"research_admitted":false,"sha256":hash,"payload":raw}),
    )
}

/// Validate a saved current-format snapshot against its exact request and content hash.
/// Corruption, a different query or malformed row/schema data never triggers a silent refetch.
pub fn verify_snapshot(value: &Value, request: &ReadRequest) -> io::Result<()> {
    let sql = request.sql()?;
    let payload = &value["payload"];
    let rows = payload["rows"]
        .as_array()
        .ok_or_else(|| invalid("bigquant_cache_invalid"))?;
    let hash = Sha256::digest(serde_json::to_vec(payload)?)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();
    if value["schema_version"] != 1
        || value["provider"] != "bigquant"
        || value["transport"] != "official_python_arrow_flight"
        || value["sdk"] != json!({"bigquant":"0.1.11","bigquant_core":"0.1.14"})
        || !value["complete"].is_boolean()
        || value["research_admitted"] != false
        || value["observed_at"]
            .as_str()
            .is_none_or(|v| chrono::DateTime::parse_from_rfc3339(v).is_err())
        || value["sha256"] != hash
        || payload["request"] != serde_json::to_value(request)?
        || payload["sql"] != sql
        || payload["metadata"]["id"] != request.table
        || !payload["arrow_schema"].is_string()
        || rows.len() > request.limit
        || rows.iter().any(|row| {
            row.as_object().is_none_or(|object| {
                object.len() != request.columns.len()
                    || request.columns.iter().any(|c| !object.contains_key(c))
            })
        })
    {
        return Err(invalid("bigquant_cache_invalid_or_request_mismatch"));
    }
    Ok(())
}

/// Reuse a verified local snapshot without any HTTP/SDK traffic, or atomically save a new one.
/// Returns `(snapshot, cache_hit)`. An existing output is immutable and must match the request.
pub fn fetch(python: &Path, request: &ReadRequest, output: &Path) -> io::Result<(Value, bool)> {
    request.sql()?;
    cached(
        output,
        || read(python, request),
        |value| verify_snapshot(value, request),
    )
}
fn cached(
    output: &Path,
    acquire: impl FnOnce() -> io::Result<Value>,
    verify: impl Fn(&Value) -> io::Result<()>,
) -> io::Result<(Value, bool)> {
    match fs::read(output) {
        Ok(bytes) => {
            let value: Value =
                serde_json::from_slice(&bytes).map_err(|_| invalid("bigquant_cache_invalid"))?;
            verify(&value)?;
            return Ok((value, true));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    if !parent.is_dir() {
        return Err(invalid("bigquant_output_parent_missing"));
    }
    // Reserve a per-output lock before quota use. A stale lock requires explicit local handling.
    let lock = output.with_file_name(format!(
        ".{}.lock",
        output
            .file_name()
            .ok_or_else(|| invalid("bigquant_output_invalid"))?
            .to_string_lossy()
    ));
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&lock)?;
    let result = (|| {
        // Another process may have completed between the first read and lock acquisition.
        if output.exists() {
            return Err(invalid("bigquant_output_exists"));
        }
        let value = acquire()?;
        verify(&value)?;
        file.write_all(&serde_json::to_vec_pretty(&value)?)?;
        file.sync_all()?;
        // Same directory/filesystem; create the final name without overwriting an existing file.
        fs::hard_link(&lock, output)?;
        fs::File::open(parent)?.sync_all()?;
        Ok((value, false))
    })();
    let _ = fs::remove_file(lock);
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sdk_hmac_and_bounded_query_contract() {
        assert_eq!(
            signature("key", "The quick brown fox jumps over the lazy dog", ""),
            "f7bc83f430538424b13298e6aa6fb143ef4d59a14946175997479dbc2d1a3cd8"
        );
        let mut r = ReadRequest {
            table: "cn_stock_capital".into(),
            columns: vec!["instrument".into(), "total_shares".into()],
            date_column: "publish_date".into(),
            start: "2020-01-01".into(),
            end: "2020-12-31".into(),
            instruments: vec!["600519.SH".into()],
            limit: 5,
        };
        let sql = r.sql().unwrap();
        assert!(sql.contains("LIMIT 6"));
        assert!(sql.contains("AND instrument IN ('600519.SH')"));
        r.instruments = vec!["x');DROP TABLE x;--".into()];
        assert!(r.sql().is_err());
        r.instruments.clear();
        r.end = "2019-01-01".into();
        assert!(r.sql().is_err());
        r.end = "2020-12-31".into();
        r.columns.push("x;DELETE".into());
        assert!(r.sql().is_err());
        assert!(key_parts("bad\nkey.sk").is_err());
    }

    #[test]
    fn cached_snapshot_preserves_nulls_and_never_refetches_corruption_or_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let request = ReadRequest {
            table: "cn_stock_shares".into(),
            columns: vec!["instrument".into(), "total_shares".into()],
            date_column: "date".into(),
            start: "2024-01-01".into(),
            end: "2024-01-02".into(),
            instruments: vec!["600519.SH".into()],
            limit: 2,
        };
        let payload = json!({"request":request,"sql":request.sql().unwrap(),"metadata":{"id":request.table},"arrow_schema":"instrument: string\ntotal_shares: double","rows":[{"instrument":"600519.SH","total_shares":null}]});
        let hash = Sha256::digest(serde_json::to_vec(&payload).unwrap())
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        let snapshot = json!({"schema_version":1,"provider":"bigquant","transport":"official_python_arrow_flight","sdk":{"bigquant":"0.1.11","bigquant_core":"0.1.14"},"observed_at":"2026-10-19T00:00:00Z","complete":true,"research_admitted":false,"sha256":hash,"payload":payload});
        let output = temp.path().join("cached.json");
        fs::write(&output, serde_json::to_vec(&snapshot).unwrap()).unwrap();
        let (loaded, hit) = fetch(Path::new("/no-sdk-installed"), &request, &output).unwrap();
        assert!(hit);
        assert!(loaded["payload"]["rows"][0]["total_shares"].is_null());
        let mut mismatch = request.clone();
        mismatch.end = "2024-01-03".into();
        assert!(fetch(Path::new("/no-sdk-installed"), &mismatch, &output).is_err());
        let mut corrupt = snapshot.clone();
        corrupt["payload"]["rows"][0]["total_shares"] = json!(0);
        fs::write(&output, serde_json::to_vec(&corrupt).unwrap()).unwrap();
        assert!(fetch(Path::new("/no-sdk-installed"), &request, &output).is_err());
        assert_eq!(
            serde_json::from_slice::<Value>(&fs::read(output).unwrap()).unwrap(),
            corrupt
        );
    }
}
