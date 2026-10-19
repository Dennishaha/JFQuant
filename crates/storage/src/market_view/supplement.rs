//! Immutable missing-cell overlays with replayable source and normalization validation.
use super::*;
use crate::daily::DailyDataset;
use analytics_core::Date;
use serde_json::{Value, json};
/// One explicitly selected source and set of cells; no source priority fallback.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    /// Workspace-relative registered native market directory.
    pub source_path: PathBuf,
    /// Full parent tree digest, including acquisition receipts and original responses.
    pub source_sha256: String,
    /// Frozen logical security identity.
    pub instrument: String,
    /// Exact dates to repair, restricted to the base calendar.
    pub dates: Vec<Date>,
    /// Raw fields: open/high/low/close/volume/amount.
    pub fields: Vec<String>,
}
/// Versioned reviewed missing-cell repair request and immutable view manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SupplementView {
    /// Version 1, read explicitly without conversion.
    pub schema_version: u32,
    /// Original or preceding immutable view, relative to the workspace.
    pub base_path: PathBuf,
    /// Base directory hash, retaining the entire original snapshot.
    pub base_sha256: String,
    /// Explicit native source selections.
    pub selections: Vec<Selection>,
}
/// Verified replacement, carrying both raw and original-basis research values.
#[derive(Debug, Clone)]
pub struct Cell {
    /// Security.
    pub instrument: String,
    /// Trading day.
    pub date: Date,
    /// Raw field name.
    pub field: String,
    /// Unadjusted CNY or shares.
    pub raw: f64,
    /// Research value using the original normalization basis.
    pub research: f64,
}
fn resolve(workspace: &Path, path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(invalid("supplement_path_must_be_workspace_relative"));
    }
    let full = workspace.join(path).canonicalize()?;
    if !full.starts_with(workspace.canonicalize()?) {
        return Err(invalid("supplement_path_outside_workspace"));
    }
    Ok(full)
}
fn raw(data: &DailyDataset, id: &str, field: &str) -> io::Result<crate::qlib::DailyField> {
    match field {
        "volume" => data.raw_volume(id),
        "amount" => data.raw_amount(id),
        _ => data.raw_field(id, field),
    }
}
impl SupplementView {
    /// Verify immutable sources and recompute every selected repair. No files are written.
    pub fn validate(&self, workspace: &Path) -> io::Result<(PathBuf, Vec<Cell>, Value)> {
        if self.schema_version != 1 || self.selections.is_empty() {
            return Err(invalid("invalid_market_supplement_request"));
        }
        let base = resolve(workspace, &self.base_path)?;
        if tree_hash(&base)? != self.base_sha256 {
            return Err(invalid("supplement_base_changed"));
        }
        let data = DailyDataset::open(&base)?;
        let mut cells = vec![];
        let mut problems = vec![];
        let mut seen = std::collections::BTreeSet::new();
        for selection in &self.selections {
            analytics_core::task::TaskContext::current().check()?;
            let source = resolve(workspace, &selection.source_path)?;
            let parent = source
                .parent()
                .ok_or_else(|| invalid("supplement_source_parent_missing"))?;
            if tree_hash(parent)? != selection.source_sha256 {
                return Err(invalid("supplement_source_or_receipts_changed"));
            }
            let metadata: Value =
                serde_json::from_slice(&fs::read(parent.join("jfquant_source.json"))?)
                    .map_err(invalid)?;
            if metadata["snapshot_verified"] != true
                || metadata["independent_download"] != true
                || metadata["downloader"] != "JFQuant analytics-storage"
                || !matches!(metadata["provider"].as_str(), Some("hithink" | "tdxaidata"))
            {
                return Err(invalid("supplement_requires_native_market_capture"));
            }
            let expected = if metadata["provider"] == "hithink" {
                crate::hithink::producer_signature()
            } else {
                crate::collector::collector_signature()
            };
            if metadata["collector_signature"] != expected {
                return Err(invalid("supplement_unreviewed_capture_producer"));
            }
            let candidate = DailyDataset::open(&source)?;
            if selection.dates.is_empty()
                || selection.fields.is_empty()
                || selection.fields.iter().any(|f| {
                    !matches!(
                        f.as_str(),
                        "open" | "high" | "low" | "close" | "volume" | "amount"
                    )
                })
            {
                return Err(invalid("invalid_supplement_selection"));
            }
            let id = &selection.instrument;
            let base_fields = ["open", "high", "low", "close", "volume", "amount"]
                .into_iter()
                .map(|f| Ok((f, raw(&data, id, f)?)))
                .collect::<io::Result<BTreeMap<_, _>>>()?;
            let source_fields = ["open", "high", "low", "close", "volume", "amount"]
                .into_iter()
                .map(|f| Ok((f, raw(&candidate, id, f)?)))
                .collect::<io::Result<BTreeMap<_, _>>>()?;
            let factors = data.field(id, "factor")?;
            for date in &selection.dates {
                let attempt = (|| -> io::Result<Vec<Cell>> {
                    let day = data
                        .calendar
                        .binary_search(date)
                        .map_err(|_| invalid("supplement_date_outside_base_calendar"))?;
                    let other = candidate
                        .calendar
                        .binary_search(date)
                        .map_err(|_| invalid("supplement_source_date_missing"))?;
                    if data.exchange_symbol(id, *date)? != candidate.exchange_symbol(id, *date)? {
                        return Err(invalid("supplement_security_identity_conflict"));
                    }
                    let mut merged = BTreeMap::new();
                    for field in ["open", "high", "low", "close", "volume", "amount"] {
                        let old = base_fields[field].get(day);
                        let new = source_fields[field].get(other);
                        if let (Some(a), Some(b)) = (old, new) {
                            let tolerance = if field == "volume" {
                                1_f64.max(a.abs() * 1e-6)
                            } else if field == "amount" {
                                0.01_f64.max(a.abs() * 1e-6)
                            } else {
                                0.00500001
                            };
                            if (a - b).abs() > tolerance {
                                return Err(invalid(format!(
                                    "supplement_existing_value_conflict:{field}"
                                )));
                            }
                        }
                        let chosen = if selection.fields.iter().any(|f| f == field) {
                            old.or(new)
                        } else {
                            old
                        };
                        merged.insert(field, chosen);
                    }
                    let value = |f: &str| {
                        merged[f].ok_or_else(|| invalid(format!("supplement_incomplete_row:{f}")))
                    };
                    let (o, h, l, c, v) = (
                        value("open")?,
                        value("high")?,
                        value("low")?,
                        value("close")?,
                        value("volume")?,
                    );
                    if !([o, h, l, c, v].iter().all(|v| v.is_finite())
                        && o > 0.
                        && l > 0.
                        && h >= o.max(c)
                        && l <= o.min(c)
                        && v >= 0.
                        && merged["amount"].is_none_or(|a| a.is_finite() && a >= 0.))
                    {
                        return Err(invalid("supplement_ohlcv_inconsistent"));
                    }
                    let mut result = vec![];
                    for field in &selection.fields {
                        if base_fields[field.as_str()].get(day).is_some() {
                            continue;
                        }
                        let raw = source_fields[field.as_str()]
                            .get(other)
                            .filter(|v| v.is_finite())
                            .ok_or_else(|| invalid(format!("supplement_fact_missing:{field}")))?;
                        let factor = factors
                            .get(day)
                            .filter(|f| f.is_finite() && *f > 0.)
                            .ok_or_else(|| invalid("supplement_original_normalization_missing"))?;
                        let research = match field.as_str() {
                            "volume" => raw / factor / 100.,
                            "amount" => raw / 1000.,
                            _ => raw * factor,
                        };
                        if !research.is_finite() {
                            return Err(invalid("supplement_normalization_invalid"));
                        }
                        result.push(Cell {
                            instrument: id.clone(),
                            date: *date,
                            field: field.clone(),
                            raw,
                            research,
                        });
                    }
                    Ok(result)
                })();
                match attempt {Ok(result)=>{for cell in result {if !seen.insert((cell.instrument.clone(),cell.date,cell.field.clone())){return Err(invalid("supplement_duplicate_cell"));}cells.push(cell);}},Err(e)=>problems.push(json!({"instrument":id,"date":date,"kind":if e.to_string().contains("normalization"){"normalization_incompatible"}else if e.to_string().contains("conflict"){"source_conflict"}else{"missing_or_invalid_fact"},"reason":e.to_string(),"source":selection.source_path}))}
            }
        }
        let execution_ready = (!cells.is_empty() || !problems.is_empty())
            && problems
                .iter()
                .all(|p| p["kind"] == "normalization_incompatible");
        let report = json!({"schema_version":1,"ready":problems.is_empty()&&!cells.is_empty(),"research_ready":problems.is_empty()&&!cells.is_empty(),"execution_ready":execution_ready,"cells":cells.iter().map(|c|json!({"instrument":c.instrument,"date":c.date,"field":c.field,"raw":c.raw,"research":c.research})).collect::<Vec<_>>(),"problems":problems});
        Ok((base, cells, report))
    }
    /// Revalidate the source receipts on read, rejecting incomplete published views.
    pub fn read(root: &Path) -> io::Result<(PathBuf, Vec<Cell>)> {
        let view: Self = serde_json::from_slice(&fs::read(root.join("market_supplement.json"))?)
            .map_err(invalid)?;
        let workspace = root
            .ancestors()
            .find(|p| p.join(".jfquant/workspace.json").is_file())
            .ok_or_else(|| invalid("supplement_workspace_missing"))?;
        let (base, cells, report) = view.validate(workspace)?;
        if report["ready"] != true {
            return Err(invalid(format!("market_supplement_invalid:{report}")));
        }
        Ok((base, cells))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        adjustment::*,
        daily::{DailyBar, DailyManifest, write_bars},
    };
    #[test]
    fn repairs_share_original_price_volume_basis_preserve_values_and_revalidate_sources() {
        let tmp = tempfile::tempdir().unwrap();
        let w = tmp.path();
        fs::create_dir(w.join(".jfquant")).unwrap();
        fs::write(w.join(".jfquant/workspace.json"), b"{}").unwrap();
        let base = w.join("base");
        fs::create_dir_all(base.join("calendars")).unwrap();
        fs::create_dir_all(base.join("instruments")).unwrap();
        fs::create_dir_all(base.join("features/sh600000")).unwrap();
        fs::write(base.join("calendars/day.txt"), "2021-08-10\n2021-08-11\n").unwrap();
        for (f, a, b) in [
            ("open", 1., f32::NAN),
            ("high", 1.1, f32::NAN),
            ("low", 0.9, f32::NAN),
            ("close", 1., f32::NAN),
            ("factor", 0.1, 0.1),
            ("volume", 100., f32::NAN),
            ("amount", 10., f32::NAN),
            ("vwap", 1., f32::NAN),
        ] {
            fs::write(
                base.join(format!("features/sh600000/{f}.day.bin")),
                [0_f32, a, b]
                    .into_iter()
                    .flat_map(f32::to_le_bytes)
                    .collect::<Vec<_>>(),
            )
            .unwrap();
        }
        let parent = w.join("source");
        let src = parent.join("market");
        fs::create_dir_all(src.join("adjustments")).unwrap();
        fs::create_dir_all(src.join("bars")).unwrap();
        let dates = [
            "2021-08-10".parse::<Date>().unwrap(),
            "2021-08-11".parse().unwrap(),
        ];
        let bars = dates
            .into_iter()
            .map(|d| DailyBar {
                instrument_id: "SH600000".into(),
                trade_date: d,
                open: Some(10.),
                high: Some(11.),
                low: Some(9.),
                close: Some(10.),
                adj_open: None,
                adj_high: None,
                adj_low: None,
                adj_close: None,
                volume: Some(1000.),
                amount: Some(10000.),
            })
            .collect::<Vec<_>>();
        write_bars(&src.join("bars/SH600000.parquet"), &bars).unwrap();
        let record = AdjustmentRecord {
            index_input: None,
            schema_version: 4,
            instrument_id: "SH600000".into(),
            source_symbol: "SH600000".into(),
            basis: AdjustmentBasis::Explicit {
                source: FactorSource::SyntheticFixture,
                source_url: "JFQuant analytics-workflow synthetic-demo-v1".into(),
                verified_through: dates[1],
                points: vec![AdjustmentPoint {
                    effective_date: dates[0],
                    factor: "1".into(),
                }],
            },
        };
        fs::write(
            src.join("adjustments/SH600000.json"),
            serde_json::to_vec(&record).unwrap(),
        )
        .unwrap();
        let m = DailyManifest {
            schema_version: 5,
            calendar: dates.to_vec(),
            universes: BTreeMap::new(),
            instruments: std::collections::BTreeSet::from(["SH600000".into()]),
            price_basis: "unadjusted".into(),
            normalization: NORMALIZATION.into(),
            adjustment_anchors: BTreeMap::from([(
                "SH600000".into(),
                record.anchor(dates[0]).unwrap(),
            )]),
        };
        fs::write(
            src.join("daily_manifest.json"),
            serde_json::to_vec(&m).unwrap(),
        )
        .unwrap();
        fs::write(parent.join("jfquant_source.json"),serde_json::to_vec(&json!({"downloader":"JFQuant analytics-storage","provider":"hithink","snapshot_verified":true,"independent_download":true,"collector_signature":crate::hithink::producer_signature()})).unwrap()).unwrap();
        let mut view = SupplementView {
            schema_version: 1,
            base_path: "base".into(),
            base_sha256: tree_hash(&base).unwrap(),
            selections: vec![Selection {
                source_path: "source/market".into(),
                source_sha256: tree_hash(&parent).unwrap(),
                instrument: "SH600000".into(),
                dates: vec![dates[1]],
                fields: vec![
                    "open".into(),
                    "high".into(),
                    "low".into(),
                    "close".into(),
                    "volume".into(),
                    "amount".into(),
                ],
            }],
        };
        let (_, cells, report) = view.validate(w).unwrap();
        assert_eq!(cells.len(), 6);
        assert_eq!(report["ready"], true);
        let output = w.join("view");
        fs::create_dir(&output).unwrap();
        fs::write(
            output.join("market_supplement.json"),
            serde_json::to_vec(&view).unwrap(),
        )
        .unwrap();
        let data = DailyDataset::open(&output).unwrap();
        assert_eq!(
            data.raw_field("SH600000", "close").unwrap().get(1),
            Some(10.)
        );
        assert!((data.field("SH600000", "close").unwrap().get(1).unwrap() - 1.).abs() < 1e-6);
        assert!((data.field("SH600000", "volume").unwrap().get(1).unwrap() - 100.).abs() < 1e-4);
        assert_eq!(data.raw_amount("SH600000").unwrap().get(1), Some(10000.));
        assert!((data.field("SH600000", "vwap").unwrap().get(1).unwrap() - 1.).abs() < 1e-6);
        assert_eq!(data.field("SH600000", "vwap").unwrap().get(0), Some(1.));
        assert_eq!(tree_hash(&base).unwrap(), view.base_sha256);
        // A calculated factor sidecar belongs to this view, while quotation reads retain the base root.
        let factors = crate::three_factor::ThreeFactorReturns {
            schema_version: 1,
            source: "synthetic view fixture".into(),
            market: "fixture".into(),
            return_basis: "synthetic decimal returns".into(),
            unit: "decimal".into(),
            observations: vec![crate::three_factor::Observation {
                trade_date: dates[1],
                mkt: Some(0.01),
                smb: Some(-0.02),
                hml: Some(0.03),
            }],
        };
        fs::write(
            output.join(crate::three_factor::FILE_NAME),
            serde_json::to_vec(&factors).unwrap(),
        )
        .unwrap();
        let data = DailyDataset::open(&output).unwrap();
        assert_eq!(
            data.three_factor_path(),
            output
                .join(crate::three_factor::FILE_NAME)
                .canonicalize()
                .unwrap()
        );
        assert_eq!(data.field("SH600000", "mkt").unwrap().get(1), Some(0.01));
        view.selections[0].dates = vec![dates[0]];
        assert_eq!(view.validate(w).unwrap().2["ready"], false);
        view.selections[0].dates = vec![dates[1]];
        fs::write(
            base.join("features/sh600000/factor.day.bin"),
            [0_f32, 0.1, f32::NAN]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        view.base_sha256 = tree_hash(&base).unwrap();
        let report = view.validate(w).unwrap().2;
        assert_eq!(report["ready"], false);
        assert!(report["problems"].to_string().contains("normalization"));
        assert_eq!(report["execution_ready"], true);
        assert_eq!(report["research_ready"], false);
        fs::write(parent.join("tampered"), b"changed").unwrap();
        assert!(view.validate(w).is_err());
    }
}
