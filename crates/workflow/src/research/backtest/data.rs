use super::*;
use analytics_factor::{FactorRunRequest, InMemoryFactorExecutor, Panel};
use analytics_storage::qlib::{DailyField, Membership};
use std::{
    collections::HashMap,
    io::{BufReader, BufWriter, Read, Write},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Sample {
    pub day: usize,
    pub instrument: String,
    pub label: Option<f64>,
    pub label_end: Option<Date>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct YearMeta {
    pub signature: String,
    pub factor_ids: Vec<String>,
    pub rows: Vec<Sample>,
    pub finite_counts: BTreeMap<String, usize>,
}
pub(super) struct YearData {
    pub meta: YearMeta,
    pub features: Vec<f32>,
}
fn paths(cache: &Path, year: i32) -> (PathBuf, PathBuf) {
    (
        cache.join(format!("{year}.json")),
        cache.join(format!("{year}.bin")),
    )
}
pub(super) fn load_meta(cache: &Path, year: i32) -> Result<YearMeta> {
    Ok(serde_json::from_reader(BufReader::new(fs::File::open(
        paths(cache, year).0,
    )?))?)
}
pub(super) fn load_year(cache: &Path, year: i32, ids: &[String]) -> Result<YearData> {
    let meta = load_meta(cache, year)?;
    if meta.factor_ids != ids {
        return Err("factor cache schema/order mismatch".into());
    }
    let mut bytes = Vec::new();
    fs::File::open(paths(cache, year).1)?.read_to_end(&mut bytes)?;
    if bytes.len() != meta.rows.len() * ids.len() * 4 {
        return Err("truncated factor cache".into());
    }
    let features = bytes
        .as_chunks::<4>()
        .0
        .iter()
        .map(|b| f32::from_le_bytes(*b))
        .collect();
    Ok(YearData { meta, features })
}
pub(super) fn empty_matrix(columns: usize) -> DenseMatrix {
    DenseMatrix {
        features: Vec::new(),
        labels: Vec::new(),
        rows: 0,
        columns,
    }
}
pub(super) fn prediction_range(
    cache: &Path,
    range: &DateRange,
    ids: &[String],
    calendar: &[Date],
) -> Result<YearData> {
    let mut rows = Vec::new();
    let mut features = Vec::new();
    for year in range.start.year()..=range.end.year() {
        let data = load_year(cache, year, ids)?;
        for (mut row, values) in data
            .meta
            .rows
            .into_iter()
            .zip(data.features.chunks_exact(ids.len()))
        {
            if calendar[row.day] >= range.start && calendar[row.day] <= range.end {
                if row.label_end.is_none_or(|d| d > range.end) {
                    row.label = None;
                }
                rows.push(row);
                features.extend_from_slice(values);
            }
        }
    }
    Ok(YearData {
        meta: YearMeta {
            signature: String::new(),
            factor_ids: ids.to_vec(),
            rows,
            finite_counts: BTreeMap::new(),
        },
        features,
    })
}
#[cfg(test)]
pub(super) fn matrix_range(
    cache: &Path,
    range: &DateRange,
    ids: &[String],
    calendar: &[Date],
) -> Result<DenseMatrix> {
    Ok(matrix_range_with_policy(
        cache,
        range,
        ids,
        calendar,
        &ResearchQualityPolicy::default(),
    )?
    .matrix)
}

pub(super) struct SelectionMask {
    pub row_mask: Vec<bool>,
    pub row_order_sha256: String,
    pub excluded: Vec<analytics_core::research_quality::QualityDecision>,
    pub retained_rows: usize,
}
pub(super) struct MatrixSelection {
    pub matrix: DenseMatrix,
    pub mask: SelectionMask,
}
fn selection_mask(
    data: &YearData,
    range: &DateRange,
    ids: &[String],
    calendar: &[Date],
    policy: &ResearchQualityPolicy,
    require_labels: bool,
) -> Result<SelectionMask> {
    let row_order_sha256 = format!(
        "sha256:{:x}",
        Sha256::digest(serde_json::to_vec(
            &data
                .meta
                .rows
                .iter()
                .map(|r| (r.day, &r.instrument))
                .collect::<Vec<_>>()
        )?)
    );
    let mut row_mask = Vec::new();
    let mut excluded = Vec::new();
    for (row, values) in data
        .meta
        .rows
        .iter()
        .zip(data.features.chunks_exact(ids.len()))
    {
        let features =
            policy.accepts_features(values.iter().filter(|v| v.is_finite()).count(), ids.len());
        let labeled = !require_labels
            || (row.label.is_some() && row.label_end.is_some_and(|d| d <= range.end));
        row_mask.push(features && labeled);
        if policy.strictness != analytics_core::research_quality::ResearchStrictness::Strict
            && (!features || !labeled)
        {
            excluded.push(analytics_core::research_quality::QualityDecision {
                security: row.instrument.clone(),
                coverage: DateRange {
                    start: calendar[row.day],
                    end: calendar[row.day],
                },
                code: if !labeled {
                    "label_missing_or_outside_split"
                } else {
                    "insufficient_finite_features"
                }
                .into(),
                kind: ProblemKind::Missing,
                disposition: ProblemDisposition::ExcludeSample,
                message: if !labeled {
                    "缺少有效标签或标签越过截止日期"
                } else {
                    "有效因子数量不足"
                }
                .into(),
                evidence: vec![row_order_sha256.clone()],
                affected_factors: ids.to_vec(),
            });
        }
    }
    excluded.sort_by_key(|d| (d.security.clone(), d.code.clone(), d.coverage.start));
    let mut compressed: Vec<analytics_core::research_quality::QualityDecision> = Vec::new();
    for decision in excluded {
        if let Some(last) = compressed.last_mut()
            && last.security == decision.security
            && last.code == decision.code
            && calendar
                .binary_search(&last.coverage.end)
                .ok()
                .zip(calendar.binary_search(&decision.coverage.start).ok())
                .is_some_and(|(a, b)| a + 1 == b)
        {
            last.coverage.end = decision.coverage.end;
        } else {
            compressed.push(decision);
        }
    }
    Ok(SelectionMask {
        retained_rows: row_mask.iter().filter(|v| **v).count(),
        row_mask,
        row_order_sha256,
        excluded: compressed,
    })
}
pub(super) fn prediction_mask(
    cache: &Path,
    range: &DateRange,
    ids: &[String],
    calendar: &[Date],
    policy: &ResearchQualityPolicy,
) -> Result<SelectionMask> {
    selection_mask(
        &prediction_range(cache, range, ids, calendar)?,
        range,
        ids,
        calendar,
        policy,
        false,
    )
}
pub(super) fn prediction_range_with_policy(
    cache: &Path,
    range: &DateRange,
    ids: &[String],
    calendar: &[Date],
    policy: &ResearchQualityPolicy,
) -> Result<YearData> {
    let mut data = prediction_range(cache, range, ids, calendar)?;
    let mask = selection_mask(&data, range, ids, calendar, policy, false)?;
    let mut rows = Vec::new();
    let mut features = Vec::new();
    for ((row, values), retain) in data
        .meta
        .rows
        .into_iter()
        .zip(data.features.chunks_exact(ids.len()))
        .zip(mask.row_mask)
    {
        if retain {
            rows.push(row);
            features.extend_from_slice(values);
        }
    }
    data.meta.rows = rows;
    data.features = features;
    Ok(data)
}
pub(super) fn matrix_range_with_policy(
    cache: &Path,
    range: &DateRange,
    ids: &[String],
    calendar: &[Date],
    policy: &ResearchQualityPolicy,
) -> Result<MatrixSelection> {
    let data = prediction_range(cache, range, ids, calendar)?;
    let mask = selection_mask(&data, range, ids, calendar, policy, true)?;
    let mut matrix = empty_matrix(ids.len());
    for ((row, values), retain) in data
        .meta
        .rows
        .iter()
        .zip(data.features.chunks_exact(ids.len()))
        .zip(&mask.row_mask)
    {
        if *retain && let Some(label) = row.label {
            matrix.features.extend(values.iter().map(|v| *v as f64));
            matrix.labels.push(label as f32);
            matrix.rows += 1;
        }
    }
    Ok(MatrixSelection { matrix, mask })
}

fn field(map: &BTreeMap<&str, DailyField>, name: &str, day: usize) -> Option<f64> {
    map.get(name).and_then(|v| v.get(day))
}
fn ratio(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    a.zip(b).filter(|(_, b)| *b > 0.0).map(|(a, b)| a / b)
}

fn directional_movements(
    open: Option<f64>,
    previous_open: Option<f64>,
    high: Option<f64>,
    low: Option<f64>,
) -> (Option<f64>, Option<f64>) {
    let dtm = open
        .zip(previous_open)
        .zip(high)
        .map(|((o, p), h)| if o <= p { 0.0 } else { (h - o).max(o - p) });
    let dbm = open
        .zip(previous_open)
        .zip(low)
        .map(|((o, p), l)| if o >= p { 0.0 } else { (o - l).max(o - p) });
    (dtm, dbm)
}

pub(super) struct YearInputs<'a> {
    pub dataset: &'a DailyDataset,
    pub members: &'a BTreeMap<String, Vec<Membership>>,
    pub benchmark: &'a str,
    pub universe_id: &'a str,
    pub pool_start: Date,
    pub allow_missing: bool,
    pub price_basis: analytics_core::research_quality::PriceBasis,
}
// Supplier fields already carry the requested price/share/CNY units. Neutral
// scaling here is not a synthetic adjustment coefficient exposed to research.
fn input_scales(supplier: bool, factor: &DailyField) -> Result<(f64, f64, f64)> {
    if supplier {
        return Ok((1.0, 1.0, 1.0));
    }
    let anchor = factor
        .values
        .iter()
        .flatten()
        .copied()
        .find(|v| *v > 0.0)
        .ok_or("research adjustment anchor missing")?;
    Ok((anchor, anchor * 100.0, 1000.0))
}
pub(super) fn prepare_year(
    inputs: YearInputs<'_>,
    registry: &FactorRegistry,
    ids: &[String],
    year: i32,
    cache: &Path,
    signature: &str,
) -> Result<()> {
    let YearInputs {
        dataset,
        members,
        benchmark,
        universe_id,
        pool_start,
        allow_missing,
        price_basis,
    } = inputs;
    let (meta_path, feature_path) = paths(cache, year);
    let first = dataset.calendar.partition_point(|d| d.year() < year);
    let end = dataset.calendar.partition_point(|d| d.year() <= year);
    if first >= end {
        return Err(format!("no trading dates in {year}").into());
    }
    let warmup = first.saturating_sub(756);
    let benchmark_open = dataset.field(benchmark, "open")?;
    let benchmark_close = dataset.field(benchmark, "close")?;
    let benchmark_factor = dataset.field(benchmark, "factor")?;
    let (b_anchor, _, _) =
        input_scales(dataset.uses_supplier_prices(benchmark), &benchmark_factor)?;
    let mut panel = Panel {
        instrument_id: Vec::new(),
        trade_date: Vec::new(),
        columns: HashMap::new(),
    };
    let names = [
        "open",
        "high",
        "low",
        "close",
        "volume",
        "amount",
        "vwap",
        "ret",
        "hd",
        "ld",
        "tr",
        "dtm",
        "dbm",
        "banchmarkindexclose",
        "banchmarkindexopen",
        "__universe_member",
    ];
    for name in names {
        panel.columns.insert(name.into(), Vec::new());
    }
    let mut anchors = BTreeMap::from([(benchmark.to_owned(), b_anchor)]);
    let mut selected = Vec::new();
    let mut rows = Vec::new();
    for (instrument, windows) in members {
        analytics_core::task::TaskContext::current().check()?;
        if !windows.iter().any(|w| w.start < end && w.end >= warmup) {
            continue;
        }
        let mut raw = BTreeMap::new();
        for name in [
            "open", "high", "low", "close", "volume", "vwap", "amount", "factor",
        ] {
            raw.insert(
                name,
                dataset.research_field(instrument, name, allow_missing && name != "factor")?,
            );
        }
        let (anchor, volume_scale, amount_scale) =
            input_scales(dataset.uses_supplier_prices(instrument), &raw["factor"])?;
        anchors.insert(instrument.clone(), anchor);
        let p = |name: &str, day: usize| field(&raw, name, day).map(|v| v / anchor);
        for day in warmup..end {
            let close = p("close", day);
            if !close.is_some_and(|v| v > 0.0) {
                continue;
            }
            let open = p("open", day);
            let high = p("high", day);
            let low = p("low", day);
            let previous = day.checked_sub(1).and_then(|d| p("close", d));
            let previous_open = day.checked_sub(1).and_then(|d| p("open", d));
            let volume = field(&raw, "volume", day).map(|v| v * volume_scale);
            let member = windows.iter().any(|w| w.start <= day && day <= w.end);
            let index = panel.instrument_id.len();
            panel.instrument_id.push(instrument.clone());
            panel.trade_date.push(dataset.calendar[day]);
            let tr = high
                .zip(low)
                .zip(previous)
                .map(|((h, l), c)| (h - l).max((h - c).abs()).max((l - c).abs()));
            let (dtm, dbm) = directional_movements(open, previous_open, high, low);
            let values = [
                open,
                high,
                low,
                close,
                volume,
                field(&raw, "amount", day).map(|v| v * amount_scale),
                p("vwap", day),
                ratio(close, previous).map(|v| v - 1.0),
                high.zip(day.checked_sub(1).and_then(|d| p("high", d)))
                    .map(|(a, b)| a - b),
                day.checked_sub(1)
                    .and_then(|d| p("low", d))
                    .zip(low)
                    .map(|(a, b)| a - b),
                tr,
                dtm,
                dbm,
                benchmark_close.get(day).map(|v| v / b_anchor),
                benchmark_open.get(day).map(|v| v / b_anchor),
                if dataset.calendar[day] < pool_start {
                    None
                } else {
                    Some(if member { 1.0 } else { 0.0 })
                },
            ];
            for (name, value) in names.into_iter().zip(values) {
                panel
                    .columns
                    .get_mut(name)
                    .unwrap()
                    .push(value.filter(|v| v.is_finite()));
            }
            if day >= first && member {
                let label = ratio(p("open", day + 2), p("open", day + 1))
                    .map(|r| r - 1.0)
                    .filter(|v| v.is_finite());
                selected.push(index);
                rows.push(Sample {
                    day,
                    instrument: instrument.clone(),
                    label,
                    label_end: dataset.calendar.get(day + 2).copied(),
                });
            }
        }
    }
    // Factors see the historical cross section; model/cache rows are date-major.
    let mut order: Vec<_> = (0..rows.len()).collect();
    order.sort_by_key(|i| (rows[*i].day, rows[*i].instrument.clone()));
    let selected: Vec<_> = order.iter().map(|i| selected[*i]).collect();
    let rows: Vec<_> = order.iter().map(|i| rows[*i].clone()).collect();
    // Hash the actual executor panel, including pre-year history and normalization
    // anchors. Labels and task-wide selection policy are deliberately separate.
    let mut fingerprint = Sha256::new();
    fingerprint.update(serde_json::to_vec(&(
        "factor-shard-v1",
        ids,
        registry
            .all()
            .map(|s| (&s.id, &s.version))
            .collect::<Vec<_>>(),
        universe_id,
        price_basis,
        &anchors,
        dataset.source_map(),
    ))?);
    for (instrument, date) in panel.instrument_id.iter().zip(&panel.trade_date) {
        fingerprint.update((instrument.len() as u64).to_le_bytes());
        fingerprint.update(instrument.as_bytes());
        fingerprint.update(date.to_string().as_bytes());
    }
    for index in &selected {
        fingerprint.update((*index as u64).to_le_bytes());
    }
    for name in names {
        fingerprint.update(name.as_bytes());
        for value in &panel.columns[name] {
            fingerprint.update([u8::from(value.is_some())]);
            fingerprint.update(value.unwrap_or(0.).to_le_bytes());
        }
    }
    let shard_signature = format!("sha256:{:x}", fingerprint.finalize());
    let shared = cache
        .parent()
        .and_then(Path::parent)
        .unwrap_or(cache)
        .join(".factor-shards-v1")
        .join(shard_signature.trim_start_matches("sha256:"));
    if shared.join("manifest.json").is_file() {
        let manifest: serde_json::Value =
            serde_json::from_slice(&fs::read(shared.join("manifest.json"))?)?;
        if manifest["schema_version"] != 1
            || manifest["signature"] != shard_signature
            || manifest["features"] != serde_json::to_value(ids)?
            || manifest["rows"] != rows.len()
            || manifest["sha256"] != file_hash(&shared.join("features.parquet"))?
        {
            return Err("factor shard content/identity mismatch".into());
        }
        let features = read_features(&shared.join("features.parquet"), ids, rows.len())?;
        let finite_counts = serde_json::from_value(manifest["finite_counts"].clone())?;
        write_local(&feature_path, &features)?;
        save(
            meta_path,
            &YearMeta {
                signature: signature.into(),
                factor_ids: ids.to_vec(),
                rows,
                finite_counts,
            },
        )?;
        save(
            cache.join(format!("{year}.receipt.json")),
            &serde_json::json!({"signature":shard_signature,"cache_hit":true,"labels":"recomputed from current input; split boundaries applied at selection"}),
        )?;
        analytics_core::task::TaskContext::current().report(format!("{year} 因子缓存命中"))?;
        return Ok(());
    }
    let mut features = vec![f32::NAN; rows.len() * ids.len()];
    let mut finite_counts = BTreeMap::new();
    let executor = InMemoryFactorExecutor {
        registry: registry.clone(),
        panel,
    };
    eprintln!(
        "year={year} panel_rows={} active_rows={} computing {} native factors",
        executor.panel.instrument_id.len(),
        rows.len(),
        ids.len()
    );
    let request = FactorRunRequest {
        date_range: DateRange {
            start: dataset.calendar[warmup],
            end: dataset.calendar[end - 1],
        },
        universe_id: universe_id.into(),
        factor_ids: ids.to_vec(),
        formula_version: registry.all().next().unwrap().version.clone(),
    };
    let start = std::time::Instant::now();
    let mut column = 0;
    executor.compute_streaming(
        &request,
        &analytics_core::task::TaskContext::current().cancel,
        |spec, values| {
            let mut count = 0;
            for (row, index) in selected.iter().enumerate() {
                if let Some(value) = values[*index]
                    && value.is_finite()
                    && (value as f32).is_finite()
                {
                    features[row * ids.len() + column] = value as f32;
                    count += 1;
                }
            }
            finite_counts.insert(spec.id.clone(), count);
            column += 1;
            analytics_core::task::TaskContext::current()
                .report(format!("{year} 因子 {column}/{}", ids.len()))
                .map_err(|e| analytics_factor::FactorError::InvalidRequest(e.to_string()))?;
            if column % 10 == 0 || column == ids.len() {
                eprintln!(
                    "year={year} factors={column}/{} elapsed={:.1}s",
                    ids.len(),
                    start.elapsed().as_secs_f64()
                );
            }
            Ok(())
        },
    )?;
    let meta = YearMeta {
        signature: signature.into(),
        factor_ids: ids.to_vec(),
        rows,
        finite_counts,
    };
    write_local(&feature_path, &features)?;
    let staging = shared.with_extension(format!("{}.partial", uuid::Uuid::new_v4()));
    fs::create_dir_all(&staging)?;
    write_features(&staging.join("features.parquet"), ids, &features)?;
    save(
        staging.join("manifest.json"),
        &serde_json::json!({"schema_version":1,"signature":shard_signature,"features":ids,"rows":meta.rows.len(),"finite_counts":meta.finite_counts,"sha256":file_hash(&staging.join("features.parquet"))?}),
    )?;
    analytics_core::task::TaskContext::current().check()?;
    fs::rename(staging, &shared)?;
    save(
        cache.join(format!("{year}.receipt.json")),
        &serde_json::json!({"signature":shard_signature,"cache_hit":false}),
    )?;
    save(meta_path, &meta)?;
    eprintln!(
        "year={year} factors saved; elapsed={:.1}s",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}

fn write_local(path: &Path, features: &[f32]) -> Result<()> {
    let part = path.with_extension("bin.partial");
    let mut output = BufWriter::new(fs::File::create(&part)?);
    for value in features {
        output.write_all(&value.to_le_bytes())?;
    }
    output.flush()?;
    fs::rename(part, path)?;
    Ok(())
}
fn write_features(path: &Path, ids: &[String], features: &[f32]) -> Result<()> {
    use arrow_array::{ArrayRef, Float32Array, RecordBatch};
    use arrow_schema::{DataType, Field, Schema};
    use std::sync::Arc;
    let schema = Arc::new(Schema::new(
        ids.iter()
            .map(|id| Field::new(id, DataType::Float32, true))
            .collect::<Vec<_>>(),
    ));
    let columns = ids
        .iter()
        .enumerate()
        .map(|(i, _)| {
            Arc::new(Float32Array::from(
                features
                    .chunks_exact(ids.len())
                    .map(|r| r[i].is_finite().then_some(r[i]))
                    .collect::<Vec<_>>(),
            )) as ArrayRef
        })
        .collect();
    let batch = RecordBatch::try_new(schema.clone(), columns)?;
    let mut writer = parquet::arrow::ArrowWriter::try_new(fs::File::create(path)?, schema, None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}
fn read_features(path: &Path, ids: &[String], rows: usize) -> Result<Vec<f32>> {
    use arrow_array::{Array, Float32Array};
    let builder = parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder::try_new(
        fs::File::open(path)?,
    )?;
    if builder
        .schema()
        .fields()
        .iter()
        .map(|f| f.name())
        .ne(ids.iter())
    {
        return Err("factor shard feature order mismatch".into());
    }
    let mut features = Vec::with_capacity(rows * ids.len());
    for batch in builder.build()? {
        let batch = batch?;
        let arrays = batch
            .columns()
            .iter()
            .map(|a| {
                a.as_any()
                    .downcast_ref::<Float32Array>()
                    .ok_or("factor shard type mismatch")
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        for row in 0..batch.num_rows() {
            for a in &arrays {
                features.push(if a.is_null(row) {
                    f32::NAN
                } else {
                    a.value(row)
                });
            }
        }
    }
    if features.len() != rows * ids.len() {
        return Err("factor shard row count mismatch".into());
    }
    Ok(features)
}

#[cfg(test)]
mod directional_tests {
    use super::*;

    #[test]
    fn cache_checks_order_size_and_purges_unknown_or_cross_year_labels() {
        let dir = tempfile::tempdir().unwrap();
        let ids = vec!["alpha_002".into(), "alpha_001".into()];
        let meta = YearMeta {
            signature: "fixture".into(),
            factor_ids: ids.clone(),
            rows: vec![
                Sample {
                    day: 0,
                    instrument: "A".into(),
                    label: Some(0.1),
                    label_end: Some("2020-12-31".parse().unwrap()),
                },
                Sample {
                    day: 1,
                    instrument: "A".into(),
                    label: Some(0.2),
                    label_end: Some("2021-01-02".parse().unwrap()),
                },
                Sample {
                    day: 2,
                    instrument: "A".into(),
                    label: None,
                    label_end: Some("2020-12-31".parse().unwrap()),
                },
            ],
            finite_counts: BTreeMap::new(),
        };
        let (json, bin) = paths(dir.path(), 2020);
        save(json, &meta).unwrap();
        let bytes: Vec<_> = [f32::NAN, 2.0, 3.0, 4.0, 5.0, 6.0]
            .into_iter()
            .flat_map(f32::to_le_bytes)
            .collect();
        fs::write(&bin, &bytes).unwrap();
        let matrix = matrix_range(
            dir.path(),
            &DateRange {
                start: "2020-01-01".parse().unwrap(),
                end: "2020-12-31".parse().unwrap(),
            },
            &ids,
            &[
                "2020-01-01".parse().unwrap(),
                "2020-06-01".parse().unwrap(),
                "2020-12-31".parse().unwrap(),
            ],
        )
        .unwrap();
        assert_eq!(matrix.rows, 1);
        assert_eq!(matrix.labels, [0.1]);
        assert!(matrix.features[0].is_nan());
        assert_eq!(matrix.features[1], 2.0);
        assert!(load_year(dir.path(), 2020, &[ids[1].clone(), ids[0].clone()]).is_err());
        fs::write(&bin, &bytes[..bytes.len() - 1]).unwrap();
        assert!(load_year(dir.path(), 2020, &ids).is_err());
    }
    #[test]
    fn sample_masks_preserve_nulls_purge_labels_and_predict_tail_without_labels() {
        let dir = tempfile::tempdir().unwrap();
        let ids = vec!["alpha_002".into(), "alpha_014".into()];
        let calendar = vec![
            "2020-01-02".parse().unwrap(),
            "2020-01-03".parse().unwrap(),
            "2020-01-06".parse().unwrap(),
        ];
        let range = DateRange {
            start: calendar[0],
            end: calendar[2],
        };
        let meta = YearMeta {
            signature: "fixture".into(),
            factor_ids: ids.clone(),
            rows: vec![
                Sample {
                    day: 0,
                    instrument: "A".into(),
                    label: Some(0.1),
                    label_end: Some(calendar[2]),
                },
                Sample {
                    day: 1,
                    instrument: "A".into(),
                    label: Some(0.2),
                    label_end: Some(calendar[2]),
                },
                Sample {
                    day: 2,
                    instrument: "A".into(),
                    label: None,
                    label_end: None,
                },
            ],
            finite_counts: Default::default(),
        };
        let (json, bin) = paths(dir.path(), 2020);
        save(json, &meta).unwrap();
        fs::write(
            bin,
            [f32::NAN, f32::NAN, 1., f32::NAN, 2., 3.]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let policy = ResearchQualityPolicy {
            strictness: analytics_core::research_quality::ResearchStrictness::AllowMissing,
            ..Default::default()
        };
        let mask = matrix_range_with_policy(dir.path(), &range, &ids, &calendar, &policy).unwrap();
        assert_eq!(mask.mask.row_mask, vec![false, true, false]);
        assert!(mask.matrix.features[1].is_nan());
        let predictions =
            prediction_range_with_policy(dir.path(), &range, &ids, &calendar, &policy).unwrap();
        assert_eq!(predictions.meta.rows.len(), 2);
        assert!(predictions.meta.rows[1].label.is_none());
        let strict =
            matrix_range_with_policy(dir.path(), &range, &ids, &calendar, &Default::default())
                .unwrap();
        assert_eq!(strict.matrix.rows, 2);
        let policy = ResearchQualityPolicy {
            min_valid_feature_ratio: 1.,
            ..policy
        };
        assert_eq!(
            matrix_range_with_policy(dir.path(), &range, &ids, &calendar, &policy)
                .unwrap()
                .matrix
                .rows,
            0
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn supplier_fields_keep_shares_cny_and_do_not_require_independent_coefficients() {
        let absent = DailyField {
            start: 0,
            values: vec![None, None],
        };
        assert_eq!(input_scales(true, &absent).unwrap(), (1., 1., 1.));
        assert!(input_scales(false, &absent).is_err());
        let explicit = DailyField {
            start: 0,
            values: vec![Some(0.1)],
        };
        assert_eq!(input_scales(false, &explicit).unwrap(), (0.1, 10., 1000.));
    }
    #[test]
    fn supplier_stock_and_index_fixture_prepare_features_with_null_vwap() {
        use analytics_storage::{
            adjustment::{AdjustmentBasis, AdjustmentRecord, HITHINK_NORMALIZATION},
            daily::DailyManifest,
        };
        let temp = tempfile::tempdir().unwrap();
        let w = crate::Workspace::open(temp.path()).unwrap();
        let (market, _, _) = crate::demo::create_canonical(&w).unwrap();
        let root = w.resolve(market.path);
        let path = root.join("daily_manifest.json");
        let mut manifest: DailyManifest =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        manifest.schema_version = 6;
        manifest.normalization = HITHINK_NORMALIZATION.into();
        for id in &manifest.instruments {
            let mut record = AdjustmentRecord::read(&root, id).unwrap();
            record.basis = if id == "SH000905" {
                AdjustmentBasis::PriceIndex {
                    source: "hithink.a-share-index.prices.historical".into(),
                }
            } else {
                AdjustmentBasis::SupplierForward {
                    verified_through: *manifest.calendar.last().unwrap(),
                }
            };
            save(root.join("adjustments").join(format!("{id}.json")), &record).unwrap();
            manifest.adjustment_anchors.get_mut(id).unwrap().factor = "1".into();
        }
        save(&path, &manifest).unwrap();
        let dataset = DailyDataset::open(&root).unwrap();
        let pool = dataset.pool("csi500").unwrap();
        let members = dataset.align_membership(&pool);
        let ids = vec!["alpha_002".into(), "alpha_013".into()];
        let cache = temp.path().join("cache");
        fs::create_dir(&cache).unwrap();
        prepare_year(
            YearInputs {
                dataset: &dataset,
                members: &members,
                benchmark: "SH000905",
                universe_id: "csi500",
                pool_start: pool.coverage.start,
                allow_missing: false,
                price_basis: analytics_core::research_quality::PriceBasis::Provider,
            },
            &FactorRegistry::default(),
            &ids,
            2008,
            &cache,
            "supplier-fixture",
        )
        .unwrap();
        let data = load_year(&cache, 2008, &ids).unwrap();
        assert!(!data.meta.rows.is_empty());
        assert!(
            data.features
                .as_chunks::<2>()
                .0
                .iter()
                .any(|v| v[0].is_finite())
        );
        assert!(
            data.features
                .as_chunks::<2>()
                .0
                .iter()
                .all(|v| v[1].is_nan())
        );
    }
    #[test]
    fn directional_inputs_follow_original_dbm_sign_and_preserve_missing_history() {
        assert_eq!(
            directional_movements(Some(10.0), Some(12.0), Some(11.0), Some(9.0)),
            (Some(0.0), Some(1.0))
        );
        assert_eq!(
            directional_movements(Some(12.0), Some(10.0), Some(12.5), Some(11.0)),
            (Some(2.0), Some(0.0))
        );
        assert_eq!(
            directional_movements(Some(10.0), None, Some(11.0), Some(9.0)),
            (None, None)
        );
    }
}

#[cfg(test)]
mod shard_tests {
    use super::*;
    #[test]
    fn parquet_retains_missing_values_order_and_rejects_wrong_shape() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("features.parquet");
        let ids = vec!["a".into(), "b".into()];
        write_features(&p, &ids, &[1., f32::NAN, 3., 4.]).unwrap();
        let v = read_features(&p, &ids, 2).unwrap();
        assert_eq!(v[0], 1.);
        assert!(v[1].is_nan());
        assert_eq!(v[2..], [3., 4.]);
        assert!(read_features(&p, &["b".into(), "a".into()], 2).is_err());
        assert!(read_features(&p, &ids, 3).is_err());
    }
}

#[cfg(test)]
mod dependency_tests {
    use super::*;
    #[test]
    fn appended_labels_reuse_factors_but_anchor_warmup_and_cross_section_changes_invalidate() {
        let temp = tempfile::tempdir().unwrap();
        let w = crate::Workspace::open(temp.path()).unwrap();
        let (m, _, _) = crate::demo::create(&w).unwrap();
        let root = w.resolve(m.path);
        let mut dataset = DailyDataset::open(&root).unwrap();
        let pool = dataset.pool("csi500").unwrap();
        let members = dataset.align_membership(&pool);
        let registry =
            FactorRegistry::for_set(analytics_core::research_quality::FactorSet::Gtja191);
        let ids = vec!["alpha_002".into()];
        let run =
            |dataset: &DailyDataset, members: &BTreeMap<String, Vec<Membership>>, name: &str| {
                let cache = temp.path().join("runs").join(name).join(".factor-staging");
                fs::create_dir_all(&cache).unwrap();
                prepare_year(
                    YearInputs {
                        dataset,
                        members,
                        benchmark: "SH000905",
                        universe_id: "csi500",
                        pool_start: pool.coverage.start,
                        allow_missing: false,
                        price_basis: analytics_core::research_quality::PriceBasis::Provider,
                    },
                    &registry,
                    &ids,
                    2008,
                    &cache,
                    "task-signature",
                )
                .unwrap();
                let receipt: serde_json::Value =
                    serde_json::from_slice(&fs::read(cache.join("2008.receipt.json")).unwrap())
                        .unwrap();
                (
                    receipt,
                    load_meta(&cache, 2008).unwrap(),
                    fs::read(cache.join("2008.bin")).unwrap(),
                )
            };
        let full_calendar = dataset.calendar.clone();
        dataset.calendar.retain(|d| d.year() <= 2008);
        let (first, before, before_bytes) = run(&dataset, &members, "before");
        assert_eq!(first["cache_hit"], false);
        let last = before
            .rows
            .iter()
            .find(|r| r.instrument == "SH600000" && r.day == 21)
            .unwrap();
        assert!(last.label_end.is_none());
        dataset.calendar = full_calendar;
        let (second, after, after_bytes) = run(&dataset, &members, "after");
        assert_eq!(second["cache_hit"], true);
        assert_eq!(second["signature"], first["signature"]);
        assert_eq!(before_bytes, after_bytes);
        let last = after
            .rows
            .iter()
            .find(|r| r.instrument == "SH600000" && r.day == 21)
            .unwrap();
        assert!(last.label_end.is_some());
        let factor = root.join("features/sh600000/factor.day.bin");
        let mut bytes = fs::read(&factor).unwrap();
        bytes[4..8].copy_from_slice(&0.2f32.to_le_bytes());
        fs::write(factor, bytes).unwrap();
        let (anchor, _, _) = run(&DailyDataset::open(&root).unwrap(), &members, "anchor");
        assert_eq!(anchor["cache_hit"], false);
        assert_ne!(anchor["signature"], second["signature"]);
        let high = root.join("features/sh600000/high.day.bin");
        let mut bytes = fs::read(&high).unwrap();
        bytes[4..8].copy_from_slice(&1.3f32.to_le_bytes());
        fs::write(high, bytes).unwrap();
        let (warmup, _, _) = run(&DailyDataset::open(&root).unwrap(), &members, "warmup");
        assert_eq!(warmup["cache_hit"], false);
        assert_ne!(warmup["signature"], anchor["signature"]);
        let mut different_members = members.clone();
        different_members.remove("SH600001");
        let (cross_section, _, _) = run(
            &DailyDataset::open(&root).unwrap(),
            &different_members,
            "members",
        );
        assert_eq!(cross_section["cache_hit"], false);
        assert_ne!(cross_section["signature"], warmup["signature"]);
    }
}
