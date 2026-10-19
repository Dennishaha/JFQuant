//! Collector publication boundary.
use super::*;
pub(super) fn finish_collection(
    request: &CollectionRequest,
    staging: &Path,
    operation: &str,
    receipts: BTreeMap<String, String>,
    complete: Value,
) -> io::Result<PathBuf> {
    verify_collector_identity(&complete)?;
    if request.provider == Provider::Tdxaidata && complete["versions"]["tdxaidata"] != "1.2.2" {
        return Err(invalid("tdx_sdk_version_missing"));
    }
    let completion_receipts = verify_http_receipts(staging, &complete["evidence"])?;
    if receipts.keys().cloned().collect::<BTreeSet<_>>() != request.instruments {
        let mut failures = BTreeMap::<String, Value>::new();
        for id in request
            .instruments
            .iter()
            .filter(|id| !receipts.contains_key(*id))
        {
            let path = staging.join("raw").join(format!("{id}-failed.json"));
            if path.is_file() {
                failures.insert(
                    id.clone(),
                    serde_json::from_slice(&fs::read(path)?).map_err(invalid)?,
                );
            }
        }
        save(
            &staging.join("acquisition_readiness.json"),
            &json!({"ready":false,"requested":request.instruments.len(),"verified_complete":receipts.len(),"failures":failures}),
        )?;
        return Err(invalid("incomplete collection"));
    }
    if operation == "fundamentals" {
        let mut instruments = BTreeMap::new();
        let mut evidence = completion_receipts;
        for (id, sha) in &receipts {
            let name = format!("{id}-actions-structured.json");
            let msg: Value = serde_json::from_slice(&fs::read(staging.join("raw").join(&name))?)
                .map_err(invalid)?;
            evidence.extend(verify_selected_receipts(
                request,
                staging,
                &msg["evidence"],
            )?);
            evidence.insert(name, sha.clone());
            instruments.insert(
                id.clone(),
                crate::three_factor::acquire::normalize(request, staging, id, &msg)?,
            );
        }
        evidence.insert(
            "complete-sources-structured.json".into(),
            hash(&staging.join("raw/complete-sources-structured.json"))?,
        );
        let path = staging.join("fundamentals.json");
        save(
            &path,
            &json!({"schema_version":1,"normalization":crate::three_factor::acquire::VERSION,
            "request":request,"instruments":instruments,"evidence_sha256":evidence,"collector":complete,
            "revision_vintage_verified":false}),
        )?;
        return Ok(path);
    }
    if matches!(operation, "calendar" | "members") {
        let id = request
            .instruments
            .iter()
            .next()
            .ok_or_else(|| invalid("reference_security_missing"))?;
        let msg: Value = serde_json::from_slice(&fs::read(
            staging
                .join("raw")
                .join(format!("{id}-actions-structured.json")),
        )?)
        .map_err(invalid)?;
        let reference = &msg["reference"];
        let calendar: Vec<Date> =
            serde_json::from_value(reference["calendar"].clone()).map_err(invalid)?;
        if operation == "calendar"
            && (calendar.is_empty()
                || calendar.windows(2).any(|p| p[0] >= p[1])
                || calendar
                    .iter()
                    .any(|d| *d < request.start || *d > request.end))
        {
            return Err(invalid("invalid_reference_calendar"));
        }
        let pools: BTreeMap<String, analytics_core::pool::PoolMembership> =
            serde_json::from_value(reference["universes"].clone()).map_err(invalid)?;
        for pool in pools.values() {
            pool.validate().map_err(invalid)?;
        }
        if operation == "members"
            && pools.keys().cloned().collect::<BTreeSet<_>>() != request.universes
        {
            return Err(invalid("incomplete_reference_members"));
        }
        let path = staging.join("reference.json");
        save(
            &path,
            &json!({"schema_version":1,"request":request,"data":reference,"collector":complete,"evidence":msg["evidence"]}),
        )?;
        return Ok(path);
    }
    if operation == "coefficients" {
        let mut records = BTreeMap::new();
        let mut evidence = completion_receipts;
        for id in receipts.keys() {
            let record = AdjustmentRecord::read(&staging.join("market"), id)?;
            let msg: Value = serde_json::from_slice(&fs::read(
                staging
                    .join("raw")
                    .join(format!("{id}-actions-structured.json")),
            )?)
            .map_err(invalid)?;
            if serde_json::to_value(&record).map_err(invalid)? != msg["adjustment"] {
                return Err(invalid("coefficient checkpoint mismatch"));
            }
            evidence.extend(verify_selected_receipts(
                request,
                staging,
                &msg["evidence"],
            )?);
            evidence.insert(
                format!("{id}-actions-structured.json"),
                receipts[id].clone(),
            );
            records.insert(
                id.clone(),
                hash(
                    &staging
                        .join("market/adjustments")
                        .join(format!("{id}.json")),
                )?,
            );
        }
        evidence.insert(
            "complete-sources-structured.json".into(),
            hash(&staging.join("raw/complete-sources-structured.json"))?,
        );
        let bundle = CoefficientBundle {
            schema_version: 1,
            request: request.clone(),
            records,
            evidence,
            collector: complete,
        };
        let path = staging.join("coefficients.json");
        save(&path, &bundle)?;
        CoefficientBundle::read(&path)?;
        return Ok(path);
    }
    if operation == "reconcile_actions" {
        let mut results = BTreeMap::new();
        for id in receipts.keys() {
            let shard: Value = serde_json::from_slice(&fs::read(
                staging
                    .join("raw")
                    .join(format!("{id}-actions-structured.json")),
            )?)
            .map_err(invalid)?;
            results.insert(id, shard["evidence"]["reconciliation"].clone());
        }
        let path = staging.join("reconciliation.json");
        save(
            &path,
            &json!({"schema_version":1,"collector_signature":collector_signature(),"results":results}),
        )?;
        return Ok(path);
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
            field_facts: Default::default(),
            structured_coverage: Default::default(),
            execution_issues: Vec::new(),
            execution_data: None,
            schema_version: 6,
            source_bindings: request
                .source_bindings
                .iter()
                .map(|(id, symbol)| {
                    (
                        id.clone(),
                        analytics_core::source_map::SourceLocator {
                            provider: match request.provider {
                                Provider::Hithink => "hithink",
                                Provider::Cninfo => "cninfo",
                                Provider::Tdxaidata => "tdxaidata",
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
            book.raw_sha256.extend(verify_selected_receipts(
                request,
                staging,
                &msg["evidence"],
            )?);
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
        crate::corporate_actions::facts::refresh(&mut book, staging)?;
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
        price_basis: if request.provider == Provider::Tdxaidata {
            "unadjusted"
        } else {
            "unavailable"
        }
        .into(),
        normalization: NORMALIZATION.into(),
        adjustment_anchors,
    };
    save(&staging.join("market/daily_manifest.json"), &manifest)?;
    let source_map = analytics_core::source_map::SourceMap {
        schema_version: 2,
        exceptions: request
            .source_bindings
            .iter()
            .filter(|(id, source)| id != source || request.index_inputs.contains_key(*id))
            .map(|(id, source)| {
                let coverage = analytics_core::DateRange {
                    start: request.start,
                    end: request.end,
                };
                (
                    id.clone(),
                    analytics_core::source_map::SourceException {
                        exchange_symbol: if request.benchmarks.contains(id) {
                            String::new()
                        } else {
                            source.clone()
                        },
                        kind: if request.benchmarks.contains(id) {
                            "price_index"
                        } else {
                            "a_share"
                        }
                        .into(),
                        coverage: coverage.clone(),
                        locators: vec![analytics_core::source_map::SourceLocator {
                            provider: match request.provider {
                                Provider::Hithink => "hithink",
                                Provider::Cninfo => "cninfo",
                                Provider::Tdxaidata => "tdxaidata",
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
        &json!({"downloader":"JFQuant analytics-storage","independent_download":true,"snapshot_verified":true,"exception_bindings":!request.source_bindings.is_empty(),"provider":request.provider,"request":request,"evidence_sha256":evidence_hashes,"collector_protocol":COLLECTOR_PROTOCOL,"versions":complete["versions"],"collector_signature":complete["collector_signature"],"python_runtime":serde_json::from_slice::<Value>(include_bytes!("../../collectors/python-runtime.lock.json")).map_err(invalid)?,"calendar_source":if request.provider==Provider::Tdxaidata {"tdxaidata.get_trading_dates"} else {"unsupported"},"dependencies_sha256":format!("sha256:{:x}",analytics_core::hash::Hex(Sha256::digest(include_bytes!("../../collectors/requirements.lock")))),"price_basis":manifest.price_basis,"normalization":manifest.normalization,"adjustment_source":request.adjustment_source,"adjustment_anchors":manifest.adjustment_anchors}),
    )?;
    Ok(staging.join("market"))
}
