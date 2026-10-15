//! Immutable offline reconciliation using the same bundled rules as live acquisition.
use super::*;
use crate::collector::{CollectionRequest, Provider};
use analytics_core::DateRange;
use serde_json::json;
use std::path::PathBuf;

#[cfg(test)]
use crate::collector::REVIEWED_COLLECTOR_V9;
fn verify_bytes(path: &Path, expected: &str) -> io::Result<Vec<u8>> {
    let bytes = fs::read(path)?;
    if format!("sha256:{:x}", Sha256::digest(&bytes)) != expected {
        return Err(invalid("reconciliation evidence hash mismatch"));
    }
    Ok(bytes)
}
fn safe_name(name: &str) -> io::Result<()> {
    if Path::new(name).file_name().and_then(|p| p.to_str()) != Some(name) {
        return Err(invalid("unsafe reconciliation evidence path"));
    }
    Ok(())
}
fn capture(
    output: &Path,
    name: &str,
    bytes: &[u8],
    hashes: &mut BTreeMap<String, String>,
) -> io::Result<()> {
    safe_name(name)?;
    let hash = format!("sha256:{:x}", Sha256::digest(bytes));
    if hashes.get(name).is_some_and(|h| *h != hash) {
        return Err(invalid("reconciliation evidence collision"));
    }
    fs::write(output.join("raw").join(name), bytes)?;
    hashes.insert(name.into(), hash);
    Ok(())
}
/// Validate a terminal collector's owned native receipts. Empty replies remain unknown.
fn native_evidence(
    root: &Path,
    output: &Path,
    hashes: &mut BTreeMap<String, String>,
) -> io::Result<BTreeMap<(String, String), Vec<Value>>> {
    let owner_bytes = fs::read(root.join("owner.json"))?;
    let owner: Value = serde_json::from_slice(&owner_bytes).map_err(invalid)?;
    if !crate::collector::reviewed_collection_owner(&owner)
        || owner["operation"] != "actions"
        || owner["request"]["provider"] != "tdxaidata"
    {
        return Err(invalid("unreviewed native evidence producer/operation"));
    }
    let request: CollectionRequest =
        serde_json::from_value(owner["request"].clone()).map_err(invalid)?;
    request.validate()?;
    let checkpoints: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(root.join("receipts.json"))?).map_err(invalid)?;
    let receipt_bytes = fs::read(root.join("raw/sdk-requests.jsonl"))?;
    let receipts = String::from_utf8(receipt_bytes.clone()).map_err(invalid)?;
    let mut out = BTreeMap::<(String, String), Vec<Value>>::new();
    for line in receipts.lines() {
        task_context_check()?;
        let r: Value = serde_json::from_str(line).map_err(invalid)?;
        if r["function"] != "GetCWDATAInStr" {
            continue;
        }
        if r["sdk"] != "tdxaidata" || r["version"] != "1.2.2" {
            return Err(invalid("native SDK identity mismatch"));
        }
        let code = r["arguments"][0]
            .as_str()
            .ok_or_else(|| invalid("native security missing"))?;
        if code.len() != 9 || !code.is_ascii() || code.as_bytes()[6] != b'.' {
            return Err(invalid("native security invalid"));
        }
        let symbol = format!("{}{}", &code[7..], &code[..6]);
        let id = request
            .instruments
            .iter()
            .find(|id| request.source_bindings.get(*id).unwrap_or(id) == &symbol)
            .ok_or_else(|| invalid("native evidence source binding not requested"))?
            .clone();
        if r["arguments"][1] != format!("{} 00:00:00", request.start)
            || r["arguments"][2] != format!("{} 00:00:00", request.end)
        {
            return Err(invalid("native evidence request interval mismatch"));
        }
        let filename = r["filename"]
            .as_str()
            .ok_or_else(|| invalid("native evidence filename missing"))?;
        safe_name(filename)?;
        let bytes = verify_bytes(
            &root.join("raw").join(filename),
            r["sha256"].as_str().unwrap_or(""),
        )?;
        capture(output, filename, &bytes, hashes)?;
        let (decoded, _, bad) = encoding_rs::GBK.decode(&bytes);
        if bad {
            return Err(invalid("native evidence encoding invalid"));
        }
        let v: Value = serde_json::from_str(&decoded).map_err(invalid)?;
        if !v["ErrorId"].is_null() && v["ErrorId"] != "0" && v["ErrorId"] != 0 {
            continue;
        }
        // An unshaped empty reply contributes no coverage or zero-event assertion.
        if v["Date"].is_null() && v["Value"] == json!([]) {
            continue;
        }
        let Some(expected) = checkpoints.get(&id) else {
            continue;
        };
        let shard_name = format!("{id}-actions-structured.json");
        let shard_bytes = verify_bytes(&root.join("raw").join(&shard_name), expected)?;
        let shard: Value = serde_json::from_slice(&shard_bytes).map_err(invalid)?;
        if shard["instrument"] != id
            || shard["evidence"]["provider"] != "tdxaidata"
            || !shard["evidence"]["sdk_receipts"]
                .as_array()
                .is_some_and(|refs| refs.contains(&r))
        {
            return Err(invalid(
                "native evidence is not bound to a completed security receipt",
            ));
        }
        capture(
            output,
            &format!("native-shard-{}.json", &expected[7..]),
            &shard_bytes,
            hashes,
        )?;
        let dates = v["Date"]
            .as_array()
            .ok_or_else(|| invalid("native dates missing"))?;
        let kinds = v["Type"]
            .as_array()
            .ok_or_else(|| invalid("native types missing"))?;
        let values = v["Value"]
            .as_array()
            .ok_or_else(|| invalid("native terms missing"))?;
        if dates.len() != kinds.len() || dates.len() != values.len() {
            return Err(invalid("truncated native event table"));
        }
        for ((date, kind), terms) in dates.iter().zip(kinds).zip(values) {
            let date = date
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| date.to_string());
            let date = chrono::NaiveDate::parse_from_str(&date, "%Y%m%d").map_err(invalid)?;
            if date < request.start || date > request.end {
                continue;
            }
            let terms = terms
                .as_array()
                .filter(|t| t.len() == 4)
                .ok_or_else(|| invalid("native event terms truncated"))?;
            let value = |i: usize| {
                terms[i]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| terms[i].to_string())
            };
            let kind = kind
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| kind.to_string());
            let event = json!({"date":date,"type":kind,"bonus_per_ten":value(0),"rights_price":value(1),"shares_per_ten":value(2),"rights_per_ten":value(3)});
            let key = (id.clone(), symbol.clone());
            if !out.entry(key.clone()).or_default().contains(&event) {
                out.get_mut(&key).unwrap().push(event);
            }
        }
    }
    let digest = format!("{:x}", Sha256::digest(&receipt_bytes));
    capture(
        output,
        &format!("native-journal-{digest}.jsonl"),
        &receipt_bytes,
        hashes,
    )?;
    capture(
        output,
        &format!("native-owner-{:x}.json", Sha256::digest(&owner_bytes)),
        &owner_bytes,
        hashes,
    )?;
    Ok(out)
}
// Freeze complete single-security announcement receipts, even from a failed batch.
// Documents remain candidates until the existing field-evidence revision is applied.
pub(super) fn announcement_evidence(
    root: &Path,
    output: &Path,
    hashes: &mut BTreeMap<String, String>,
) -> io::Result<String> {
    let owner_bytes = fs::read(root.join("owner.json"))?;
    let owner: Value = serde_json::from_slice(&owner_bytes).map_err(invalid)?;
    if !crate::collector::reviewed_collection_owner(&owner)
        || owner["operation"] != "announcements"
        || owner["request"]["provider"] != "akshare"
    {
        return Err(invalid("unreviewed announcement evidence producer"));
    }
    let request: CollectionRequest =
        serde_json::from_value(owner["request"].clone()).map_err(invalid)?;
    request.validate()?;
    let receipts: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(root.join("receipts.json"))?).map_err(invalid)?;
    if receipts.is_empty() {
        return Err(invalid("no complete single-security announcement receipts"));
    }
    let mut documents = Vec::new();
    let mut wanted = BTreeMap::new();
    for (id, expected) in &receipts {
        if !request.instruments.contains(id) {
            return Err(invalid("unrequested announcement security"));
        }
        let name = format!("{id}-actions-structured.json");
        let bytes = verify_bytes(&root.join("raw").join(&name), expected)?;
        let shard: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
        if shard["instrument"] != *id || shard["evidence"]["pagination_complete"] != true {
            return Err(invalid("incomplete announcement security receipt"));
        }
        wanted.insert(name, expected.clone());
        for receipt in shard["evidence"]["http_receipts"]
            .as_array()
            .ok_or_else(|| invalid("announcement HTTP receipts missing"))?
        {
            wanted.insert(
                receipt["filename"]
                    .as_str()
                    .ok_or_else(|| invalid("HTTP filename missing"))?
                    .to_owned(),
                receipt["sha256"]
                    .as_str()
                    .ok_or_else(|| invalid("HTTP hash missing"))?
                    .to_owned(),
            );
        }
        documents.extend(
            shard["evidence"]["documents"]
                .as_array()
                .ok_or_else(|| invalid("announcement documents missing"))?
                .clone(),
        );
    }
    let manifest = json!({"schema_version":2,"receipts":receipts,"documents":documents});
    let verified = super::review::verified_document_records(root, &manifest)?;
    for (document, _) in verified.values() {
        for (field, digest) in [("pdf", "pdf_sha256"), ("text", "text_sha256")] {
            wanted.insert(
                document[field].as_str().unwrap().to_owned(),
                document[digest].as_str().unwrap().to_owned(),
            );
        }
    }
    let identity = format!("{:x}", Sha256::digest(&owner_bytes));
    let mut files = BTreeMap::new();
    for (name, expected) in wanted {
        task_context_check()?;
        safe_name(&name)?;
        let bytes = verify_bytes(&root.join("raw").join(&name), &expected)?;
        let copied = format!("announcement-{identity}-{name}");
        capture(output, &copied, &bytes, hashes)?;
        files.insert(name, copied);
    }
    let owner_file = format!("announcement-owner-{identity}.json");
    capture(output, &owner_file, &owner_bytes, hashes)?;
    let bytes=serde_json::to_vec_pretty(&json!({"schema_version":1,"owner":owner,"owner_file":owner_file,"owner_sha256":format!("sha256:{identity}"),"manifest":manifest,"files":files,"candidate_only":true,"resolution":"use existing ReviseActions field evidence; no automatic coverage assertion"})).map_err(invalid)?;
    let name = format!("announcement-evidence-{:x}.json", Sha256::digest(&bytes));
    capture(output, &name, &bytes, hashes)?;
    Ok(name)
}
/// Publish a new format-6 bundle, including unresolved ordinary issues and skip warnings.
/// `evidence_roots` are terminal action/announcement directories resolved by workflow.
pub fn apply(
    source_path: &Path,
    evidence_roots: &[PathBuf],
    calendar: &[Date],
    output: &Path,
    context: &analytics_core::task::TaskContext,
) -> io::Result<ActionBundle> {
    context.report("校验基础版本与历史采集实现")?;
    let started = std::time::Instant::now();
    let source = ActionBundle::read(source_path)?;
    if source.downloader != "JFQuant analytics-workflow synthetic-demo-v1" {
        let root = source_path
            .parent()
            .ok_or_else(|| invalid("action root missing"))?;
        let name = "complete-sources-structured.json";
        let complete: Value = serde_json::from_slice(&verify_bytes(
            &root.join("raw").join(name),
            source
                .raw_sha256
                .get(name)
                .ok_or_else(|| invalid("primary producer receipt missing"))?,
        )?)
        .map_err(invalid)?;
        crate::collector::verify_coefficient_producer(&complete, Provider::Akshare)?;
    }
    if output.exists() {
        return Err(invalid("reconciliation output already exists"));
    }
    let source_root = source_path
        .parent()
        .ok_or_else(|| invalid("action root missing"))?;
    fs::create_dir_all(output.join("raw"))?;
    let mut bundle = source.clone();
    for (name, expected) in &source.raw_sha256 {
        let bytes = verify_bytes(&source_root.join("raw").join(name), expected)?;
        if name == "reconciliation-v1.json" {
            bundle.raw_sha256.remove(name);
            capture(
                output,
                &format!("previous-reconciliation-{}.json", &expected[7..]),
                &bytes,
                &mut bundle.raw_sha256,
            )?;
        } else {
            capture(output, name, &bytes, &mut bundle.raw_sha256)?;
        }
    }
    context.report("核验并冻结完整单证券原生回执")?;
    let mut native = BTreeMap::<(String, String), Vec<Value>>::new();
    let mut announcements = Vec::new();
    for root in evidence_roots {
        let owner: Value =
            serde_json::from_slice(&fs::read(root.join("owner.json"))?).map_err(invalid)?;
        if owner["operation"] == "announcements" {
            announcements.push(announcement_evidence(root, output, &mut bundle.raw_sha256)?);
            continue;
        }
        for (id, events) in native_evidence(root, output, &mut bundle.raw_sha256)? {
            for event in events {
                if !native.entry(id.clone()).or_default().contains(&event) {
                    native.get_mut(&id).unwrap().push(event);
                }
            }
        }
    }
    let mut inputs = serde_json::Map::new();
    let mut first = None::<Date>;
    let mut last = None::<Date>;
    for (id, coverage) in &source.structured_coverage {
        if coverage.source != "akshare.cninfo/implemented/per-ten-shares-v1" {
            continue;
        }
        let name = coverage
            .evidence
            .first()
            .ok_or_else(|| invalid("source evidence missing"))?;
        let captured: Value = serde_json::from_slice(&verify_bytes(
            &source_root.join("raw").join(name),
            source
                .raw_sha256
                .get(name)
                .ok_or_else(|| invalid("unhashed primary evidence"))?,
        )?)
        .map_err(invalid)?;
        let e = &captured["evidence"];
        let rows = e
            .get("cninfo_decimal_records")
            .unwrap_or(&e["cninfo"]["records"])
            .as_array()
            .ok_or_else(|| invalid("CNINFO records unavailable for replay"))?;
        let symbol = source.source_symbol(id)?;
        inputs.insert(id.clone(), json!({"symbol":symbol,"rows":rows,"start":coverage.dates.start,"end":coverage.dates.end,
            "sina_rows":e["sina_dividends"].as_array().cloned().unwrap_or_default(),"rights":e["sina_rights"].as_array().cloned().unwrap_or_default(),
            "native":native.get(&(id.clone(),symbol.to_owned())).cloned().unwrap_or_default()}));
        first = Some(first.map_or(coverage.dates.start, |d| d.min(coverage.dates.start)));
        last = Some(last.map_or(coverage.dates.end, |d| d.max(coverage.dates.end)));
    }
    if inputs.is_empty() {
        return Err(invalid("no supported primary evidence to reconcile"));
    }
    let request = CollectionRequest {
        provider: Provider::Akshare,
        adjustment_source: crate::adjustment::AdjustmentSource::SinaHfq,
        start: first.unwrap(),
        end: last.unwrap(),
        instruments: inputs.keys().cloned().collect(),
        source_bindings: inputs
            .keys()
            .filter_map(|id| {
                source
                    .source_bindings
                    .get(id)
                    .map(|l| (id.clone(), l.symbol.clone()))
            })
            .collect(),
        benchmarks: BTreeSet::new(),
        universes: BTreeSet::new(),
    };
    let staging = output.join("replay");
    let evidence_seconds = started.elapsed().as_secs_f64();
    context.report(format!("离线重放{}只证券的统一核对规则", inputs.len()))?;
    let replay_started = std::time::Instant::now();
    let result_path = crate::collector::reconcile_actions(
        &request,
        &staging,
        &json!({"securities":inputs,"calendar":calendar}),
        context,
    )?;
    let result: Value = serde_json::from_slice(&fs::read(&result_path)?).map_err(invalid)?;
    // Keep audited announcement replacements when replaying the original provider rows.
    let replay_seconds = replay_started.elapsed().as_secs_f64();
    context.report("校验规范事件与核对回执，准备发布新版本")?;
    let mut reviewed = BTreeMap::new();
    let mut review_evidence = BTreeMap::new();
    let mut closed_issues = BTreeSet::new();
    for name in source
        .raw_sha256
        .keys()
        .filter(|n| n.starts_with("review-") && n.ends_with(".json"))
    {
        let receipt: Value = serde_json::from_slice(&verify_bytes(
            &source_root.join("raw").join(name),
            &source.raw_sha256[name],
        )?)
        .map_err(invalid)?;
        for resolution in receipt["request"]["resolve_issues"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if let Some(id) = resolution["id"].as_str() {
                closed_issues.insert(id.to_owned());
            }
        }
        for revision in receipt["request"]["events"]
            .as_array()
            .ok_or_else(|| invalid("invalid review receipt"))?
        {
            let event: CorporateAction =
                serde_json::from_value(revision["replacement"].clone()).map_err(invalid)?;
            if let Some(current) = source.actions.iter().find(|a| a.id == event.id)
                && serde_json::to_value(current).map_err(invalid)?
                    == serde_json::to_value(&event).map_err(invalid)?
            {
                let key = (event.instrument.clone(), event.ex_date);
                reviewed.insert(key.clone(), current.clone());
                review_evidence.insert(key, name.clone());
            }
        }
    }
    let mut resolved = BTreeSet::new();
    let mut receipt_results = serde_json::Map::new();
    for (id, spec) in inputs {
        let mut r = result["results"][&id].clone();
        if r.is_null() {
            return Err(invalid("missing reconciliation result"));
        }
        for a in r["records"]
            .as_array_mut()
            .ok_or_else(|| invalid("missing normalized events"))?
        {
            a["instrument"] = json!(id);
            a["id"] = json!(format!("{id}:{}", a["ex_date"].as_str().unwrap_or("")));
        }
        for i in r["issues"]
            .as_array_mut()
            .ok_or_else(|| invalid("missing issues"))?
        {
            i["instrument"] = json!(id);
            i["id"] = json!(format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(&json!([id, i["id"]])).map_err(invalid)?)
            ));
        }
        for decision in r["decisions"]
            .as_array_mut()
            .ok_or_else(|| invalid("missing decisions"))?
        {
            if decision["event"].is_object() {
                decision["event"]["instrument"] = json!(id);
                decision["event"]["id"] = json!(format!(
                    "{id}:{}",
                    decision["event"]["ex_date"].as_str().unwrap_or("")
                ));
            }
        }
        // Reviewed ordinary facts override provider candidates, never silently overwritten.
        let r_records = r["records"].as_array_mut().unwrap();
        for ((security, date), event) in &reviewed {
            if security == &id
                && date.to_string().as_str() >= spec["start"].as_str().unwrap_or("")
                && date.to_string().as_str() <= spec["end"].as_str().unwrap_or("")
            {
                r_records.retain(|a| a["ex_date"] != date.to_string());
                r_records.push(serde_json::to_value(event).map_err(invalid)?);
            }
        }
        for ((security, date), event) in &reviewed {
            if security == &id
                && date.to_string().as_str() >= spec["start"].as_str().unwrap_or("")
                && date.to_string().as_str() <= spec["end"].as_str().unwrap_or("")
            {
                r["decisions"].as_array_mut().unwrap().push(json!({"rule":"verified_implementation_revision_preserved","event":event,"evidence":review_evidence.get(&(security.clone(),*date)).into_iter().collect::<Vec<_>>() }));
            }
        }
        r["issues"].as_array_mut().unwrap().retain(|i| {
            !i["id"]
                .as_str()
                .is_some_and(|id| closed_issues.contains(id))
        });
        // Typed historical/special facts stay independent; do not overwrite audited revisions.
        let records: Vec<CorporateAction> =
            serde_json::from_value(r["records"].clone()).map_err(invalid)?;
        for a in &records {
            a.validate_adjustment_terms().map_err(invalid)?;
        }
        let range: DateRange = DateRange {
            start: serde_json::from_value(spec["start"].clone()).map_err(invalid)?,
            end: serde_json::from_value(spec["end"].clone()).map_err(invalid)?,
        };
        bundle.actions.retain(|a| {
            a.instrument != id
                || !range.contains(a.ex_date)
                || (!matches!(
                    a.terms,
                    analytics_core::actions::EventTerms::Distribution { allocation: None }
                ) && !reviewed.contains_key(&(a.instrument.clone(), a.ex_date)))
        });
        for a in records {
            if bundle.actions.iter().any(|old| old.id == a.id) {
                return Err(invalid(
                    "normalized event conflicts with reviewed typed event",
                ));
            }
            bundle.actions.push(a);
        }
        bundle.execution_issues.retain(|i| {
            i.instrument != id
                || i.dates.end < range.start
                || i.dates.start > range.end
                || !matches!(
                    i.code.as_str(),
                    "source_event_conflict"
                        | "cninfo_history_gap"
                        | "cninfo_event_date_missing"
                        | "cninfo_event_chronology_conflict"
                        | "cninfo_amount_missing"
                        | "cninfo_event_parse_error"
                        | "rights_issue_candidate_unresolved"
                        | "execution_event_unsupported_skipped"
                )
        });
        bundle.execution_issues.extend(
            serde_json::from_value::<Vec<crate::execution_data::ExecutionIssue>>(
                r["issues"].clone(),
            )
            .map_err(invalid)?,
        );
        resolved.insert(id.clone());
        receipt_results.insert(id, r);
    }
    let input_bytes = fs::read(staging.join("reconciliation-inputs.json"))?;
    let input_hash = format!("sha256:{:x}", Sha256::digest(&input_bytes));
    capture(
        output,
        &format!("reconciliation-inputs-{}.json", &input_hash[7..]),
        &input_bytes,
        &mut bundle.raw_sha256,
    )?;
    let base_hash = format!("sha256:{:x}", Sha256::digest(fs::read(source_path)?));
    for issue in &mut bundle.execution_issues {
        if issue.id.is_empty() {
            issue.id = format!(
                "sha256:{:x}",
                Sha256::digest(serde_json::to_vec(issue).map_err(invalid)?)
            );
        }
        if issue.event_category.is_empty() {
            issue.event_category =
                if issue.code.starts_with("cninfo_") || issue.code == "source_event_conflict" {
                    "distribution"
                } else {
                    "unknown"
                }
                .into();
        }
        if issue.evidence.is_empty() {
            issue.evidence.push(format!("base-actions:{base_hash}"));
        }
    }
    bundle
        .actions
        .sort_by_key(|a| (a.instrument.clone(), a.ex_date));
    bundle
        .execution_issues
        .sort_by_key(|i| (i.instrument.clone(), i.id.clone(), i.code.clone()));
    bundle.schema_version = 6;
    let receipt = json!({"schema_version":1,"rule_version":"jfquant-actions-v1","base_actions_sha256":format!("sha256:{:x}",Sha256::digest(fs::read(source_path)?)),
        "collector_signature":result["collector_signature"],"announcement_evidence":announcements,"inputs_sha256":input_hash,"evidence_sha256":bundle.raw_sha256,"timings_seconds":{"evidence":evidence_seconds,"replay":replay_seconds,"before_publication":started.elapsed().as_secs_f64()},"results":receipt_results,"reconciled_securities":resolved,"unresolved_count":bundle.execution_issues.iter().filter(|i| !i.skipped_by_engine()).count(),"warnings_count":bundle.execution_issues.iter().filter(|i| i.skipped_by_engine()).count()});
    // The supervised replay directory is temporary; immutable proof is the hashed receipt.
    fs::remove_dir_all(&staging)?;
    capture(
        output,
        "reconciliation-v1.json",
        &serde_json::to_vec_pretty(&receipt).map_err(invalid)?,
        &mut bundle.raw_sha256,
    )?;
    bundle.downloaded_at = chrono::Utc::now().to_rfc3339();
    fs::write(
        output.join("actions.json"),
        serde_json::to_vec_pretty(&bundle).map_err(invalid)?,
    )?;
    ActionBundle::read(&output.join("actions.json"))?;
    Ok(bundle)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn immutable_replay_deduplicates_decimal_components_and_detects_tampering() {
        let tmp = tempfile::tempdir().unwrap();
        let source = tmp.path().join("base");
        fs::create_dir_all(source.join("raw")).unwrap();
        let row = json!({"F006D":"2024-06-01","F018D":"2024-06-03","F020D":"2024-06-04","F023D":null,
            "F044V":"年度分红","F012N":"30.7","F010N":"5","F011N":"0","F007V":"10派30.7元送5股"});
        let mut special = row.clone();
        special["F044V"] = json!("特别分红");
        special["F012N"] = json!("12.3");
        special["F010N"] = json!("0");
        let evidence = json!({"instrument":"SH600188","evidence":{"cninfo":{"records":[row.clone(),special,row]},"sina_dividends":[],"sina_rights":[]}});
        let bytes = serde_json::to_vec(&evidence).unwrap();
        fs::write(source.join("raw/primary.json"), &bytes).unwrap();
        let book:ActionBundle=serde_json::from_value(json!({"schema_version":5,"downloader":"JFQuant analytics-workflow synthetic-demo-v1","downloaded_at":"fixture",
            "source_bindings":{},"coverage":{},"queried_symbols":["SH600188"],"actions":[],"failures":{"SH600188":"history coverage failure outside repaired date"},
            "structured_coverage":{"SH600188":{"dates":{"start":"2012-01-01","end":"2026-10-12"},"source":"akshare.cninfo/implemented/per-ten-shares-v1","evidence":["primary.json"]}},
            "raw_sha256":{"primary.json":format!("sha256:{:x}",Sha256::digest(&bytes))}})).unwrap();
        let path = source.join("actions.json");
        fs::write(&path, serde_json::to_vec(&book).unwrap()).unwrap();
        let context = analytics_core::task::TaskContext::current();
        let output = tmp.path().join("reconciled");
        let normalized = apply(&path, &[], &[], &output, &context).unwrap();
        assert_eq!(normalized.schema_version, 6);
        assert_eq!(normalized.failures, book.failures);
        assert_eq!(normalized.actions.len(), 1);
        assert_eq!(normalized.actions[0].cash_per_share, "4.30");
        assert_eq!(normalized.actions[0].share_multiplier().unwrap(), "1.5");
        assert_eq!(fs::read(&path).unwrap(), serde_json::to_vec(&book).unwrap());
        let second = apply(
            &output.join("actions.json"),
            &[],
            &[],
            &tmp.path().join("repeated"),
            &context,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&second.actions).unwrap(),
            serde_json::to_value(&normalized.actions).unwrap()
        );
        let mut altered = normalized.clone();
        altered.actions[0].cash_per_share = "5".into();
        fs::write(
            output.join("actions.json"),
            serde_json::to_vec(&altered).unwrap(),
        )
        .unwrap();
        assert!(ActionBundle::read(&output.join("actions.json")).is_err());
        fs::write(
            output.join("actions.json"),
            serde_json::to_vec(&normalized).unwrap(),
        )
        .unwrap();
        fs::write(output.join("raw/primary.json"), b"tampered").unwrap();
        assert!(ActionBundle::read(&output.join("actions.json")).is_err());
    }
    #[test]
    fn failed_task_native_receipts_require_reviewed_code_hash_range_and_body_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("task");
        let output = tmp.path().join("out");
        fs::create_dir_all(root.join("raw")).unwrap();
        fs::create_dir_all(output.join("raw")).unwrap();
        let owner = json!({"operation":"actions","collector_protocol":9,"collector_signature":REVIEWED_COLLECTOR_V9,"normalization":crate::adjustment::NORMALIZATION,
            "dependencies_sha256":format!("sha256:{:x}",Sha256::digest(include_bytes!("../../collectors/requirements.lock"))),
            "python_runtime_sha256":format!("sha256:{:x}",Sha256::digest(include_bytes!("../../collectors/python-runtime.lock.json"))),
            "request":{"provider":"tdxaidata","adjustment_source":"tdx_forward","start":"2024-01-01","end":"2024-12-31","instruments":["SH600188"],"source_bindings":{},"benchmarks":[],"universes":[]}});
        fs::write(root.join("owner.json"), serde_json::to_vec(&owner).unwrap()).unwrap();
        let bytes = br#"{"Date":[20240604],"Type":["1"],"Value":[[43,0,5,0]]}"#;
        fs::write(root.join("raw/sdk-body.body"), bytes).unwrap();
        let receipt = json!({"function":"GetCWDATAInStr","sdk":"tdxaidata","version":"1.2.2","arguments":["600188.SH","2024-01-01 00:00:00","2024-12-31 00:00:00"],"filename":"sdk-body.body","sha256":format!("sha256:{:x}",Sha256::digest(bytes))});
        let shard=serde_json::to_vec(&json!({"instrument":"SH600188","evidence":{"provider":"tdxaidata","sdk_receipts":[receipt.clone()]}})).unwrap();
        fs::write(root.join("raw/SH600188-actions-structured.json"), &shard).unwrap();
        fs::write(
            root.join("receipts.json"),
            serde_json::to_vec(&json!({"SH600188":format!("sha256:{:x}",Sha256::digest(&shard))}))
                .unwrap(),
        )
        .unwrap();
        fs::write(
            root.join("raw/sdk-requests.jsonl"),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        let result = native_evidence(&root, &output, &mut BTreeMap::new()).unwrap();
        assert_eq!(
            result[&("SH600188".into(), "SH600188".into())][0]["bonus_per_ten"],
            "43"
        );
        let mut alias_owner = owner.clone();
        alias_owner["request"]["instruments"] = json!(["historical_issuer"]);
        alias_owner["request"]["source_bindings"] = json!({"historical_issuer":"SH600188"});
        fs::write(
            root.join("owner.json"),
            serde_json::to_vec(&alias_owner).unwrap(),
        )
        .unwrap();
        let alias_shard=serde_json::to_vec(&json!({"instrument":"historical_issuer","evidence":{"provider":"tdxaidata","sdk_receipts":[receipt.clone()]}})).unwrap();
        fs::write(
            root.join("raw/historical_issuer-actions-structured.json"),
            &alias_shard,
        )
        .unwrap();
        fs::write(
            root.join("receipts.json"),
            serde_json::to_vec(
                &json!({"historical_issuer":format!("sha256:{:x}",Sha256::digest(&alias_shard))}),
            )
            .unwrap(),
        )
        .unwrap();
        let alias = native_evidence(&root, &output, &mut BTreeMap::new()).unwrap();
        assert!(alias.contains_key(&("historical_issuer".into(), "SH600188".into())));
        assert!(!alias.contains_key(&("SH600188".into(), "SH600188".into())));
        fs::write(root.join("owner.json"), serde_json::to_vec(&owner).unwrap()).unwrap();
        fs::write(
            root.join("receipts.json"),
            serde_json::to_vec(&json!({"SH600188":format!("sha256:{:x}",Sha256::digest(&shard))}))
                .unwrap(),
        )
        .unwrap();
        fs::write(root.join("raw/sdk-body.body"), b"modified").unwrap();
        assert!(native_evidence(&root, &output, &mut BTreeMap::new()).is_err());
        let mut owner = owner;
        owner["collector_signature"] = json!("unknown");
        fs::write(root.join("owner.json"), serde_json::to_vec(&owner).unwrap()).unwrap();
        assert!(native_evidence(&root, &output, &mut BTreeMap::new()).is_err());
    }
}
