//! Preserve independently authenticated suspension evidence during offline reconciliation.
use super::*;

pub(super) fn attach(root: &Path, output: &Path, bundle: &mut ActionBundle) -> io::Result<()> {
    // The caller has validated this collector's ownership and native journal.
    let owner: Value =
        serde_json::from_slice(&fs::read(root.join("owner.json"))?).map_err(invalid)?;
    let request: CollectionRequest =
        serde_json::from_value(owner["request"].clone()).map_err(invalid)?;
    let receipts: BTreeMap<String, String> =
        serde_json::from_slice(&fs::read(root.join("receipts.json"))?).map_err(invalid)?;
    let range = DateRange {
        start: request.start,
        end: request.end,
    };
    for (id, hash) in receipts {
        task_context_check()?;
        if !request.instruments.contains(&id)
            || !(bundle.queried_symbols.contains(&id) || bundle.failures.contains_key(&id))
        {
            return Err(invalid("status_attachment_security_not_in_source"));
        }
        let bytes = verify_bytes(
            &root
                .join("raw")
                .join(format!("{id}-actions-structured.json")),
            &hash,
        )?;
        let shard: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
        if shard["instrument"] != id {
            return Err(invalid("status_attachment_security_mismatch"));
        }
        if shard["evidence"]["stock_status"]["status"] != "captured" {
            continue;
        }
        let source = request.source_bindings.get(&id).unwrap_or(&id);
        if bundle.source_symbol(&id)? != source {
            return Err(invalid("status_attachment_source_identity_mismatch"));
        }
        let symbol = format!("{}.{}", &source[2..], &source[..2]);
        let name = format!("status-shard-{}.json", &hash[7..]);
        let capability = json!({"status":"available","source_symbol":symbol,"range":range,"structured":name,"use":"retrospective-account-validation-only;not-a-signal","right_censored":true});
        let key = format!("{}:{id}", crate::tdx_status::RULE);
        if let Some(existing) = bundle
            .execution_data
            .as_ref()
            .and_then(|d| d.capabilities.get(&key))
            && existing != &capability
        {
            return Err(invalid("status_attachment_conflicts_with_frozen_evidence"));
        }
        // Keep the original shard and every native response it references, so
        // ActionBundle::read can replay the exact same status interpretation.
        for receipt in shard["evidence"]["sdk_receipts"]
            .as_array()
            .ok_or_else(|| invalid("status_receipts_missing"))?
        {
            let file = receipt["filename"]
                .as_str()
                .ok_or_else(|| invalid("status_filename_missing"))?;
            safe_name(file)?;
            let raw = verify_bytes(
                &root.join("raw").join(file),
                receipt["sha256"].as_str().unwrap_or(""),
            )?;
            capture(output, file, &raw, &mut bundle.raw_sha256)?;
        }
        capture(output, &name, &bytes, &mut bundle.raw_sha256)?;
        let days = match crate::tdx_status::from_evidence(root, &shard["evidence"], &symbol, &range)
        {
            Ok(days) => days,
            Err(error) => {
                let data = bundle.execution_data.get_or_insert_with(Default::default);
                data.schema_version = 2;
                data.capabilities.insert(format!("tdxaidata.gp29-unavailable-v1:{id}"), json!({"status":"unavailable","source_symbol":symbol,"range":range,"structured":name,"reason":error.to_string()}));
                continue;
            }
        };
        let data = bundle.execution_data.get_or_insert_with(Default::default);
        data.schema_version = 2;
        data.suspensions.insert(id, days);
        data.capabilities.insert(key, capability);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_survives_failed_history_and_is_replayable_without_altering_events() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("capture");
        let output = tmp.path().join("output");
        fs::create_dir_all(root.join("raw")).unwrap();
        fs::create_dir_all(output.join("raw")).unwrap();
        let request = json!({"provider":"tdxaidata","adjustment_source":"none","start":"2021-01-01","end":"2021-03-01","instruments":["SH600340"],"source_bindings":{},"benchmarks":[],"universes":[]});
        fs::write(
            root.join("owner.json"),
            serde_json::to_vec(&json!({"request":request})).unwrap(),
        )
        .unwrap();
        let mut refs = Vec::new();
        for (function, arguments, body) in [
            ("GetTradeCalendarInStr", json!(["2010-12-25 00:00:00","2021-03-01 00:00:00","SH",-1]), json!({"Date":["20210128","20210129","20210201","20210202"]})),
            ("GetProDataInStr", json!([json!({"type":3,"code":"600340.SH","table_list":["GP29"],"start_time":"2021-01-01 00:00:00","end_time":"2021-03-01 00:00:00"}).to_string()]), json!({"ErrorId":"0","Value":{"GP29":[{"Date":"20210202","Value":["2.00","0.00"]}]}})),
        ] {
            let bytes = serde_json::to_vec(&body).unwrap();
            let name = format!("sdk-{function}.body");
            fs::write(root.join("raw").join(&name), &bytes).unwrap();
            refs.push(json!({"kind":"sdk_native_response","sdk":"tdxaidata","version":"1.2.2","function":function,"arguments":arguments,"filename":name,"sha256":format!("sha256:{:x}",analytics_core::hash::Hex(Sha256::digest(&bytes)))}));
        }
        let shard = json!({"instrument":"SH600340","history_verified":false,"evidence":{"stock_status":{"status":"captured","rule":"tdxaidata.gp29-resumption-v1","calendar_start":"2010-12-25"},"sdk_receipts":refs}});
        let bytes = serde_json::to_vec(&shard).unwrap();
        let hash = format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(&bytes))
        );
        fs::write(root.join("raw/SH600340-actions-structured.json"), bytes).unwrap();
        fs::write(
            root.join("receipts.json"),
            serde_json::to_vec(&json!({"SH600340":hash})).unwrap(),
        )
        .unwrap();
        let mut book: ActionBundle = serde_json::from_value(json!({"schema_version":9,"downloader":"JFQuant analytics-workflow synthetic-demo-v1","downloaded_at":"fixture","source_bindings":{},"coverage":{},"queried_symbols":[],"actions":[],"failures":{"SH600340":"history unknown"},"raw_sha256":{}})).unwrap();
        let failures = book.failures.clone();
        attach(&root, &output, &mut book).unwrap();
        assert_eq!(book.failures, failures);
        assert!(book.actions.is_empty());
        assert_eq!(
            book.execution_data.as_ref().unwrap().suspensions["SH600340"].len(),
            2
        );
        fs::write(
            output.join("actions.json"),
            serde_json::to_vec(&book).unwrap(),
        )
        .unwrap();
        ActionBundle::read(&output.join("actions.json")).unwrap();
        fs::write(output.join("raw/sdk-GetProDataInStr.body"), b"tampered").unwrap();
        assert!(ActionBundle::read(&output.join("actions.json")).is_err());
        book.failures.clear();
        assert!(attach(&root, &output, &mut book).is_err());
    }
}
