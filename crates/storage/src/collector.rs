//! Supervised bundled Python acquisition. The Rust owner validates and publishes all bytes.
use crate::adjustment::{
    AdjustmentBasis, AdjustmentRecord, AdjustmentSource, FactorSource, NORMALIZATION,
};
use crate::daily::{DailyBar, DailyManifest, read_bars, write_bars};
use analytics_core::{Date, pool::validate_instrument, task::TaskContext};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{self, BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};
/// Explicit user-selected upstream; failed requests never switch automatically.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Provider {
    /// A-share default source.
    #[default]
    Akshare,
    /// Yahoo candidate/verification source.
    Yfinance,
}
/// Public collection request; no executable paths or arbitrary functions are accepted.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CollectionRequest {
    /// Opaque security ID to explicitly chosen dated provider locator. Empty means source candidates only.
    pub source_bindings: BTreeMap<String, String>,
    /// Chosen upstream.
    pub provider: Provider,
    /// Explicit independent coefficient source; no automatic fallback.
    pub adjustment_source: AdjustmentSource,
    /// Inclusive first requested date.
    pub start: Date,
    /// Inclusive last requested date.
    pub end: Date,
    /// Explicit stocks plus benchmark securities.
    pub instruments: BTreeSet<String>,
    /// Securities treated as index quotes.
    pub benchmarks: BTreeSet<String>,
    /// Current constituent lists to record, not historic backfill.
    pub universes: BTreeSet<String>,
}
impl CollectionRequest {
    /// Validate before a process is created or a checkpoint changed.
    pub fn validate(&self) -> io::Result<()> {
        if self.start > self.end
            || self.instruments.is_empty()
            || self.instruments.len() > 10_000
            || !self.benchmarks.is_subset(&self.instruments)
        {
            return Err(invalid("invalid collection range/securities"));
        }
        if !matches!(
            (self.provider, self.adjustment_source),
            (Provider::Akshare, AdjustmentSource::SinaHfq)
                | (Provider::Yfinance, AdjustmentSource::None)
        ) || self.benchmarks.iter().any(|id| {
            !matches!(
                self.source_bindings.get(id).unwrap_or(id).as_str(),
                "SH000300" | "SH000905"
            )
        }) || self.instruments.iter().any(|id| {
            matches!(
                self.source_bindings.get(id).unwrap_or(id).as_str(),
                "SH000300" | "SH000905"
            ) && !self.benchmarks.contains(id)
        }) {
            return Err(invalid(
                "invalid provider/adjustment source or unverified price-index identity",
            ));
        }
        let mut used = BTreeSet::new();
        for id in &self.instruments {
            validate_instrument(id).map_err(invalid)?;
            let source = self.source_bindings.get(id).unwrap_or(id);
            analytics_core::pool::validate_exchange_symbol(source).map_err(invalid)?;
            if !used.insert(source) {
                return Err(invalid("duplicate provider locator"));
            }
        }
        if self
            .source_bindings
            .keys()
            .any(|id| !self.instruments.contains(id))
        {
            return Err(invalid("unexpected source binding"));
        }
        if self
            .universes
            .iter()
            .any(|u| !matches!(u.as_str(), "csi300" | "csi500"))
        {
            return Err(invalid("unsupported constituent source"));
        }
        Ok(())
    }
}
/// Hash of every bundled collector implementation file, frozen into acquired source packages.
pub fn collector_signature() -> String {
    let mut hash = Sha256::new();
    for (name, bytes) in [
        (
            "__init__.py",
            include_bytes!("../collectors/jfquant_collectors/__init__.py").as_slice(),
        ),
        (
            "__main__.py",
            include_bytes!("../collectors/jfquant_collectors/__main__.py").as_slice(),
        ),
        (
            "providers.py",
            include_bytes!("../collectors/jfquant_collectors/providers.py").as_slice(),
        ),
        (
            "evidence.py",
            include_bytes!("../collectors/jfquant_collectors/evidence.py").as_slice(),
        ),
        (
            "adjustment.py",
            include_bytes!("../collectors/jfquant_collectors/adjustment.py").as_slice(),
        ),
        (
            "announcements.py",
            include_bytes!("../collectors/jfquant_collectors/announcements.py").as_slice(),
        ),
    ] {
        hash.update(name);
        hash.update([0]);
        hash.update(bytes);
        hash.update([0]);
    }
    format!("sha256:{:x}", hash.finalize())
}
fn invalid(s: impl std::fmt::Display) -> io::Error {
    io::Error::other(s.to_string())
}
fn hash(path: &Path) -> io::Result<String> {
    Ok(format!("sha256:{:x}", Sha256::digest(fs::read(path)?)))
}
pub(crate) fn verify_http_receipts(
    staging: &Path,
    evidence: &Value,
) -> io::Result<BTreeMap<String, String>> {
    let mut hashes = BTreeMap::new();
    if let Some(receipts) = evidence["http_receipts"].as_array() {
        for r in receipts {
            let name = r["filename"]
                .as_str()
                .ok_or_else(|| invalid("missing HTTP receipt filename"))?;
            if Path::new(name).file_name().and_then(|n| n.to_str()) != Some(name)
                || !name.starts_with("http-")
            {
                return Err(invalid("unsafe HTTP evidence name"));
            }
            let expected = r["sha256"]
                .as_str()
                .ok_or_else(|| invalid("missing HTTP hash"))?;
            if hash(&staging.join("raw").join(name))? != expected {
                return Err(invalid("HTTP evidence hash mismatch"));
            }
            hashes.insert(name.into(), expected.into());
        }
    }
    Ok(hashes)
}
fn save(path: &Path, value: &impl Serialize) -> io::Result<()> {
    let part = path.with_extension("part");
    let mut f = fs::File::create(&part)?;
    f.write_all(&serde_json::to_vec_pretty(value).map_err(invalid)?)?;
    f.sync_all()?;
    fs::rename(part, path)
}
fn read_line_limited(reader: &mut impl BufRead, limit: usize) -> io::Result<Option<String>> {
    let mut bytes = Vec::new();
    let count = reader
        .take(limit as u64 + 1)
        .read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    if count > limit {
        return Err(invalid("collector message too large"));
    }
    if bytes.pop() != Some(b'\n') {
        return Err(invalid("incomplete collector message"));
    }
    if bytes.last() == Some(&b'\r') {
        bytes.pop();
    }
    String::from_utf8(bytes).map(Some).map_err(invalid)
}
fn dependencies_signature() -> String {
    format!(
        "sha256:{:x}",
        Sha256::digest(include_bytes!("../collectors/requirements.lock"))
    )
}
fn verify_collector_identity(message: &Value) -> io::Result<()> {
    if message["collector_signature"] != collector_signature() {
        return Err(invalid(
            "collector code signature mismatch; rebuild the bundled runtime",
        ));
    }
    if message["versions"]["akshare"] != "1.19.1"
        || message["versions"]["yfinance"] != "1.7.0"
        || message["versions"]["python"] != "3.12.15"
    {
        return Err(invalid("collector dependency version mismatch"));
    }
    Ok(())
}
/// Resolve the package runtime first; development builds may use the documented local build.
pub fn runtime() -> io::Result<PathBuf> {
    let executable = std::env::current_exe()?;
    let contents = executable
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| invalid("missing executable parent"))?;
    let bundled = contents.join("Resources/python/bin/python3");
    if bundled.is_file() {
        return Ok(bundled);
    }
    if cfg!(debug_assertions) {
        let root = std::env::var_os("JFQUANT_COLLECTOR_RUNTIME")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/collector-build/python/bin/python3")
            });
        if root.is_file() {
            return Ok(root);
        }
    }
    Err(invalid(
        "collector_runtime_unavailable: install the complete JFQuant App, or build the development collector runtime",
    ))
}
struct OwnedChild(Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
        }
        let _ = self.0.wait();
    }
}
/// Acquire verified security shards and publish a canonical manifest only after completion.
/// The caller atomically moves this staging directory into the immutable data catalog.
pub fn collect(
    request: &CollectionRequest,
    staging: &Path,
    context: &TaskContext,
) -> io::Result<PathBuf> {
    collect_operation(request, staging, context, "market")
}
/// Collect typed company-action candidates; incomplete dates are retained and block execution.
pub fn collect_actions(
    request: &CollectionRequest,
    staging: &Path,
    context: &TaskContext,
) -> io::Result<PathBuf> {
    collect_operation(request, staging, context, "actions")
}
fn collect_operation(
    request: &CollectionRequest,
    staging: &Path,
    context: &TaskContext,
    operation: &str,
) -> io::Result<PathBuf> {
    request.validate()?;
    context.check()?;
    let mut command = Command::new(runtime()?);
    command.args(["-I", "-B", "-u", "-m", "jfquant_collectors"]);
    collect_with_command(
        request,
        staging,
        context,
        operation,
        command,
        Duration::from_secs(180),
    )
}
/// Fetch fixed-source implementation documents through the same supervised collector.
pub fn collect_announcements(
    request: &CollectionRequest,
    staging: &Path,
    context: &TaskContext,
) -> io::Result<PathBuf> {
    let mut command = Command::new(runtime()?);
    command.args(["-I", "-B", "-u", "-m", "jfquant_collectors"]);
    collect_with_command(
        request,
        staging,
        context,
        "announcements",
        command,
        Duration::from_secs(180),
    )
}
fn collect_with_command(
    request: &CollectionRequest,
    staging: &Path,
    context: &TaskContext,
    operation: &str,
    mut command: Command,
    idle_timeout: Duration,
) -> io::Result<PathBuf> {
    request.validate()?;
    context.check()?;
    fs::create_dir_all(staging.join("market/bars"))?;
    fs::create_dir_all(staging.join("market/adjustments"))?;
    fs::create_dir_all(staging.join("raw"))?;
    let owner = staging.join("owner.json");
    let owner_value = json!({"operation":operation,"request":request,"collector_protocol":6,"normalization":NORMALIZATION,
        "collector_signature":collector_signature(),"dependencies_sha256":dependencies_signature(),
        "python_runtime_sha256":format!("sha256:{:x}",Sha256::digest(include_bytes!("../collectors/python-runtime.lock.json")))});
    if owner.exists() {
        let old: Value = serde_json::from_slice(&fs::read(&owner)?).map_err(invalid)?;
        if old != owner_value {
            return Err(invalid("checkpoint owner/request mismatch"));
        }
    } else {
        save(&owner, &owner_value)?;
    }
    let receipt_path = staging.join("receipts.json");
    let mut receipts: BTreeMap<String, String> = if receipt_path.exists() {
        serde_json::from_slice(&fs::read(&receipt_path)?).map_err(invalid)?
    } else {
        BTreeMap::new()
    };
    for (id, expected) in &receipts {
        validate_instrument(id).map_err(invalid)?;
        if !request.instruments.contains(id) {
            return Err(invalid("checkpoint contains an unrequested security"));
        }
        let path = if operation == "market" {
            staging.join("market/bars").join(format!("{id}.parquet"))
        } else {
            staging
                .join("raw")
                .join(format!("{id}-actions-structured.json"))
        };
        if hash(&path)? != *expected {
            return Err(invalid("checkpoint hash mismatch"));
        }
        if operation == "market" {
            read_bars(&path)?;
            let adjustment = staging
                .join("market/adjustments")
                .join(format!("{id}.json"));
            let expected: String = serde_json::from_slice(&fs::read(
                staging
                    .join("raw")
                    .join(format!("{id}-adjustment-sha256.json")),
            )?)
            .map_err(invalid)?;
            if hash(&adjustment)? != expected {
                return Err(invalid("checkpoint adjustment hash mismatch"));
            }
            AdjustmentRecord::read(&staging.join("market"), id)?;
            let evidence = staging.join("raw").join(format!("{id}-structured.json"));
            let hash_record = staging
                .join("raw")
                .join(format!("{id}-evidence-sha256.json"));
            let expected: String =
                serde_json::from_slice(&fs::read(hash_record)?).map_err(invalid)?;
            if hash(&evidence)? != expected {
                return Err(invalid("checkpoint evidence hash mismatch"));
            }
            let evidence: Value = serde_json::from_slice(&fs::read(evidence)?).map_err(invalid)?;
            verify_http_receipts(staging, &evidence)?;
        } else {
            let msg: Value = serde_json::from_slice(&fs::read(&path)?).map_err(invalid)?;
            verify_http_receipts(staging, &msg["evidence"])?;
        }
    }
    let pending: BTreeSet<_> = request
        .instruments
        .iter()
        .filter(|id| !receipts.contains_key(*id))
        .cloned()
        .collect();
    let cache = staging.join("cache");
    fs::create_dir_all(&cache)?;
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(staging.join("collector.stderr.log"))?;
    let mut child = OwnedChild(
        command
            .current_dir(staging)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?,
    );
    let mut input = child
        .0
        .stdin
        .take()
        .ok_or_else(|| invalid("missing child input"))?;
    let stdout = child.0.stdout.take().unwrap();
    let stderr = child.0.stderr.take().unwrap();
    let (tx, rx) = mpsc::sync_channel(8);
    let reader = std::thread::spawn(move || {
        let mut reader = BufReader::new(stdout);
        loop {
            match read_line_limited(&mut reader, 32 * 1024 * 1024) {
                Ok(Some(line)) => {
                    if tx.send(Ok(line)).is_err() {
                        break;
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    let _ = tx.send(Err(error));
                    break;
                }
            }
        }
    });
    let logger_context = context.clone();
    let logger = std::thread::spawn(move || -> io::Result<()> {
        let mut reader = BufReader::new(stderr);
        // Diagnostics may end without a newline; preserve them without unbounded allocation.
        loop {
            let mut bytes = Vec::new();
            let count = (&mut reader)
                .take(64 * 1024 + 1)
                .read_until(b'\n', &mut bytes)?;
            if count == 0 {
                break;
            }
            if count > 64 * 1024 {
                return Err(invalid("collector diagnostic too large"));
            }
            log.write_all(&bytes)?;
            let line = String::from_utf8_lossy(&bytes);
            let message: String = line.chars().take(500).collect();
            logger_context.report(format!("采集诊断：{message}"))?;
        }
        log.sync_all()
    });
    let mut payload = serde_json::to_value(request).map_err(invalid)?;
    payload["version"] = json!(6);
    payload["expected_collector_signature"] = json!(collector_signature());
    payload["operation"] = json!(operation);
    let native: BTreeMap<String, String> = pending
        .iter()
        .map(|id| {
            (
                request.source_bindings.get(id).unwrap_or(id).clone(),
                id.clone(),
            )
        })
        .collect();
    payload["instruments"] = json!(native.keys().collect::<Vec<_>>());
    payload["benchmarks"] = json!(
        request
            .benchmarks
            .iter()
            .map(|id| request.source_bindings.get(id).unwrap_or(id))
            .collect::<Vec<_>>()
    );
    payload["security_ids"] = json!(native);
    payload["cache_dir"] = json!(cache);
    writeln!(input, "{}", payload)?;
    let mut completed = None;
    let mut identified = false;
    let mut last_message = Instant::now();
    let result = (|| -> io::Result<()> {
        loop {
            context.check()?;
            if last_message.elapsed() > idle_timeout {
                return Err(invalid("collection_timeout: collector made no progress"));
            }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(line) => {
                    last_message = Instant::now();
                    let line = line?;
                    if line.len() > 32 * 1024 * 1024 {
                        return Err(invalid("collector message too large"));
                    }
                    let mut msg: Value = serde_json::from_str(&line).map_err(invalid)?;
                    if let Some(source) = msg["instrument"].as_str().map(str::to_owned)
                        && let Some(id) = native.get(&source)
                    {
                        msg["instrument"] = json!(id);
                        if let Some(bars) = msg["bars"].as_array_mut() {
                            for b in bars {
                                if b["instrument_id"] != source {
                                    return Err(invalid("provider shard identity mismatch"));
                                }
                                b["instrument_id"] = json!(id);
                            }
                        }
                        if msg["adjustment"].is_object() {
                            msg["adjustment"]["instrument_id"] = json!(id);
                        }
                        if let Some(records) = msg["records"].as_array_mut() {
                            for r in records {
                                if r["instrument"] != source {
                                    return Err(invalid("provider action identity mismatch"));
                                }
                                r["instrument"] = json!(id);
                                if let Some(date) = r["ex_date"].as_str() {
                                    r["id"] = json!(format!("{id}:{date}"));
                                }
                            }
                        }
                        if let Some(docs) = msg["evidence"]["documents"].as_array_mut() {
                            for d in docs {
                                d["source_symbol"] = json!(source);
                                d["instrument"] = json!(id);
                            }
                        }
                    }
                    if msg["event"] == "complete"
                        && let Some(universes) = msg["universes"].as_object_mut()
                    {
                        for pool in universes.values_mut() {
                            if let Some(members) = pool["intervals"].as_object_mut() {
                                let old = std::mem::take(members);
                                for (code, ranges) in old {
                                    members
                                        .insert(native.get(&code).unwrap_or(&code).clone(), ranges);
                                }
                            }
                        }
                    }
                    if msg["version"] != 6 {
                        return Err(invalid("collector_protocol_mismatch"));
                    }
                    if completed.is_some() {
                        return Err(invalid("collector message after completion"));
                    }
                    match msg["event"].as_str() {
                        Some("ready") => {
                            if identified {
                                return Err(invalid("duplicate collector identity"));
                            }
                            verify_collector_identity(&msg)?;
                            identified = true;
                        }
                        Some("progress") => {
                            context.report(msg["message"].as_str().unwrap_or("采集进度"))?
                        }
                        Some("action_shard")
                            if operation == "actions" || operation == "announcements" =>
                        {
                            if !identified {
                                return Err(invalid(
                                    "collector identity required before checkpoints",
                                ));
                            }
                            let id = msg["instrument"]
                                .as_str()
                                .ok_or_else(|| invalid("missing action security"))?;
                            if !pending.contains(id) || receipts.contains_key(id) {
                                return Err(invalid("unexpected action security"));
                            }
                            verify_http_receipts(staging, &msg["evidence"])?;
                            let path = staging
                                .join("raw")
                                .join(format!("{id}-actions-structured.json"));
                            save(&path, &msg)?;
                            receipts.insert(id.into(), hash(&path)?);
                            save(&receipt_path, &receipts)?;
                        }
                        Some("shard") if operation == "market" => {
                            if !identified {
                                return Err(invalid(
                                    "collector identity required before checkpoints",
                                ));
                            }
                            let id = msg["instrument"]
                                .as_str()
                                .ok_or_else(|| invalid("missing shard identity"))?;
                            if !pending.contains(id) || receipts.contains_key(id) {
                                return Err(invalid("unexpected or duplicate shard"));
                            }
                            let bars: Vec<DailyBar> =
                                serde_json::from_value(msg["bars"].clone()).map_err(invalid)?;
                            if bars.is_empty()
                                || bars.iter().any(|b| {
                                    b.instrument_id != id
                                        || b.trade_date < request.start
                                        || b.trade_date > request.end
                                })
                            {
                                return Err(invalid("empty/out-of-range shard"));
                            }
                            verify_http_receipts(staging, &msg["evidence"])?;
                            let adjustment: AdjustmentRecord =
                                serde_json::from_value(msg["adjustment"].clone())
                                    .map_err(invalid)?;
                            adjustment.validate()?;
                            let allowed = match (
                                &adjustment.basis,
                                request.provider,
                                request.benchmarks.contains(id),
                            ) {
                                (
                                    AdjustmentBasis::Explicit {
                                        source: FactorSource::SinaHfq,
                                        ..
                                    },
                                    Provider::Akshare,
                                    false,
                                ) => true,
                                (
                                    AdjustmentBasis::PriceIndex { source },
                                    Provider::Akshare,
                                    true,
                                ) => source == "akshare.index_zh_a_hist",
                                (AdjustmentBasis::Unavailable { .. }, Provider::Yfinance, _) => {
                                    true
                                }
                                _ => false,
                            };
                            if adjustment.instrument_id != id || !allowed {
                                return Err(invalid("adjustment source/security mismatch"));
                            }
                            if !matches!(adjustment.basis, AdjustmentBasis::Unavailable { .. }) {
                                let first =
                                    bars.iter().find(|b| b.close.is_some()).ok_or_else(|| {
                                        invalid("raw close missing for adjustment anchor")
                                    })?;
                                adjustment.anchor(first.trade_date)?;
                            }
                            let adjustment_path = staging
                                .join("market/adjustments")
                                .join(format!("{id}.json"));
                            save(&adjustment_path, &adjustment)?;
                            save(
                                &staging
                                    .join("raw")
                                    .join(format!("{id}-adjustment-sha256.json")),
                                &hash(&adjustment_path)?,
                            )?;
                            let path = staging.join("market/bars").join(format!("{id}.parquet"));
                            let part = path.with_extension("part");
                            write_bars(&part, &bars)?;
                            read_bars(&part)?;
                            fs::rename(part, &path)?;
                            save(
                                &staging.join("raw").join(format!("{id}-structured.json")),
                                &msg["evidence"],
                            )?;
                            save(
                                &staging
                                    .join("raw")
                                    .join(format!("{id}-evidence-sha256.json")),
                                &hash(&staging.join("raw").join(format!("{id}-structured.json")))?,
                            )?;
                            receipts.insert(id.into(), hash(&path)?);
                            save(&receipt_path, &receipts)?;
                        }
                        Some("complete") => {
                            if !identified {
                                return Err(invalid(
                                    "collector identity required before completion",
                                ));
                            }
                            if completed.is_some() {
                                return Err(invalid("duplicate completion"));
                            }
                            completed = Some(msg);
                        }
                        Some("shard_error") => {
                            if !identified {
                                return Err(invalid("collector identity required before failure"));
                            }
                            let id = msg["instrument"]
                                .as_str()
                                .ok_or_else(|| invalid("missing failed security"))?;
                            if !pending.contains(id) {
                                return Err(invalid("unexpected failed source"));
                            }
                            save(&staging.join("raw").join(format!("{id}-failed.json")), &msg)?;
                            context.report(format!("{id}: {}", msg["message"]))?;
                        }
                        Some("error") => {
                            return Err(invalid(format!("{}: {}", msg["code"], msg["message"])));
                        }
                        _ => return Err(invalid("unknown collector event")),
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        let status = loop {
            context.check()?;
            if let Some(status) = child.0.try_wait()? {
                break status;
            }
            if last_message.elapsed() > idle_timeout {
                return Err(invalid("collection_timeout: collector did not exit"));
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        if !status.success() {
            return Err(invalid(format!(
                "collector exited {status}; see collector.stderr.log"
            )));
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = child.0.kill();
        let _ = child.0.wait();
    }
    drop(input);
    drop(rx);
    let reader_result = reader.join();
    let logger_result = logger.join();
    result?;
    reader_result.map_err(|_| invalid("collector reader panicked"))?;
    logger_result.map_err(|_| invalid("collector logger panicked"))??;
    let mut complete = completed.ok_or_else(|| invalid("collector completion missing"))?;
    verify_collector_identity(&complete)?;
    let completion_receipts = verify_http_receipts(staging, &complete["evidence"])?;
    complete["dependencies_sha256"] = json!(dependencies_signature());
    complete["python_runtime"] =
        serde_json::from_slice(include_bytes!("../collectors/python-runtime.lock.json"))
            .map_err(invalid)?;
    save(
        &staging.join("raw/complete-sources-structured.json"),
        &complete,
    )?;

    if receipts.keys().cloned().collect::<BTreeSet<_>>() != request.instruments {
        return Err(invalid("incomplete collection"));
    }
    if operation == "announcements" {
        let mut documents = Vec::new();
        for id in receipts.keys() {
            let value: Value = serde_json::from_slice(&fs::read(
                staging
                    .join("raw")
                    .join(format!("{id}-actions-structured.json")),
            )?)
            .map_err(invalid)?;
            for d in value["evidence"]["documents"]
                .as_array()
                .ok_or_else(|| invalid("missing announcement documents"))?
            {
                for (name, sha) in [("pdf", "pdf_sha256"), ("text", "text_sha256")] {
                    let file = d[name]
                        .as_str()
                        .ok_or_else(|| invalid("missing evidence filename"))?;
                    if Path::new(file).components().count() != 1
                        || !file.starts_with(d["source_symbol"].as_str().unwrap_or(id))
                        || file.contains("..")
                        || hash(&staging.join("raw").join(file))? != d[sha]
                    {
                        return Err(invalid("announcement evidence identity/hash mismatch"));
                    }
                }
                if d["instrument"] != *id {
                    return Err(invalid("announcement security mismatch"));
                }
                documents.push(d.clone());
            }
        }
        save(
            &staging.join("announcements.json"),
            &json!({"schema_version":2,"request":request,"documents":documents,"complete":complete,"receipts":receipts}),
        )?;
        return Ok(staging.join("announcements.json"));
    }
    if operation == "actions" {
        let mut book = crate::corporate_actions::ActionBundle {
            schema_version: 4,
            source_bindings: request
                .source_bindings
                .iter()
                .map(|(id, symbol)| {
                    (
                        id.clone(),
                        analytics_core::source_map::SourceLocator {
                            provider: match request.provider {
                                Provider::Akshare => "akshare",
                                Provider::Yfinance => "yfinance",
                            }
                            .into(),
                            symbol: symbol.clone(),
                            coverage: analytics_core::DateRange {
                                start: request.start,
                                end: request.end,
                            },
                            evidence: vec![],
                        },
                    )
                })
                .collect(),
            coverage: Default::default(),
            downloader: "JFQuant analytics-storage corporate-actions".into(),
            downloaded_at: chrono::Utc::now().to_rfc3339(),
            queried_symbols: BTreeSet::new(),
            actions: vec![],
            failures: BTreeMap::new(),
            raw_sha256: BTreeMap::new(),
        };
        book.raw_sha256.extend(completion_receipts);
        book.raw_sha256.insert(
            "complete-sources-structured.json".into(),
            hash(&staging.join("raw/complete-sources-structured.json"))?,
        );
        for (id, sha) in &receipts {
            let name = format!("{id}-actions-structured.json");
            let msg: Value = serde_json::from_slice(&fs::read(staging.join("raw").join(&name))?)
                .map_err(invalid)?;
            book.raw_sha256.insert(name, sha.clone());
            book.raw_sha256
                .extend(verify_http_receipts(staging, &msg["evidence"])?);
            if let Some(receipts) = msg["evidence"]["http_receipts"].as_array() {
                let source = request.source_bindings.get(id).unwrap_or(id);
                let ending = format!("stockid/{}.phtml", &source[2..]);
                if let Some(r) = receipts.iter().find(|r| {
                    r["url"]
                        .as_str()
                        .is_some_and(|u| u.contains("vISSUE_ShareBonus") && u.contains(&ending))
                }) {
                    let name = format!("{id}-sina.html");
                    fs::copy(
                        staging.join("raw").join(r["filename"].as_str().unwrap()),
                        staging.join("raw").join(&name),
                    )?;
                    book.raw_sha256
                        .insert(name, r["sha256"].as_str().unwrap().into());
                }
            }
            if msg["history_verified"] != true {
                book.failures.insert(
                    id.clone(),
                    msg["failure"]
                        .as_str()
                        .unwrap_or("history coverage unverified")
                        .into(),
                );
                continue;
            }
            book.queried_symbols.insert(id.clone());
            for row in msg["records"]
                .as_array()
                .ok_or_else(|| invalid("missing company-action records"))?
            {
                match serde_json::from_value::<analytics_core::execution::CorporateAction>(
                    row.clone(),
                ) {
                    Ok(a) => {
                        if a.instrument != *id {
                            return Err(invalid("action security mismatch"));
                        }
                        if request.source_bindings.contains_key(id)
                            && (a.ex_date < request.start || a.ex_date > request.end)
                        {
                            continue;
                        }
                        book.actions.push(a);
                    }
                    Err(e) => {
                        book.failures.insert(
                            id.clone(),
                            format!("missing required company-action fields: {e}"),
                        );
                    }
                }
            }
        }
        save(&staging.join("actions.json"), &book)?;
        crate::corporate_actions::ActionBundle::read(&staging.join("actions.json"))?;
        return Ok(staging.join("actions.json"));
    }
    let mut adjustment_anchors = BTreeMap::new();
    for id in &request.instruments {
        let record = AdjustmentRecord::read(&staging.join("market"), id)?;
        if !matches!(record.basis, AdjustmentBasis::Unavailable { .. }) {
            let bars = read_bars(&staging.join("market/bars").join(format!("{id}.parquet")))?;
            let first = bars
                .iter()
                .find(|b| b.close.is_some())
                .ok_or_else(|| invalid("raw close missing for anchor"))?;
            adjustment_anchors.insert(id.clone(), record.anchor(first.trade_date)?);
        }
    }
    let manifest = DailyManifest {
        schema_version: 5,
        calendar: serde_json::from_value(complete["calendar"].clone()).map_err(invalid)?,
        universes: serde_json::from_value(complete["universes"].clone()).map_err(invalid)?,
        instruments: request.instruments.clone(),
        price_basis: if request.provider == Provider::Akshare {
            "unadjusted"
        } else {
            "split_adjusted_candidate"
        }
        .into(),
        normalization: NORMALIZATION.into(),
        adjustment_anchors,
    };
    save(&staging.join("market/daily_manifest.json"), &manifest)?;
    let source_map = analytics_core::source_map::SourceMap {
        schema_version: 1,
        exceptions: request
            .source_bindings
            .iter()
            .filter(|(id, source)| id != source)
            .map(|(id, source)| {
                let coverage = analytics_core::DateRange {
                    start: request.start,
                    end: request.end,
                };
                (
                    id.clone(),
                    analytics_core::source_map::SourceException {
                        exchange_symbol: source.clone(),
                        kind: if request.benchmarks.contains(id) {
                            "price_index"
                        } else {
                            "a_share"
                        }
                        .into(),
                        coverage: coverage.clone(),
                        locators: vec![analytics_core::source_map::SourceLocator {
                            provider: match request.provider {
                                Provider::Akshare => "akshare",
                                Provider::Yfinance => "yfinance",
                            }
                            .into(),
                            symbol: source.clone(),
                            coverage,
                            evidence: Vec::new(),
                        }],
                    },
                )
            })
            .collect(),
    };
    source_map.validate().map_err(invalid)?;
    save(&staging.join("market/source_map.json"), &source_map)?;
    crate::daily::DailyDataset::open(staging.join("market"))?;
    let mut evidence_hashes = BTreeMap::new();
    for e in fs::read_dir(staging.join("raw"))? {
        let path = e?.path();
        if path.is_file() {
            evidence_hashes.insert(
                path.file_name().unwrap().to_string_lossy().to_string(),
                hash(&path)?,
            );
        }
    }
    save(
        &staging.join("jfquant_source.json"),
        &json!({"downloader":"JFQuant analytics-storage","independent_download":true,"snapshot_verified":true,"exception_bindings":!request.source_bindings.is_empty(),"provider":request.provider,"request":request,"evidence_sha256":evidence_hashes,"collector_protocol":6,"versions":complete["versions"],"collector_signature":complete["collector_signature"],"python_runtime":serde_json::from_slice::<Value>(include_bytes!("../collectors/python-runtime.lock.json")).map_err(invalid)?,"calendar_source":"akshare.tool_trade_date_hist_sina","dependencies_sha256":format!("sha256:{:x}",Sha256::digest(include_bytes!("../collectors/requirements.lock"))),"price_basis":manifest.price_basis,"normalization":manifest.normalization,"adjustment_source":request.adjustment_source,"adjustment_anchors":manifest.adjustment_anchors}),
    )?;
    Ok(staging.join("market"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn python_provider_contracts_run_in_the_distribution_runtime() {
        let tests = Path::new(env!("CARGO_MANIFEST_DIR")).join("collectors/tests");
        let output = Command::new(runtime().expect("build the collector runtime before nextest"))
            .args(["-I", "-B", "-m", "unittest", "discover", "-s"])
            .arg(tests)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    #[test]
    fn invalid_request_does_not_start_a_process_or_modify_checkpoints() {
        let dir = tempfile::tempdir().unwrap();
        let request = CollectionRequest {
            source_bindings: Default::default(),
            provider: Provider::Akshare,
            adjustment_source: AdjustmentSource::SinaHfq,
            start: "2020-01-03".parse().unwrap(),
            end: "2020-01-02".parse().unwrap(),
            instruments: BTreeSet::from(["SH600000".into()]),
            benchmarks: BTreeSet::new(),
            universes: BTreeSet::new(),
        };
        assert!(
            collect(
                &request,
                &dir.path().join("staging"),
                &TaskContext::current()
            )
            .is_err()
        );
        assert!(!dir.path().join("staging").exists());
    }
}

#[cfg(test)]
mod supervisor_tests {
    use super::*;
    fn request() -> CollectionRequest {
        CollectionRequest {
            source_bindings: Default::default(),
            provider: Provider::Akshare,
            adjustment_source: AdjustmentSource::SinaHfq,
            start: "2020-01-02".parse().unwrap(),
            end: "2020-01-03".parse().unwrap(),
            instruments: BTreeSet::from(["SH600000".into()]),
            benchmarks: BTreeSet::new(),
            universes: BTreeSet::new(),
        }
    }
    fn command(dir: &Path, mode: &str) -> Command {
        let script = dir.join("mock.py");
        fs::write(&script,r#"
import sys,json,time,os,hashlib
r=json.loads(sys.stdin.readline());mode=sys.argv[1]
open('collector.pid','w').write(str(os.getpid()))
versions={'akshare':'1.19.1','yfinance':'1.7.0','python':'3.12.15'}
identity=r['expected_collector_signature'] if mode!='bad_identity' else 'wrong code'
if mode!='missing_identity': print(json.dumps({'version':6,'event':'ready','collector_signature':identity,'versions':versions}),flush=True)
print(json.dumps({'version':6,'event':'progress','message':'ready'}),flush=True)
if mode=='timeout': time.sleep(30)
if mode=='crash': sys.exit(1)
if mode=='bad_protocol': print(json.dumps({'version':0,'event':'complete'}),flush=True);sys.exit(0)
if 'SH600000' in r['instruments']:
 body=b'original HTTP body'
 open('raw/http-fixture.body','wb').write(body)
 evidence={'kind':'structured_library_response','http_receipts':[{'filename':'http-fixture.body','sha256':'sha256:'+hashlib.sha256(body).hexdigest(),'url':'https://example.test/history'}]}
 if r['operation']=='actions':
  print(json.dumps({'version':6,'event':'action_shard','instrument':'SH600000','history_verified':True,'records':[],'evidence':evidence}),flush=True)
 b={'instrument_id':'SH600000','trade_date':'2020-01-02','open':10.,'high':11.,'low':9.,'close':10.,'adj_open':10.,'adj_high':11.,'adj_low':9.,'adj_close':10.,'volume':1000.,'amount':10000.}
 adjustment={'schema_version':3,'instrument_id':'SH600000','source_symbol':'SH600000','basis':{'kind':'explicit','source':'sina_hfq','source_url':'https://finance.sina.com.cn/realstock/company/sh600000/hfq.js','verified_through':r['end'],'points':[{'effective_date':'1900-01-01','factor':'2'}]}}
 if r['operation']=='market': print(json.dumps({'version':6,'event':'shard','instrument':'SH600000','bars':[b],'adjustment':adjustment,'evidence':evidence}),flush=True)
if mode=='partial': sys.exit(1)
print(json.dumps({'version':6,'event':'complete','collector_signature':r['expected_collector_signature'],'calendar':['2020-01-02','2020-01-03'],'universes':{},'versions':versions}),flush=True)
if mode=='after_complete': print(json.dumps({'version':6,'event':'progress','message':'late'}),flush=True)
"#).unwrap();
        let mut c = Command::new(runtime().unwrap());
        c.args(["-I", "-B", "-u"]).arg(script).arg(mode);
        c
    }
    #[test]
    fn failed_collection_resumes_only_hash_verified_shards() {
        let d = tempfile::tempdir().unwrap();
        let stage = d.path().join("stage");
        let r = request();
        let ctx = TaskContext::current();
        assert!(
            collect_with_command(
                &r,
                &stage,
                &ctx,
                "market",
                command(d.path(), "partial"),
                Duration::from_secs(2)
            )
            .is_err()
        );
        assert!(!stage.join("market/daily_manifest.json").exists());
        collect_with_command(
            &r,
            &stage,
            &ctx,
            "market",
            command(d.path(), "ok"),
            Duration::from_secs(2),
        )
        .unwrap();
        let file = stage.join("market/bars/SH600000.parquet");
        fs::write(file, "tampered").unwrap();
        assert!(
            collect_with_command(
                &r,
                &stage,
                &ctx,
                "market",
                command(d.path(), "ok"),
                Duration::from_secs(2)
            )
            .unwrap_err()
            .to_string()
            .contains("hash mismatch")
        );
    }
    #[test]
    fn opaque_ids_are_not_sent_to_the_provider_and_shards_keep_both_identities() {
        let d = tempfile::tempdir().unwrap();
        let mut r = request();
        let id = "SH600000_OLD";
        r.instruments = [id.to_string()].into();
        r.source_bindings.insert(id.into(), "SH600000".into());
        let stage = d.path().join("bound");
        collect_with_command(
            &r,
            &stage,
            &TaskContext::current(),
            "market",
            command(d.path(), "complete"),
            Duration::from_secs(2),
        )
        .unwrap();
        let bars = read_bars(&stage.join("market/bars").join(format!("{id}.parquet"))).unwrap();
        assert_eq!(bars[0].instrument_id, id);
        let adjustment = AdjustmentRecord::read(&stage.join("market"), id).unwrap();
        assert_eq!(adjustment.source_symbol, "SH600000");
        let data = crate::daily::DailyDataset::open(stage.join("market")).unwrap();
        assert_eq!(data.exchange_symbol(id, r.start).unwrap(), "SH600000");
        assert!(!stage.join("market/bars/SH600000.parquet").exists());
    }
    #[test]
    fn adjustment_checkpoints_are_verified_and_old_owners_are_not_migrated() {
        for mode in ["factor_tamper", "old_owner"] {
            let d = tempfile::tempdir().unwrap();
            let stage = d.path().join("stage");
            let r = request();
            let ctx = TaskContext::current();
            assert!(
                collect_with_command(
                    &r,
                    &stage,
                    &ctx,
                    "market",
                    command(d.path(), "partial"),
                    Duration::from_secs(2)
                )
                .is_err()
            );
            if mode == "factor_tamper" {
                fs::write(
                    stage.join("market/adjustments/SH600000.json"),
                    "modified coefficient",
                )
                .unwrap();
            } else {
                save(
                    &stage.join("owner.json"),
                    &json!({"operation":"market","request":r}),
                )
                .unwrap();
            }
            let before = fs::read(stage.join("owner.json")).unwrap();
            let error = collect_with_command(
                &r,
                &stage,
                &ctx,
                "market",
                command(d.path(), "ok"),
                Duration::from_secs(2),
            )
            .unwrap_err();
            assert!(error.to_string().contains(if mode == "factor_tamper" {
                "adjustment hash mismatch"
            } else {
                "owner/request mismatch"
            }));
            assert_eq!(fs::read(stage.join("owner.json")).unwrap(), before);
            assert!(!stage.join("market/daily_manifest.json").exists());
        }
    }
    #[test]
    fn missing_source_and_stock_index_misclassification_are_rejected_before_acquisition() {
        let mut value = serde_json::to_value(request()).unwrap();
        value.as_object_mut().unwrap().remove("adjustment_source");
        assert!(serde_json::from_value::<CollectionRequest>(value).is_err());
        let mut r = request();
        r.benchmarks.insert("SH600000".into());
        assert!(r.validate().is_err());
        r.benchmarks.clear();
        r.adjustment_source = AdjustmentSource::None;
        assert!(r.validate().is_err());
        r.provider = Provider::Yfinance;
        assert!(r.validate().is_ok());
    }
    #[test]
    fn resumed_http_evidence_and_collector_identity_are_reverified() {
        for operation in ["market", "actions"] {
            let d = tempfile::tempdir().unwrap();
            let stage = d.path().join("stage");
            let r = request();
            let ctx = TaskContext::current();
            assert!(
                collect_with_command(
                    &r,
                    &stage,
                    &ctx,
                    operation,
                    command(d.path(), "partial"),
                    Duration::from_secs(2)
                )
                .is_err()
            );
            fs::write(stage.join("raw/http-fixture.body"), "tampered").unwrap();
            let error = collect_with_command(
                &r,
                &stage,
                &ctx,
                operation,
                command(d.path(), "ok"),
                Duration::from_secs(2),
            )
            .unwrap_err();
            assert!(error.to_string().contains("HTTP evidence hash mismatch"));
            assert!(!stage.join("market/daily_manifest.json").exists());
            assert!(!stage.join("actions.json").exists());
            fs::write(stage.join("raw/http-fixture.body"), "original HTTP body").unwrap();
            let mut owner: Value =
                serde_json::from_slice(&fs::read(stage.join("owner.json")).unwrap()).unwrap();
            owner["collector_signature"] = json!("another code version");
            save(&stage.join("owner.json"), &owner).unwrap();
            assert!(
                collect_with_command(
                    &r,
                    &stage,
                    &ctx,
                    operation,
                    command(d.path(), "ok"),
                    Duration::from_secs(2)
                )
                .unwrap_err()
                .to_string()
                .contains("owner/request mismatch")
            );
        }
    }
    #[test]
    fn action_version_freezes_completion_provenance_and_dependency_identity() {
        let d = tempfile::tempdir().unwrap();
        let stage = d.path().join("stage");
        let path = collect_with_command(
            &request(),
            &stage,
            &TaskContext::current(),
            "actions",
            command(d.path(), "ok"),
            Duration::from_secs(2),
        )
        .unwrap();
        let book = crate::corporate_actions::ActionBundle::read(&path).unwrap();
        let name = "complete-sources-structured.json";
        assert_eq!(
            book.raw_sha256[name],
            hash(&stage.join("raw").join(name)).unwrap()
        );
        let complete: Value =
            serde_json::from_slice(&fs::read(stage.join("raw").join(name)).unwrap()).unwrap();
        assert_eq!(complete["dependencies_sha256"], dependencies_signature());
        assert_eq!(complete["collector_signature"], collector_signature());
        assert!(complete["python_runtime"]["sha256"].is_string());
    }
    #[test]
    fn protocol_reader_bounds_allocation_and_requires_complete_utf8_lines() {
        let mut reader = io::Cursor::new(b"123456789\nnext\n");
        assert!(
            read_line_limited(&mut reader, 8)
                .unwrap_err()
                .to_string()
                .contains("too large")
        );
        assert_eq!(reader.position(), 9);
        let mut reader = io::Cursor::new(b"first\r\nsecond\n");
        assert_eq!(
            read_line_limited(&mut reader, 8).unwrap(),
            Some("first".into())
        );
        assert_eq!(
            read_line_limited(&mut reader, 8).unwrap(),
            Some("second".into())
        );
        assert_eq!(read_line_limited(&mut reader, 8).unwrap(), None);
        assert!(read_line_limited(&mut io::Cursor::new(b"partial"), 8).is_err());
        assert!(read_line_limited(&mut io::Cursor::new(b"\xff\n"), 8).is_err());
    }
    #[test]
    fn diagnostics_must_be_persistable_before_starting_the_collector() {
        let d = tempfile::tempdir().unwrap();
        let stage = d.path().join("stage");
        fs::create_dir_all(stage.join("collector.stderr.log")).unwrap();
        assert!(
            collect_with_command(
                &request(),
                &stage,
                &TaskContext::current(),
                "market",
                command(d.path(), "ok"),
                Duration::from_secs(2)
            )
            .is_err()
        );
        assert!(!stage.join("receipts.json").exists());
        assert!(!stage.join("market/daily_manifest.json").exists());
    }
    #[test]
    fn mismatched_or_absent_identity_cannot_create_security_checkpoints() {
        for mode in ["bad_identity", "missing_identity"] {
            let d = tempfile::tempdir().unwrap();
            let stage = d.path().join("stage");
            assert!(
                collect_with_command(
                    &request(),
                    &stage,
                    &TaskContext::current(),
                    "market",
                    command(d.path(), mode),
                    Duration::from_secs(2)
                )
                .is_err()
            );
            assert!(!stage.join("receipts.json").exists());
            assert!(!stage.join("market/bars/SH600000.parquet").exists());
        }
    }
    #[test]
    fn timeout_cancel_crash_and_protocol_failure_never_publish() {
        for mode in ["timeout", "crash", "bad_protocol", "after_complete"] {
            let d = tempfile::tempdir().unwrap();
            let stage = d.path().join("stage");
            let began = Instant::now();
            assert!(
                collect_with_command(
                    &request(),
                    &stage,
                    &TaskContext::current(),
                    "market",
                    command(d.path(), mode),
                    Duration::from_millis(200)
                )
                .is_err()
            );
            assert!(began.elapsed() < Duration::from_secs(3));
            assert!(!stage.join("market/daily_manifest.json").exists());
        }
        let d = tempfile::tempdir().unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let ctx = TaskContext::new(uuid::Uuid::new_v4(), cancel.clone(), |_| {});
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        assert!(
            collect_with_command(
                &request(),
                &d.path().join("stage"),
                &ctx,
                "market",
                command(d.path(), "timeout"),
                Duration::from_secs(5)
            )
            .is_err()
        );
    }
}
