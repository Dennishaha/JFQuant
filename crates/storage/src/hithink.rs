//! Worker-local HiThink REST acquisition. Supplier OHLC is distinct from raw execution prices.
//! Public contract: https://fuyao.aicubes.cn/llms-full.txt (checked 2026-10-15).
use crate::{
    adjustment::{AdjustmentBasis, AdjustmentRecord, HITHINK_NORMALIZATION},
    collector::{CollectionRequest, Provider},
    daily::{DailyBar, DailyManifest, write_bars},
};
use analytics_core::{Date, DateRange, task::TaskContext};
use chrono::Timelike;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::Duration,
};
const BASE: &str = "https://fuyao.aicubes.cn";
fn invalid(s: impl std::fmt::Display) -> io::Error {
    io::Error::other(s.to_string())
}
fn digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}
fn save(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    let part = path.with_extension("part");
    fs::write(&part, serde_json::to_vec_pretty(value).map_err(invalid)?)?;
    fs::File::open(&part)?.sync_all()?;
    fs::rename(part, path)
}
/// Exact native implementation identity, frozen in owned checkpoints.
pub fn producer_signature() -> String {
    let mut hash = Sha256::new();
    for bytes in [
        include_bytes!("hithink.rs").as_slice(),
        include_bytes!("daily.rs").as_slice(),
        include_bytes!("adjustment.rs").as_slice(),
    ] {
        hash.update(bytes);
        hash.update([0]);
    }
    format!("sha256:{:x}", hash.finalize())
}
/// Exchange symbols are unambiguous and retain their exchange through conversion.
pub fn thscode(symbol: &str) -> io::Result<String> {
    analytics_core::pool::validate_exchange_symbol(symbol).map_err(invalid)?;
    Ok(format!("{}.{}", &symbol[2..], &symbol[..2]))
}
fn local_symbol(code: &str) -> io::Result<String> {
    let (ticker, exchange) = code
        .split_once('.')
        .ok_or_else(|| invalid("hithink_invalid_code"))?;
    let symbol = format!("{exchange}{ticker}");
    thscode(&symbol)?;
    Ok(symbol)
}
fn zone() -> chrono::FixedOffset {
    chrono::FixedOffset::east_opt(8 * 3600).unwrap()
}
fn now() -> chrono::DateTime<chrono::FixedOffset> {
    chrono::Utc::now().with_timezone(&zone())
}
fn millis(date: Date) -> i64 {
    date.and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(zone())
        .unwrap()
        .timestamp_millis()
}
fn day(v: &Value) -> io::Result<Date> {
    let t = v.as_i64().ok_or_else(|| invalid("hithink_date_missing"))?;
    let t = chrono::DateTime::from_timestamp_millis(t)
        .ok_or_else(|| invalid("hithink_date_invalid"))?
        .with_timezone(&zone());
    if t.time() != chrono::NaiveTime::from_hms_opt(0, 0, 0).unwrap() {
        return Err(invalid("hithink_nonmidnight_date"));
    }
    Ok(t.date_naive())
}
fn numeric(v: &Value, price: bool) -> io::Result<Option<f64>> {
    if v.is_null() {
        return Ok(None);
    }
    let n = v
        .as_f64()
        .filter(|n| n.is_finite() && if price { *n > 0. } else { *n >= 0. })
        .ok_or_else(|| invalid("hithink_invalid_numeric"))?;
    Ok(Some(n))
}
fn items(data: &Value) -> io::Result<&Vec<Value>> {
    let rows = data["item"]
        .as_array()
        .ok_or_else(|| invalid("hithink_missing_items"))?;
    if data
        .get("total")
        .is_some_and(|v| v.as_u64() != Some(rows.len() as u64))
    {
        return Err(invalid("hithink_truncated_items"));
    }
    Ok(rows)
}

fn check_identity(data: &Value, code: &str) -> io::Result<()> {
    if data.get("thscode").is_some_and(|s| s != code)
        || data.get("ticker").is_some_and(|s| s != &code[..6])
    {
        return Err(invalid("hithink_security_mismatch"));
    }
    Ok(())
}
fn transient(status: u16, code: i64) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504) || matches!(code, 4001 | 5001 | 5002 | 5003)
}
struct Client<'a> {
    key: String,
    root: &'a Path,
    context: &'a TaskContext,
    receipts: Vec<Value>,
    #[cfg(test)]
    test_base: Option<String>,
}
impl Client<'_> {
    fn get(&mut self, path: &str, params: &[(&str, String)]) -> io::Result<Value> {
        // All paths and values originate from fixed routes and validated identifiers/dates.
        if params.iter().any(|(_, v)| {
            !v.bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"._,-".contains(&c))
        }) {
            return Err(invalid("hithink_invalid_query"));
        }
        #[cfg(not(test))]
        let base = BASE;
        #[cfg(test)]
        let base = self.test_base.as_deref().unwrap_or(BASE);
        let url = format!(
            "{base}{path}{}{}",
            if params.is_empty() { "" } else { "?" },
            params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&")
        );
        for attempt in 0..3 {
            self.context.check()?;
            let config = format!("header = \"X-api-key: {}\"\n", self.key);
            let mut cmd = Command::new("/usr/bin/curl");
            let protocol = "=https";
            #[cfg(test)]
            let protocol = if base.starts_with("http://127.0.0.1:") {
                "=http,https"
            } else {
                protocol
            };
            cmd.args([
                "--disable",
                "--silent",
                "--show-error",
                "--config",
                "-",
                "--connect-timeout",
                "10",
                "--max-time",
                "30",
                "--max-filesize",
                "33554432",
                "--proto",
                protocol,
                "--write-out",
                "\n%{http_code}",
                &url,
            ]);
            let result = self
                .context
                .execute(|| crate::transport::curl_input(&mut cmd, Some(config.as_bytes())))?;
            if !result.status.success() {
                if attempt == 2 {
                    return Err(invalid("hithink_transport_failed"));
                }
                self.pause(attempt)?;
                continue;
            }
            let split = result
                .stdout
                .iter()
                .rposition(|b| *b == b'\n')
                .ok_or_else(|| invalid("hithink_http_status_missing"))?;
            let body = &result.stdout[..split];
            if body.len() > 32 * 1024 * 1024
                || body
                    .windows(self.key.len())
                    .any(|w| w == self.key.as_bytes())
            {
                return Err(invalid("hithink_unsafe_response"));
            }
            let status = std::str::from_utf8(&result.stdout[split + 1..])
                .ok()
                .and_then(|s| s.parse::<u16>().ok())
                .ok_or_else(|| invalid("hithink_invalid_status"))?;
            let filename = format!(
                "http-{}.body",
                &digest(format!("{url}{}", digest(body)).as_bytes())[7..]
            );
            fs::write(self.root.join("raw").join(&filename), body)?;
            self.receipts.push(json!({"url":url,"status":status,"filename":filename,"sha256":digest(body),"kind":"http_response_body_decoded"}));
            let parsed: Result<Value, _> = serde_json::from_slice(body);
            let code = parsed
                .as_ref()
                .ok()
                .and_then(|v| v["code"].as_i64())
                .unwrap_or(-1);
            if transient(status, code) && attempt < 2 {
                self.pause(attempt)?;
                continue;
            }
            if status != 200 {
                return Err(invalid(format!("hithink_http_error:{status}")));
            }
            let value = parsed.map_err(|_| invalid("hithink_invalid_json"))?;
            if code != 0 {
                if path == "/api/a-share/corporate-actions/adjustment-factors"
                    && let Some((_, symbol)) = params.iter().find(|(name, _)| *name == "thscode")
                    && let Ok(data) = action_payload(&value, symbol)
                {
                    return Ok(data);
                }
                return Err(invalid(format!("hithink_business_error:{code}")));
            }
            if !value["data"].is_object() {
                return Err(invalid("hithink_invalid_data"));
            }
            return Ok(value["data"].clone());
        }
        Err(invalid("hithink_retry_exhausted"))
    }
    fn pause(&self, attempt: usize) -> io::Result<()> {
        for _ in 0..(5 << attempt) {
            self.context.check()?;
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    }
    fn evidence(&mut self) -> Value {
        json!({"provider":"hithink","http_receipts":std::mem::take(&mut self.receipts)})
    }
    fn calendar(&mut self, range: &DateRange) -> io::Result<Vec<Date>> {
        let today = now().date_naive();
        let first = today
            .checked_sub_months(chrono::Months::new(12))
            .ok_or_else(|| invalid("date_overflow"))?;
        if range.start < first || range.end > today {
            return Err(invalid("hithink_calendar_coverage_insufficient"));
        }
        let data = self.get("/api/a-share/calendar/trading-days", &[])?;
        let mut dates = BTreeSet::new();
        for row in items(&data)? {
            let d = day(&row["date_ms"])?;
            if row["date"] != d.format("%Y%m%d").to_string() || !dates.insert(d) {
                return Err(invalid("hithink_calendar_date_conflict"));
            }
        }
        let dates: Vec<_> = dates.into_iter().filter(|d| range.contains(*d)).collect();
        if dates.is_empty() {
            return Err(invalid("hithink_empty_calendar"));
        }
        Ok(dates)
    }
    fn members(
        &mut self,
        request: &CollectionRequest,
    ) -> io::Result<BTreeMap<String, analytics_core::pool::PoolMembership>> {
        let mut result = BTreeMap::new();
        for name in &request.universes {
            let code = if name == "csi300" {
                "000300.SH"
            } else {
                "000905.SH"
            };
            let data = self.get(
                "/api/a-share-index/constituents/ths-stock-list",
                &[("thscode", code.into())],
            )?;
            check_identity(&data, code)?;
            let observed = chrono::DateTime::from_timestamp_millis(
                data["timestamp"]
                    .as_i64()
                    .ok_or_else(|| invalid("hithink_member_timestamp_missing"))?,
            )
            .ok_or_else(|| invalid("hithink_invalid_timestamp"))?
            .with_timezone(&zone())
            .date_naive();
            let coverage = DateRange {
                start: observed,
                end: observed,
            };
            let mut intervals = BTreeMap::new();
            for row in items(&data)? {
                let symbol = local_symbol(
                    row["thscode"]
                        .as_str()
                        .ok_or_else(|| invalid("hithink_member_code_missing"))?,
                )?;
                if intervals.insert(symbol, vec![coverage.clone()]).is_some() {
                    return Err(invalid("hithink_duplicate_member"));
                }
            }
            if intervals.is_empty() {
                return Err(invalid("hithink_empty_members"));
            }
            result.insert(
                name.clone(),
                analytics_core::pool::PoolMembership {
                    coverage,
                    intervals,
                },
            );
        }
        Ok(result)
    }
    fn history(
        &mut self,
        symbol: &str,
        range: &DateRange,
        index: bool,
        adjust: &str,
    ) -> io::Result<Vec<DailyBar>> {
        let code = thscode(symbol)?;
        if index
            && range.start
                < now()
                    .date_naive()
                    .checked_sub_months(chrono::Months::new(60))
                    .unwrap()
        {
            return Err(invalid("hithink_index_coverage_insufficient"));
        }
        let path = if index {
            "/api/a-share-index/prices/historical"
        } else {
            "/api/a-share/prices/historical"
        };
        let mut output = BTreeMap::new();
        let mut start = range.start;
        loop {
            let end = start
                .checked_add_months(chrono::Months::new(108))
                .unwrap_or(range.end)
                .min(range.end);
            let mut params = vec![
                ("thscode", code.clone()),
                ("interval", "1d".into()),
                ("start", millis(start).to_string()),
                ("end", millis(end).to_string()),
            ];
            if !index {
                params.push(("adjust", adjust.into()));
            }
            let data = self.get(path, &params)?;
            check_identity(&data, &code)?;
            if data
                .get("adjust")
                .is_some_and(|v| if index { !v.is_null() } else { v != adjust })
            {
                return Err(invalid("hithink_adjustment_mismatch"));
            }
            let rows = parse_bars(symbol, &data, &DateRange { start, end })?;
            merge_window(&mut output, &rows)?;
            if end == range.end {
                break;
            }
            // Use five actual common observations, rather than assuming five calendar days trade.
            if rows.len() < 5 {
                return Err(invalid("hithink_window_overlap_insufficient"));
            }
            let next = rows[rows.len() - 5].trade_date;
            if next <= start {
                return Err(invalid("hithink_window_no_progress"));
            }
            start = next;
        }
        Ok(output.into_values().collect())
    }
}
fn parse_bars(symbol: &str, data: &Value, range: &DateRange) -> io::Result<Vec<DailyBar>> {
    let mut rows = vec![];
    for row in items(data)? {
        check_identity(row, &thscode(symbol)?)?;
        let date = day(&row["date_ms"])?;
        if !range.contains(date) || rows.last().is_some_and(|r: &DailyBar| r.trade_date >= date) {
            return Err(invalid("hithink_duplicate_or_out_of_range_date"));
        }
        for key in [
            "open_price",
            "high_price",
            "low_price",
            "close_price",
            "volume",
            "turnover",
        ] {
            if row.get(key).is_none() {
                return Err(invalid("hithink_truncated_fields"));
            }
        }
        rows.push(DailyBar {
            instrument_id: symbol.into(),
            trade_date: date,
            open: numeric(&row["open_price"], true)?,
            high: numeric(&row["high_price"], true)?,
            low: numeric(&row["low_price"], true)?,
            close: numeric(&row["close_price"], true)?,
            adj_open: None,
            adj_high: None,
            adj_low: None,
            adj_close: None,
            volume: numeric(&row["volume"], false)?,
            amount: numeric(&row["turnover"], false)?,
        });
    }
    if rows.is_empty() {
        return Err(invalid("hithink_empty_history"));
    }
    Ok(rows)
}
fn merge_window(output: &mut BTreeMap<Date, DailyBar>, rows: &[DailyBar]) -> io::Result<()> {
    if !output.is_empty() && !rows.iter().any(|r| output.contains_key(&r.trade_date)) {
        return Err(invalid("hithink_window_overlap_missing"));
    }
    for row in rows {
        if let Some(old) = output.get(&row.trade_date) {
            if serde_json::to_value(old).map_err(invalid)?
                != serde_json::to_value(row).map_err(invalid)?
            {
                return Err(invalid("hithink_cross_window_conflict"));
            }
        } else {
            output.insert(row.trade_date, row.clone());
        }
    }
    Ok(())
}
fn pair_rows(raw: &mut [DailyBar], adjusted: &[DailyBar]) -> io::Result<()> {
    if raw.len() != adjusted.len() {
        return Err(invalid("hithink_adjusted_dates_mismatch"));
    }
    for (r, a) in raw.iter_mut().zip(adjusted) {
        if r.trade_date != a.trade_date
            || r.instrument_id != a.instrument_id
            || r.volume != a.volume
            || r.amount != a.amount
        {
            return Err(invalid("hithink_raw_adjusted_mismatch"));
        }
        r.adj_open = a.open;
        r.adj_high = a.high;
        r.adj_low = a.low;
        r.adj_close = a.close;
    }
    Ok(())
}
fn verify_checkpoint(root: &Path, files: &BTreeMap<String, String>) -> io::Result<()> {
    for (name, sha) in files {
        let p = Path::new(name);
        if p.is_absolute()
            || p.components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
            || digest(&fs::read(root.join(name))?) != *sha
        {
            return Err(invalid("hithink_checkpoint_hash_mismatch"));
        }
    }
    Ok(())
}
fn capture_files(root: &Path, names: &[String]) -> io::Result<BTreeMap<String, String>> {
    names
        .iter()
        .map(|name| Ok((name.clone(), digest(&fs::read(root.join(name))?))))
        .collect()
}
/// Collect HiThink data directly from Rust. No Python or third-party SDK is invoked.
pub fn collect(
    request: &CollectionRequest,
    root: &Path,
    context: &TaskContext,
    operation: &str,
) -> io::Result<PathBuf> {
    collect_with_calendar(request, root, context, operation, None)
}
/// Use a workflow-verified role-specific calendar for long history, retaining its source evidence.
/// The workflow copies the reference evidence into the published snapshot alongside quotations.
pub fn collect_with_calendar(
    request: &CollectionRequest,
    root: &Path,
    context: &TaskContext,
    operation: &str,
    calendar: Option<&[Date]>,
) -> io::Result<PathBuf> {
    request.validate()?;
    context.check()?;
    if request.provider != Provider::Hithink
        || !matches!(
            operation,
            "market" | "quotes" | "actions" | "calendar" | "members"
        )
    {
        return Err(invalid("unsupported_hithink_operation"));
    }
    for dir in ["raw", "market/bars", "market/adjustments"] {
        fs::create_dir_all(root.join(dir))?;
    }
    let clock = now();
    let closed_end = if clock.hour() < 16 {
        clock.date_naive().pred_opt().unwrap()
    } else {
        clock.date_naive()
    };
    let effective_end = if matches!(operation, "market" | "quotes") {
        request.end.min(closed_end)
    } else {
        request.end
    };
    if request.start > effective_end {
        return Err(invalid("hithink_no_completed_trading_date"));
    }
    let identity = json!({"schema_version":1,"operation":operation,"request":request,"collector_protocol":12,"collector_signature":producer_signature(),"calendar":calendar,"effective_end":effective_end,"normalization":HITHINK_NORMALIZATION});
    let owner = root.join("owner.json");
    if owner.exists() {
        if serde_json::from_slice::<Value>(&fs::read(&owner)?).map_err(invalid)? != identity {
            return Err(invalid("checkpoint owner/request mismatch"));
        }
    } else {
        save(&owner, &identity)?;
    }
    let key = crate::credentials::load_source("hithink")?;
    let mut client = Client {
        key,
        root,
        context,
        receipts: vec![],
        #[cfg(test)]
        test_base: None,
    };
    let range = DateRange {
        start: request.start,
        end: effective_end,
    };
    if matches!(operation, "calendar" | "members") {
        let data = if operation == "calendar" {
            json!({"calendar":client.calendar(&range)?})
        } else {
            json!({"universes":client.members(request)?})
        };
        let result =
            json!({"schema_version":1,"data":data,"evidence":client.evidence(),"owner":identity});
        let path = root.join("reference.json");
        save(&path, &result)?;
        return Ok(path);
    }
    if operation == "actions" {
        return actions(&mut client, request);
    }
    let dates = if let Some(dates) = calendar {
        if dates.is_empty() || dates.windows(2).any(|w| w[0] >= w[1]) {
            return Err(invalid("invalid_verified_calendar"));
        }
        dates
            .iter()
            .copied()
            .filter(|d| range.contains(*d))
            .collect::<Vec<_>>()
    } else {
        client.calendar(&range)?
    };
    if dates.is_empty() {
        return Err(invalid("empty_verified_calendar"));
    }
    let calendar_evidence = client.evidence();
    save(
        &root.join("raw/calendar-structured.json"),
        &calendar_evidence,
    )?;
    let universes = client.members(request)?;
    let member_evidence = client.evidence();
    save(&root.join("raw/members-structured.json"), &member_evidence)?;
    let mut anchors = BTreeMap::new();
    let mut errors = vec![];
    for id in &request.instruments {
        context.report(format!("HiThink 日线 {id}"))?;
        let checkpoint = root.join(format!("{id}-checkpoint.json"));
        if checkpoint.exists() {
            let files: BTreeMap<String, String> =
                serde_json::from_slice(&fs::read(&checkpoint)?).map_err(invalid)?;
            verify_checkpoint(root, &files)?;
        } else {
            let result = (|| -> io::Result<()> {
                let symbol = request.source_bindings.get(id).unwrap_or(id);
                let index = request.benchmarks.contains(id);
                let mut raw = client.history(symbol, &range, index, "none")?;
                let adjusted = if index {
                    raw.clone()
                } else {
                    client.history(symbol, &range, false, "forward")?
                };
                pair_rows(&mut raw, &adjusted)?;
                if raw
                    .iter()
                    .any(|r| dates.binary_search(&r.trade_date).is_err())
                {
                    return Err(invalid("hithink_bar_outside_calendar"));
                }
                for row in &mut raw {
                    row.instrument_id = id.clone();
                }
                let basis = if index {
                    AdjustmentBasis::PriceIndex {
                        source: "hithink.a-share-index.prices.historical".into(),
                    }
                } else {
                    AdjustmentBasis::SupplierForward {
                        verified_through: effective_end,
                    }
                };
                let adjustment = AdjustmentRecord {
                    schema_version: 3,
                    instrument_id: id.clone(),
                    source_symbol: symbol.clone(),
                    basis,
                };
                adjustment.validate()?;
                let evidence = client.evidence();
                let name = format!("raw/{id}-structured.json");
                save(&root.join(&name), &evidence)?;
                let bar_name = format!("market/bars/{id}.parquet");
                let part = root.join(format!("market/bars/{id}.part"));
                write_bars(&part, &raw)?;
                fs::rename(part, root.join(&bar_name))?;
                let adj_name = format!("market/adjustments/{id}.json");
                save(&root.join(&adj_name), &adjustment)?;
                let mut names = vec![name, bar_name, adj_name];
                for r in evidence["http_receipts"].as_array().unwrap() {
                    names.push(format!("raw/{}", r["filename"].as_str().unwrap()));
                }
                let files = capture_files(root, &names)?;
                context.check()?;
                save(&checkpoint, &files)
            })();
            if let Err(e) = result {
                context.check()?;
                errors.push(format!("{id}: {e}"));
                client.receipts.clear();
                continue;
            }
        }
        let adjustment = AdjustmentRecord::read(&root.join("market"), id)?;
        let bars = crate::daily::read_bars(&root.join(format!("market/bars/{id}.parquet")))?;
        let first = bars
            .iter()
            .find(|r| r.close.is_some())
            .ok_or_else(|| invalid("hithink_anchor_missing"))?;
        anchors.insert(id.clone(), adjustment.anchor(first.trade_date)?);
    }
    if !errors.is_empty() {
        save(&root.join("failures.json"), &errors)?;
        return Err(invalid(format!(
            "hithink_incomplete_batch:{}; inspect failures.json",
            errors.len()
        )));
    }
    let manifest = DailyManifest {
        schema_version: 6,
        calendar: dates,
        universes,
        instruments: request.instruments.clone(),
        price_basis: "unadjusted".into(),
        normalization: HITHINK_NORMALIZATION.into(),
        adjustment_anchors: anchors,
    };
    save(&root.join("market/daily_manifest.json"), &manifest)?;
    let mut mapping = analytics_core::source_map::SourceMap::default();
    for (id, symbol) in &request.source_bindings {
        if id != symbol {
            mapping.exceptions.insert(
                id.clone(),
                analytics_core::source_map::SourceException {
                    exchange_symbol: symbol.clone(),
                    kind: if request.benchmarks.contains(id) {
                        "price_index"
                    } else {
                        "a_share"
                    }
                    .into(),
                    coverage: range.clone(),
                    locators: vec![analytics_core::source_map::SourceLocator {
                        provider: "hithink".into(),
                        symbol: symbol.clone(),
                        coverage: range.clone(),
                        evidence: vec![],
                    }],
                },
            );
        }
    }
    save(&root.join("market/source_map.json"), &mapping)?;
    let hashes: BTreeMap<_, _> = fs::read_dir(root.join("raw"))?
        .map(|e| {
            let e = e?;
            Ok((
                e.file_name().to_string_lossy().into_owned(),
                digest(&fs::read(e.path())?),
            ))
        })
        .collect::<io::Result<_>>()?;
    save(
        &root.join("jfquant_source.json"),
        &json!({"downloader":"JFQuant analytics-storage","provider":"hithink","snapshot_verified":true,"independent_download":true,"request":request,"normalization":HITHINK_NORMALIZATION,"collector_signature":producer_signature(),"evidence_sha256":hashes,"calendar_external":calendar.is_some()}),
    )?;
    let dataset = crate::daily::DailyDataset::open(root.join("market"))?;
    for id in &request.instruments {
        dataset.validate_factor_basis(id)?;
    }
    context.check()?;
    Ok(root.join("market"))
}
pub(crate) fn action_receipt_url(url: &str, code: &str, range: &DateRange) -> bool {
    let suffix = format!(
        "/api/a-share/corporate-actions/adjustment-factors?thscode={code}&from={}&to={}",
        range.start, range.end
    );
    let matches = url == format!("{BASE}{suffix}");
    #[cfg(test)]
    let matches = matches
        || url
            .strip_prefix("http://127.0.0.1:")
            .and_then(|s| s.split_once('/'))
            .is_some_and(|(_, path)| format!("/{path}") == suffix);
    matches
}
/// Authenticate the action endpoint's explicit no-event response without treating
/// generic missing data, unknown codes, authentication or permissions as success.
pub(crate) fn action_payload(value: &Value, code: &str) -> io::Result<Value> {
    local_symbol(code)?;
    if value["code"] == 0 && value["data"].is_object() {
        return Ok(value["data"].clone());
    }
    if value["code"] == 3002
        && value["message"] == format!("No adjustment events for thscode={code}")
        && value.get("data").is_none_or(Value::is_null)
    {
        return Ok(
            json!({"thscode":code,"ticker":&code[..6],"item":[],"empty_business_code":3002}),
        );
    }
    Err(invalid("hithink_action_response_not_verified"))
}

fn actions(client: &mut Client<'_>, request: &CollectionRequest) -> io::Result<PathBuf> {
    let mut bundle = crate::corporate_actions::ActionBundle {
        schema_version: 6,
        source_bindings: Default::default(),
        structured_coverage: Default::default(),
        execution_issues: vec![],
        execution_data: None,
        coverage: Default::default(),
        downloader: "JFQuant analytics-storage corporate-actions".into(),
        downloaded_at: chrono::Utc::now().to_rfc3339(),
        queried_symbols: BTreeSet::new(),
        actions: vec![],
        failures: BTreeMap::new(),
        raw_sha256: BTreeMap::new(),
    };
    for id in &request.instruments {
        client.context.check()?;
        let symbol = request.source_bindings.get(id).unwrap_or(id);
        let code = thscode(symbol)?;
        let checkpoint = client.root.join(format!("{id}-action-checkpoint.json"));
        if checkpoint.exists() {
            let files: BTreeMap<String, String> =
                serde_json::from_slice(&fs::read(&checkpoint)?).map_err(invalid)?;
            verify_checkpoint(client.root, &files)?;
        }
        let result = (|| -> io::Result<()> {
            let name = format!("{id}-actions-structured.json");
            let saved = client.root.join("raw").join(&name);
            let shard: Value = if checkpoint.exists() {
                serde_json::from_slice(&fs::read(&saved)?).map_err(invalid)?
            } else {
                let data = client.get(
                    "/api/a-share/corporate-actions/adjustment-factors",
                    &[
                        ("thscode", code.clone()),
                        ("from", request.start.to_string()),
                        ("to", request.end.to_string()),
                    ],
                )?;
                check_identity(&data, &code)?;
                let (records, mut issues) = parse_action_events(
                    id,
                    &code,
                    &data,
                    &DateRange {
                        start: request.start,
                        end: request.end,
                    },
                )?;
                let mut evidence = client.evidence();
                evidence["history_complete"] = json!(true);
                evidence["source_rule"] = json!("hithink.events/per-share-v1");
                evidence["request_range"] = json!({"start":request.start,"end":request.end});
                evidence["events"] = data;
                for issue in &mut issues {
                    issue.evidence.push(name.clone());
                }
                let shard = json!({"instrument":id,"history_verified":true,"records":records,"execution_issues":issues,"evidence":evidence});
                save(&saved, &shard)?;
                let mut names = vec![format!("raw/{name}")];
                for r in shard["evidence"]["http_receipts"].as_array().unwrap() {
                    names.push(format!("raw/{}", r["filename"].as_str().unwrap()));
                }
                let files = capture_files(client.root, &names)?;
                client.context.check()?;
                save(&checkpoint, &files)?;
                shard
            };
            if shard["instrument"] != *id {
                return Err(invalid("hithink_action_checkpoint_identity"));
            }
            let evidence = &shard["evidence"];
            let records: Vec<analytics_core::execution::CorporateAction> =
                serde_json::from_value(shard["records"].clone()).map_err(invalid)?;
            bundle
                .raw_sha256
                .extend(crate::collector::verify_http_receipts(
                    client.root,
                    evidence,
                )?);
            bundle
                .raw_sha256
                .insert(name.clone(), digest(&fs::read(&saved)?));
            bundle.queried_symbols.insert(id.clone());
            bundle.structured_coverage.insert(
                id.clone(),
                crate::execution_data::StructuredCoverage {
                    dates: DateRange {
                        start: request.start,
                        end: request.end,
                    },
                    source: "hithink.events/per-share-v1".into(),
                    evidence: vec![name],
                },
            );
            bundle.actions.extend(records);
            if let Some(issues) = shard.get("execution_issues") {
                bundle.execution_issues.extend(
                    serde_json::from_value::<Vec<crate::execution_data::ExecutionIssue>>(
                        issues.clone(),
                    )
                    .map_err(invalid)?,
                );
            }
            if symbol != id {
                bundle.source_bindings.insert(
                    id.clone(),
                    analytics_core::source_map::SourceLocator {
                        provider: "hithink".into(),
                        symbol: symbol.clone(),
                        coverage: DateRange {
                            start: request.start,
                            end: request.end,
                        },
                        evidence: vec![],
                    },
                );
            }
            Ok(())
        })();
        if let Err(e) = result {
            client.context.check()?;
            bundle.failures.insert(id.clone(), e.to_string());
            client.receipts.clear();
        }
    }
    let complete = json!({"collector_signature":producer_signature(),"provider":"hithink","collector_protocol":12});
    save(
        &client.root.join("raw/complete-sources-structured.json"),
        &complete,
    )?;
    bundle.raw_sha256.insert(
        "complete-sources-structured.json".into(),
        digest(&serde_json::to_vec_pretty(&complete).map_err(invalid)?),
    );
    let path = client.root.join("actions.json");
    save(&path, &bundle)?;
    crate::corporate_actions::ActionBundle::read(&path)?;
    client.context.check()?;
    if !bundle.failures.is_empty() {
        return Err(invalid(
            "hithink_action_batch_incomplete: resume verified shards",
        ));
    }
    Ok(path)
}

type ParsedActionEvents = (
    Vec<analytics_core::execution::CorporateAction>,
    Vec<crate::execution_data::ExecutionIssue>,
);
fn parse_action_events(
    id: &str,
    code: &str,
    data: &Value,
    range: &DateRange,
) -> io::Result<ParsedActionEvents> {
    let mut issues = vec![];
    let mut dates = BTreeSet::new();
    let mut records = vec![];
    for row in items(data)? {
        check_identity(row, code)?;
        let ex = day(&row["ex_date_ms"])?;
        if !range.contains(ex) || !dates.insert(ex) {
            return Err(invalid("hithink_event_duplicate_or_out_of_range"));
        }
        let cash = numeric(&row["dividend_per_share"], false)?
            .ok_or_else(|| invalid("hithink_event_terms_missing"))?;
        let shares = numeric(&row["per_share_bonus"], false)?
            .ok_or_else(|| invalid("hithink_event_terms_missing"))?;
        if cash == 0. && shares == 0. {
            issues.push(crate::execution_data::ExecutionIssue {id:format!("hithink-zero:{id}:{ex}"),instrument:id.into(),announced:None,dates:DateRange{start:ex,end:ex},code:"hithink_event_type_unknown".into(),event_category:"unknown".into(),evidence:vec![],message:"HiThink zero-cash zero-bonus event requires independent classification; no rights or dates inferred".into()});
            continue;
        }
        records.push(analytics_core::execution::CorporateAction {
            id: format!("{id}:{ex}"),
            instrument: id.into(),
            announced: None,
            record_date: None,
            ex_date: ex,
            pay_date: None,
            listing_date: None,
            cash_per_share: row["dividend_per_share"].to_string(),
            shares_per_share: row["per_share_bonus"].to_string(),
            terms: Default::default(),
            source: format!(
                "{BASE}/api/a-share/corporate-actions/adjustment-factors?thscode={code}"
            ),
        });
    }
    Ok((records, issues))
}

#[cfg(test)]
fn parse_actions(
    id: &str,
    code: &str,
    data: &Value,
    range: &DateRange,
) -> io::Result<Vec<analytics_core::execution::CorporateAction>> {
    Ok(parse_action_events(id, code, data, range)?.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{Read, Write},
        net::TcpListener,
    };
    fn fixture() -> Value {
        json!({"thscode":"600000.SH","item":[{"date_ms":millis("2024-01-02".parse().unwrap()),"open_price":10.,"high_price":11.,"low_price":9.,"close_price":10.,"volume":1000.,"turnover":10000.}]})
    }
    fn range() -> DateRange {
        DateRange {
            start: "2024-01-02".parse().unwrap(),
            end: "2024-01-05".parse().unwrap(),
        }
    }
    #[test]
    fn explicit_empty_actions_and_unknown_zero_events_keep_evidence_semantics() {
        let empty = json!({"code":3002,"message":"No adjustment events for thscode=600000.SH"});
        let data = action_payload(&empty, "600000.SH").unwrap();
        assert!(data["item"].as_array().unwrap().is_empty());
        assert!(action_payload(&empty, "000001.SZ").is_err());
        assert!(action_payload(&json!({"code":3002,"message":"no data"}), "600000.SH").is_err());
        let events = json!({"thscode":"600000.SH","item":[{"ex_date_ms":millis(range().start),"dividend_per_share":0,"per_share_bonus":0},{"ex_date_ms":millis(range().end),"dividend_per_share":0.2,"per_share_bonus":0}]});
        let (ordinary, issues) =
            parse_action_events("SH600000", "600000.SH", &events, &range()).unwrap();
        assert_eq!(ordinary.len(), 1);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].event_category, "unknown");
        assert!(!issues[0].skipped_by_engine());
    }
    #[test]
    fn codes_exchange_dates_units_null_and_duplicates_are_strict() {
        for symbol in ["SH600000", "SZ000001", "BJ430047"] {
            assert_eq!(local_symbol(&thscode(symbol).unwrap()).unwrap(), symbol);
        }
        assert!(thscode("600000.SH").is_err());
        let rows = parse_bars("SH600000", &fixture(), &range()).unwrap();
        assert_eq!(rows[0].volume, Some(1000.));
        assert_eq!(rows[0].amount, Some(10000.));
        assert_eq!(rows[0].trade_date, range().start);
        let mut data = fixture();
        data["item"][0]["open_price"] = Value::Null;
        assert_eq!(
            parse_bars("SH600000", &data, &range()).unwrap()[0].open,
            None
        );
        let duplicate = data["item"][0].clone();
        data["item"].as_array_mut().unwrap().push(duplicate);
        assert!(parse_bars("SH600000", &data, &range()).is_err());
        let mut data = fixture();
        data["item"][0]["date_ms"] = json!(millis(range().start) + 1);
        assert!(parse_bars("SH600000", &data, &range()).is_err());
        let mut data = fixture();
        data["thscode"] = json!("000001.SZ");
        assert!(check_identity(&data, "600000.SH").is_err());
        data = fixture();
        data["total"] = json!(2);
        assert!(parse_bars("SH600000", &data, &range()).is_err());
        data = fixture();
        data["item"][0].as_object_mut().unwrap().remove("turnover");
        assert!(parse_bars("SH600000", &data, &range()).is_err());
    }
    #[test]
    fn window_conflicts_and_missing_overlap_fail_without_rebasing_prices() {
        let rows = parse_bars("SH600000", &fixture(), &range()).unwrap();
        let mut output = BTreeMap::new();
        merge_window(&mut output, &rows).unwrap();
        merge_window(&mut output, &rows).unwrap();
        assert_eq!(output.len(), 1);
        let mut changed = rows.clone();
        changed[0].close = Some(12.);
        assert!(merge_window(&mut output, &changed).is_err());
        changed[0].trade_date = range().end;
        assert!(merge_window(&mut output, &changed).is_err());
        let mut raw = rows.clone();
        let mut adjusted = rows.clone();
        adjusted[0].close = Some(5.);
        pair_rows(&mut raw, &adjusted).unwrap();
        assert_eq!(raw[0].close, Some(10.));
        assert_eq!(raw[0].adj_close, Some(5.));
        adjusted[0].volume = Some(999.);
        assert!(pair_rows(&mut raw, &adjusted).is_err());
    }
    #[test]
    fn supplier_fields_keep_raw_execution_and_provenance_separate() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("market");
        fs::create_dir_all(root.join("bars")).unwrap();
        fs::create_dir_all(root.join("adjustments")).unwrap();
        let mut raw = parse_bars("SH600000", &fixture(), &range()).unwrap();
        let mut adjusted = raw.clone();
        adjusted[0].open = Some(5.);
        adjusted[0].close = Some(5.);
        adjusted[0].high = Some(5.5);
        adjusted[0].low = Some(4.5);
        pair_rows(&mut raw, &adjusted).unwrap();
        write_bars(&root.join("bars/SH600000.parquet"), &raw).unwrap();
        let record = AdjustmentRecord {
            schema_version: 3,
            instrument_id: "SH600000".into(),
            source_symbol: "SH600000".into(),
            basis: AdjustmentBasis::SupplierForward {
                verified_through: range().end,
            },
        };
        save(&root.join("adjustments/SH600000.json"), &record).unwrap();
        let manifest = DailyManifest {
            schema_version: 6,
            calendar: vec![range().start],
            instruments: BTreeSet::from(["SH600000".into()]),
            universes: BTreeMap::new(),
            price_basis: "unadjusted".into(),
            normalization: HITHINK_NORMALIZATION.into(),
            adjustment_anchors: BTreeMap::from([(
                "SH600000".into(),
                record.anchor(range().start).unwrap(),
            )]),
        };
        save(&root.join("daily_manifest.json"), &manifest).unwrap();
        save(
            &temp.path().join("jfquant_source.json"),
            &json!({"provider":"hithink","snapshot_verified":true}),
        )
        .unwrap();
        let data = crate::daily::DailyDataset::open(&root).unwrap();
        assert!(data.supports_provider_training());
        data.validate_factor_basis("SH600000").unwrap();
        assert_eq!(data.field("SH600000", "close").unwrap().get(0), Some(5.));
        assert_eq!(
            data.raw_field("SH600000", "close").unwrap().get(0),
            Some(10.)
        );
        assert_eq!(
            data.field("SH600000", "volume").unwrap().get(0),
            Some(1000.)
        );
        assert_eq!(
            data.field("SH600000", "amount").unwrap().get(0),
            Some(10000.)
        );
        assert_eq!(data.field("SH600000", "vwap").unwrap().get(0), None);
        assert_eq!(data.adjustment_factors("SH600000").unwrap().get(0), None);
        assert_eq!(
            data.price_inputs(&BTreeSet::from(["SH600000".into()]))
                .unwrap()["normalization"],
            HITHINK_NORMALIZATION
        );
        let mut manifest = manifest;
        manifest.normalization = crate::adjustment::NORMALIZATION.into();
        save(&root.join("daily_manifest.json"), &manifest).unwrap();
        assert!(crate::daily::DailyDataset::open(&root).is_err());
    }
    #[test]
    fn action_terms_do_not_invent_settlement_dates_or_empty_event_types() {
        let data = json!({"item":[{"ticker":"600000","ex_date_ms":millis(range().start),"dividend_per_share":0.42,"per_share_bonus":0.1}]});
        let records = parse_actions("SH600000", "600000.SH", &data, &range()).unwrap();
        assert_eq!(records[0].cash_per_share, "0.42");
        assert!(
            records[0].pay_date.is_none()
                && records[0].record_date.is_none()
                && records[0].listing_date.is_none()
        );
        let mut data = data;
        data["item"][0]["dividend_per_share"] = json!(0);
        data["item"][0]["per_share_bonus"] = json!(0);
        let (ordinary, issues) =
            parse_action_events("SH600000", "600000.SH", &data, &range()).unwrap();
        assert!(ordinary.is_empty());
        assert_eq!(issues.len(), 1);
        assert!(!issues[0].skipped_by_engine());
    }
    fn server(replies: Vec<(u16, Value)>) -> (String, std::thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut requests = vec![];
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut buffer = [0; 8192];
                let count = stream.read(&mut buffer).unwrap();
                requests.push(String::from_utf8_lossy(&buffer[..count]).into_owned());
                let body = serde_json::to_vec(&body).unwrap();
                write!(
                    stream,
                    "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                stream.write_all(&body).unwrap();
            }
            requests
        });
        (url, handle)
    }
    #[test]
    fn real_http_transport_retries_business_limits_and_keeps_key_out_of_evidence() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("raw")).unwrap();
        let context = TaskContext::default();
        let (base, server) = server(vec![
            (200, json!({"code":4001,"data":null})),
            (200, json!({"code":0,"data":{"item":[]}})),
        ]);
        let mut client = Client {
            key: "fixture-private-hithink-key".into(),
            root: temp.path(),
            context: &context,
            receipts: vec![],
            test_base: Some(base),
        };
        assert!(client.get("/api/a-share/prices/snapshot", &[]).is_ok());
        assert_eq!(client.receipts.len(), 2);
        let requests = server.join().unwrap();
        assert!(
            requests
                .iter()
                .all(|s| s.contains("X-api-key: fixture-private-hithink-key"))
        );
        for entry in fs::read_dir(temp.path().join("raw")).unwrap() {
            assert!(
                !String::from_utf8_lossy(&fs::read(entry.unwrap().path()).unwrap())
                    .contains("fixture-private-hithink-key")
            );
        }
        let (base, server) = self::server(vec![(200, json!({"code":2003,"data":null}))]);
        client.test_base = Some(base);
        assert!(
            client
                .get("/api/a-share/prices/snapshot", &[])
                .unwrap_err()
                .to_string()
                .contains("2003")
        );
        assert_eq!(server.join().unwrap().len(), 1);
    }
    #[test]
    fn authentication_echo_and_cancel_are_rejected_and_checkpoint_tampering_fails() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("raw")).unwrap();
        let context = TaskContext::default();
        let (base, server) = server(vec![(
            200,
            json!({"code":0,"data":{"echo":"fixture-private-hithink-key"}}),
        )]);
        let mut client = Client {
            key: "fixture-private-hithink-key".into(),
            root: temp.path(),
            context: &context,
            receipts: vec![],
            test_base: Some(base),
        };
        assert!(client.get("/api/a-share/prices/snapshot", &[]).is_err());
        server.join().unwrap();
        assert_eq!(fs::read_dir(temp.path().join("raw")).unwrap().count(), 0);
        fs::write(temp.path().join("data"), "original").unwrap();
        let hashes = capture_files(temp.path(), &["data".into()]).unwrap();
        verify_checkpoint(temp.path(), &hashes).unwrap();
        fs::write(temp.path().join("data"), "tampered").unwrap();
        assert!(verify_checkpoint(temp.path(), &hashes).is_err());
        context
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            client
                .get("/api/a-share/prices/snapshot", &[])
                .unwrap_err()
                .kind(),
            io::ErrorKind::Interrupted
        );
    }
    #[test]
    fn action_batch_resumes_complete_security_evidence_and_rejects_tampering() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("raw")).unwrap();
        let context = TaskContext::default();
        let data = |code: &str| json!({"code":0,"data":{"thscode":code,"ticker":&code[..6],"item":[{"ticker":&code[..6],"ex_date_ms":millis(range().start),"dividend_per_share":0.42,"per_share_bonus":0}]}});
        let request = CollectionRequest {
            provider: Provider::Hithink,
            adjustment_source: crate::adjustment::AdjustmentSource::HithinkForward,
            source_bindings: BTreeMap::new(),
            start: range().start,
            end: range().end,
            instruments: BTreeSet::from(["SH600000".into(), "SZ000001".into()]),
            benchmarks: BTreeSet::new(),
            universes: BTreeSet::new(),
        };
        let (base, first_server) = server(vec![
            (200, data("600000.SH")),
            (200, json!({"code":2003,"data":null})),
        ]);
        let mut client = Client {
            key: "fixture-private-hithink-key".into(),
            root: temp.path(),
            context: &context,
            receipts: vec![],
            test_base: Some(base),
        };
        assert!(actions(&mut client, &request).is_err());
        assert_eq!(first_server.join().unwrap().len(), 2);
        assert!(
            temp.path()
                .join("SH600000-action-checkpoint.json")
                .is_file()
        );
        let (base, second_server) = server(vec![(200, data("000001.SZ"))]);
        client.test_base = Some(base);
        let path = actions(&mut client, &request).unwrap();
        assert_eq!(second_server.join().unwrap().len(), 1);
        let book = crate::corporate_actions::ActionBundle::read(&path).unwrap();
        assert_eq!(book.actions.len(), 2);
        assert!(book.failures.is_empty());
        fs::write(
            temp.path().join("raw/SH600000-actions-structured.json"),
            b"tampered",
        )
        .unwrap();
        assert!(actions(&mut client, &request).is_err());
    }
    #[test]
    fn captured_empty_and_unknown_events_survive_bundle_verification_and_tampering_is_rejected() {
        for zero in [false, true] {
            let temp = tempfile::tempdir().unwrap();
            fs::create_dir(temp.path().join("raw")).unwrap();
            let context = TaskContext::default();
            let body = if zero {
                json!({"code":0,"data":{"thscode":"600000.SH","ticker":"600000","item":[{"ticker":"600000","ex_date_ms":millis(range().start),"dividend_per_share":0,"per_share_bonus":0}]}})
            } else {
                json!({"code":3002,"message":"No adjustment events for thscode=600000.SH"})
            };
            let (base, server) = server(vec![(200, body)]);
            let mut client = Client {
                key: "fixture-private-hithink-key".into(),
                root: temp.path(),
                context: &context,
                receipts: vec![],
                test_base: Some(base),
            };
            let request = CollectionRequest {
                provider: Provider::Hithink,
                adjustment_source: crate::adjustment::AdjustmentSource::HithinkForward,
                source_bindings: BTreeMap::new(),
                start: range().start,
                end: range().end,
                instruments: ["SH600000".into()].into(),
                benchmarks: BTreeSet::new(),
                universes: BTreeSet::new(),
            };
            let path = actions(&mut client, &request).unwrap();
            assert_eq!(server.join().unwrap().len(), 1);
            let book = crate::corporate_actions::ActionBundle::read(&path).unwrap();
            assert!(book.actions.is_empty());
            assert_eq!(book.execution_issues.len(), usize::from(zero));
            assert!(book.structured_coverage.contains_key("SH600000"));
            let raw = book
                .raw_sha256
                .keys()
                .find(|name| name.ends_with(".body"))
                .unwrap();
            fs::write(temp.path().join("raw").join(raw), b"tampered").unwrap();
            assert!(crate::corporate_actions::ActionBundle::read(&path).is_err());
        }
    }
    #[test]
    fn old_providers_cannot_deserialize_and_old_source_policy_has_no_fallback() {
        for name in ["akshare", "yfinance", "tushare"] {
            assert!(serde_json::from_value::<Provider>(json!(name)).is_err());
        }
        assert_eq!(Provider::default(), Provider::Hithink);
    }
}
