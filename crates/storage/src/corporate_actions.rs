//! JFQuant-owned public distribution downloader. Incomplete dates stay null.
//! Provider success is recorded separately from execution eligibility.
mod aastocks;
mod checkpoints;
pub use aastocks::{fetch_pay_dates, repair_pay_dates};

use analytics_core::{Date, execution::CorporateAction, market_rules::classify};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::Path,
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
fn task_context_check() -> io::Result<()> {
    analytics_core::task::TaskContext::current().check()
}
fn invalid(message: impl std::fmt::Display) -> io::Error {
    io::Error::other(message.to_string())
}
const HISTORY: &str = "https://datacenter-web.eastmoney.com/api/data/v1/get";
const SETTLEMENT: &str = "https://emweb.securities.eastmoney.com/PC_HSF10/BonusFinancing/PageAjax";

/// Versioned public-download bundle. Missing dates never count as complete.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionBundle {
    /// JSON schema version.
    pub schema_version: u32,
    /// JFQuant downloader identity.
    pub downloader: String,
    /// Acquisition time in UTC.
    pub downloaded_at: String,
    /// Successfully queried complete history pages, including genuine empty histories.
    pub queried_symbols: BTreeSet<String>,
    /// Implemented distributions, potentially missing pay/listing dates.
    pub actions: Vec<CorporateAction>,
    /// Failed symbols and reasons; must be retried before using their history.
    pub failures: BTreeMap<String, String>,
    /// Saved raw JSON filename to SHA256, for independent provenance review.
    pub raw_sha256: BTreeMap<String, String>,
}
impl ActionBundle {
    fn validate_identity(&self) -> io::Result<()> {
        if self.schema_version != 1
            || (self.downloader != "JFQuant analytics-storage corporate-actions"
                && self.downloader != "JFQuant analytics-workflow synthetic-demo-v1")
        {
            return Err(invalid("invalid action bundle identity"));
        }
        for id in self.queried_symbols.iter().chain(self.failures.keys()) {
            classify(id).map_err(invalid)?;
        }
        for a in &self.actions {
            classify(&a.instrument).map_err(invalid)?;
            if a.id != format!("{}:{}", a.instrument, a.ex_date) {
                return Err(invalid("corporate action ID/security/date mismatch"));
            }
        }
        for name in self.raw_sha256.keys() {
            if Path::new(name).file_name().and_then(|s| s.to_str()) != Some(name.as_str()) {
                return Err(invalid("unsafe evidence filename"));
            }
        }
        Ok(())
    }
    /// Require all symbols and settlement dates within a proposed execution range.
    pub fn require_coverage(
        &self,
        symbols: &BTreeSet<String>,
        start: Date,
        end: Date,
    ) -> io::Result<()> {
        self.validate_identity()?;
        if start > end {
            return Err(invalid("invalid action bundle identity/range"));
        }
        for id in symbols {
            if !self.queried_symbols.contains(id) || self.failures.contains_key(id) {
                return Err(invalid(format!("company-action history unavailable: {id}")));
            }
        }
        let mut seen = BTreeSet::new();
        for a in &self.actions {
            if !seen.insert(a.id.clone()) {
                return Err(invalid("duplicate corporate-action event"));
            }
            if symbols.contains(&a.instrument) && a.ex_date >= start && a.record_date <= end {
                a.validate()
                    .map_err(|e| invalid(format!("{} {}: {e}", a.instrument, a.ex_date)))?;
            }
        }
        Ok(())
    }
    /// Read without silently upgrading a historical schema.
    pub fn read(path: &Path) -> io::Result<Self> {
        let bundle: Self = serde_json::from_slice(&fs::read(path)?).map_err(invalid)?;
        bundle.validate_identity()?;
        Ok(bundle)
    }
}
fn date(row: &Value, key: &str) -> Option<Date> {
    row[key]
        .as_str()
        .and_then(|s| s.get(..10))
        .and_then(|s| s.parse().ok())
}
fn number(row: &Value, key: &str) -> io::Result<f64> {
    if row[key].is_null() {
        return Ok(0.0);
    }
    row[key]
        .as_f64()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .ok_or_else(|| invalid(format!("invalid {key}")))
}
fn parse_history(
    symbol: &str,
    history: &Value,
    settlement: &Value,
) -> io::Result<Vec<CorporateAction>> {
    if history["success"] != true {
        return Err(invalid(format!(
            "history API failed: {}",
            history["message"]
        )));
    }
    let result = &history["result"];
    let rows = result["data"]
        .as_array()
        .ok_or_else(|| invalid("history rows missing; not a verified empty history"))?;
    if result["pages"].as_u64().unwrap_or(1) > 1
        || result["count"].as_u64() != Some(rows.len() as u64)
    {
        return Err(invalid("history pagination/count mismatch"));
    }
    let pay_rows = settlement["fhyx"]
        .as_array()
        .ok_or_else(|| invalid("settlement rows missing"))?;
    let mut actions = Vec::new();
    for row in rows {
        if row["ASSIGN_PROGRESS"] != "实施分配" {
            continue;
        }
        let security = row["SECUCODE"]
            .as_str()
            .ok_or_else(|| invalid("missing source security"))?;
        if security != format!("{}.{}", &symbol[2..], &symbol[..2]) {
            return Err(invalid("source security mismatch"));
        }
        let cash = number(row, "PRETAX_BONUS_RMB")? / 10.0;
        let components = number(row, "BONUS_RATIO")? + number(row, "IT_RATIO")?;
        let total = number(row, "BONUS_IT_RATIO")?;
        if components > 0.0 && total > 0.0 && (components - total).abs() > 1e-6 {
            return Err(invalid("combined and component bonus ratios disagree"));
        }
        let shares = components.max(total) / 10.0;
        if cash + shares == 0.0 {
            continue;
        }
        let ex = date(row, "EX_DIVIDEND_DATE")
            .ok_or_else(|| invalid("implemented event missing ex date"))?;
        let record = date(row, "EQUITY_RECORD_DATE")
            .ok_or_else(|| invalid("implemented event missing record date"))?;
        let pay = pay_rows
            .iter()
            .find(|p| {
                date(p, "EX_DIVIDEND_DATE") == Some(ex)
                    && date(p, "EQUITY_RECORD_DATE") == Some(record)
            })
            .and_then(|p| date(p, "PAY_CASH_DATE"));
        actions.push(CorporateAction {
            id: format!("{symbol}:{ex}"),
            instrument: symbol.into(),
            announced: date(row, "NOTICE_DATE")
                .ok_or_else(|| invalid("implementation announcement date missing"))?,
            record_date: record,
            ex_date: ex,
            pay_date: pay,
            listing_date: None,
            cash_per_share: cash,
            shares_per_share: shares,
            source: format!("{HISTORY}?reportName=RPT_SHAREBONUS_DET; {SETTLEMENT}?code={symbol}"),
        });
    }
    Ok(actions)
}
fn fetch(url: &str, params: &[String], path: &Path) -> io::Result<Value> {
    let mut command = Command::new("/usr/bin/curl");
    command.args([
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--retry",
        "2",
        "--max-time",
        "45",
        "--connect-timeout",
        "15",
        "--get",
        url,
    ]);
    for param in params {
        command.args(["--data-urlencode", param]);
    }
    let output = crate::transport::curl(&mut command)?;
    if !output.status.success() {
        return Err(invalid(format!(
            "public download failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    let json: Value = serde_json::from_slice(&output.stdout).map_err(invalid)?;
    fs::write(path, &output.stdout)?;
    Ok(json)
}
/// Download fresh public histories for explicit symbols, using four bounded workers.
/// Saves raw responses and a bundle even when individual symbols fail. Never uses
/// another project or previous download; choose a new output directory per snapshot.
pub fn fetch_public(symbols: &[String], output: &Path) -> io::Result<ActionBundle> {
    fetch_histories(symbols, output, false)
}
/// Continue JFQuant-owned symbol checkpoints after interruption.
pub fn fetch_public_resumable(symbols: &[String], output: &Path) -> io::Result<ActionBundle> {
    fetch_histories(symbols, output, true)
}
fn fetch_histories(symbols: &[String], output: &Path, resume: bool) -> io::Result<ActionBundle> {
    if symbols.is_empty() {
        return Err(invalid("no symbols requested"));
    }
    let mut unique = BTreeSet::new();
    for id in symbols {
        classify(id).map_err(invalid)?;
        if !unique.insert(id) {
            return Err(invalid("duplicate requested symbol"));
        }
    }
    let owner = serde_json::json!({"schema_version":1,"downloader":"JFQuant analytics-storage company-actions","symbols":symbols});
    if resume && output.exists() {
        let saved: Value =
            serde_json::from_slice(&fs::read(output.join("owner.json"))?).map_err(invalid)?;
        if saved != owner {
            return Err(invalid("action staging owner/symbols mismatch"));
        }
    } else {
        fs::create_dir(output)?;
        fs::create_dir(output.join("raw"))?;
        fs::create_dir(output.join("checkpoints"))?;
        fs::write(
            output.join("owner.json"),
            serde_json::to_vec(&owner).map_err(invalid)?,
        )?;
    }
    let index = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let task_context = analytics_core::task::TaskContext::current();
    std::thread::scope(|scope| {
        for _ in 0..4.min(symbols.len()) {
            scope.spawn(|| {
                let _scope = task_context.install();
                loop {
                    if task_context.check().is_err() {
                        break;
                    }
                    let i = index.fetch_add(1, Ordering::Relaxed);
                    let Some(symbol) = symbols.get(i) else { break };
                    let result: io::Result<Vec<CorporateAction>> = (|| {
                        if resume && let Ok(checkpoint) = read_checkpoint(output, symbol) {
                            return Ok(checkpoint);
                        }
                        let history = fetch(
                            HISTORY,
                            &[
                                "reportName=RPT_SHAREBONUS_DET".into(),
                                "columns=ALL".into(),
                                "pageSize=500".into(),
                                "pageNumber=1".into(),
                                format!("filter=(SECUCODE=\"{}.{}\")", &symbol[2..], &symbol[..2]),
                            ],
                            &output.join(format!("raw/{symbol}-history.json")),
                        )?;
                        let settlement = fetch(
                            SETTLEMENT,
                            &[format!("code={symbol}")],
                            &output.join(format!("raw/{symbol}-settlement.json")),
                        )?;
                        let actions = parse_history(symbol, &history, &settlement)?;
                        save_checkpoint(output, symbol, &actions)?;
                        Ok(actions)
                    })();
                    if i.is_multiple_of(100) {
                        let _ = task_context.report(format!("公司行动 {i}/{}", symbols.len()));
                    }
                    results
                        .lock()
                        .expect("worker result mutex")
                        .push((symbol.clone(), result));
                }
            });
        }
    });
    task_context_check()?;
    let mut bundle = ActionBundle {
        schema_version: 1,
        downloader: "JFQuant analytics-storage corporate-actions".into(),
        downloaded_at: chrono::Utc::now().to_rfc3339(),
        queried_symbols: BTreeSet::new(),
        actions: Vec::new(),
        failures: BTreeMap::new(),
        raw_sha256: BTreeMap::new(),
    };
    for (symbol, result) in results.into_inner().map_err(|_| invalid("worker panic"))? {
        match result {
            Ok(actions) => {
                bundle.queried_symbols.insert(symbol);
                bundle.actions.extend(actions);
            }
            Err(e) => {
                bundle.failures.insert(symbol, e.to_string());
            }
        }
    }
    bundle.actions.sort_by(|a, b| a.id.cmp(&b.id));
    for entry in fs::read_dir(output.join("raw"))? {
        let p = entry?.path();
        bundle.raw_sha256.insert(
            p.file_name().unwrap().to_string_lossy().into(),
            format!("sha256:{:x}", Sha256::digest(fs::read(p)?)),
        );
    }
    fs::write(
        output.join("actions.json"),
        serde_json::to_vec_pretty(&bundle).map_err(invalid)?,
    )?;
    let incomplete: Vec<_> = bundle
        .actions
        .iter()
        .filter_map(|a| a.validate().err().map(|e| format!("{}: {e}", a.id)))
        .collect();
    fs::write(output.join("coverage.json"),serde_json::to_vec_pretty(&serde_json::json!({"symbols_requested":symbols.len(),"symbols_downloaded":bundle.queried_symbols.len(),"failures":bundle.failures,"events":bundle.actions.len(),"incomplete_events":incomplete,"complete_for_execution":incomplete.is_empty() && bundle.failures.is_empty(),"limitations":"F10 payment dates cover recent records only; history endpoint lacks share listing dates. Nulls require announcement-backed supplements; never substitute ex date."})).map_err(invalid)?)?;
    Ok(bundle)
}

fn cells(row: &str) -> Vec<String> {
    row.split("<td")
        .skip(1)
        .filter_map(|cell| {
            let content = cell.split_once('>')?.1.split("</td>").next()?;
            let mut inside = false;
            Some(
                content
                    .chars()
                    .filter(|c| {
                        if *c == '<' {
                            inside = true;
                            false
                        } else if *c == '>' {
                            inside = false;
                            false
                        } else {
                            !inside
                        }
                    })
                    .collect::<String>()
                    .trim()
                    .to_string(),
            )
        })
        .collect()
}
fn decode_table(bytes: &[u8], marker: &[u8]) -> io::Result<String> {
    let start = bytes
        .windows(marker.len())
        .position(|w| w == marker)
        .ok_or_else(|| invalid("provider table marker missing"))?;
    let end = bytes[start..]
        .windows(8)
        .position(|w| w == b"</table>")
        .map(|n| start + n + 8)
        .ok_or_else(|| invalid("provider table end missing"))?;
    let (html, _, errors) = encoding_rs::GBK.decode(&bytes[start..end]);
    if errors {
        return Err(invalid("invalid encoding inside provider table"));
    }
    Ok(html.into_owned())
}

fn parse_sina(symbol: &str, html: &str) -> io::Result<Vec<CorporateAction>> {
    let table = html
        .split("id=\"sharebonus_1\"")
        .nth(1)
        .and_then(|s| s.split("</table>").next())
        .ok_or_else(|| invalid("Sina distribution table missing"))?;
    if !table.contains("红股上市日") {
        return Err(invalid("Sina distribution schema changed"));
    }
    let body = table
        .split("<tbody>")
        .nth(1)
        .ok_or_else(|| invalid("Sina rows missing"))?;
    let mut out = Vec::new();
    for row in body.split("<tr>").skip(1) {
        let c = cells(row);
        if c.len() == 1 && c[0].contains("暂时没有数据") {
            continue;
        }
        if c.len() != 9 {
            return Err(invalid("Sina distribution row schema changed"));
        }
        if c[4] != "实施" {
            continue;
        }
        let numeric = |index: usize| -> io::Result<f64> {
            c[index]
                .parse::<f64>()
                .ok()
                .filter(|v| v.is_finite() && *v >= 0.0)
                .ok_or_else(|| invalid("invalid Sina distribution quantity"))
        };
        let cash = numeric(3)? / 10.0;
        let shares = (numeric(1)? + numeric(2)?) / 10.0;
        if cash + shares == 0.0 {
            continue;
        }
        let d = |i: usize| -> io::Result<Date> {
            c[i].parse().map_err(|e| {
                invalid(format!(
                    "{symbol}: invalid Sina date column {i} {:?} (announcement {}): {e}",
                    c[i], c[0]
                ))
            })
        };
        let ex = d(5)?;
        out.push(CorporateAction {
            id: format!("{symbol}:{ex}"),
            instrument: symbol.into(),
            announced: d(0)?,
            record_date: d(6)?,
            ex_date: ex,
            pay_date: None,
            listing_date: c[7].parse().ok(),
            cash_per_share: cash,
            shares_per_share: shares,
            source: format!(
                "https://money.finance.sina.com.cn/corp/go.php/vISSUE_ShareBonus/stockid/{}.phtml",
                &symbol[2..]
            ),
        });
    }
    if out.is_empty() && !body.contains("暂时没有数据") && !body.contains("<td>") {
        return Err(invalid("unverified empty Sina table"));
    }
    Ok(out)
}
/// Independently fetch a second public history for delisted names and listing dates.
/// Merges only matching distributions. Conflicting rates/dates are reported; old
/// cash payment dates still require announcement-backed supplements.
pub fn enrich_public(
    source: &ActionBundle,
    source_root: &Path,
    output: &Path,
) -> io::Result<ActionBundle> {
    source.validate_identity()?;
    if source.schema_version != 1
        || source.downloader != "JFQuant analytics-storage corporate-actions"
    {
        return Err(invalid("invalid source bundle"));
    }
    let symbols: Vec<_> = source
        .queried_symbols
        .iter()
        .chain(source.failures.keys())
        .cloned()
        .collect();
    for id in &symbols {
        classify(id).map_err(invalid)?;
    }
    checkpoints::prepare(source, source_root, output, "enrich", Value::Null)?;

    let index = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let task_context = analytics_core::task::TaskContext::current();
    std::thread::scope(|scope| {
        for _ in 0..4.min(symbols.len()) {
            scope.spawn(||{let _scope=task_context.install(); loop{
            if task_context.check().is_err(){break;}
            let i=index.fetch_add(1,Ordering::Relaxed);let Some(id)=symbols.get(i) else{break};
            let result=(||{
                let url=format!("https://money.finance.sina.com.cn/corp/go.php/vISSUE_ShareBonus/stockid/{}.phtml",&id[2..]);
                let bytes=checkpoints::page(&url,Command::new("/usr/bin/curl").args(["--fail","--silent","--show-error","--location","--retry","2","--max-time","45","--connect-timeout","15",&url]),&output.join(format!("raw/{id}-sina.html")))?;
                let html=decode_table(&bytes,b"id=\"sharebonus_1\"")?;
                parse_sina(id,&html)
            })();
            if i.is_multiple_of(100){let _=task_context.report(format!("新浪历史 {i}/{}",symbols.len()));}
            results.lock().expect("worker result mutex").push((id.clone(),result));
        }});
        }
    });
    task_context_check()?;
    let mut bundle = source.clone();
    bundle.downloaded_at = chrono::Utc::now().to_rfc3339();
    for (id, result) in results.into_inner().map_err(|_| invalid("worker panic"))? {
        match result {
            Err(e) => {
                if !bundle.queried_symbols.contains(&id) {
                    bundle.failures.insert(id, e.to_string());
                }
            }
            Ok(actions) => {
                let mut conflict = None;
                for a in actions {
                    if let Some(existing) = bundle.actions.iter_mut().find(|old| old.id == a.id) {
                        if (existing.cash_per_share - a.cash_per_share).abs() > 1e-6
                            || (existing.shares_per_share - a.shares_per_share).abs() > 1e-6
                            || existing.record_date != a.record_date
                        {
                            conflict = Some(format!("public sources disagree for {}", a.id));
                            continue;
                        }
                        if a.listing_date.is_some() {
                            existing.listing_date = a.listing_date;
                        }
                        existing.source = format!("{}; {}", existing.source, a.source);
                    } else {
                        bundle.actions.push(a);
                    }
                }
                if let Some(e) = conflict {
                    bundle.failures.insert(id, e);
                } else {
                    bundle.queried_symbols.insert(id.clone());
                    bundle.failures.remove(&id);
                }
            }
        }
    }
    bundle.actions.sort_by(|a, b| a.id.cmp(&b.id));
    // Preserve both generations of raw evidence beside the normalized bundle.
    for entry in fs::read_dir(output.join("raw"))? {
        let p = entry?.path();
        bundle.raw_sha256.insert(
            p.file_name().unwrap().to_string_lossy().into(),
            format!("sha256:{:x}", Sha256::digest(fs::read(p)?)),
        );
    }
    fs::write(
        output.join("actions.json"),
        serde_json::to_vec_pretty(&bundle).map_err(invalid)?,
    )?;
    let missing: Vec<_> = bundle
        .actions
        .iter()
        .filter_map(|a| a.validate().err().map(|e| format!("{}: {e}", a.id)))
        .collect();
    fs::write(output.join("coverage.json"),serde_json::to_vec_pretty(&serde_json::json!({"symbols_downloaded":bundle.queried_symbols.len(),"events":bundle.actions.len(),"failures":bundle.failures,"incomplete_events":missing,"complete_for_execution":missing.is_empty()&&bundle.failures.is_empty()})).map_err(invalid)?)?;
    Ok(bundle)
}

fn detail_dates(action: &CorporateAction, html: &str) -> io::Result<(Option<Date>, Option<Date>)> {
    let table = html
        .split("id=\"sharebonusdetail\"")
        .nth(1)
        .and_then(|s| s.split("</table>").next())
        .ok_or_else(|| invalid("Sina detail table missing"))?;
    let fields: BTreeMap<_, _> = table
        .split("<tr>")
        .skip(1)
        .filter_map(|r| {
            let c = cells(r);
            (c.len() == 2).then(|| (c[0].clone(), c[1].clone()))
        })
        .collect();
    let d = |key: &str| fields.get(key).and_then(|s| s.parse::<Date>().ok());
    if d("登记日") != Some(action.record_date) || d("除息日") != Some(action.ex_date) {
        return Err(invalid("Sina detail identity/date mismatch"));
    }
    let cash = fields
        .get("税前红利（报价币种）")
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
        / 10.0;
    if (cash - action.cash_per_share).abs() > 0.001 {
        return Err(invalid("Sina detail dividend amount conflicts"));
    }
    // This generic field can mean stock arrival for mixed schemes. It is a cash
    // payment date only for an exclusively cash distribution. Never infer both.
    let pay = if action.cash_per_share > 0.0 && action.shares_per_share == 0.0 {
        d("红利/配股起始日（送、转股到账日）")
    } else {
        None
    };
    Ok((pay, d("上市日")))
}
/// Fetch public detail pages within an explicit execution range. Supplements only
/// unambiguous cash-only payment dates and explicitly named stock listing dates.
/// Per-event failures remain in details_coverage.json; incomplete actions still
/// fail require_coverage. Raw responses and inherited evidence are retained.
pub fn fetch_details(
    source: &ActionBundle,
    source_root: &Path,
    output: &Path,
    start: Date,
    end: Date,
) -> io::Result<ActionBundle> {
    source.validate_identity()?;
    if start > end || source.schema_version != 1 {
        return Err(invalid("invalid detail range/bundle"));
    }
    checkpoints::prepare(
        source,
        source_root,
        output,
        "details",
        serde_json::json!([start, end]),
    )?;

    let mut announcements = BTreeMap::new();
    for id in &source.queried_symbols {
        if let Ok(bytes) = fs::read(source_root.join(format!("raw/{id}-sina.html")))
            && let Ok(html) = decode_table(&bytes, b"id=\"sharebonus_1\"")
            && let Ok(actions) = parse_sina(id, &html)
        {
            for a in actions {
                announcements.insert(a.id, a.announced);
            }
        }
    }
    let candidates: Vec<_> = source
        .actions
        .iter()
        .filter(|a| {
            a.ex_date >= start
                && a.ex_date <= end
                && ((a.cash_per_share > 0.0 && a.pay_date.is_none())
                    || (a.shares_per_share > 0.0 && a.listing_date.is_none()))
        })
        .collect();
    let index = AtomicUsize::new(0);
    let results = Mutex::new(Vec::new());
    let task_context = analytics_core::task::TaskContext::current();
    std::thread::scope(|scope| {
        for _ in 0..4.min(candidates.len()) {
            scope.spawn(||{let _scope=task_context.install(); loop{
            if task_context.check().is_err(){break;}
            let i=index.fetch_add(1,Ordering::Relaxed);let Some(a)=candidates.get(i) else{break};
            let announced=announcements.get(&a.id).copied().unwrap_or(a.announced);
            let url=format!("https://money.finance.sina.com.cn/corp/view/vISSUE_ShareBonusDetail.php?stockid={}&type=1&end_date={announced}",&a.instrument[2..]);
            let result=(||{
                let bytes=checkpoints::page(&url,Command::new("/usr/bin/curl").args(["--fail","--silent","--show-error","--location","--retry","2","--connect-timeout","15","--max-time","45",&url]),&output.join(format!("raw/{}-{}-detail.html",a.instrument,a.ex_date)))?;
                let html=decode_table(&bytes,b"id=\"sharebonusdetail\"")?;
                detail_dates(a,&html)
            })();
            if i.is_multiple_of(100){let _=task_context.report(format!("分红详情 {i}/{}",candidates.len()));}
            results.lock().expect("worker result mutex").push((a.id.clone(),url,result));
        }});
        }
    });
    task_context_check()?;
    let mut bundle = source.clone();
    bundle.downloaded_at = chrono::Utc::now().to_rfc3339();
    let mut failures = BTreeMap::new();
    let mut cash_added = 0;
    let mut listings_added = 0;
    for (id, url, result) in results.into_inner().map_err(|_| invalid("worker panic"))? {
        match result {
            Err(e) => {
                failures.insert(id, e.to_string());
            }
            Ok((pay, listing)) => {
                let a = bundle
                    .actions
                    .iter_mut()
                    .find(|a| a.id == id)
                    .ok_or_else(|| invalid("detail event missing"))?;
                if a.pay_date.is_none() && pay.is_some() {
                    a.pay_date = pay;
                    cash_added += 1;
                }
                if a.listing_date.is_none() && a.shares_per_share > 0.0 && listing.is_some() {
                    a.listing_date = listing;
                    listings_added += 1;
                }
                a.source = format!("{}; {url}", a.source);
            }
        }
    }
    for entry in fs::read_dir(output.join("raw"))? {
        let p = entry?.path();
        bundle.raw_sha256.insert(
            p.file_name().unwrap().to_string_lossy().into(),
            format!("sha256:{:x}", Sha256::digest(fs::read(p)?)),
        );
    }
    fs::write(
        output.join("actions.json"),
        serde_json::to_vec_pretty(&bundle).map_err(invalid)?,
    )?;
    let missing: Vec<_> = bundle
        .actions
        .iter()
        .filter(|a| a.ex_date >= start && a.ex_date <= end)
        .filter_map(|a| a.validate().err().map(|e| format!("{}: {e}", a.id)))
        .collect();
    fs::write(output.join("details_coverage.json"),serde_json::to_vec_pretty(&serde_json::json!({"start":start,"end":end,"requested":candidates.len(),"cash_dates_added":cash_added,"listing_dates_added":listings_added,"detail_failures":failures,"incomplete_events":missing,"complete_for_execution":missing.is_empty()&&bundle.failures.is_empty(),"mixed_scheme_dates":"generic arrival date is not inferred as cash payment"})).map_err(invalid)?)?;
    Ok(bundle)
}

/// Reparse already independently downloaded evidence after a parser repair.
/// Only retries symbol failures; keeps prior dates and rejects conflicting rates.
/// Does not contact the network or use another project's cache.
pub fn repair_from_evidence(
    source: &ActionBundle,
    source_root: &Path,
    output: &Path,
) -> io::Result<ActionBundle> {
    source.validate_identity()?;
    if source.schema_version != 1
        || source.downloader != "JFQuant analytics-storage corporate-actions"
    {
        return Err(invalid("invalid source bundle"));
    }
    checkpoints::prepare(source, source_root, output, "repair", Value::Null)?;

    task_context_check()?;
    let mut bundle = source.clone();
    let failed: Vec<_> = bundle.failures.keys().cloned().collect();
    for id in failed {
        let path = source_root.join(format!("raw/{id}-sina.html"));
        let result = fs::read(path)
            .and_then(|bytes| decode_table(&bytes, b"id=\"sharebonus_1\""))
            .and_then(|html| parse_sina(&id, &html));
        if let Ok(actions) = result {
            let mut conflict = None;
            for a in actions {
                if let Some(old) = bundle.actions.iter_mut().find(|old| old.id == a.id) {
                    if (old.cash_per_share - a.cash_per_share).abs() > 1e-6
                        || (old.shares_per_share - a.shares_per_share).abs() > 1e-6
                        || old.record_date != a.record_date
                    {
                        conflict = Some(format!("public sources disagree for {}", a.id));
                        continue;
                    }
                    if old.listing_date.is_none() {
                        old.listing_date = a.listing_date;
                    }
                } else {
                    bundle.actions.push(a);
                }
            }
            if let Some(reason) = conflict {
                bundle.failures.insert(id, reason);
            } else {
                bundle.queried_symbols.insert(id.clone());
                bundle.failures.remove(&id);
            }
        } else if let Err(e) = result {
            bundle.failures.insert(id, e.to_string());
        }
    }
    bundle.actions.sort_by(|a, b| a.id.cmp(&b.id));
    fs::write(
        output.join("actions.json"),
        serde_json::to_vec_pretty(&bundle).map_err(invalid)?,
    )?;
    fs::write(output.join("repair_coverage.json"),serde_json::to_vec_pretty(&serde_json::json!({"reparsed_at":chrono::Utc::now().to_rfc3339(),"source_failures_before":source.failures.len(),"source_failures_after":bundle.failures.len(),"symbols":bundle.queried_symbols.len(),"events":bundle.actions.len(),"new_events":bundle.actions.len()-source.actions.len(),"failures":bundle.failures})).map_err(invalid)?)?;
    Ok(bundle)
}

/// Retry failed source histories into a new version, preserving unresolved cross-source conflicts.
pub fn retry_public(
    source: &ActionBundle,
    source_root: &Path,
    output: &Path,
) -> io::Result<ActionBundle> {
    source.validate_identity()?;
    let symbols: Vec<_> = source.failures.keys().cloned().collect();
    if symbols.is_empty() {
        return Err(invalid("no failed histories to retry"));
    }
    fs::create_dir(output)?;
    fs::create_dir(output.join("raw"))?;
    for (name, hash) in &source.raw_sha256 {
        task_context_check()?;
        let bytes = fs::read(source_root.join("raw").join(name))?;
        if format!("sha256:{:x}", Sha256::digest(&bytes)) != *hash {
            return Err(invalid("source evidence hash mismatch"));
        }
        fs::write(output.join("raw").join(name), bytes)?;
    }
    let retry = fetch_public(&symbols, &output.join("retry"))?;
    let mut bundle = source.clone();
    bundle.downloaded_at = chrono::Utc::now().to_rfc3339();
    for id in &retry.queried_symbols {
        bundle.actions.retain(|a| &a.instrument != id);
        bundle.actions.extend(
            retry
                .actions
                .iter()
                .filter(|a| &a.instrument == id)
                .cloned(),
        );
        bundle.queried_symbols.insert(id.clone());
        if !source
            .failures
            .get(id)
            .is_some_and(|e| e.contains("disagree"))
        {
            bundle.failures.remove(id);
        }
    }
    bundle.failures.extend(retry.failures);
    for (name, hash) in retry.raw_sha256 {
        fs::copy(
            output.join("retry/raw").join(&name),
            output.join("raw").join(&name),
        )?;
        bundle.raw_sha256.insert(name, hash);
    }
    bundle.actions.sort_by(|a, b| a.id.cmp(&b.id));
    fs::write(
        output.join("actions.json"),
        serde_json::to_vec_pretty(&bundle).map_err(invalid)?,
    )?;
    Ok(bundle)
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryCheckpoint {
    symbol: String,
    actions: Vec<CorporateAction>,
    evidence: BTreeMap<String, String>,
}
fn read_checkpoint(root: &Path, symbol: &str) -> io::Result<Vec<CorporateAction>> {
    let c: HistoryCheckpoint =
        serde_json::from_slice(&fs::read(root.join(format!("checkpoints/{symbol}.json")))?)
            .map_err(invalid)?;
    if c.symbol != symbol
        || c.actions
            .iter()
            .any(|a| a.instrument != symbol || a.id != format!("{symbol}:{}", a.ex_date))
    {
        return Err(invalid("checkpoint identity mismatch"));
    }
    let expected: BTreeSet<_> = [
        format!("{symbol}-history.json"),
        format!("{symbol}-settlement.json"),
    ]
    .into_iter()
    .collect();
    if c.evidence.keys().cloned().collect::<BTreeSet<_>>() != expected {
        return Err(invalid("checkpoint evidence missing"));
    }
    for (name, hash) in c.evidence {
        if format!(
            "sha256:{:x}",
            Sha256::digest(fs::read(root.join("raw").join(name))?)
        ) != hash
        {
            return Err(invalid("checkpoint evidence mismatch"));
        }
    }
    Ok(c.actions)
}
fn save_checkpoint(root: &Path, symbol: &str, actions: &[CorporateAction]) -> io::Result<()> {
    let mut evidence = BTreeMap::new();
    for name in [
        format!("{symbol}-history.json"),
        format!("{symbol}-settlement.json"),
    ] {
        evidence.insert(
            name.clone(),
            format!(
                "sha256:{:x}",
                Sha256::digest(fs::read(root.join("raw").join(name))?)
            ),
        );
    }
    let c = HistoryCheckpoint {
        symbol: symbol.into(),
        actions: actions.into(),
        evidence,
    };
    let path = root.join(format!("checkpoints/{symbol}.json"));
    let part = path.with_extension("tmp");
    fs::write(&part, serde_json::to_vec(&c).map_err(invalid)?)?;
    fs::rename(part, path)
}

#[cfg(test)]
mod checkpoint_tests {
    use super::*;
    #[test]
    fn cancelled_history_keeps_owned_stage_and_resumes_verified_symbols_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("stage");
        let symbols = vec!["SH600000".into()];
        let context = analytics_core::task::TaskContext::new(
            analytics_core::task::TaskContext::default().id,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            |_| {},
        );
        let error = context
            .execute(|| fetch_public_resumable(&symbols, &root))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(root.join("owner.json").exists());
        assert!(!root.join("actions.json").exists());
        fs::write(
            root.join("raw/SH600000-history.json"),
            br#"{"success":true,"result":{"pages":1,"count":0,"data":[]}}"#,
        )
        .unwrap();
        fs::write(root.join("raw/SH600000-settlement.json"), br#"{"fhyx":[]}"#).unwrap();
        save_checkpoint(&root, "SH600000", &[]).unwrap();
        let bundle = fetch_public_resumable(&symbols, &root).unwrap();
        assert!(bundle.queried_symbols.contains("SH600000"));
        assert!(bundle.failures.is_empty());
        fs::write(root.join("raw/SH600000-history.json"), "tampered").unwrap();
        assert!(read_checkpoint(&root, "SH600000").is_err());
        assert!(fetch_public_resumable(&["SH600001".into()], &root).is_err());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn malformed_bundle_is_rejected_before_worker_urls_or_evidence_paths() {
        let mut bundle = ActionBundle {
            schema_version: 1,
            downloader: "JFQuant analytics-storage corporate-actions".into(),
            downloaded_at: "fixture".into(),
            queried_symbols: BTreeSet::from(["x".into()]),
            actions: vec![],
            failures: BTreeMap::new(),
            raw_sha256: BTreeMap::new(),
        };
        assert!(bundle.validate_identity().is_err());
        bundle.queried_symbols = BTreeSet::from(["SH600000".into()]);
        bundle
            .raw_sha256
            .insert("../outside".into(), "invalid".into());
        assert!(bundle.validate_identity().is_err());
    }
    #[test]
    fn invalid_advertisement_encoding_cannot_hide_a_valid_distribution_table() {
        let text = "id=\"sharebonus_1\"><thead>红股上市日</thead><tbody><tr><td colspan=\"9\">暂时没有数据！</td></tr></tbody></table>";
        let (bytes, _, _) = encoding_rs::GBK.encode(text);
        let mut page = vec![0xff];
        page.extend_from_slice(&bytes);
        page.push(0xff);
        let table = decode_table(&page, b"id=\"sharebonus_1\"").unwrap();
        assert!(parse_sina("SH600000", &table).unwrap().is_empty());
        assert!(decode_table(&[0xff], b"id=\"sharebonus_1\"").is_err());
    }
    #[test]
    fn detail_payment_is_not_ex_date_and_mixed_scheme_is_not_guessed() {
        let html = "<table id=\"sharebonusdetail\"><tr><td>登记日</td><td>2013-05-31</td></tr><tr><td>除息日</td><td>2013-06-03</td></tr><tr><td>税前红利（报价币种）</td><td>5.50</td></tr><tr><td>红利/配股起始日（送、转股到账日）</td><td>2013-06-07</td></tr><tr><td>上市日</td><td>--</td></tr></table>";
        let mut a = CorporateAction {
            id: "SH600000:2013-06-03".into(),
            instrument: "SH600000".into(),
            announced: "2013-05-27".parse().unwrap(),
            record_date: "2013-05-31".parse().unwrap(),
            ex_date: "2013-06-03".parse().unwrap(),
            pay_date: None,
            listing_date: None,
            cash_per_share: 0.55,
            shares_per_share: 0.0,
            source: "synthetic provider schema fixture".into(),
        };
        assert_eq!(
            detail_dates(&a, html).unwrap(),
            (Some("2013-06-07".parse().unwrap()), None)
        );
        a.shares_per_share = 0.5;
        assert_eq!(detail_dates(&a, html).unwrap(), (None, None));
        a.record_date = "2013-05-30".parse().unwrap();
        assert!(detail_dates(&a, html).is_err());
    }
    #[test]
    fn sina_history_restores_delisted_names_and_explicit_listing_dates() {
        let html = "<table id=\"sharebonus_1\"><thead>红股上市日</thead><tbody><tr><td>2013-05-27</td><td>1</td><td>2</td><td>4.2</td><td>实施</td><td>2013-06-03</td><td>2013-05-31</td><td>2013-06-05</td><td><a href=\"ignored\">查看</a></td></tr></tbody></table>";
        let a = parse_sina("SH600000", html).unwrap().remove(0);
        assert_eq!(a.listing_date, Some("2013-06-05".parse().unwrap()));
        assert_eq!(a.pay_date, None);
        assert!((a.shares_per_share - 0.3).abs() < 1e-12);
        assert!(parse_sina("SH600000", "blocked page").is_err());
    }
    #[test]
    fn provider_units_dates_and_incomplete_history_are_not_guessed() {
        let h = serde_json::json!({"success":true,"result":{"pages":1,"count":1,"data":[{"SECUCODE":"600000.SH","ASSIGN_PROGRESS":"实施分配","NOTICE_DATE":"2024-06-01 00:00:00","EQUITY_RECORD_DATE":"2024-06-03 00:00:00","EX_DIVIDEND_DATE":"2024-06-04 00:00:00","PRETAX_BONUS_RMB":4.2,"BONUS_RATIO":1,"IT_RATIO":2}]}});
        let p = serde_json::json!({"fhyx":[{"EQUITY_RECORD_DATE":"2024-06-03 00:00:00","EX_DIVIDEND_DATE":"2024-06-04 00:00:00","PAY_CASH_DATE":"2024-06-07 00:00:00"}]});
        let a = parse_history("SH600000", &h, &p).unwrap().remove(0);
        assert!((a.cash_per_share - 0.42).abs() < 1e-12);
        assert!((a.shares_per_share - 0.3).abs() < 1e-12);
        assert_eq!(a.pay_date, Some("2024-06-07".parse().unwrap()));
        assert_eq!(a.listing_date, None);
        assert!(a.validate().is_err());
        assert!(parse_history("SZ000001", &h, &p).is_err());
        assert!(parse_history("SH600000", &serde_json::json!({"success":false}), &p).is_err());
    }
}
