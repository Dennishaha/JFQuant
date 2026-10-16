//! Collector process boundary.
use super::*;
pub(super) fn collect_with_command(
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
    let mut owner_value = json!({"operation":operation,"request":request,"collector_protocol":13,"normalization":NORMALIZATION,
        "collector_signature":collector_signature(),"dependencies_sha256":dependencies_signature(),
        "python_runtime_sha256":format!("sha256:{:x}",analytics_core::hash::Hex(Sha256::digest(include_bytes!("../../collectors/python-runtime.lock.json"))))});
    if operation == "reconcile_actions" {
        owner_value["reconciliation_inputs_sha256"] =
            json!(hash(&staging.join("reconciliation-inputs.json"))?);
    }
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
        let path = if matches!(operation, "market" | "quotes") {
            staging.join("market/bars").join(format!("{id}.parquet"))
        } else {
            staging
                .join("raw")
                .join(format!("{id}-actions-structured.json"))
        };
        if hash(&path)? != *expected {
            return Err(invalid("checkpoint hash mismatch"));
        }
        if matches!(operation, "market" | "quotes") {
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
            verify_selected_receipts(request, staging, &evidence)?;
        } else {
            let msg: Value = serde_json::from_slice(&fs::read(&path)?).map_err(invalid)?;
            verify_selected_receipts(request, staging, &msg["evidence"])?;
        }
    }
    let pending: BTreeSet<_> = request
        .instruments
        .iter()
        .filter(|id| !receipts.contains_key(*id))
        .cloned()
        .collect();
    // A completed, authenticated checkpoint must not launch Python with an empty
    // pending list: that produces an empty completion calendar. Rebuild derived
    // metadata from its original verified completion and keep the evidence intact.
    let completion_path = staging.join("raw/complete-sources-structured.json");
    if pending.is_empty() && completion_path.is_file() {
        let complete: Value =
            serde_json::from_slice(&fs::read(completion_path)?).map_err(invalid)?;
        return finish_collection(request, staging, operation, receipts, complete);
    }
    let cache = staging.join("cache");
    fs::create_dir_all(&cache)?;
    let mut log = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(staging.join("collector.stderr.log"))?;
    let secrets: Vec<String> = command
        .get_envs()
        .filter(|(k, _)| matches!(k.to_str(), Some("JFQUANT_TDX_KEY")))
        .filter_map(|(_, v)| v.and_then(|v| v.to_str()).map(str::to_owned))
        .filter(|s| !s.is_empty())
        .collect();
    let diagnostic_secrets = secrets.clone();
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
                Ok(Some(mut line)) => {
                    for secret in &secrets {
                        line = line.replace(secret, "[redacted]");
                    }
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
            let mut line = String::from_utf8_lossy(&bytes).into_owned();
            for secret in &diagnostic_secrets {
                line = line.replace(secret, "[redacted]");
            }
            log.write_all(line.as_bytes())?;
            let message: String = line.chars().take(500).collect();
            logger_context.report(format!("采集诊断：{message}"))?;
        }
        log.sync_all()
    });
    let mut payload = serde_json::to_value(request).map_err(invalid)?;
    payload["version"] = json!(13);
    payload["expected_collector_signature"] = json!(collector_signature());
    payload["operation"] = json!(operation);
    if operation == "reconcile_actions" {
        let input: Value =
            serde_json::from_slice(&fs::read(staging.join("reconciliation-inputs.json"))?)
                .map_err(invalid)?;
        let mut inputs = serde_json::Map::new();
        for id in &pending {
            let symbol = request.source_bindings.get(id).unwrap_or(id);
            if inputs
                .insert(symbol.clone(), input["securities"][id].clone())
                .is_some()
            {
                return Err(invalid("ambiguous reconciliation source binding"));
            }
        }
        payload["reconciliation_inputs"] = inputs.into();
        payload["reconciliation_calendar"] = input["calendar"].clone();
    }
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
                        if let Some(aux) = msg["execution_data"].as_object_mut() {
                            for field in ["suspensions", "limits", "coefficients"] {
                                if let Some(values) =
                                    aux.get_mut(field).and_then(Value::as_object_mut)
                                    && let Some(v) = values.remove(&source)
                                {
                                    values.insert(id.clone(), v);
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
                    if msg["version"] != 13 {
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
                        Some("adjustment_shard") if operation == "coefficients" => {
                            if !identified {
                                return Err(invalid(
                                    "collector identity required before checkpoints",
                                ));
                            }
                            let id = msg["instrument"]
                                .as_str()
                                .ok_or_else(|| invalid("missing coefficient security"))?;
                            if !pending.contains(id) || receipts.contains_key(id) {
                                return Err(invalid("unexpected coefficient security"));
                            }
                            verify_selected_receipts(request, staging, &msg["evidence"])?;
                            let record: AdjustmentRecord =
                                serde_json::from_value(msg["adjustment"].clone())
                                    .map_err(invalid)?;
                            record.validate()?;
                            if record.instrument_id != id
                                || record.source_symbol
                                    != *request.source_bindings.get(id).unwrap_or(&id.to_string())
                                || (request.provider != Provider::Tdxaidata
                                    && record.on(request.start)?.is_none())
                                || record.on(request.end)?.is_none()
                            {
                                return Err(invalid("coefficient security/coverage mismatch"));
                            }
                            if !matches!(
                                (&record.basis, request.benchmarks.contains(id)),
                                (
                                    AdjustmentBasis::Explicit {
                                        source: FactorSource::TdxForward,
                                        ..
                                    },
                                    false
                                ) | (AdjustmentBasis::PriceIndex { .. }, true)
                            ) {
                                return Err(invalid("coefficient source mismatch"));
                            }
                            if let AdjustmentBasis::Explicit { source, .. } = &record.basis
                                && (*source == FactorSource::TdxForward)
                                    != (request.provider == Provider::Tdxaidata)
                            {
                                return Err(invalid("coefficient source mismatch"));
                            }
                            save(
                                &staging
                                    .join("market/adjustments")
                                    .join(format!("{id}.json")),
                                &record,
                            )?;
                            let path = staging
                                .join("raw")
                                .join(format!("{id}-actions-structured.json"));
                            save(&path, &msg)?;
                            receipts.insert(id.into(), hash(&path)?);
                            save(&receipt_path, &receipts)?;
                        }
                        Some("action_shard")
                            if matches!(
                                operation,
                                "actions"
                                    | "reconcile_actions"
                                    | "announcements"
                                    | "calendar"
                                    | "members"
                            ) =>
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
                            verify_selected_receipts(request, staging, &msg["evidence"])?;
                            let path = staging
                                .join("raw")
                                .join(format!("{id}-actions-structured.json"));
                            save(&path, &msg)?;
                            receipts.insert(id.into(), hash(&path)?);
                            save(&receipt_path, &receipts)?;
                        }
                        Some("shard") if matches!(operation, "market" | "quotes") => {
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
                            verify_selected_receipts(request, staging, &msg["evidence"])?;
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
                                        source: FactorSource::TdxForward,
                                        ..
                                    },
                                    Provider::Tdxaidata,
                                    false,
                                ) => true,
                                (
                                    AdjustmentBasis::PriceIndex { source },
                                    Provider::Tdxaidata,
                                    true,
                                ) => source == "tdxaidata.get_market_data",
                                (AdjustmentBasis::Unavailable { .. }, _, _)
                                    if operation == "quotes" =>
                                {
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
    if request.provider == Provider::Tdxaidata && complete["versions"]["tdxaidata"] != "1.2.2" {
        return Err(invalid("tdx_sdk_version_missing"));
    }
    verify_http_receipts(staging, &complete["evidence"])?;
    complete["dependencies_sha256"] = json!(dependencies_signature());
    complete["python_runtime"] =
        serde_json::from_slice(include_bytes!("../../collectors/python-runtime.lock.json"))
            .map_err(invalid)?;
    save(
        &staging.join("raw/complete-sources-structured.json"),
        &complete,
    )?;

    finish_collection(request, staging, operation, receipts, complete)
}
