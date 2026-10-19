use super::*;
use crate::{adjustment::AdjustmentSource, collector::Provider};
use sha2::{Digest, Sha256};

fn d(s: &str) -> Date {
    s.parse().unwrap()
}
fn request() -> CollectionRequest {
    CollectionRequest {
        index_inputs: Default::default(),
        source_bindings: Default::default(),
        provider: Provider::Tdxaidata,
        adjustment_source: AdjustmentSource::TdxForward,
        start: d("2023-12-01"),
        end: d("2024-07-02"),
        instruments: BTreeSet::from(["SH600001".into()]),
        benchmarks: Default::default(),
        universes: Default::default(),
    }
}
fn receipt(root: &Path, function: &str, arguments: Value, value: Value) -> Value {
    let bytes = serde_json::to_vec(&if function == "GetHISDATsInStr" {
        value
    } else {
        json!({"ErrorId":"0","Value":value})
    })
    .unwrap();
    let sha = format!("{:x}", analytics_core::hash::Hex(Sha256::digest(&bytes)));
    let name = format!("sdk-{sha}.body");
    fs::write(root.join("raw").join(&name), bytes).unwrap();
    json!({"kind":"sdk_native_response","sdk":"tdxaidata","version":"1.2.2",
        "function":function,"arguments":arguments,"filename":name,"sha256":format!("sha256:{sha}")})
}
fn fixture(root: &Path) -> Value {
    fs::create_dir_all(root.join("raw")).unwrap();
    // Hand-authored accounting golden: CNY 1bn - (10000+20000)*10000 = 700m;
    // 100m actual shares * CNY7/share = 700m. A later inconsistent revision must mask it.
    let fin = json!({"FN004":["7.00","8.00"],"FN238":["100000000.00","100000000.00"],
        "FN271":["1000000000.00","1100000000.00"],"FN428":["10000.00","10000.00"],
        "FN429":["20000.00","20000.00"],"announce_time":["20240402","20240504"],"tag_time":["20231231","20231231"]});
    let arg = json!({"type":1,"code":"600001.SH","table_list":FIELDS,"start_time":"2023-12-01 00:00:00",
        "end_time":"2024-07-02 00:00:00","report_type":"announce_time"});
    let mut receipts = vec![receipt(
        root,
        "GetProDataInStr",
        json!([arg.to_string()]),
        fin,
    )];
    for (start, end, rows) in [
        (
            "2023-12-01",
            "2023-12-31",
            json!([{"Date":20231229,"Zgb":100000000.0}]),
        ),
        (
            "2024-01-01",
            "2024-07-02",
            json!([{"Date":20240628,"Zgb":200000000.0},{"Date":20240701,"Zgb":null}]),
        ),
    ] {
        let first = format!("{start} 00:00:00");
        let last = format!("{end} 00:00:00");
        receipts.push(receipt(
            root,
            "GetHISDATsInStr",
            json!(["600001.SH", first, last, "1d", "none", -1]),
            json!({"Date":[]}),
        ));
        receipts.push(receipt(
            root,
            "GetGbInfoInStr",
            json!(["600001.SH", 1, 0, "", first, last]),
            rows,
        ));
    }
    json!({"instrument":"SH600001","observed_on":"2024-07-03","evidence":{"sdk_receipts":receipts}})
}
#[test]
fn native_units_common_equity_and_exact_date_share_golden() {
    let temp = tempfile::tempdir().unwrap();
    let msg = fixture(temp.path());
    let rows = normalize(&request(), temp.path(), "SH600001", &msg).unwrap();
    assert_eq!(rows.annual[0].book_equity_cny, Some(700000000.));
    assert_eq!(rows.annual[1].book_equity_cny, Some(800000000.));
    assert_eq!(rows.shares[0].total_shares, Some(100000000.));
    assert_eq!(rows.shares[1].total_shares, Some(200000000.));
    assert_eq!(rows.shares[2].total_shares, None);
    let mut history = History {
        normalization: VERSION.into(),
        bundle_sha256: format!("sha256:{}", "a".repeat(64)),
        availability: Availability::ProviderDates,
        instruments: BTreeMap::from([("SH600001".into(), rows)]),
    };
    assert_eq!(
        history.book("SH600001", 2023, d("2024-05-04")),
        Some(700000000.)
    );
    assert_eq!(
        history.book("SH600001", 2023, d("2024-06-28")),
        Some(800000000.)
    );
    assert_eq!(
        history.shares("SH600001", d("2024-06-28")),
        Some(200000000.)
    );
    assert_eq!(history.shares("SH600001", d("2024-06-29")), None);
    assert_eq!(history.shares("SH600001", d("2024-07-01")), None);
    history.availability = Availability::ObservedOn;
    assert_eq!(history.book("SH600001", 2023, d("2024-06-28")), None);
    assert_eq!(history.book("SH600001", 2023, d("2024-07-03")), None);
    assert_eq!(
        history.book("SH600001", 2023, d("2024-07-04")),
        Some(800000000.)
    );
    assert_eq!(history.shares("SH600001", d("2024-06-28")), None);
}
#[test]
fn missing_preferred_and_inconsistent_latest_revision_stay_null() {
    for replacement in [Value::Null, json!("0.00")] {
        let temp = tempfile::tempdir().unwrap();
        let mut msg = fixture(temp.path());
        let r = &mut msg["evidence"]["sdk_receipts"][0];
        let mut data = body(temp.path(), r).unwrap();
        // Known preferred capital cannot be silently zeroed or omitted.
        data["FN428"][1] = replacement;
        *r = receipt(temp.path(), "GetProDataInStr", r["arguments"].clone(), data);
        let rows = normalize(&request(), temp.path(), "SH600001", &msg).unwrap();
        assert_eq!(rows.annual[0].book_equity_cny, Some(700000000.));
        assert_eq!(rows.annual[1].book_equity_cny, None);
        assert_eq!(
            rows.issues[0].code,
            "annual_common_equity_missing_or_inconsistent"
        );
        let history = History {
            normalization: VERSION.into(),
            bundle_sha256: String::new(),
            availability: Availability::ProviderDates,
            instruments: BTreeMap::from([("SH600001".into(), rows)]),
        };
        assert_eq!(history.book("SH600001", 2023, d("2024-06-28")), None);
    }
}
#[test]
fn altered_receipt_wrong_symbol_date_zero_missing_window_and_bad_numbers_rejected() {
    let temp = tempfile::tempdir().unwrap();
    let original = fixture(temp.path());
    for kind in 0..7 {
        let mut msg = original.clone();
        match kind {
            0 => msg["instrument"] = json!("SH600002"),
            1 => msg["evidence"]["sdk_receipts"][0]["sha256"] = json!("sha256:bad"),
            2 => {
                msg["evidence"]["sdk_receipts"]
                    .as_array_mut()
                    .unwrap()
                    .pop();
            }
            3 => msg["evidence"]["sdk_receipts"][2]["arguments"][0] = json!("600002.SH"),
            _ => {
                let r = &mut msg["evidence"]["sdk_receipts"][2];
                let mut data = body(temp.path(), r).unwrap();
                if kind == 4 {
                    data[0]["Date"] = json!(0);
                } else {
                    data[0]["Zgb"] = if kind == 5 { json!("NaN") } else { json!(true) };
                }
                *r = receipt(temp.path(), "GetGbInfoInStr", r["arguments"].clone(), data);
            }
        }
        assert!(
            normalize(&request(), temp.path(), "SH600001", &msg).is_err(),
            "case {kind}"
        );
    }
}
#[test]
fn mismatched_array_and_duplicate_financial_version_rejected() {
    for duplicate in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let mut msg = fixture(temp.path());
        let r = &mut msg["evidence"]["sdk_receipts"][0];
        let mut data = body(temp.path(), r).unwrap();
        if duplicate {
            data["announce_time"][1] = data["announce_time"][0].clone();
        } else {
            data["FN004"].as_array_mut().unwrap().pop();
        }
        *r = receipt(temp.path(), "GetProDataInStr", r["arguments"].clone(), data);
        assert!(normalize(&request(), temp.path(), "SH600001", &msg).is_err());
    }
}

#[test]
fn retained_package_replays_native_bytes_and_rejects_tampered_derived_equity() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let msg = fixture(root);
    let captured = normalize(&request(), root, "SH600001", &msg).unwrap();
    fs::write(
        root.join("raw/SH600001-actions-structured.json"),
        serde_json::to_vec(&msg).unwrap(),
    )
    .unwrap();
    let mut bundle = json!({"schema_version":1,"normalization":VERSION,"request":request(),
        "instruments":{"SH600001":captured},"evidence_sha256":{"SH600001-actions-structured.json":
            crate::market_view::file_hash(&root.join("raw/SH600001-actions-structured.json")).unwrap()}});
    fs::write(
        root.join("fundamentals.json"),
        serde_json::to_vec(&bundle).unwrap(),
    )
    .unwrap();
    let mut history = History {
        normalization: VERSION.into(),
        bundle_sha256: crate::market_view::file_hash(&root.join("fundamentals.json")).unwrap(),
        availability: Availability::ProviderDates,
        instruments: serde_json::from_value(bundle["instruments"].clone()).unwrap(),
    };
    verify_bundle(root, &history).unwrap();
    bundle["instruments"]["SH600001"]["annual"][0]["book_equity_cny"] = json!(900000000.);
    fs::write(
        root.join("fundamentals.json"),
        serde_json::to_vec(&bundle).unwrap(),
    )
    .unwrap();
    assert!(verify_bundle(root, &history).is_err());
    history.bundle_sha256 = crate::market_view::file_hash(&root.join("fundamentals.json")).unwrap();
    history.instruments = serde_json::from_value(bundle["instruments"].clone()).unwrap();
    assert!(
        verify_bundle(root, &history)
            .unwrap_err()
            .to_string()
            .contains("normalization_mismatch")
    );
}
