//! Common worker-local daily reader for Qlib and versioned canonical Parquet snapshots.
use crate::adjustment::{
    AdjustmentAnchor, AdjustmentBasis, AdjustmentRecord, NORMALIZATION, coefficient,
};
use crate::qlib::{DailyField, Membership, QlibDataset, raw_price};
use analytics_core::{
    Date, DateRange,
    pool::{PoolMembership, validate_instrument},
};
use arrow_array::{
    Array, ArrayRef, BooleanArray, Date32Array, Float64Array, RecordBatch, StringArray,
};
use arrow_schema::{DataType, Field, Schema};
use parquet::arrow::{ArrowWriter, arrow_reader::ParquetRecordBatchReaderBuilder};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    path::{Path, PathBuf},
    sync::Arc,
};

/// Canonical daily record. Raw and adjusted fields are never interchangeable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DailyBar {
    /// Snapshot data code; ordinary symbols need no identity directory.
    pub instrument_id: String,
    /// Exchange-local trading date.
    pub trade_date: Date,
    /// Verified unadjusted OHLC, absent for split-adjusted Yahoo observations.
    pub open: Option<f64>,
    /// High.
    pub high: Option<f64>,
    /// Low.
    pub low: Option<f64>,
    /// Close.
    pub close: Option<f64>,
    /// Provider-adjusted OHLC retained only as source evidence, never as research input.
    pub adj_open: Option<f64>,
    /// Adj high.
    pub adj_high: Option<f64>,
    /// Adj low.
    pub adj_low: Option<f64>,
    /// Adj close.
    pub adj_close: Option<f64>,
    /// Actual shares traded and CNY turnover.
    pub volume: Option<f64>,
    /// Amount.
    pub amount: Option<f64>,
}
/// Published snapshot metadata; no incomplete download can generate this manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DailyManifest {
    /// Canonical daily format version, 5.
    pub schema_version: u32,
    /// Actual exchange trading calendar.
    pub calendar: Vec<Date>,
    /// Known, dated pool sources (current-only lists remain current-only).
    pub universes: BTreeMap<String, PoolMembership>,
    /// Downloaded securities, including explicitly selected benchmarks.
    pub instruments: BTreeSet<String>,
    /// unadjusted or split_adjusted_candidate.
    pub price_basis: String,
    /// Fixed, auditable unit/price transformation description.
    pub normalization: String,
    /// First actual raw close and its source coefficient, frozen independently for every security.
    pub adjustment_anchors: BTreeMap<String, AdjustmentAnchor>,
}
/// Common daily data reader; created, used and destroyed on the computation worker.
pub struct DailyDataset {
    /// Absolute input root.
    pub root: PathBuf,
    /// Trading calendar.
    pub calendar: Vec<Date>,
    kind: DailyKind,
    source_map: analytics_core::source_map::SourceMap,
    read_range: Option<DateRange>,
    field_cache: std::cell::RefCell<BTreeMap<(String, String), DailyField>>,
    bar_cache: std::cell::RefCell<BTreeMap<String, Arc<Vec<DailyBar>>>>,
    adjustment_cache: std::cell::RefCell<BTreeMap<String, AdjustmentRecord>>,
    #[cfg(test)]
    physical_reads: std::cell::Cell<usize>,
}
struct CanonicalInputs<'a> {
    bars: Arc<Vec<DailyBar>>,
    adjustment: AdjustmentRecord,
    anchor: &'a AdjustmentAnchor,
}
enum DailyKind {
    Qlib(QlibDataset),
    Parquet(DailyManifest),
}
fn invalid(s: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, s.to_string())
}
impl DailyDataset {
    /// Install the selected snapshot's exceptional bindings; ordinary codes need none.
    pub fn bind_source_map(
        &mut self,
        map: analytics_core::source_map::SourceMap,
    ) -> io::Result<()> {
        map.validate().map_err(invalid)?;
        self.source_map = map;
        Ok(())
    }
    /// Snapshot-local mapping frozen by caches and models.
    pub fn source_map(&self) -> &analytics_core::source_map::SourceMap {
        &self.source_map
    }
    /// Limit reads to the requested study, without rewriting original files.
    pub fn restrict_reads(&mut self, range: DateRange) -> io::Result<()> {
        range.validate().map_err(invalid)?;
        self.read_range = Some(range);
        Ok(())
    }
    /// Resolve only execution-date market rules.
    pub fn exchange_symbol(&self, id: &str, date: Date) -> io::Result<String> {
        self.source_map
            .exchange_symbol(id, date)
            .map(str::to_owned)
            .map_err(invalid)
    }
    /// Clip exceptional source history to its known scope; normal records use the request.
    pub fn security_range(&self, id: &str, start: Date, end: Date) -> io::Result<DateRange> {
        let r = self
            .source_map
            .exceptions
            .get(id)
            .map_or(DateRange { start, end }, |e| DateRange {
                start: start.max(e.coverage.start),
                end: end.min(e.coverage.end),
            });
        r.validate().map_err(invalid)?;
        Ok(r)
    }
    fn validate_read_key(&self, id: &str) -> io::Result<()> {
        validate_instrument(id).map_err(invalid)
    }
    fn needs_binding(&self, id: &str) -> bool {
        self.source_map.exceptions.get(id).is_some_and(|e| {
            e.locators
                .iter()
                .any(|l| l.provider == "qlib" && l.symbol != id)
        }) && matches!(self.kind, DailyKind::Qlib(_))
    }
    fn locators(&self, id: &str) -> io::Result<Vec<analytics_core::source_map::SourceLocator>> {
        let e = self
            .source_map
            .exceptions
            .get(id)
            .ok_or_else(|| invalid("source_binding_missing"))?;
        let ls: Vec<_> = e
            .locators
            .iter()
            .filter(|l| {
                l.provider == "qlib"
                    && self
                        .read_range
                        .as_ref()
                        .is_none_or(|r| l.coverage.start <= r.end && l.coverage.end >= r.start)
            })
            .cloned()
            .collect();
        if ls.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "source_binding_missing",
            ));
        }
        Ok(ls)
    }
    fn qlib_field(&self, q: &QlibDataset, id: &str, name: &str) -> io::Result<DailyField> {
        let key = (id.to_owned(), name.to_owned());
        if let Some(f) = self.field_cache.borrow().get(&key) {
            return Ok(f.clone());
        }
        let f = q.field(id, name)?;
        #[cfg(test)]
        self.physical_reads.set(self.physical_reads.get() + 1);
        self.field_cache.borrow_mut().insert(key, f.clone());
        Ok(f)
    }
    fn unbound(&self) -> io::Result<Self> {
        Ok(Self {
            root: self.root.clone(),
            calendar: self.calendar.clone(),
            kind: DailyKind::Qlib(QlibDataset::open(&self.root)?),
            source_map: Default::default(),
            read_range: self.read_range.clone(),
            field_cache: Default::default(),
            bar_cache: Default::default(),
            adjustment_cache: Default::default(),
            #[cfg(test)]
            physical_reads: Default::default(),
        })
    }
    fn bound_field(&self, id: &str, name: &str) -> io::Result<DailyField> {
        self.bound_field_policy(id, name, false)
    }
    fn bound_field_policy(
        &self,
        id: &str,
        name: &str,
        allow_missing: bool,
    ) -> io::Result<DailyField> {
        let mut values = vec![None; self.calendar.len()];
        let mut assigned = vec![false; values.len()];
        let direct = self.unbound()?;
        for l in self.locators(id)? {
            let result = match name {
                "__raw_volume" => direct.raw_volume(&l.symbol),
                "__factors" => direct.adjustment_factors(&l.symbol),
                _ if name.starts_with("__raw_") => direct.raw_field(&l.symbol, &name[6..]),
                _ => direct.field(&l.symbol, name),
            };
            let f = self.nullable_absence(result, allow_missing)?;
            for (i, d) in self.calendar.iter().enumerate() {
                if l.coverage.start <= *d && *d <= l.coverage.end {
                    if assigned[i] {
                        return Err(invalid("overlapping source bindings"));
                    }
                    assigned[i] = true;
                    values[i] = f.get(i);
                }
            }
        }
        Ok(DailyField { start: 0, values })
    }

    /// Open the current canonical format or the supported Qlib binary format.
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().canonicalize()?;
        let mapping = root.join("source_map.json");
        let source_map: analytics_core::source_map::SourceMap = if mapping.exists() {
            serde_json::from_slice(&fs::read(mapping)?).map_err(invalid)?
        } else {
            Default::default()
        };
        source_map.validate().map_err(invalid)?;
        if root.join("daily_manifest.json").exists() {
            let value: serde_json::Value =
                serde_json::from_slice(&fs::read(root.join("daily_manifest.json"))?)
                    .map_err(invalid)?;
            if value["schema_version"] != 5 {
                return Err(invalid(
                    "unsupported_data_format: canonical daily schema 5 required; reacquire data without migration",
                ));
            }
            let m: DailyManifest = serde_json::from_value(value).map_err(invalid)?;
            if m.schema_version != 5
                || m.calendar.is_empty()
                || m.calendar.windows(2).any(|w| w[0] >= w[1])
                || !matches!(
                    m.price_basis.as_str(),
                    "unadjusted" | "split_adjusted_candidate"
                )
                || m.normalization != NORMALIZATION
            {
                return Err(invalid("unsupported canonical daily format"));
            }
            for pool in m.universes.values() {
                pool.validate().map_err(invalid)?;
            }
            for id in &m.instruments {
                validate_instrument(id).map_err(invalid)?;
                AdjustmentRecord::read(&root, id)?;
                if !root.join("bars").join(format!("{id}.parquet")).is_file() {
                    return Err(invalid(format!("missing instrument file {id}")));
                }
            }
            Ok(Self {
                root,
                source_map: source_map.clone(),
                read_range: None,
                field_cache: Default::default(),
                bar_cache: Default::default(),
                adjustment_cache: Default::default(),
                #[cfg(test)]
                physical_reads: Default::default(),
                calendar: m.calendar.clone(),
                kind: DailyKind::Parquet(m),
            })
        } else {
            let q = QlibDataset::open(&root)?;
            Ok(Self {
                root,
                source_map: source_map.clone(),
                read_range: None,
                field_cache: Default::default(),
                bar_cache: Default::default(),
                adjustment_cache: Default::default(),
                #[cfg(test)]
                physical_reads: Default::default(),
                calendar: q.calendar.clone(),
                kind: DailyKind::Qlib(q),
            })
        }
    }
    /// Whether the format provides verified raw execution prices.
    pub fn has_raw_prices(&self) -> bool {
        match &self.kind {
            DailyKind::Qlib(_) => true,
            DailyKind::Parquet(m) => m.price_basis == "unadjusted",
        }
    }
    fn bars(&self, id: &str) -> io::Result<Arc<Vec<DailyBar>>> {
        if let Some(bars) = self.bar_cache.borrow().get(id) {
            return Ok(bars.clone());
        }
        let bars = Arc::new(read_bars(
            &self.root.join("bars").join(format!("{id}.parquet")),
        )?);
        #[cfg(test)]
        self.physical_reads.set(self.physical_reads.get() + 1);
        self.bar_cache.borrow_mut().insert(id.into(), bars.clone());
        Ok(bars)
    }
    fn adjustment_record(&self, id: &str) -> io::Result<AdjustmentRecord> {
        if let Some(record) = self.adjustment_cache.borrow().get(id) {
            return Ok(record.clone());
        }
        let record = AdjustmentRecord::read(&self.root, id)?;
        #[cfg(test)]
        self.physical_reads.set(self.physical_reads.get() + 1);
        self.adjustment_cache
            .borrow_mut()
            .insert(id.into(), record.clone());
        Ok(record)
    }
    fn canonical_inputs(&self, id: &str) -> io::Result<CanonicalInputs<'_>> {
        self.validate_read_key(id)?;
        let DailyKind::Parquet(manifest) = &self.kind else {
            return Err(invalid("expected canonical daily data"));
        };
        let adjustment = self.adjustment_record(id)?;
        if let Some(e) = self.source_map.exceptions.get(id)
            && (e.kind == "price_index")
                != matches!(adjustment.basis, AdjustmentBasis::PriceIndex { .. })
        {
            return Err(invalid("asset type conflicts with coefficient basis"));
        }
        if let AdjustmentBasis::Unavailable { reason } = &adjustment.basis {
            return Err(invalid(reason));
        }
        let bars = self.bars(id)?;
        let first = bars
            .iter()
            .find(|b| b.close.is_some())
            .ok_or_else(|| invalid("first raw close missing for adjustment anchor"))?;
        let anchor = manifest
            .adjustment_anchors
            .get(id)
            .ok_or_else(|| invalid("adjustment anchor missing"))?;
        let expected = adjustment.anchor(first.trade_date)?;
        if anchor.trade_date != expected.trade_date || anchor.factor != expected.factor {
            return Err(invalid(
                "adjustment anchor does not match first raw close/source coefficient",
            ));
        }
        let base = coefficient(&anchor.factor)?;
        for date in self.calendar.iter().filter(|d| **d >= anchor.trade_date) {
            let scale = adjustment
                .on(*date)?
                .ok_or_else(|| invalid(format!("{id} {date}: adjustment coverage missing")))?
                / base;
            if !scale.is_finite() || scale <= 0. {
                return Err(invalid("invalid normalized adjustment coefficient"));
            }
        }
        for b in bars.iter() {
            if b.instrument_id != id {
                return Err(invalid("instrument identity mismatch"));
            }
            if self.calendar.binary_search(&b.trade_date).is_err() {
                return Err(invalid("observation outside trading calendar"));
            }
            let scale = adjustment.on(b.trade_date)?.ok_or_else(|| {
                invalid(format!(
                    "{id} {}: adjustment coverage missing",
                    b.trade_date
                ))
            })? / base;
            if [b.open, b.high, b.low, b.close]
                .iter()
                .flatten()
                .any(|p| !(p * scale).is_finite() || p * scale <= 0.)
                || b.volume.is_some_and(|v| !(v / scale).is_finite())
                || b.amount
                    .zip(b.volume)
                    .is_some_and(|(a, v)| v > 0. && !(a / v * scale).is_finite())
            {
                return Err(invalid(
                    "normalized research price/volume/VWAP cannot be represented",
                ));
            }
        }
        Ok(CanonicalInputs {
            bars,
            adjustment,
            anchor,
        })
    }
    /// Validate independent coefficient coverage, identity and the frozen first raw-close anchor.
    pub fn validate_factor_basis(&self, id: &str) -> io::Result<()> {
        if self.needs_binding(id) {
            for l in self.locators(id)? {
                self.unbound()?.validate_factor_basis(&l.symbol)?;
            }
            return Ok(());
        }
        self.validate_read_key(id)?;
        if let DailyKind::Qlib(q) = &self.kind {
            let values = self.qlib_field(q, id, "factor")?;
            if values
                .values
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    self.read_range.as_ref().is_none_or(|r| {
                        self.calendar
                            .get(values.start + *i)
                            .is_some_and(|d| r.contains(*d))
                    })
                })
                .filter_map(|(_, v)| *v)
                .any(|f| f <= 0.)
                || !values.values.iter().flatten().any(|f| *f > 0.)
            {
                return Err(invalid(
                    "Qlib adjustment coefficients must have a positive finite anchor",
                ));
            }
            return Ok(());
        }
        self.canonical_inputs(id).map(|_| ())
    }
    /// True when coefficients come from a separate dated record rather than float32 Qlib fields.
    pub fn has_independent_coefficients(&self) -> bool {
        matches!(self.kind, DailyKind::Parquet(_))
    }
    /// Prevent a real snapshot from being labelled with synthetic coefficient provenance,
    /// or a fixture from silently acquiring public coefficients.
    pub fn validate_coefficient_origin(&self, id: &str, synthetic: bool) -> io::Result<()> {
        if self.needs_binding(id) {
            for l in self.locators(id)? {
                self.unbound()?
                    .validate_coefficient_origin(&l.symbol, synthetic)?;
            }
            return Ok(());
        }
        self.validate_read_key(id)?;
        if matches!(self.kind, DailyKind::Qlib(_)) {
            return Ok(());
        }
        let record = self.adjustment_record(id)?;
        let is_synthetic = match &record.basis {
            AdjustmentBasis::Explicit { source, .. } => {
                *source == crate::adjustment::FactorSource::SyntheticFixture
            }
            AdjustmentBasis::PriceIndex { source } => {
                source == "JFQuant analytics-workflow synthetic-demo-v1"
            }
            AdjustmentBasis::Unavailable { reason } => return Err(invalid(reason)),
        };
        if is_synthetic != synthetic {
            return Err(invalid("synthetic/public coefficient provenance mismatch"));
        }
        Ok(())
    }
    /// Actual dated source transitions, including non-trading dates that must be explained.
    pub fn adjustment_change_dates(&self, id: &str) -> io::Result<Vec<Date>> {
        if self.needs_binding(id) {
            let mut dates = vec![];
            for l in self.locators(id)? {
                dates.extend(
                    self.unbound()?
                        .adjustment_change_dates(&l.symbol)?
                        .into_iter()
                        .filter(|d| l.coverage.start <= *d && *d <= l.coverage.end),
                );
            }
            dates.sort();
            dates.dedup();
            return Ok(dates);
        }
        self.validate_read_key(id)?;
        match &self.kind {
            DailyKind::Qlib(_) => Ok(Vec::new()),
            DailyKind::Parquet(_) => match self.adjustment_record(id)?.basis {
                AdjustmentBasis::Explicit { points, .. } => points
                    .windows(2)
                    .filter_map(|w| {
                        let a = coefficient(&w[0].factor);
                        let b = coefficient(&w[1].factor);
                        match (a, b) {
                            (Ok(a), Ok(b)) if (b / a - 1.).abs() > 1e-12 => {
                                Some(Ok(w[1].effective_date))
                            }
                            (Err(e), _) | (_, Err(e)) => Some(Err(e)),
                            _ => None,
                        }
                    })
                    .collect(),
                _ => Ok(Vec::new()),
            },
        }
    }
    /// Original independent coefficients for event verification, without deriving them from prices.
    pub fn adjustment_factors(&self, id: &str) -> io::Result<DailyField> {
        if self.needs_binding(id) {
            return self.bound_field(id, "__factors");
        }
        self.validate_read_key(id)?;
        match &self.kind {
            DailyKind::Qlib(q) => self.qlib_field(q, id, "factor"),
            DailyKind::Parquet(_) => {
                let record = self.adjustment_record(id)?;
                Ok(DailyField {
                    start: 0,
                    values: self
                        .calendar
                        .iter()
                        .map(|d| record.on(*d))
                        .collect::<io::Result<_>>()?,
                })
            }
        }
    }
    /// Actual traded shares; no research volume normalization enters execution.
    pub fn raw_volume(&self, id: &str) -> io::Result<DailyField> {
        if self.needs_binding(id) {
            return self.bound_field(id, "__raw_volume");
        }
        self.validate_read_key(id)?;
        match &self.kind {
            DailyKind::Qlib(q) => {
                let v = self.qlib_field(q, id, "volume")?;
                let f = self.qlib_field(q, id, "factor")?;
                Ok(DailyField {
                    start: 0,
                    values: (0..self.calendar.len())
                        .map(|i| v.get(i).zip(f.get(i)).map(|(v, f)| v * f * 100.))
                        .collect(),
                })
            }
            DailyKind::Parquet(_) => {
                let bars = self.bars(id)?;
                let mut values = vec![None; self.calendar.len()];
                for b in bars.iter() {
                    if b.instrument_id != id {
                        return Err(invalid("instrument identity mismatch"));
                    }
                    let i = self
                        .calendar
                        .binary_search(&b.trade_date)
                        .map_err(|_| invalid("date outside calendar"))?;
                    values[i] = b.volume;
                }
                Ok(DailyField { start: 0, values })
            }
        }
    }
    /// True only for a verified price-index basis; benchmark selection alone has no effect.
    pub fn is_price_index(&self, id: &str) -> io::Result<bool> {
        if self.needs_binding(id) {
            let ls = self.locators(id)?;
            let direct = self.unbound()?;
            return Ok(self.source_map.is_price_index(id)
                && ls
                    .iter()
                    .all(|l| direct.is_price_index(&l.symbol).unwrap_or(false)));
        }
        self.validate_read_key(id)?;
        match &self.kind {
            DailyKind::Qlib(_) => Ok(self.source_map.is_price_index(id)),
            DailyKind::Parquet(_) => Ok(matches!(
                self.adjustment_record(id)?.basis,
                AdjustmentBasis::PriceIndex { .. }
            )),
        }
    }
    /// Freeze coefficient provenance, anchors and bytes into model/cache/research signatures.
    pub fn price_inputs(&self, ids: &BTreeSet<String>) -> io::Result<serde_json::Value> {
        use sha2::{Digest, Sha256};
        let mut securities = BTreeMap::new();
        for id in ids {
            analytics_core::task::TaskContext::current().check()?;
            self.validate_read_key(id)?;
            if self.needs_binding(id) {
                let ls = self.locators(id)?;
                let source_ids = ls.iter().map(|l| l.symbol.clone()).collect();
                let source = self.unbound()?.price_inputs(&source_ids)?;
                let receipt = serde_json::json!({"source_mapping":self.source_map.exceptions[id],"bindings":ls,"source":source});
                let mut value = receipt.clone();
                value["sha256"] = serde_json::json!(format!(
                    "sha256:{:x}",
                    Sha256::digest(serde_json::to_vec(&receipt).map_err(invalid)?)
                ));
                securities.insert(id, value);
                continue;
            }
            let value = match &self.kind {
                DailyKind::Qlib(_) => {
                    let path = self
                        .root
                        .join("features")
                        .join(id.to_ascii_lowercase())
                        .join("factor.day.bin");
                    let factors = self.adjustment_factors(id)?;
                    let (offset, value) = factors
                        .values
                        .iter()
                        .enumerate()
                        .find_map(|(i, f)| f.filter(|f| *f > 0.).map(|f| (i, f)))
                        .ok_or_else(|| invalid("Qlib adjustment anchor missing"))?;
                    serde_json::json!({"basis":"qlib_stored_factor","anchor_mode":"first_stored_factor","anchor":{"trade_date":self.calendar[factors.start+offset],"factor":value.to_string()},"precision":"float32","sha256":format!("sha256:{:x}",Sha256::digest(fs::read(path)?))})
                }
                DailyKind::Parquet(m) => {
                    let record = self.adjustment_record(id)?;
                    let mut basis = serde_json::to_value(&record.basis).map_err(invalid)?;
                    if let Some(points) = basis.as_object_mut().and_then(|o| o.remove("points")) {
                        basis["point_count"] =
                            serde_json::json!(points.as_array().map_or(0, Vec::len));
                        basis["history_start"] = points[0]["effective_date"].clone();
                    }
                    serde_json::json!({"basis":basis,"source_symbol":record.source_symbol,"anchor":m.adjustment_anchors.get(id),"sha256":format!("sha256:{:x}",Sha256::digest(fs::read(self.root.join("adjustments").join(format!("{id}.json")))?))})
                }
            };
            securities.insert(id, value);
        }
        Ok(
            serde_json::json!({"normalization":match self.kind {DailyKind::Qlib(_)=>"qlib-first-stored-factor-v2",DailyKind::Parquet(_)=>NORMALIZATION},"securities":securities}),
        )
    }
    /// Immutable historical membership and explicit coverage.
    pub fn pool(&self, universe: &str) -> io::Result<PoolMembership> {
        match &self.kind {
            DailyKind::Parquet(m) => m
                .universes
                .get(universe)
                .cloned()
                .ok_or_else(|| invalid("unknown universe")),
            DailyKind::Qlib(q) => {
                let members = q.membership(universe)?;
                let first = members
                    .values()
                    .flatten()
                    .map(|w| w.start)
                    .min()
                    .ok_or_else(|| invalid("empty universe source"))?;
                let last = members.values().flatten().map(|w| w.end).max().unwrap();
                Ok(PoolMembership {
                    coverage: DateRange {
                        start: self.calendar[first],
                        end: self.calendar[last],
                    },
                    intervals: members
                        .into_iter()
                        .map(|(id, rs)| {
                            (
                                id,
                                rs.into_iter()
                                    .map(|r| DateRange {
                                        start: self.calendar[r.start],
                                        end: self.calendar[r.end],
                                    })
                                    .collect(),
                            )
                        })
                        .collect(),
                })
            }
        }
    }
    /// Calendar-index intervals for a validated effective pool.
    pub fn align_membership(&self, pool: &PoolMembership) -> BTreeMap<String, Vec<Membership>> {
        pool.intervals
            .iter()
            .map(|(id, rs)| {
                (
                    id.clone(),
                    rs.iter()
                        .filter_map(|r| {
                            let lo = self.calendar.partition_point(|d| *d < r.start);
                            let hi = self.calendar.partition_point(|d| *d <= r.end);
                            (lo < hi).then_some(Membership {
                                start: lo,
                                end: hi.saturating_sub(1),
                            })
                        })
                        .collect(),
                )
            })
            .collect()
    }
    /// Read source membership as effective calendar intervals.
    pub fn membership(&self, universe: &str) -> io::Result<BTreeMap<String, Vec<Membership>>> {
        Ok(self.align_membership(&self.pool(universe)?))
    }
    /// Normalized factor fields with the same rebase/price-volume contract for both formats.
    /// Read a research input, allowing only a genuinely absent file to become null.
    /// InvalidData, permission failures and malformed evidence are never softened.
    pub fn research_field(
        &self,
        id: &str,
        name: &str,
        allow_missing: bool,
    ) -> io::Result<DailyField> {
        if allow_missing && self.needs_binding(id) {
            return self.bound_field_policy(id, name, true);
        }
        self.nullable_absence(self.field(id, name), allow_missing)
    }
    fn nullable_absence(
        &self,
        result: io::Result<DailyField>,
        allow_missing: bool,
    ) -> io::Result<DailyField> {
        match result {
            Err(e) if allow_missing && e.kind() == io::ErrorKind::NotFound => Ok(DailyField {
                start: 0,
                values: vec![None; self.calendar.len()],
            }),
            r => r,
        }
    }
    /// Verified raw research prices with absence restricted to the affected source binding.
    pub fn research_raw_field(
        &self,
        id: &str,
        name: &str,
        allow_missing: bool,
    ) -> io::Result<DailyField> {
        if allow_missing && self.needs_binding(id) {
            return self.bound_field_policy(id, &format!("__raw_{name}"), true);
        }
        self.nullable_absence(self.raw_field(id, name), allow_missing)
    }
    /// Real-share volume with dated missing bindings; never used to invent trade availability.
    pub fn research_raw_volume(&self, id: &str, allow_missing: bool) -> io::Result<DailyField> {
        if allow_missing && self.needs_binding(id) {
            return self.bound_field_policy(id, "__raw_volume", true);
        }
        self.nullable_absence(self.raw_volume(id), allow_missing)
    }
    /// Normalized factor fields with the same rebase/price-volume contract for both formats.
    pub fn field(&self, id: &str, name: &str) -> io::Result<DailyField> {
        if self.needs_binding(id) {
            return self.bound_field(id, name);
        }
        self.validate_read_key(id)?;
        if let DailyKind::Qlib(q) = &self.kind {
            let field = self.qlib_field(q, id, name)?;
            if field
                .values
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    self.read_range.as_ref().is_none_or(|r| {
                        self.calendar
                            .get(field.start + *i)
                            .is_some_and(|d| r.contains(*d))
                    })
                })
                .filter_map(|(_, v)| v.as_ref())
                .any(|value| match name {
                    "open" | "high" | "low" | "close" | "vwap" | "factor" => *value <= 0.,
                    "volume" | "amount" => *value < 0.,
                    _ => false,
                })
            {
                return Err(invalid(format!(
                    "{id} {name}: invalid nonpositive price/coefficient or negative volume/amount"
                )));
            }
            return Ok(field);
        }
        let inputs = self.canonical_inputs(id)?;
        let base = coefficient(&inputs.anchor.factor)?;
        let mut values = vec![None; self.calendar.len()];
        if name == "factor" {
            for (i, date) in self.calendar.iter().enumerate() {
                if *date >= inputs.anchor.trade_date {
                    values[i] = inputs.adjustment.on(*date)?.map(|f| f / base);
                }
            }
        } else {
            for b in inputs.bars.iter() {
                if b.trade_date < inputs.anchor.trade_date {
                    continue;
                }
                let day = self
                    .calendar
                    .binary_search(&b.trade_date)
                    .map_err(|_| invalid("date outside calendar"))?;
                let scale = inputs
                    .adjustment
                    .on(b.trade_date)?
                    .ok_or_else(|| invalid("adjustment coverage missing"))?
                    / base;
                let value = match name {
                    "open" => b.open.map(|p| p * scale),
                    "high" => b.high.map(|p| p * scale),
                    "low" => b.low.map(|p| p * scale),
                    "close" => b.close.map(|p| p * scale),
                    "volume" => b.volume.map(|v| v / scale / 100.),
                    "amount" => b.amount.map(|a| a / 1000.),
                    "vwap" => b
                        .amount
                        .zip(b.volume)
                        .filter(|(_, v)| *v > 0.)
                        .map(|(a, v)| a / v * scale),
                    _ => return Err(invalid("unknown daily field")),
                };
                values[day] = value.filter(|v| v.is_finite());
            }
        }
        Ok(DailyField { start: 0, values })
    }
    /// Verified unadjusted field for the execution ledger.
    pub fn raw_field(&self, id: &str, name: &str) -> io::Result<DailyField> {
        if self.needs_binding(id) {
            return self.bound_field(id, &format!("__raw_{name}"));
        }
        self.validate_read_key(id)?;
        match &self.kind {
            DailyKind::Qlib(q) => {
                let p = self.qlib_field(q, id, name)?;
                let f = self.qlib_field(q, id, "factor")?;
                Ok(DailyField {
                    start: 0,
                    values: (0..self.calendar.len())
                        .map(|i| raw_price(p.get(i), f.get(i)))
                        .collect(),
                })
            }
            DailyKind::Parquet(_) => {
                if !self.has_raw_prices() {
                    return Err(invalid("unverified raw execution price basis"));
                }
                let bars = self.bars(id)?;
                let mut values = vec![None; self.calendar.len()];
                for b in bars.iter() {
                    if b.instrument_id != id {
                        return Err(invalid("instrument identity mismatch"));
                    }
                    let i = self
                        .calendar
                        .binary_search(&b.trade_date)
                        .map_err(|_| invalid("date outside calendar"))?;
                    values[i] = match name {
                        "open" => b.open,
                        "high" => b.high,
                        "low" => b.low,
                        "close" => b.close,
                        _ => return Err(invalid("unknown raw field")),
                    };
                }
                Ok(DailyField { start: 0, values })
            }
        }
    }
}

/// Persist one validated security shard using nullable Arrow fields.
pub fn write_bars(path: &Path, bars: &[DailyBar]) -> io::Result<()> {
    validate_bars(bars)?;
    let names = [
        "open",
        "high",
        "low",
        "close",
        "adj_open",
        "adj_high",
        "adj_low",
        "adj_close",
        "volume",
        "amount",
    ];
    let mut fields = vec![
        Field::new("instrument_id", DataType::Utf8, false),
        Field::new("trade_date", DataType::Date32, false),
    ];
    fields.extend(names.map(|n| Field::new(n, DataType::Float64, true)));
    fields.push(Field::new("is_trading", DataType::Boolean, true));
    fields.push(Field::new("listing_date", DataType::Date32, true));
    let schema = Arc::new(Schema::new(fields));
    let epoch: Date = "1970-01-01".parse().unwrap();
    let mut arrays: Vec<ArrayRef> = vec![
        Arc::new(StringArray::from(
            bars.iter()
                .map(|b| b.instrument_id.as_str())
                .collect::<Vec<_>>(),
        )),
        Arc::new(Date32Array::from(
            bars.iter()
                .map(|b| (b.trade_date - epoch).num_days() as i32)
                .collect::<Vec<_>>(),
        )),
    ];
    for n in 0..10 {
        arrays.push(Arc::new(Float64Array::from(
            bars.iter()
                .map(|b| {
                    [
                        b.open,
                        b.high,
                        b.low,
                        b.close,
                        b.adj_open,
                        b.adj_high,
                        b.adj_low,
                        b.adj_close,
                        b.volume,
                        b.amount,
                    ][n]
                })
                .collect::<Vec<_>>(),
        )));
    }
    arrays.push(Arc::new(BooleanArray::from(
        bars.iter()
            .map(|b| b.volume.map(|v| v > 0.0))
            .collect::<Vec<_>>(),
    )));
    arrays.push(Arc::new(Date32Array::from(vec![None; bars.len()])));
    let batch = RecordBatch::try_new(schema.clone(), arrays).map_err(invalid)?;
    fs::create_dir_all(path.parent().ok_or_else(|| invalid("missing parent"))?)?;
    let mut w = ArrowWriter::try_new(fs::File::create(path)?, schema, None).map_err(invalid)?;
    w.write(&batch).map_err(invalid)?;
    w.close().map_err(invalid)?;
    fs::File::open(path)?.sync_all()?;
    Ok(())
}
/// Read and validate canonical schema, identities, date order, nullable prices and units.
pub fn read_bars(path: &Path) -> io::Result<Vec<DailyBar>> {
    let reader = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(path)?)
        .map_err(invalid)?
        .build()
        .map_err(invalid)?;
    let mut out: Vec<DailyBar> = vec![];
    for batch in reader {
        let b = batch.map_err(invalid)?;
        let ids = b
            .column_by_name("instrument_id")
            .and_then(|a| a.as_any().downcast_ref::<StringArray>())
            .ok_or_else(|| invalid("invalid identifier column"))?;
        let dates = b
            .column_by_name("trade_date")
            .and_then(|a| a.as_any().downcast_ref::<Date32Array>())
            .ok_or_else(|| invalid("invalid date column"))?;
        let fields = [
            "open",
            "high",
            "low",
            "close",
            "adj_open",
            "adj_high",
            "adj_low",
            "adj_close",
            "volume",
            "amount",
        ]
        .map(|n| {
            b.column_by_name(n)
                .and_then(|a| a.as_any().downcast_ref::<Float64Array>())
                .ok_or_else(|| invalid(format!("missing/invalid {n}")))
        });
        let fields = fields.into_iter().collect::<io::Result<Vec<_>>>()?;
        for i in 0..b.num_rows() {
            if ids.is_null(i) || dates.is_null(i) {
                return Err(invalid("null identity/date"));
            }
            let id = ids.value(i);
            validate_instrument(id).map_err(invalid)?;
            let date: Date = "1970-01-01".parse().unwrap();
            let date = date
                .checked_add_signed(chrono::Duration::days(dates.value(i).into()))
                .ok_or_else(|| invalid("daily date outside supported calendar range"))?;
            if out
                .last()
                .is_some_and(|r| r.instrument_id != id || r.trade_date >= date)
            {
                return Err(invalid(
                    "mixed identity, duplicate or unordered daily dates",
                ));
            }
            let mut v = [None; 10];
            for (j, f) in fields.iter().enumerate() {
                if !f.is_null(i) {
                    let n = f.value(i);
                    if !n.is_finite() || (j < 4 && n <= 0.0) || (j >= 8 && n < 0.0) {
                        return Err(invalid("invalid price/volume/amount"));
                    }
                    v[j] = Some(n);
                }
            }
            out.push(DailyBar {
                instrument_id: id.into(),
                trade_date: date,
                open: v[0],
                high: v[1],
                low: v[2],
                close: v[3],
                adj_open: v[4],
                adj_high: v[5],
                adj_low: v[6],
                adj_close: v[7],
                volume: v[8],
                amount: v[9],
            });
        }
    }
    validate_bars(&out)?;
    Ok(out)
}

fn validate_bars(bars: &[DailyBar]) -> io::Result<()> {
    let first = bars.first().ok_or_else(|| invalid("empty daily shard"))?;
    validate_instrument(&first.instrument_id).map_err(invalid)?;
    for (i, b) in bars.iter().enumerate() {
        if b.instrument_id != first.instrument_id
            || (i > 0 && bars[i - 1].trade_date >= b.trade_date)
        {
            return Err(invalid(
                "mixed identity, duplicate or unordered daily dates",
            ));
        }
        for (i, v) in [
            b.open,
            b.high,
            b.low,
            b.close,
            b.adj_open,
            b.adj_high,
            b.adj_low,
            b.adj_close,
            b.volume,
            b.amount,
        ]
        .iter()
        .enumerate()
        {
            if v.is_some_and(|v| !v.is_finite() || (i < 4 && v <= 0.0) || (i >= 8 && v < 0.0)) {
                return Err(invalid("invalid price/volume/amount"));
            }
        }
        {
            let [open, high, low, close] = [b.open, b.high, b.low, b.close];
            if low.zip(high).is_some_and(|(l, h)| l > h)
                || [open, close]
                    .iter()
                    .flatten()
                    .any(|p| low.is_some_and(|l| *p < l) || high.is_some_and(|h| *p > h))
            {
                return Err(invalid("inconsistent OHLC range"));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adjustment::{AdjustmentPoint, FactorSource};
    fn bar() -> DailyBar {
        DailyBar {
            instrument_id: "SH600000".into(),
            trade_date: "2020-01-02".parse().unwrap(),
            open: Some(10.),
            high: Some(11.),
            low: Some(9.),
            close: Some(10.),
            adj_open: Some(25.),
            adj_high: Some(27.),
            adj_low: Some(23.),
            adj_close: Some(25.),
            volume: Some(1000.),
            amount: Some(10000.),
        }
    }
    fn record(points: &[(&str, &str)], end: Date) -> AdjustmentRecord {
        AdjustmentRecord {
            schema_version: 3,
            instrument_id: "SH600000".into(),
            source_symbol: "SH600000".into(),
            basis: AdjustmentBasis::Explicit {
                source: FactorSource::SyntheticFixture,
                source_url: "JFQuant analytics-workflow synthetic-demo-v1".into(),
                verified_through: end,
                points: points
                    .iter()
                    .map(|(d, f)| AdjustmentPoint {
                        effective_date: d.parse().unwrap(),
                        factor: (*f).into(),
                    })
                    .collect(),
            },
        }
    }
    fn dataset(root: &Path, bars: &[DailyBar], record: &AdjustmentRecord) -> DailyDataset {
        write_bars(&root.join("bars/SH600000.parquet"), bars).unwrap();
        fs::create_dir_all(root.join("adjustments")).unwrap();
        fs::write(
            root.join("adjustments/SH600000.json"),
            serde_json::to_vec(record).unwrap(),
        )
        .unwrap();
        let anchors = record
            .anchor(bars.iter().find(|b| b.close.is_some()).unwrap().trade_date)
            .ok()
            .map(|a| BTreeMap::from([("SH600000".into(), a)]))
            .unwrap_or_default();
        let m = DailyManifest {
            schema_version: 5,
            calendar: bars.iter().map(|b| b.trade_date).collect(),
            universes: BTreeMap::new(),
            instruments: BTreeSet::from(["SH600000".into()]),
            price_basis: "unadjusted".into(),
            normalization: NORMALIZATION.into(),
            adjustment_anchors: anchors,
        };
        fs::write(
            root.join("daily_manifest.json"),
            serde_json::to_vec(&m).unwrap(),
        )
        .unwrap();
        DailyDataset::open(root).unwrap()
    }
    #[test]
    fn canonical_fields_are_read_once_per_security_independent_of_days() {
        let tmp = tempfile::tempdir().unwrap();
        let mut bars = vec![bar(); 30];
        for (i, b) in bars.iter_mut().enumerate() {
            b.trade_date = "2020-01-02".parse::<Date>().unwrap() + chrono::Duration::days(i as i64);
        }
        let data = dataset(
            tmp.path(),
            &bars,
            &record(&[("1900-01-01", "2")], bars.last().unwrap().trade_date),
        );
        for _ in 0..3 {
            for name in [
                "open", "high", "low", "close", "volume", "amount", "vwap", "factor",
            ] {
                assert_eq!(data.field("SH600000", name).unwrap().values.len(), 30);
            }
            data.raw_field("SH600000", "close").unwrap();
            data.raw_volume("SH600000").unwrap();
        }
        assert_eq!(
            data.physical_reads.get(),
            2,
            "one coefficient record and one bar file"
        );
    }
    #[test]
    fn malformed_raw_prices_and_empty_shards_fail_before_writing_but_nulls_remain() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("bad.parquet");
        assert!(write_bars(&path, &[]).is_err());
        let mut b = bar();
        b.high = Some(9.5);
        assert!(
            write_bars(&path, &[b])
                .unwrap_err()
                .to_string()
                .contains("OHLC")
        );
        let mut b = bar();
        b.volume = Some(f64::INFINITY);
        assert!(write_bars(&path, &[b]).is_err());
        assert!(!path.exists());
        let mut b = bar();
        b.open = None;
        b.volume = None;
        write_bars(&path, &[b]).unwrap();
        let loaded = read_bars(&path).unwrap();
        assert!(loaded[0].open.is_none() && loaded[0].volume.is_none());
    }
    #[test]
    fn provider_adjusted_quotes_do_not_drive_scaling_and_raw_identity_is_checked() {
        let d = tempfile::tempdir().unwrap();
        let b = bar();
        let data = dataset(
            d.path(),
            std::slice::from_ref(&b),
            &record(&[("1900-01-01", "2")], b.trade_date),
        );
        data.validate_factor_basis("SH600000").unwrap();
        assert_eq!(
            data.raw_field("SH600000", "close").unwrap().get(0),
            Some(10.)
        );
        assert_eq!(data.field("SH600000", "close").unwrap().get(0), Some(10.));
        assert_eq!(data.field("SH600000", "volume").unwrap().get(0), Some(10.));
        assert_eq!(data.raw_volume("SH600000").unwrap().get(0), Some(1000.));
        assert_eq!(data.field("SH600000", "vwap").unwrap().get(0), Some(10.));
        let mut b = b;
        b.adj_close = Some(-5.);
        b.adj_low = Some(-6.);
        write_bars(
            &d.path().join("bars/SH600000.parquet"),
            std::slice::from_ref(&b),
        )
        .unwrap();
        assert_eq!(data.field("SH600000", "close").unwrap().get(0), Some(10.));
        b.instrument_id = "SZ000001".into();
        write_bars(&d.path().join("bars/SH600000.parquet"), &[b]).unwrap();
        let data = DailyDataset::open(d.path()).unwrap();
        assert!(data.raw_field("SH600000", "close").is_err());
        assert!(data.raw_volume("SH600000").is_err());
    }
    #[test]
    fn dividends_splits_preserve_research_price_and_amount_while_actual_volume_stays_raw() {
        let mut bars = vec![bar(); 3];
        for (b, (day, price, volume)) in bars.iter_mut().zip([
            ("2020-01-02", 10., 1000.),
            ("2020-01-03", 9., 1000.),
            ("2020-01-06", 4.5, 2000.),
        ]) {
            b.trade_date = day.parse().unwrap();
            b.open = Some(price);
            b.high = Some(price);
            b.low = Some(price);
            b.close = Some(price);
            b.volume = Some(volume);
            b.amount = Some(price * volume);
        }
        let r = record(
            &[
                ("1900-01-01", "2"),
                ("2020-01-03", "2.2222222222222222"),
                ("2020-01-06", "4.4444444444444444"),
            ],
            bars[2].trade_date,
        );
        let d = tempfile::tempdir().unwrap();
        let data = dataset(d.path(), &bars, &r);
        for (i, bar) in bars.iter().enumerate() {
            let p = data.field("SH600000", "close").unwrap().get(i).unwrap();
            let v = data.field("SH600000", "volume").unwrap().get(i).unwrap() * 100.;
            let vw = data.field("SH600000", "vwap").unwrap().get(i).unwrap();
            let a = data.field("SH600000", "amount").unwrap().get(i).unwrap() * 1000.;
            assert!((p - 10.).abs() < 1e-12);
            assert!((vw * v - a).abs() < 1e-8);
            assert_eq!(data.raw_volume("SH600000").unwrap().get(i), bar.volume);
        }
        assert!(
            (data.field("SH600000", "volume").unwrap().get(2).unwrap() * 100. - 900.).abs() < 1e-8
        );
        let mut zero = bars[2].clone();
        zero.volume = Some(0.);
        zero.amount = Some(0.);
        bars[2] = zero;
        write_bars(&d.path().join("bars/SH600000.parquet"), &bars).unwrap();
        let data = DailyDataset::open(d.path()).unwrap();
        assert_eq!(data.field("SH600000", "vwap").unwrap().get(2), None);
        assert_eq!(data.field("SH600000", "volume").unwrap().get(2), Some(0.));
    }
    #[test]
    fn future_points_and_global_rescaling_do_not_change_past_inputs_but_change_signatures() {
        let d = tempfile::tempdir().unwrap();
        let b = bar();
        let r = record(&[("1900-01-01", "2")], b.trade_date);
        let data = dataset(d.path(), std::slice::from_ref(&b), &r);
        let ids = BTreeSet::from(["SH600000".into()]);
        let before = data.price_inputs(&ids).unwrap();
        let close = data.field("SH600000", "close").unwrap().get(0);
        let volume = data.field("SH600000", "volume").unwrap().get(0);
        let changed = record(&[("1900-01-01", "14"), ("2021-01-04", "200")], b.trade_date);
        let data = dataset(d.path(), &[b], &changed);
        assert_eq!(data.field("SH600000", "close").unwrap().get(0), close);
        assert_eq!(data.field("SH600000", "volume").unwrap().get(0), volume);
        assert_ne!(data.price_inputs(&ids).unwrap(), before);
    }
    #[test]
    fn missing_initial_coefficient_and_old_formats_fail_without_migration() {
        let d = tempfile::tempdir().unwrap();
        let b = bar();
        let r = record(&[("2020-01-03", "2")], "2020-01-06".parse().unwrap());
        let data = dataset(d.path(), &[b], &r);
        assert!(data.validate_factor_basis("SH600000").is_err());
        let path = d.path().join("daily_manifest.json");
        let old = br#"{"schema_version":1}"#;
        fs::write(&path, old).unwrap();
        assert!(
            DailyDataset::open(d.path())
                .err()
                .unwrap()
                .to_string()
                .contains("unsupported_data_format")
        );
        assert_eq!(fs::read(path).unwrap(), old);
    }
    #[test]
    fn extreme_date32_returns_a_data_error_without_panicking() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("valid.parquet");
        write_bars(&path, &[bar()]).unwrap();
        let batch = ParquetRecordBatchReaderBuilder::try_new(fs::File::open(&path).unwrap())
            .unwrap()
            .build()
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let mut columns = batch.columns().to_vec();
        columns[1] = Arc::new(Date32Array::from(vec![i32::MAX]));
        let bad = RecordBatch::try_new(batch.schema(), columns).unwrap();
        let mut writer =
            ArrowWriter::try_new(fs::File::create(&path).unwrap(), bad.schema(), None).unwrap();
        writer.write(&bad).unwrap();
        writer.close().unwrap();
        assert!(
            read_bars(&path)
                .unwrap_err()
                .to_string()
                .contains("outside supported calendar")
        );
    }
}
