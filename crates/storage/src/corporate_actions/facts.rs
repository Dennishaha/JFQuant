//! Replayable field facts from fixed native TDX requests, never executable rights terms.
use super::*;
use serde_json::json;

/// Frozen interpretation, including bounded unique event association.
pub const RULE: &str = "tdxaidata.rights-fields-v1";
/// One value with its original bytes, field path, units and time semantics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FieldFact {
    /// Value after the declared unit conversion.
    pub value: Value,
    /// Original supplier value.
    pub native_value: Value,
    /// Original unit; no total is interpreted as a per-share ratio.
    pub unit: String,
    /// Captured response filename.
    pub filename: String,
    /// Captured response hash.
    pub sha256: String,
    /// Exact native function and arguments.
    pub request: Value,
    /// JSON pointer into the captured original reply.
    pub field_path: String,
    /// Event date associated with this value, distinct from acquisition time.
    pub effective_date: Date,
    /// Earliest disclosure, unknown for these endpoints.
    pub announced: Option<Date>,
}
/// A rights event with partial facts and explicit unsupported execution status.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RightsFacts {
    /// Canonical event identity, anchored to the ex date.
    pub event_id: String,
    /// verified or conflict; a verified partial record is not execution-ready.
    pub status: String,
    /// Field name to evidence-backed value.
    pub fields: BTreeMap<String, FieldFact>,
    /// Settlement facts absent from these structured endpoints.
    pub missing_fields: Vec<String>,
    /// Always false in this acquisition-only implementation.
    pub engine_supported: bool,
}
/// Query coverage and derived facts for one authenticated source shard.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct FactReport {
    /// Interpretation rule version.
    pub rule: String,
    /// Canonical security.
    pub instrument: String,
    /// Supplier security (possibly a dated historical mapping).
    pub source_symbol: String,
    /// Exact queried date range.
    pub range: analytics_core::DateRange,
    /// UTC time captured by the acquisition worker.
    pub acquired_at: String,
    /// Hashed structured shard containing the native receipts.
    pub evidence_name: String,
    /// GP field outcomes: verified, valid_empty, unavailable, invalid, or coverage_unknown.
    pub queries: BTreeMap<String, String>,
    /// Partial rights events; no complete-history or account-readiness assertion.
    pub events: Vec<RightsFacts>,
}
fn fail(s: &str) -> io::Error {
    invalid(s)
}
fn date(v: &Value) -> io::Result<Date> {
    Date::parse_from_str(
        v.as_str().ok_or_else(|| fail("native date must be text"))?,
        "%Y%m%d",
    )
    .map_err(invalid)
}
fn decimal(v: &Value, shift: i32) -> io::Result<String> {
    let s = v
        .as_str()
        .ok_or_else(|| fail("native decimal must be text"))?;
    let (whole, fraction) = s.split_once('.').unwrap_or((s, ""));
    if whole.is_empty()
        || !whole
            .bytes()
            .chain(fraction.bytes())
            .all(|c| c.is_ascii_digit())
        || s.len() > 32
    {
        return Err(fail("invalid rights decimal"));
    }
    let digits = format!("{whole}{fraction}");
    let scale = fraction.len() as i32 - shift;
    let text = if scale <= 0 {
        format!("{digits}{}", "0".repeat((-scale) as usize))
    } else {
        let padded = format!(
            "{}{}",
            "0".repeat((scale as usize + 1).saturating_sub(digits.len())),
            digits
        );
        let split = padded.len() - scale as usize;
        format!("{}.{}", &padded[..split], &padded[split..])
    };
    let (w, f) = text.split_once('.').unwrap_or((&text, ""));
    let w = w.trim_start_matches('0');
    let w = if w.is_empty() { "0" } else { w };
    let f = f.trim_end_matches('0');
    Ok(if f.is_empty() {
        w.into()
    } else {
        format!("{w}.{f}")
    })
}
fn response(root: &Path, r: &Value) -> io::Result<Value> {
    let name = r["filename"]
        .as_str()
        .ok_or_else(|| fail("missing native filename"))?;
    if Path::new(name).file_name().and_then(|p| p.to_str()) != Some(name) {
        return Err(fail("unsafe native filename"));
    }
    let bytes = fs::read(root.join("raw").join(name))?;
    if r["sha256"]
        != format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(&bytes))
        )
    {
        return Err(fail("rights native hash mismatch"));
    }
    let (text, _, bad) = encoding_rs::GBK.decode(&bytes);
    if bad {
        return Err(fail("rights native encoding invalid"));
    }
    let v: Value = serde_json::from_str(&text).map_err(invalid)?;
    Ok(v)
}
fn business_ok(v: &Value) -> bool {
    v["ErrorId"].is_null() || v["ErrorId"] == "0" || v["ErrorId"] == 0
}
fn proof(
    r: &Value,
    path: String,
    original: Value,
    value: Value,
    unit: &str,
    day: Date,
) -> FieldFact {
    FieldFact {
        value,
        native_value: original,
        unit: unit.into(),
        filename: r["filename"].as_str().unwrap_or("").into(),
        sha256: r["sha256"].as_str().unwrap_or("").into(),
        request: json!({"function":r["function"],"arguments":r["arguments"]}),
        field_path: path,
        effective_date: day,
        announced: None,
    }
}
fn gp_rows(
    v: &Value,
    field: &str,
    range: &analytics_core::DateRange,
) -> io::Result<Vec<(Date, usize)>> {
    if !business_ok(v) {
        return Err(fail("rights business error"));
    }
    let rows = v["Value"][field]
        .as_array()
        .ok_or_else(|| fail("rights field absent; coverage unknown"))?;
    let mut seen = BTreeSet::new();
    let mut out = vec![];
    for (i, row) in rows.iter().enumerate() {
        let day = date(&row["Date"])?;
        if !range.contains(day)
            || !seen.insert(day)
            || row["Value"].as_array().is_none_or(|a| a.len() != 2)
        {
            return Err(fail("rights date/shape invalid"));
        }
        for n in row["Value"].as_array().unwrap() {
            decimal(n, 0)?;
        }
        out.push((day, i));
    }
    Ok(out)
}
/// Recompute all facts from captured original requests and bodies, including auxiliary shards.
pub fn derive(bundle: &ActionBundle, root: &Path) -> io::Result<BTreeMap<String, FactReport>> {
    let mut reports = BTreeMap::new();
    for (name, hash) in &bundle.raw_sha256 {
        task_context_check()?;
        if !name.ends_with("-actions-structured.json") {
            continue;
        }
        if Path::new(name).file_name().and_then(|p| p.to_str()) != Some(name.as_str()) {
            return Err(fail("unsafe rights shard filename"));
        }
        let bytes = fs::read(root.join("raw").join(name))?;
        if format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(&bytes))
        ) != *hash
        {
            return Err(fail("rights shard hash mismatch"));
        }
        let shard: Value = serde_json::from_slice(&bytes).map_err(invalid)?;
        let e = &shard["evidence"];
        if e["provider"] != "tdxaidata" || !e["rights_fields"].is_object() {
            continue;
        }
        let instrument = shard["instrument"]
            .as_str()
            .ok_or_else(|| fail("rights instrument missing"))?;
        analytics_core::pool::validate_instrument(instrument).map_err(invalid)?;
        let symbol = crate::hithink::thscode(bundle.source_symbol(instrument)?)?;
        let range: analytics_core::DateRange =
            serde_json::from_value(e["request_range"].clone()).map_err(invalid)?;
        range.validate().map_err(invalid)?;
        let acquired = e["acquired_at"]
            .as_str()
            .ok_or_else(|| fail("rights acquisition time missing"))?;
        chrono::DateTime::parse_from_rfc3339(acquired).map_err(invalid)?;
        crate::collector::verify_http_receipts(root, e)?;
        let receipts = e["sdk_receipts"]
            .as_array()
            .ok_or_else(|| fail("rights native receipts missing"))?;
        let mut report = FactReport {
            rule: RULE.into(),
            instrument: instrument.into(),
            source_symbol: symbol.clone(),
            range: range.clone(),
            acquired_at: acquired.into(),
            evidence_name: name.clone(),
            queries: BTreeMap::new(),
            events: vec![],
        };
        let mut tables = BTreeMap::new();
        for field in ["GP41", "GP43"] {
            let matching=receipts.iter().filter(|r|r["function"]=="GetProDataInStr" && r["arguments"].as_array().is_some_and(|a|a.len()==1) && r["arguments"][0].as_str().and_then(|s|serde_json::from_str::<Value>(s).ok()).is_some_and(|q|q==json!({"type":3,"code":symbol,"table_list":[field],"start_time":format!("{} 00:00:00",range.start),"end_time":format!("{} 00:00:00",range.end)}))).collect::<Vec<_>>();
            let state = if matching.len() > 1 {
                "invalid"
            } else if let Some(r) = matching.first() {
                let v = response(root, r)?;
                if !business_ok(&v) {
                    "unavailable"
                } else if v["Value"][field].is_null() {
                    "coverage_unknown"
                } else {
                    match gp_rows(&v, field, &range) {
                        Ok(rows) => {
                            let state = if rows.is_empty() {
                                "valid_empty"
                            } else {
                                "verified"
                            };
                            tables.insert(field, ((*r).clone(), v, rows));
                            state
                        }
                        Err(_) => "invalid",
                    }
                }
            } else if e["rights_fields"][field] == "captured" {
                "invalid"
            } else {
                "unavailable"
            };
            report.queries.insert(field.into(), state.into());
        }
        let factors = receipts
            .iter()
            .filter(|r| {
                r["function"] == "GetCWDATAInStr"
                    && r["arguments"]
                        == json!([
                            symbol,
                            format!("{} 00:00:00", range.start),
                            format!("{} 00:00:00", range.end)
                        ])
            })
            .collect::<Vec<_>>();
        if factors.len() != 1 {
            report
                .queries
                .insert("rights_reference".into(), "invalid".into());
        } else {
            let r = factors[0];
            let v = response(root, r)?;
            let dates = v["Date"].as_array();
            let values = v["Value"].as_array();
            let types = v["Type"].as_array();
            if !business_ok(&v)
                || dates.is_none()
                || values.is_none()
                || types.is_none()
                || dates.map_or(0, |v| v.len()) != values.map_or(0, |v| v.len())
                || dates.map_or(0, |v| v.len()) != types.map_or(0, |v| v.len())
            {
                report
                    .queries
                    .insert("rights_reference".into(), "coverage_unknown".into());
            } else {
                let mut seen = BTreeSet::new();
                for (i, d) in dates.into_iter().flatten().enumerate() {
                    let day = date(d)?;
                    if !range.contains(day) {
                        continue;
                    }
                    let row = &v["Value"][i];
                    if v["Type"][i] != "1" && v["Type"][i] != 1 {
                        continue;
                    }
                    if !seen.insert(day) {
                        return Err(fail("duplicate rights reference date"));
                    }
                    if row.as_array().is_none_or(|a| a.len() != 4) {
                        return Err(fail("rights reference truncated"));
                    }
                    let ratio = decimal(&row[3], -1)?;
                    let price = decimal(&row[1], 0)?;
                    if ratio == "0" || price == "0" {
                        continue;
                    }
                    let mut fields = BTreeMap::from([
                        (
                            "ex_date".into(),
                            proof(
                                r,
                                format!("/Date/{i}"),
                                d.clone(),
                                json!(day),
                                "YYYYMMDD",
                                day,
                            ),
                        ),
                        (
                            "rights_ratio".into(),
                            proof(
                                r,
                                format!("/Value/{i}/3"),
                                row[3].clone(),
                                json!(ratio),
                                "new_shares_per_ten_old_shares",
                                day,
                            ),
                        ),
                        (
                            "subscription_price".into(),
                            proof(
                                r,
                                format!("/Value/{i}/1"),
                                row[1].clone(),
                                json!(price),
                                "CNY_per_new_share",
                                day,
                            ),
                        ),
                    ]);
                    let mut status = "verified";
                    if let (Some((gr, gv, gs)), Some((pr, pv, ps))) =
                        (tables.get("GP41"), tables.get("GP43"))
                    {
                        let matches = gs
                            .iter()
                            .filter_map(|(record, gi)| {
                                let distance = day.signed_duration_since(*record).num_days();
                                if !(0..=31).contains(&distance) {
                                    return None;
                                }
                                let (_, pi) = ps.iter().find(|(d, _)| d == record)?;
                                (decimal(&pv["Value"]["GP43"][pi]["Value"][0], 0).ok()
                                    == Some(price.clone()))
                                .then_some((*record, *gi, *pi))
                            })
                            .collect::<Vec<_>>();
                        if matches.len() == 1 {
                            let (record, gi, pi) = matches[0];
                            let original = &gv["Value"]["GP41"][gi]["Value"][0];
                            if decimal(original, 0)?
                                != record.format("%y%m%d").to_string().trim_start_matches('0')
                            {
                                status = "conflict";
                            } else {
                                fields.insert(
                                    "record_date".into(),
                                    proof(
                                        gr,
                                        format!("/Value/GP41/{gi}/Value/0"),
                                        original.clone(),
                                        json!(record),
                                        "YYMMDD",
                                        record,
                                    ),
                                );
                                let n = &pv["Value"]["GP43"][pi]["Value"][1];
                                fields.insert(
                                    "total_issued_shares".into(),
                                    proof(
                                        pr,
                                        format!("/Value/GP43/{pi}/Value/1"),
                                        n.clone(),
                                        json!(decimal(n, 4)?),
                                        "ten_thousand_shares_total",
                                        record,
                                    ),
                                );
                            }
                        } else if matches.len() > 1
                            || gs.iter().any(|(record, _)| {
                                (0..=31).contains(&day.signed_duration_since(*record).num_days())
                                    && ps.iter().any(|(d, _)| d == record)
                            })
                        {
                            status = "conflict";
                        }
                    }
                    let mut missing = vec![
                        "announced",
                        "subscription_start",
                        "subscription_end",
                        "unit",
                        "fee_per_share",
                        "allotment_ratio",
                        "allotment_date",
                        "refund_date",
                        "listing_date",
                        "allocation",
                    ];
                    if !fields.contains_key("record_date") {
                        missing.push("record_date");
                    }
                    report.events.push(RightsFacts {
                        event_id: format!("{instrument}:{day}"),
                        status: status.into(),
                        fields,
                        missing_fields: missing.into_iter().map(str::to_owned).collect(),
                        engine_supported: false,
                    });
                }
                report
                    .queries
                    .insert("rights_reference".into(), "verified".into());
                // A registration may not be assigned to two events.
                let mut used = BTreeMap::<String, usize>::new();
                for event in &report.events {
                    if let Some(f) = event.fields.get("record_date") {
                        *used.entry(f.value.to_string()).or_default() += 1;
                    }
                }
                for event in &mut report.events {
                    if event
                        .fields
                        .get("record_date")
                        .is_some_and(|f| used[&f.value.to_string()] > 1)
                    {
                        event.status = "conflict".into();
                        event.fields.remove("record_date");
                        event.fields.remove("total_issued_shares");
                        event.missing_fields.push("record_date".into());
                    }
                }
            }
        }
        if reports.insert(instrument.into(), report).is_some() {
            return Err(fail("duplicate structured rights shards"));
        }
    }
    Ok(reports)
}
/// Refresh the derived snapshot only after its source bytes have been captured.
pub fn refresh(bundle: &mut ActionBundle, root: &Path) -> io::Result<()> {
    bundle.schema_version = bundle.schema_version.max(8);
    bundle.field_facts = derive(bundle, root)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(
        code: &str,
        record: &str,
        ex: &str,
        price: &str,
        total: &str,
        ratio: &str,
    ) -> (tempfile::TempDir, ActionBundle) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::create_dir(root.join("raw")).unwrap();
        let symbol = format!("{}.{}", &code[2..], &code[..2]);
        let range: analytics_core::DateRange =
            serde_json::from_value(json!({"start":"2021-01-01","end":"2022-12-31"})).unwrap();
        let mut receipts = vec![];
        let mut hashes = BTreeMap::new();
        let mut add = |function: &str, args: Value, body: Value| {
            let bytes = serde_json::to_vec(&body).unwrap();
            let hash = format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            );
            let name = format!("sdk-{}.body", &hash[7..]);
            fs::write(root.join("raw").join(&name), bytes).unwrap();
            hashes.insert(name.clone(), hash.clone());
            receipts.push(json!({"kind":"sdk_native_response","sdk":"tdxaidata","version":"1.2.2","function":function,"arguments":args,"filename":name,"sha256":hash}));
        };
        add(
            "GetCWDATAInStr",
            json!([symbol, "2021-01-01 00:00:00", "2022-12-31 00:00:00"]),
            json!({"ErrorId":"0","Date":[ex],"Type":["1"],"Value":[["0",price,"0",ratio]]}),
        );
        for (field, values) in [
            ("GP41", json!([format!("{}.00", &record[2..]), "0.00"])),
            ("GP43", json!([price, total])),
        ] {
            let query = json!({"type":3,"code":symbol,"table_list":[field],"start_time":"2021-01-01 00:00:00","end_time":"2022-12-31 00:00:00"});
            add(
                "GetProDataInStr",
                json!([query.to_string()]),
                json!({"ErrorId":"0","Error":"","Value":{field:[{"Date":record,"Value":values}]}}),
            );
        }
        let name = format!("{code}-actions-structured.json");
        let shard = json!({"instrument":code,"evidence":{"provider":"tdxaidata","sdk_version":"1.2.2","sdk_receipts":receipts,"rights_fields":{"GP41":"captured","GP43":"captured"},"request_range":range,"acquired_at":"2026-10-16T00:00:00+00:00"}});
        let bytes = serde_json::to_vec(&shard).unwrap();
        hashes.insert(
            name.clone(),
            format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            ),
        );
        fs::write(root.join("raw").join(name), bytes).unwrap();
        let ex = Date::parse_from_str(ex, "%Y%m%d").unwrap();
        let mut book:ActionBundle=serde_json::from_value(json!({"schema_version":7,"source_bindings":{},"structured_coverage":{},"coverage":{},"downloader":"JFQuant analytics-storage corporate-actions","downloaded_at":"fixture","queried_symbols":[],"actions":[{"id":format!("{code}:{ex}"),"instrument":code,"announced":null,"record_date":null,"ex_date":ex,"pay_date":null,"listing_date":null,"cash_per_share":"0","shares_per_share":"0","source":"fixture","terms":{"kind":"historical","category":"rights_issue","reference":{"kind":"unchanged","evidence":"fixture"}}}],"failures":{code:"settlement unverified"},"raw_sha256":hashes})).unwrap();
        refresh(&mut book, root).unwrap();
        fs::write(
            root.join("actions.json"),
            serde_json::to_vec(&book).unwrap(),
        )
        .unwrap();
        (tmp, book)
    }
    #[test]
    fn three_historical_cases_keep_dates_and_exact_units_separate() {
        for (code, record, ex, price, total, ratio, expected) in [
            (
                "SZ002142", "20211123", "20211202", "19.97", "59557.45", "1.00", "0.1",
            ),
            (
                "SH600030",
                "20220118",
                "20220127",
                "14.43",
                "155202.17",
                "1.50",
                "0.15",
            ),
            (
                "SH600958",
                "20220420",
                "20220429",
                "8.46",
                "150290.70",
                "2.80",
                "0.28",
            ),
        ] {
            let (tmp, book) = fixture(code, record, ex, price, total, ratio);
            let event = &book.field_facts[code].events[0];
            assert_eq!(event.fields["rights_ratio"].value, expected);
            assert_eq!(event.fields["subscription_price"].value, price);
            assert_ne!(
                event.fields["record_date"].value,
                event.fields["ex_date"].value
            );
            assert_eq!(
                event.fields["total_issued_shares"].value,
                decimal(&json!(total), 4).unwrap()
            );
            assert!(!event.engine_supported);
            assert!(event.missing_fields.contains(&"listing_date".into()));
            ActionBundle::read(&tmp.path().join("actions.json")).unwrap();
        }
    }
    #[test]
    fn native_tampering_wrong_paths_and_request_identity_cannot_be_published() {
        let (tmp, mut book) = fixture(
            "SH600030",
            "20220118",
            "20220127",
            "14.43",
            "155202.17",
            "1.50",
        );
        book.field_facts.get_mut("SH600030").unwrap().events[0]
            .fields
            .get_mut("record_date")
            .unwrap()
            .field_path = "/Value/GP43/0/Value/0".into();
        fs::write(
            tmp.path().join("actions.json"),
            serde_json::to_vec(&book).unwrap(),
        )
        .unwrap();
        assert!(ActionBundle::read(&tmp.path().join("actions.json")).is_err());
        let fact = &book.field_facts["SH600030"].events[0].fields["rights_ratio"];
        fs::write(tmp.path().join("raw").join(&fact.filename), b"{}").unwrap();
        assert!(derive(&book, tmp.path()).is_err());
    }
    fn replace_query(tmp: &Path, book: &mut ActionBundle, field: &str, body: Value) {
        let name = book
            .field_facts
            .values()
            .next()
            .unwrap()
            .evidence_name
            .clone();
        let path = tmp.join("raw").join(&name);
        let mut shard: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let r = shard["evidence"]["sdk_receipts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| {
                r["function"] == "GetProDataInStr"
                    && r["arguments"][0].as_str().unwrap().contains(field)
            })
            .unwrap();
        let bytes = serde_json::to_vec(&body).unwrap();
        let hash = format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(&bytes))
        );
        let filename = format!("sdk-{}.body", &hash[7..]);
        fs::write(tmp.join("raw").join(&filename), bytes).unwrap();
        book.raw_sha256.insert(filename.clone(), hash.clone());
        r["filename"] = json!(filename);
        r["sha256"] = json!(hash);
        let bytes = serde_json::to_vec(&shard).unwrap();
        book.raw_sha256.insert(
            name,
            format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            ),
        );
        fs::write(path, bytes).unwrap();
    }
    #[test]
    fn empty_placeholder_errors_duplicates_and_ambiguous_dates_have_distinct_outcomes() {
        for (body, expected) in [
            (json!({"ErrorId":"0","Value":{"GP41":[]}}), "valid_empty"),
            (json!({"ErrorId":"0","Data":{}}), "coverage_unknown"),
            (json!({"ErrorId":10}), "unavailable"),
            (
                json!({"ErrorId":"0","Value":{"GP41":[{"Date":"0","Value":["0","0"]}]}}),
                "invalid",
            ),
            (
                json!({"ErrorId":"0","Value":{"GP41":[{"Date":"20220118","Value":["220118","0"]},{"Date":"20220118","Value":["220118","0"]}]}}),
                "invalid",
            ),
        ] {
            let (tmp, mut book) = fixture(
                "SH600030",
                "20220118",
                "20220127",
                "14.43",
                "155202.17",
                "1.50",
            );
            replace_query(tmp.path(), &mut book, "GP41", body);
            refresh(&mut book, tmp.path()).unwrap();
            assert_eq!(book.field_facts["SH600030"].queries["GP41"], expected);
            assert!(
                !book.field_facts["SH600030"].events[0]
                    .fields
                    .contains_key("record_date")
            );
        }
        let (tmp, mut book) = fixture(
            "SH600030",
            "20220118",
            "20220127",
            "14.43",
            "155202.17",
            "1.50",
        );
        replace_query(
            tmp.path(),
            &mut book,
            "GP43",
            json!({"ErrorId":"0","Value":{"GP43":[{"Date":"20220118","Value":["14.44","155202.17"]}]}}),
        );
        refresh(&mut book, tmp.path()).unwrap();
        assert!(
            !book.field_facts["SH600030"].events[0]
                .fields
                .contains_key("record_date")
        );
        assert_eq!(book.field_facts["SH600030"].events[0].status, "conflict");
    }
    #[test]
    fn different_native_event_kinds_on_one_day_do_not_erase_rights_facts() {
        let (tmp, mut book) = fixture(
            "SH600030",
            "20220118",
            "20220127",
            "14.43",
            "155202.17",
            "1.50",
        );
        let name = book.field_facts["SH600030"].evidence_name.clone();
        let path = tmp.path().join("raw").join(&name);
        let mut shard: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let r = shard["evidence"]["sdk_receipts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| r["function"] == "GetCWDATAInStr")
            .unwrap();
        let body = json!({"ErrorId":"0","Date":["20220127","20220127"],"Type":["15","1"],"Value":[["0","0","0","0"],["0","14.43","0","1.50"]]});
        let bytes = serde_json::to_vec(&body).unwrap();
        let hash = format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(&bytes))
        );
        let filename = format!("sdk-{}.body", &hash[7..]);
        fs::write(tmp.path().join("raw").join(&filename), bytes).unwrap();
        book.raw_sha256.insert(filename.clone(), hash.clone());
        r["filename"] = json!(filename);
        r["sha256"] = json!(hash);
        let bytes = serde_json::to_vec(&shard).unwrap();
        book.raw_sha256.insert(
            name,
            format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            ),
        );
        fs::write(path, bytes).unwrap();
        refresh(&mut book, tmp.path()).unwrap();
        assert_eq!(book.field_facts["SH600030"].events.len(), 1);
        assert!(
            book.field_facts["SH600030"].events[0]
                .fields
                .contains_key("record_date")
        );
        book.raw_sha256.insert(
            "../outside-actions-structured.json".into(),
            "sha256:invalid".into(),
        );
        assert!(derive(&book, tmp.path()).is_err());
    }
    #[test]
    fn ambiguous_registration_and_changed_native_identity_do_not_fill_dates() {
        let (tmp, mut book) = fixture(
            "SH600030",
            "20220118",
            "20220127",
            "14.43",
            "155202.17",
            "1.50",
        );
        replace_query(
            tmp.path(),
            &mut book,
            "GP41",
            json!({"ErrorId":"0","Value":{"GP41":[{"Date":"20220118","Value":["220118","0"]},{"Date":"20220119","Value":["220119","0"]}]}}),
        );
        replace_query(
            tmp.path(),
            &mut book,
            "GP43",
            json!({"ErrorId":"0","Value":{"GP43":[{"Date":"20220118","Value":["14.43","155202.17"]},{"Date":"20220119","Value":["14.43","155202.17"]}]}}),
        );
        refresh(&mut book, tmp.path()).unwrap();
        assert_eq!(book.field_facts["SH600030"].events[0].status, "conflict");
        assert!(
            !book.field_facts["SH600030"].events[0]
                .fields
                .contains_key("record_date")
        );
        let name = book.field_facts["SH600030"].evidence_name.clone();
        let path = tmp.path().join("raw").join(&name);
        let mut shard: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        let receipt = shard["evidence"]["sdk_receipts"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|r| {
                r["function"] == "GetProDataInStr"
                    && r["arguments"][0].as_str().unwrap().contains("GP41")
            })
            .unwrap();
        let mut query: Value =
            serde_json::from_str(receipt["arguments"][0].as_str().unwrap()).unwrap();
        query["code"] = json!("600958.SH");
        receipt["arguments"] = json!([query.to_string()]);
        let bytes = serde_json::to_vec(&shard).unwrap();
        book.raw_sha256.insert(
            name,
            format!(
                "sha256:{:x}",
                analytics_core::hash::Hex(Sha256::digest(&bytes))
            ),
        );
        fs::write(path, bytes).unwrap();
        refresh(&mut book, tmp.path()).unwrap();
        assert_eq!(book.field_facts["SH600030"].queries["GP41"], "invalid");
    }
    #[test]
    fn structured_revision_is_immutable_and_does_not_clear_settlement_gaps() {
        use super::super::review::{EventRevision, RevisionRequest, StructuredFieldRef};
        let (tmp, book) = fixture(
            "SH600030",
            "20220118",
            "20220127",
            "14.43",
            "155202.17",
            "1.50",
        );
        let original = &book.actions[0];
        let mut replacement = original.clone();
        replacement.record_date = Some("2022-01-18".parse().unwrap());
        let mut request = RevisionRequest {
            structured_manifest: None,
            schema_version: 2,
            evidence_manifest: tmp.path().join("actions.json"),
            events: vec![EventRevision {
                participants: None,
                original_id: Some(original.id.clone()),
                original_sha256: Some(format!(
                    "sha256:{:x}",
                    analytics_core::hash::Hex(Sha256::digest(
                        serde_json::to_vec(original).unwrap()
                    ))
                )),
                replacement,
                reason: "verified structured record date".into(),
                fields: BTreeMap::new(),
                structured_fields: BTreeMap::from([(
                    "record_date".into(),
                    StructuredFieldRef {
                        event_id: original.id.clone(),
                        field: "record_date".into(),
                    },
                )]),
            }],
            resolve_failures: vec![],
            resolve_issues: vec![],
            coverage_reviews: vec![],
        };
        let out = tmp.path().join("reviewed");
        let revised = super::super::review::apply(&book, tmp.path(), &request, &out).unwrap();
        assert!(revised.actions[0].record_date.is_some());
        assert_eq!(revised.failures, book.failures);
        assert!(!revised.field_facts["SH600030"].events[0].engine_supported);
        ActionBundle::read(&out.join("actions.json")).unwrap();
        assert!(
            ActionBundle::read(&tmp.path().join("actions.json"))
                .unwrap()
                .actions[0]
                .record_date
                .is_none()
        );
        request.events[0].original_sha256 = Some("stale".into());
        assert!(
            super::super::review::apply(&book, tmp.path(), &request, &tmp.path().join("stale"))
                .is_err()
        );
        request.events[0].original_sha256 = Some(format!(
            "sha256:{:x}",
            analytics_core::hash::Hex(Sha256::digest(serde_json::to_vec(original).unwrap()))
        ));
        request.events[0]
            .structured_fields
            .get_mut("record_date")
            .unwrap()
            .field = "total_issued_shares".into();
        assert!(
            super::super::review::apply(
                &book,
                tmp.path(),
                &request,
                &tmp.path().join("wrong-field")
            )
            .is_err()
        );
    }
}
