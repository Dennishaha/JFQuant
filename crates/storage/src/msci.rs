//! Fixed MSCI public close-history adapter. A selected market supplies the exchange calendar.
//! Index levels never supply an A-share calendar, executable OHLC, or historical membership.
use crate::{
    adjustment::{AdjustmentBasis, AdjustmentRecord, NORMALIZATION},
    daily::{DailyBar, DailyManifest, write_bars},
};
use analytics_core::{Date, DateRange, indices::IndexConfig, task::TaskContext};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
    process::Command,
};

/// Explicit adapter identity stored in adjustment records and frozen task contexts.
pub const SOURCE: &str = "msci.public-index-history.close-v1";
fn invalid(s: impl std::fmt::Display) -> io::Error {
    io::Error::other(s.to_string())
}
fn digest(bytes: &[u8]) -> String {
    format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(Sha256::digest(bytes))
    )
}
/// Hash of the adapter implementation; no external Python runtime is used.
pub fn signature() -> String {
    digest(include_bytes!("msci.rs"))
}
/// Validate only this adapter's fixed identity/price contract, without network access.
pub fn validate(config: &IndexConfig) -> io::Result<()> {
    config.validate().map_err(invalid)?;
    let code = config
        .mappings
        .get("msci")
        .ok_or_else(|| invalid("msci_mapping_missing"))?;
    if code.len() != 6
        || !code.bytes().all(|c| c.is_ascii_digit())
        || *code != config.definition.official_code
        || config.definition.currency != "CNY"
        || config.definition.return_basis != "price"
    {
        return Err(invalid(
            "msci_requires_official_six_digit_code_and_CNY_price",
        ));
    }
    Ok(())
}
fn url(config: &IndexConfig, range: &DateRange) -> String {
    format!(
        "https://app2.msci.com/products/service/index/indexmaster/getLevelDataForGraph?currency_symbol=CNY&index_variant=STRD&start_date={}&end_date={}&data_frequency=daily&index_codes={}",
        range.start.format("%Y%m%d"),
        range.end.format("%Y%m%d"),
        config.definition.official_code
    )
}
#[derive(Deserialize)]
struct Response {
    msci_index_code: String,
    index_variant_type: String,
    #[serde(rename = "ISO_currency_symbol")]
    currency: String,
    indexes: Levels,
}
#[derive(Deserialize)]
struct Levels {
    #[serde(rename = "INDEX_LEVELS")]
    rows: Vec<Level>,
}
#[derive(Deserialize)]
struct Level {
    calc_date: u32,
    level_eod: Option<f64>,
}
fn decode(
    config: &IndexConfig,
    range: &DateRange,
    bytes: &[u8],
) -> io::Result<BTreeMap<Date, Option<f64>>> {
    let response: Response = serde_json::from_slice(bytes).map_err(invalid)?;
    if response.msci_index_code != config.definition.official_code
        || response.currency != "CNY"
        || response.index_variant_type != "STRD"
    {
        return Err(invalid("msci_response_identity_mismatch"));
    }
    let mut rows = BTreeMap::new();
    let mut previous = None;
    for row in response.indexes.rows {
        let date = Date::parse_from_str(&row.calc_date.to_string(), "%Y%m%d").map_err(invalid)?;
        if !range.contains(date)
            || previous.is_some_and(|d| d >= date)
            || row.level_eod.is_some_and(|v| !v.is_finite() || v <= 0.)
        {
            return Err(invalid("msci_invalid_date_order_range_or_level"));
        }
        previous = Some(date);
        rows.insert(date, row.level_eod);
    }
    Ok(rows)
}
fn save(path: &Path, value: &impl serde::Serialize) -> io::Result<()> {
    fs::write(path, serde_json::to_vec_pretty(value).map_err(invalid)?)?;
    fs::File::open(path)?.sync_all()
}
/// Validate a dated calendar selection before capture; never infer sessions from index levels.
pub fn calendar(dates: &[Date], range: &DateRange) -> io::Result<Vec<Date>> {
    if range.start > range.end
        || dates.is_empty()
        || dates.windows(2).any(|w| w[0] >= w[1])
        || range.start < dates[0]
        || range.end > *dates.last().unwrap()
    {
        return Err(invalid("msci_reference_calendar_does_not_cover_request"));
    }
    let dates: Vec<_> = dates
        .iter()
        .copied()
        .filter(|d| range.contains(*d))
        .collect();
    if dates.is_empty() {
        return Err(invalid("msci_no_requested_trading_dates"));
    }
    Ok(dates)
}
/// Capture immutable raw JSON and a canonical close-only market in an owned staging directory.
/// The workflow freezes/verifies the calendar market and atomically publishes the result.
pub fn collect(
    config: &IndexConfig,
    range: &DateRange,
    dates: &[Date],
    root: &Path,
    context: &TaskContext,
) -> io::Result<PathBuf> {
    validate(config)?;
    let dates = calendar(dates, range)?;
    context.check()?;
    if root.exists() {
        return Err(invalid("msci_staging_already_exists"));
    }
    fs::create_dir_all(root.join("raw"))?;
    context.report("MSCI 官方收盘行情采集")?;
    let endpoint = url(config, range);
    let output = context.execute(|| {
        crate::transport::curl(Command::new("/usr/bin/curl").args([
            "--disable",
            "--fail",
            "--silent",
            "--show-error",
            "--proto",
            "=https",
            "--connect-timeout",
            "15",
            "--max-time",
            "45",
            "--max-filesize",
            "33554432",
            &endpoint,
        ]))
    })?;
    if !output.status.success() {
        return Err(invalid("msci_history_transport_failed"));
    }
    publish_capture(
        config,
        range,
        &dates,
        root,
        &endpoint,
        &output.stdout,
        context,
    )
}
fn publish_capture(
    config: &IndexConfig,
    range: &DateRange,
    dates: &[Date],
    root: &Path,
    endpoint: &str,
    bytes: &[u8],
    context: &TaskContext,
) -> io::Result<PathBuf> {
    context.check()?;
    let rows = decode(config, range, bytes)?;
    let end = dates
        .iter()
        .rev()
        .find(|d| rows.get(d).is_some_and(Option::is_some))
        .copied()
        .ok_or_else(|| invalid("msci_no_valid_trading_close"))?;
    let selected: Vec<_> = dates.iter().copied().take_while(|d| *d <= end).collect();
    let missing: Vec<_> = selected
        .iter()
        .copied()
        .filter(|d| !rows.get(d).is_some_and(Option::is_some))
        .collect();
    let key = &config.data_key;
    fs::create_dir_all(root.join("market/bars"))?;
    fs::create_dir_all(root.join("market/adjustments"))?;
    fs::write(root.join("raw/history.json"), bytes)?;
    let receipt = json!({"schema_version":1,"url":endpoint,"sha256":digest(bytes),"retrieved_at":chrono::Utc::now(),"identity":{"official_code":config.definition.official_code,"currency":"CNY","variant":"STRD"}});
    save(&root.join("raw/receipt.json"), &receipt)?;
    let bars: Vec<_> = selected
        .iter()
        .map(|date| DailyBar {
            instrument_id: key.clone(),
            trade_date: *date,
            open: None,
            high: None,
            low: None,
            close: rows.get(date).copied().flatten(),
            adj_open: None,
            adj_high: None,
            adj_low: None,
            adj_close: None,
            volume: None,
            amount: None,
        })
        .collect();
    let first = bars
        .iter()
        .find(|b| b.close.is_some())
        .ok_or_else(|| invalid("msci_anchor_missing"))?;
    let record = AdjustmentRecord {
        schema_version: 4,
        instrument_id: key.clone(),
        source_symbol: config.definition.official_code.clone(),
        index_input: Some(Box::new(config.clone())),
        basis: AdjustmentBasis::PriceIndex {
            source: SOURCE.into(),
        },
    };
    record.validate()?;
    write_bars(&root.join(format!("market/bars/{key}.parquet")), &bars)?;
    save(
        &root.join(format!("market/adjustments/{key}.json")),
        &record,
    )?;
    let manifest = DailyManifest {
        schema_version: 5,
        calendar: selected.clone(),
        universes: Default::default(),
        instruments: [key.clone()].into(),
        price_basis: "unadjusted".into(),
        normalization: NORMALIZATION.into(),
        adjustment_anchors: BTreeMap::from([(key.clone(), record.anchor(first.trade_date)?)]),
    };
    save(&root.join("market/daily_manifest.json"), &manifest)?;
    let coverage = DateRange {
        start: selected[0],
        end,
    };
    let mapping = analytics_core::source_map::SourceMap {
        schema_version: 2,
        exceptions: BTreeMap::from([(
            key.clone(),
            analytics_core::source_map::SourceException {
                exchange_symbol: String::new(),
                kind: "price_index".into(),
                coverage: coverage.clone(),
                locators: vec![analytics_core::source_map::SourceLocator {
                    provider: "msci".into(),
                    symbol: config.definition.official_code.clone(),
                    coverage,
                    evidence: vec!["raw/receipt.json".into()],
                }],
            },
        )]),
    };
    mapping.validate().map_err(invalid)?;
    save(&root.join("market/source_map.json"), &mapping)?;
    let report = json!({"schema_version":1,"provider":"msci","index_config":config,"adapter_signature":signature(),"requested":range,"actual":{"start":selected[0],"end":end},"source_rows":rows.len(),"valid_close_count":bars.iter().filter(|b|b.close.is_some()).count(),"missing_close_dates":missing,"tail_truncated":end < *dates.last().unwrap(),"historical_members_supported":false,"fields":["close"],"scope":"close levels on supplied exchange calendar; no OHLC, volume or membership inferred"});
    save(&root.join("report.json"), &report)?;
    let evidence = BTreeMap::from([
        ("history.json", digest(bytes)),
        (
            "receipt.json",
            digest(&fs::read(root.join("raw/receipt.json"))?),
        ),
    ]);
    save(
        &root.join("jfquant_source.json"),
        &json!({"downloader":"JFQuant analytics-storage","provider":"msci","snapshot_verified":true,"independent_download":true,"adapter_schema_version":1,"collector_signature":signature(),"evidence_sha256":evidence,"report":report}),
    )?;
    crate::daily::DailyDataset::open(root.join("market"))?.validate_factor_basis(key)?;
    context.check()?;
    Ok(root.join("market"))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn config() -> IndexConfig {
        let mut c = analytics_core::indices::builtin_config("csi500").unwrap();
        c.definition.id = "msci_fixture".into();
        c.definition.symbol = None;
        c.definition.official_code = "746059".into();
        c.data_key = "IDX_msci_fixture".into();
        c.mappings = BTreeMap::from([("msci".into(), "746059".into())]);
        c
    }
    fn range() -> DateRange {
        DateRange {
            start: "2026-03-30".parse().unwrap(),
            end: "2026-04-03".parse().unwrap(),
        }
    }
    fn response(rows: serde_json::Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"msci_index_code":"746059","ISO_currency_symbol":"CNY","index_variant_type":"STRD","indexes":{"INDEX_LEVELS":rows}})).unwrap()
    }
    #[test]
    fn msci_response_and_mapping_identity_are_checked_before_use() {
        let c = config();
        validate(&c).unwrap();
        let mut wrong = c.clone();
        wrong.mappings.insert("msci".into(), "735577".into());
        assert!(validate(&wrong).is_err());
        wrong = c.clone();
        wrong.definition.currency = "CNH".into();
        assert!(validate(&wrong).is_err());
        let bytes = response(json!([{"calc_date":20260330,"level_eod":100.}]));
        for (key, value) in [
            ("msci_index_code", "735577"),
            ("ISO_currency_symbol", "USD"),
            ("index_variant_type", "NETR"),
        ] {
            let mut v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            v[key] = value.into();
            assert!(decode(&c, &range(), &serde_json::to_vec(&v).unwrap()).is_err());
        }
        for rows in [
            json!([{"calc_date":20260330,"level_eod":100.},{"calc_date":20260330,"level_eod":101.}]),
            json!([{"calc_date":20260329,"level_eod":100.}]),
            json!([{"calc_date":20260330,"level_eod":0.}]),
        ] {
            assert!(decode(&c, &range(), &response(rows)).is_err());
        }
    }
    #[test]
    fn msci_calendar_keeps_internal_missing_truncates_tail_and_never_invents_ohlc() {
        let c = config();
        let range = range();
        // March 31 deliberately absent in the supplied session calendar. An index
        // quote on that date must not turn it into an exchange trading session.
        let dates: Vec<Date> = ["2026-03-30", "2026-04-01", "2026-04-02", "2026-04-03"]
            .map(|d| d.parse().unwrap())
            .into();
        let bytes = response(
            json!([{"calc_date":20260330,"level_eod":100.},{"calc_date":20260331,"level_eod":999.},{"calc_date":20260401,"level_eod":null},{"calc_date":20260402,"level_eod":102.},{"calc_date":20260403,"level_eod":null}]),
        );
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir(tmp.path().join("raw")).unwrap();
        let root = publish_capture(
            &c,
            &range,
            &dates,
            tmp.path(),
            &url(&c, &range),
            &bytes,
            &TaskContext::default(),
        )
        .unwrap();
        let ds = crate::daily::DailyDataset::open(&root).unwrap();
        assert_eq!(ds.calendar, dates[..3]);
        assert_eq!(ds.raw_field(&c.data_key, "close").unwrap().get(1), None);
        assert_eq!(
            ds.raw_field(&c.data_key, "close").unwrap().get(2),
            Some(102.)
        );
        for field in ["open", "high", "low"] {
            assert!(
                ds.raw_field(&c.data_key, field)
                    .unwrap()
                    .values
                    .iter()
                    .all(Option::is_none)
            );
        }
        assert!(
            crate::daily::read_bars(&root.join(format!("bars/{}.parquet", c.data_key)))
                .unwrap()
                .iter()
                .all(|b| b.volume.is_none() && b.amount.is_none())
        );
        assert!(
            ds.source_map()
                .exchange_symbol(&c.data_key, dates[0])
                .is_err()
        );
        let report: serde_json::Value =
            serde_json::from_slice(&fs::read(tmp.path().join("report.json")).unwrap()).unwrap();
        assert_eq!(report["missing_close_dates"], json!(["2026-04-01"]));
        assert_eq!(report["tail_truncated"], true);
        assert_eq!(ds.index_config("msci_fixture").unwrap(), c);
        assert_eq!(
            fs::read(tmp.path().join("raw/history.json")).unwrap(),
            bytes
        );
    }
    #[test]
    fn msci_rejects_missing_calendar_all_null_and_cancelled_capture() {
        let r = range();
        let c = config();
        assert!(calendar(&[], &r).is_err());
        assert!(calendar(&[r.start], &r).is_err());
        let tmp = tempfile::tempdir().unwrap();
        assert!(
            publish_capture(
                &c,
                &r,
                &[r.start],
                tmp.path(),
                "fixture",
                &response(json!([])),
                &TaskContext::default()
            )
            .is_err()
        );
        assert!(!tmp.path().join("market/daily_manifest.json").exists());
        let context = TaskContext::default();
        context
            .cancel
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            collect(
                &c,
                &r,
                &[r.start, r.end],
                &tmp.path().join("cancelled"),
                &context
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::Interrupted
        );
        assert!(!tmp.path().join("cancelled").exists());
    }
}
