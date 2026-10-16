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
    /// HiThink REST source, using supplier forward-adjusted prices for research.
    #[default]
    Hithink,
    /// Independent CNINFO implementation announcement candidates.
    Cninfo,
    /// Official native-backed alternative source.
    Tdxaidata,
}
impl Provider {
    /// Stable provider name in manifests and credentials.
    pub fn name(self) -> &'static str {
        match self {
            Self::Hithink => "hithink",
            Self::Cninfo => "cninfo",
            Self::Tdxaidata => "tdxaidata",
        }
    }
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
            (Provider::Tdxaidata, AdjustmentSource::TdxForward)
                | (Provider::Hithink, AdjustmentSource::HithinkForward)
                | (Provider::Cninfo, AdjustmentSource::None)
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
            "announcements.py",
            include_bytes!("../collectors/jfquant_collectors/announcements.py").as_slice(),
        ),
        (
            "tdx.py",
            include_bytes!("../collectors/jfquant_collectors/tdx.py").as_slice(),
        ),
        (
            "reconcile.py",
            include_bytes!("../collectors/jfquant_collectors/reconcile.py").as_slice(),
        ),
    ] {
        hash.update(name);
        hash.update([0]);
        hash.update(bytes);
        hash.update([0]);
    }
    format!("sha256:{:x}", analytics_core::hash::Hex(hash.finalize()))
}
fn invalid(s: impl std::fmt::Display) -> io::Error {
    io::Error::other(s.to_string())
}
fn hash(path: &Path) -> io::Result<String> {
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(fs::read(path)?))
    ))
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
    if evidence["provider"] == "tdxaidata" {
        let receipts = evidence["sdk_receipts"]
            .as_array()
            .filter(|r| !r.is_empty())
            .ok_or_else(|| invalid("tdx_native_receipts_missing"))?;
        for r in receipts {
            let name = r["filename"]
                .as_str()
                .ok_or_else(|| invalid("tdx_receipt_name_missing"))?;
            if Path::new(name).file_name().and_then(|n| n.to_str()) != Some(name)
                || !name.starts_with("sdk-")
                || r["kind"] != "sdk_native_response"
                || r["sdk"] != "tdxaidata"
                || r["version"] != "1.2.2"
                || !matches!(
                    r["function"].as_str(),
                    Some(
                        "GetHISDATsInStr"
                            | "GetTradeCalendarInStr"
                            | "GetSTOCKInStr"
                            | "GetCWDATAInStr"
                            | "GetSpecZSGPInStr"
                            | "GetProDataInStr"
                            | "GetMoreInfoInStr"
                    )
                )
            {
                return Err(invalid("tdx_invalid_native_receipt"));
            }
            let expected = r["sha256"]
                .as_str()
                .ok_or_else(|| invalid("tdx_receipt_hash_missing"))?;
            if hash(&staging.join("raw").join(name))? != expected {
                return Err(invalid("tdx_native_evidence_hash_mismatch"));
            }
            hashes.insert(name.into(), expected.into());
        }
    }
    Ok(hashes)
}
fn verify_selected_receipts(
    request: &CollectionRequest,
    staging: &Path,
    evidence: &Value,
) -> io::Result<BTreeMap<String, String>> {
    if request.provider == Provider::Tdxaidata
        && (evidence["provider"] != "tdxaidata" || evidence["sdk_version"] != "1.2.2")
    {
        return Err(invalid("tdx_source_identity_mismatch"));
    }
    verify_http_receipts(staging, evidence)
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
        analytics_core::hash::Hex(Sha256::digest(include_bytes!(
            "../collectors/requirements.lock"
        )))
    )
}
fn verify_collector_identity(message: &Value) -> io::Result<()> {
    if message["collector_signature"] != collector_signature() {
        return Err(invalid(
            "collector code signature mismatch; rebuild the bundled runtime",
        ));
    }
    if message["versions"]["python"] != "3.12.15" {
        return Err(invalid("collector dependency version mismatch"));
    }
    if message["versions"]["tdxaidata"] != "1.2.2" {
        return Err(invalid("tdx_sdk_version_mismatch"));
    }
    Ok(())
}
/// Current evidence owner only; obsolete provider checkpoints must be reacquired.
pub(crate) fn reviewed_collection_owner(owner: &Value) -> bool {
    owner["collector_protocol"] == 13
        && owner["collector_signature"] == collector_signature()
        && owner["dependencies_sha256"] == dependencies_signature()
        && owner["python_runtime_sha256"]
            == format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(include_bytes!(
                    "../collectors/python-runtime.lock.json"
                )))
            )
        && owner["normalization"] == NORMALIZATION
}
/// Verify the exact current producer; stored evidence uses the same identity check as live data.
pub(crate) fn verify_coefficient_producer(message: &Value, _provider: Provider) -> io::Result<u32> {
    verify_collector_identity(message)?;
    Ok(13)
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
/// Independently collect quotations without depending on a coefficient HTTP endpoint.
/// Raw candidate snapshots are not automatically eligible for research.
pub fn collect_quotes(
    request: &CollectionRequest,
    staging: &Path,
    context: &TaskContext,
) -> io::Result<PathBuf> {
    collect_operation(request, staging, context, "quotes")
}
/// Independent coefficient task, sharing supervision, evidence and cancellation.
pub fn collect_coefficients(
    request: &CollectionRequest,
    staging: &Path,
    context: &TaskContext,
) -> io::Result<PathBuf> {
    if !matches!(
        (request.provider, request.adjustment_source),
        (Provider::Tdxaidata, AdjustmentSource::TdxForward)
    ) {
        return Err(invalid("unsupported independent coefficient source"));
    }
    collect_operation(request, staging, context, "coefficients")
}
/// Reconcile already verified source rows without network or provider credentials.
pub fn reconcile_actions(
    request: &CollectionRequest,
    staging: &Path,
    inputs: &Value,
    context: &TaskContext,
) -> io::Result<PathBuf> {
    fs::create_dir_all(staging)?;
    let input_path = staging.join("reconciliation-inputs.json");
    if input_path.exists() {
        let frozen: Value = serde_json::from_slice(&fs::read(&input_path)?).map_err(invalid)?;
        if &frozen != inputs {
            return Err(invalid("reconciliation inputs changed"));
        }
    } else {
        save(&input_path, inputs)?;
    }
    collect_operation(request, staging, context, "reconcile_actions")
}
fn collect_operation(
    request: &CollectionRequest,
    staging: &Path,
    context: &TaskContext,
    operation: &str,
) -> io::Result<PathBuf> {
    request.validate()?;
    context.check()?;
    if request.provider == Provider::Hithink && operation != "reconcile_actions" {
        return crate::hithink::collect(request, staging, context, operation);
    }
    if request.provider == Provider::Cninfo && operation != "announcements" {
        return Err(invalid("unsupported_cninfo_operation"));
    }
    let mut command = Command::new(runtime()?);
    let _private = if request.provider == Provider::Tdxaidata {
        if !matches!(
            operation,
            "market" | "quotes" | "coefficients" | "actions" | "calendar" | "members"
        ) {
            return Err(invalid("unsupported_tdxaidata_operation"));
        }
        let private = staging.join(".tdx-runtime");
        // A killed service can leave an SDK INI/user.ini; remove the whole private
        // directory before resuming. No files from it are evidence or checkpoints.
        if private.exists() {
            fs::remove_dir_all(&private)?;
        }
        command.env(
            "JFQUANT_TDX_KEY",
            crate::credentials::load_source("tdxaidata")?,
        );
        Some(PrivateRuntime(private))
    } else {
        None
    };
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
/// Remove only reserved private SDK directories during exclusive service recovery.
/// Failed checkpoints/evidence stay intact; symlinks are never traversed.
pub fn cleanup_private_runtimes(root: &Path) -> io::Result<usize> {
    let mut removed = 0;
    if !root.is_dir() {
        return Ok(0);
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if entry.file_name() == ".tdx-runtime" {
            fs::remove_dir_all(entry.path())?;
            removed += 1;
        } else {
            removed += cleanup_private_runtimes(&entry.path())?;
        }
    }
    Ok(removed)
}
struct PrivateRuntime(PathBuf);
impl Drop for PrivateRuntime {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// Collect a calendar or current member list without downloading any quotations.
pub fn collect_reference(
    request: &CollectionRequest,
    staging: &Path,
    context: &TaskContext,
    kind: &str,
) -> io::Result<PathBuf> {
    if !matches!(kind, "calendar" | "members")
        || request.instruments.len() != 1
        || (kind == "members" && request.universes.is_empty())
    {
        return Err(invalid("invalid_reference_request"));
    }
    collect_operation(request, staging, context, kind)
}
/// Fetch fixed-source implementation documents through the same supervised collector.
pub fn collect_announcements(
    request: &CollectionRequest,
    staging: &Path,
    context: &TaskContext,
) -> io::Result<PathBuf> {
    if request.provider != Provider::Cninfo || request.adjustment_source != AdjustmentSource::None {
        return Err(invalid("announcement_source_unsupported: select cninfo"));
    }
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
            provider: Provider::Hithink,
            adjustment_source: AdjustmentSource::HithinkForward,
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
            provider: Provider::Tdxaidata,
            adjustment_source: AdjustmentSource::TdxForward,
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
versions={'tdxaidata':'1.2.2','python':'3.12.15'}
identity=r['expected_collector_signature'] if mode!='bad_identity' else 'wrong code'
if mode!='missing_identity': print(json.dumps({'version':13,'event':'ready','collector_signature':identity,'versions':versions}),flush=True)
print(json.dumps({'version':13,'event':'progress','message':'ready'}),flush=True)
if mode=='timeout': time.sleep(30)
if mode=='crash': sys.exit(1)
if mode=='bad_protocol': print(json.dumps({'version':0,'event':'complete'}),flush=True);sys.exit(0)
if 'SH600000' in r['instruments']:
 body=b'original HTTP body'
 open('raw/http-fixture.body','wb').write(body)
 open('raw/sdk-fixture.body','wb').write(body)
 evidence={'provider':'tdxaidata','sdk_version':'1.2.2','sdk_receipts':[{'kind':'sdk_native_response','sdk':'tdxaidata','version':'1.2.2','function':'GetHISDATsInStr','filename':'sdk-fixture.body','sha256':'sha256:'+hashlib.sha256(body).hexdigest()}],'kind':'structured_library_response','http_receipts':[{'filename':'http-fixture.body','sha256':'sha256:'+hashlib.sha256(body).hexdigest(),'url':'https://example.test/history'}]}
 if r['operation']=='actions':
  print(json.dumps({'version':13,'event':'action_shard','instrument':'SH600000','history_verified':True,'records':[],'evidence':evidence}),flush=True)
 b={'instrument_id':'SH600000','trade_date':'2020-01-02','open':10.,'high':11.,'low':9.,'close':10.,'adj_open':10.,'adj_high':11.,'adj_low':9.,'adj_close':10.,'volume':1000.,'amount':10000.}
 adjustment={'schema_version':3,'instrument_id':'SH600000','source_symbol':'SH600000','basis':{'kind':'explicit','source':'tdx_forward','source_url':'tdxaidata:get_market_data:none:ForwardFactor','verified_through':r['end'],'points':[{'effective_date':'1900-01-01','factor':'2'}]}}
 if r['operation']=='coefficients': adjustment['basis']['points'].append({'effective_date':'2020-01-03','factor':'4'})
 if r['operation']=='coefficients': print(json.dumps({'version':13,'event':'adjustment_shard','instrument':'SH600000','adjustment':adjustment,'evidence':evidence}),flush=True)
 if r['operation']=='market': print(json.dumps({'version':13,'event':'shard','instrument':'SH600000','bars':[b],'adjustment':adjustment,'evidence':evidence}),flush=True)
if mode=='partial': sys.exit(1)
print(json.dumps({'version':13,'event':'complete','collector_signature':r['expected_collector_signature'],'calendar':['2020-01-02','2020-01-03'],'universes':{},'versions':versions}),flush=True)
if mode=='after_complete': print(json.dumps({'version':13,'event':'progress','message':'late'}),flush=True)
"#).unwrap();
        let mut c = Command::new(runtime().unwrap());
        c.args(["-I", "-B", "-u"]).arg(script).arg(mode);
        c
    }
    #[test]
    fn independent_coefficients_resume_and_view_preserve_original_raw_scale() {
        let dir = tempfile::tempdir().unwrap();
        let stage = dir.path().join("coefficients");
        let r = request();
        assert!(
            collect_with_command(
                &r,
                &stage,
                &TaskContext::current(),
                "coefficients",
                command(dir.path(), "partial"),
                Duration::from_secs(2)
            )
            .is_err()
        );
        assert!(!stage.join("coefficients.json").exists());
        let path = collect_with_command(
            &r,
            &stage,
            &TaskContext::current(),
            "coefficients",
            command(dir.path(), "ok"),
            Duration::from_secs(2),
        )
        .unwrap();
        let bundle = CoefficientBundle::read(&path).unwrap();
        fs::create_dir_all(dir.path().join(".jfquant")).unwrap();
        fs::write(dir.path().join(".jfquant/workspace.json"), "{}").unwrap();
        let base = dir.path().join("qlib_bin");
        fs::create_dir_all(base.join("calendars")).unwrap();
        fs::create_dir_all(base.join("features/sh600000")).unwrap();
        fs::write(base.join("calendars/day.txt"), "2020-01-02\n2020-01-03").unwrap();
        for (name, values) in [
            ("close", vec![1., 1.]),
            ("open", vec![1., 1.]),
            ("factor", vec![0.1, 0.2]),
            ("volume", vec![100., 100.]),
            ("amount", vec![10., 10.]),
        ] {
            let bytes: Vec<_> = std::iter::once(0_f32)
                .chain(values)
                .flat_map(f32::to_le_bytes)
                .collect();
            fs::write(
                base.join(format!("features/sh600000/{name}.day.bin")),
                bytes,
            )
            .unwrap();
        }
        let root = dir.path().join("view");
        fs::create_dir_all(root.join("adjustments")).unwrap();
        fs::copy(
            stage.join("market/adjustments/SH600000.json"),
            root.join("adjustments/SH600000.json"),
        )
        .unwrap();
        let record = AdjustmentRecord::read(&root, "SH600000").unwrap();
        let mut view = crate::market_view::MarketView {
            schema_version: 1,
            base_path: "qlib_bin".into(),
            base_sha256: crate::market_view::tree_hash(&base).unwrap(),
            coefficients_path: "coefficients/coefficients.json".into(),
            coefficients_sha256: hash(&path).unwrap(),
            recovery: "qlib-normalized-price-over-original-factor;cent-tick;shares-v1".into(),
            records: bundle.records.clone(),
            anchors: BTreeMap::from([("SH600000".into(), record.anchor(r.start).unwrap())]),
        };
        save(&root.join("market_view.json"), &view).unwrap();
        let data = crate::daily::DailyDataset::open(&root).unwrap();
        assert_eq!(
            data.raw_field("SH600000", "close").unwrap().get(0),
            Some(10.)
        );
        assert_eq!(
            data.raw_field("SH600000", "close").unwrap().get(1),
            Some(5.)
        );
        assert_eq!(data.field("SH600000", "close").unwrap().get(1), Some(10.));
        assert!((data.raw_volume("SH600000").unwrap().get(1).unwrap() - 2000.).abs() < 0.001);
        assert!((data.field("SH600000", "volume").unwrap().get(1).unwrap() - 10.).abs() < 0.001);
        assert!((data.field("SH600000", "vwap").unwrap().get(1).unwrap() - 10.).abs() < 0.001);
        assert!(!data.supports_provider_training());
        assert_eq!(data.field("SH600000", "factor").unwrap().get(0), Some(1.));
        assert_eq!(data.field("SH600000", "factor").unwrap().get(1), Some(2.));
        view.anchors.get_mut("SH600000").unwrap().factor = "99".into();
        save(&root.join("market_view.json"), &view).unwrap();
        assert!(crate::daily::DailyDataset::open(&root).is_err());
        view.anchors.get_mut("SH600000").unwrap().factor = "2".into();
        let original_volume = fs::read(base.join("features/sh600000/volume.day.bin")).unwrap();
        fs::write(
            base.join("features/sh600000/volume.day.bin"),
            [0_f32, -10., 100.]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let original_sha = view.base_sha256.clone();
        view.base_sha256 = crate::market_view::tree_hash(&base).unwrap();
        save(&root.join("market_view.json"), &view).unwrap();
        let invalid = crate::daily::DailyDataset::open(&root).unwrap();
        assert!(invalid.field("SH600000", "volume").is_err());
        fs::write(
            base.join("features/sh600000/volume.day.bin"),
            original_volume,
        )
        .unwrap();
        view.base_sha256 = original_sha;
        fs::write(stage.join("raw/http-fixture.body"), "tampered").unwrap();
        assert!(CoefficientBundle::read(&path).is_err());
        // The composition never changes or replaces the original factor bytes.
        assert_eq!(
            crate::market_view::tree_hash(&base).unwrap(),
            view.base_sha256
        );
    }
    #[test]
    fn complete_checkpoint_resume_preserves_calendar_without_starting_a_process() {
        let temp = tempfile::tempdir().unwrap();
        let stage = temp.path().join("stage");
        let request = request();
        let context = TaskContext::current();
        collect_with_command(
            &request,
            &stage,
            &context,
            "market",
            command(temp.path(), "ok"),
            Duration::from_secs(2),
        )
        .unwrap();
        let original = fs::read(stage.join("raw/complete-sources-structured.json")).unwrap();
        let manifest = fs::read(stage.join("market/daily_manifest.json")).unwrap();
        collect_with_command(
            &request,
            &stage,
            &context,
            "market",
            Command::new("/nonexistent-must-not-start"),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(
            fs::read(stage.join("market/daily_manifest.json")).unwrap(),
            manifest
        );
        assert_eq!(
            fs::read(stage.join("raw/complete-sources-structured.json")).unwrap(),
            original
        );
        let mut complete: Value = serde_json::from_slice(&original).unwrap();
        complete["collector_signature"] = json!("tampered");
        save(
            &stage.join("raw/complete-sources-structured.json"),
            &complete,
        )
        .unwrap();
        assert!(
            collect_with_command(
                &request,
                &stage,
                &context,
                "market",
                Command::new("/nonexistent-must-not-start"),
                Duration::from_secs(2)
            )
            .is_err()
        );
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
        r.provider = Provider::Cninfo;
        assert!(r.validate().is_ok());
    }
    #[test]
    fn resumed_http_evidence_and_collector_identity_are_reverified() {
        for operation in ["market", "actions", "coefficients"] {
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

/// Independently acquired full coefficient request; a failed shard never completes it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoefficientBundle {
    /// Current immutable bundle format.
    pub schema_version: u32,
    /// Frozen source, locator and date request.
    pub request: CollectionRequest,
    /// Exact parsed record hashes under market/adjustments.
    pub records: BTreeMap<String, String>,
    /// Exact raw responses and protocol evidence under raw.
    pub evidence: BTreeMap<String, String>,
    /// Verified bundled runtime and dependency receipt.
    pub collector: Value,
}
impl CoefficientBundle {
    /// Revalidate parsed records and raw evidence before registration, reuse or composition.
    pub fn read(path: &Path) -> io::Result<Self> {
        let b: Self = serde_json::from_slice(&fs::read(path)?).map_err(invalid)?;
        b.request.validate()?;
        if b.schema_version != 1
            || !matches!(
                (b.request.provider, b.request.adjustment_source),
                (Provider::Tdxaidata, AdjustmentSource::TdxForward)
            )
            || b.records.keys().cloned().collect::<BTreeSet<_>>() != b.request.instruments
            || b.evidence.is_empty()
        {
            return Err(invalid("unsupported_data_format: coefficient version"));
        }
        let protocol = verify_coefficient_producer(&b.collector, b.request.provider)?;
        if b.collector["version"] != protocol || b.collector["event"] != "complete" {
            return Err(invalid("coefficient completion protocol mismatch"));
        }
        let root = path
            .parent()
            .ok_or_else(|| invalid("coefficient parent missing"))?;
        for (name, sha) in &b.evidence {
            if Path::new(name).components().count() != 1
                || !Path::new(name)
                    .components()
                    .all(|c| matches!(c, std::path::Component::Normal(_)))
                || hash(&root.join("raw").join(name))? != *sha
            {
                return Err(invalid("coefficient raw evidence damaged"));
            }
        }
        for (id, sha) in &b.records {
            let r = AdjustmentRecord::read(&root.join("market"), id)?;
            let name = format!("{id}-actions-structured.json");
            let msg: Value = serde_json::from_slice(&fs::read(root.join("raw").join(&name))?)
                .map_err(invalid)?;
            if b.evidence.get(&name) != Some(&hash(&root.join("raw").join(&name))?)
                || msg["version"] != protocol
                || msg["event"] != "adjustment_shard"
                || msg["instrument"] != *id
                || msg["adjustment"] != serde_json::to_value(&r).map_err(invalid)?
            {
                return Err(invalid("coefficient parsed/raw binding mismatch"));
            }
            for (name, sha) in verify_http_receipts(root, &msg["evidence"])? {
                if b.evidence.get(&name) != Some(&sha) {
                    return Err(invalid("coefficient HTTP receipt binding mismatch"));
                }
            }

            if r.source_symbol != *b.request.source_bindings.get(id).unwrap_or(id)
                || !matches!(
                    (&r.basis, b.request.benchmarks.contains(id)),
                    (
                        AdjustmentBasis::Explicit {
                            source: FactorSource::TdxForward,
                            ..
                        },
                        false
                    ) | (AdjustmentBasis::PriceIndex { .. }, true)
                )
            {
                return Err(invalid("coefficient source binding mismatch"));
            }
            if let AdjustmentBasis::Explicit { source, .. } = &r.basis
                && (*source == FactorSource::TdxForward)
                    != (b.request.provider == Provider::Tdxaidata)
            {
                return Err(invalid("coefficient source mismatch"));
            }
            if hash(&root.join("market/adjustments").join(format!("{id}.json")))? != *sha
                || (b.request.provider != Provider::Tdxaidata && r.on(b.request.start)?.is_none())
                || r.on(b.request.end)?.is_none()
            {
                return Err(invalid("coefficient record damaged/coverage missing"));
            }
        }
        Ok(b)
    }
}

#[cfg(test)]
mod tdx_evidence_tests {
    use super::*;
    #[test]
    fn native_evidence_hashes_and_private_recovery_are_checked_without_using_a_keychain() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        fs::create_dir_all(root.join("raw")).unwrap();
        let file = root.join("raw/sdk-fixture.body");
        fs::write(&file, b"{\"ErrorId\":0}").unwrap();
        let evidence = json!({"provider":"tdxaidata","sdk_receipts":[{"kind":"sdk_native_response","sdk":"tdxaidata","version":"1.2.2","function":"GetHISDATsInStr","arguments":["600000.SH"],"filename":"sdk-fixture.body","sha256":hash(&file).unwrap()}]});
        assert_eq!(verify_http_receipts(root, &evidence).unwrap().len(), 1);
        fs::write(&file, b"tampered").unwrap();
        assert!(verify_http_receipts(root, &evidence).is_err());
        assert!(
            verify_http_receipts(root, &json!({"provider":"tdxaidata","sdk_receipts":[]})).is_err()
        );
        fs::create_dir_all(root.join("staging/.tdx-runtime")).unwrap();
        fs::write(root.join("staging/.tdx-runtime/user.ini"), b"fixture only").unwrap();
        fs::write(root.join("staging/receipts.json"), b"{}").unwrap();
        assert_eq!(cleanup_private_runtimes(root).unwrap(), 1);
        assert!(root.join("staging/receipts.json").exists());
        assert!(!root.join("staging/.tdx-runtime").exists());
    }
}

#[cfg(test)]
mod historical_producer_tests {
    use super::*;
    #[test]
    fn historical_reader_does_not_relax_live_handshake_or_unknown_producers() {
        let mut receipt = json!({"collector_signature":"sha256:4311df0d8844c6cbfc9a9318e970ad806af634ec2cba47a4d8a0db9788c503ba","versions":{"python":"3.12.15","akshare":"1.19.1","yfinance":"1.7.0"}});
        assert!(verify_coefficient_producer(&receipt, Provider::Hithink).is_err());
        assert!(verify_collector_identity(&receipt).is_err());
        assert!(verify_coefficient_producer(&receipt, Provider::Tdxaidata).is_err());
        let known = json!({"collector_signature":"sha256:f5b05566abb8557c3651fc1d68d98289f7443ef406bb72d0cd07aa3ef3adcd7d","versions":{"python":"3.12.15","tdxaidata":"1.2.2"}});
        assert!(verify_coefficient_producer(&known, Provider::Tdxaidata).is_err());
        assert!(verify_collector_identity(&known).is_err());
        receipt["collector_signature"] = json!("unknown");
        assert!(verify_coefficient_producer(&receipt, Provider::Hithink).is_err());
    }
}

#[cfg(test)]
mod registered_coefficient_acceptance {
    /// Optional real-data acceptance; does not modify the registered bundle.
    #[test]
    #[ignore = "requires JFQUANT_COEFFICIENT_FIXTURE pointing to an immutable bundle"]
    fn registered_historical_coefficients_validate_without_rewriting_receipts() {
        let path = std::env::var_os("JFQUANT_COEFFICIENT_FIXTURE").expect("fixture path");
        let b = super::CoefficientBundle::read(std::path::Path::new(&path)).unwrap();
        assert!(!b.records.is_empty());
    }
}

mod process;
use process::*;
mod publication;
use publication::*;
