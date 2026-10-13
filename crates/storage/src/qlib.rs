//! Reader for independently downloaded Qlib little-endian float32 daily files.
//! This module does not import Qlib or access another project's caches.
use analytics_core::Date;
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

/// Historical effective membership interval (inclusive calendar indices).
#[derive(Debug, Clone)]
pub struct Membership {
    /// First index where membership is effective.
    pub start: usize,
    /// Last index where membership is effective.
    pub end: usize,
}

/// Read-only daily dataset with an explicit caller-provided root.
pub struct QlibDataset {
    /// Exact provider root under the JFQuant download directory.
    pub root: PathBuf,
    /// Actual trading dates; future calendar is never used.
    pub calendar: Vec<Date>,
}

/// One field's calendar-aligned observations; non-finite values remain missing.
pub struct DailyField {
    /// First calendar index stored in the file.
    pub start: usize,
    /// Nullable observations.
    pub values: Vec<Option<f64>>,
}
impl DailyField {
    /// Return the value on a global trading-calendar index.
    pub fn get(&self, index: usize) -> Option<f64> {
        index
            .checked_sub(self.start)
            .and_then(|i| self.values.get(i))
            .copied()
            .flatten()
    }
}
fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
impl QlibDataset {
    /// Open the explicit provider root and validate its chronological daily calendar.
    pub fn open(root: impl AsRef<Path>) -> io::Result<Self> {
        let root = root.as_ref().canonicalize()?;
        let calendar: Vec<Date> = fs::read_to_string(root.join("calendars/day.txt"))?
            .lines()
            .map(|line| {
                line.parse()
                    .map_err(|error: chrono::ParseError| invalid(error.to_string()))
            })
            .collect::<io::Result<_>>()?;
        if calendar.is_empty() || calendar.windows(2).any(|w| w[0] >= w[1]) {
            return Err(invalid("invalid trading calendar"));
        }
        Ok(Self { root, calendar })
    }
    /// Decode historical membership windows without substituting today's constituents.
    pub fn membership(&self, market: &str) -> io::Result<BTreeMap<String, Vec<Membership>>> {
        safe_name(market)?;
        let mut result = BTreeMap::<String, Vec<Membership>>::new();
        for line in
            fs::read_to_string(self.root.join("instruments").join(format!("{market}.txt")))?.lines()
        {
            let parts: Vec<_> = line.split_whitespace().collect();
            if parts.len() != 3 {
                return Err(invalid("malformed membership"));
            }
            safe_name(parts[0])?;
            let start: Date = parts[1]
                .parse()
                .map_err(|e: chrono::ParseError| invalid(e.to_string()))?;
            let end: Date = parts[2]
                .parse()
                .map_err(|e: chrono::ParseError| invalid(e.to_string()))?;
            if start > end {
                return Err(invalid("reversed membership interval"));
            }
            let lo = self.calendar.partition_point(|d| *d < start);
            let hi = self.calendar.partition_point(|d| *d <= end);
            if lo < hi {
                result
                    .entry(parts[0].to_string())
                    .or_default()
                    .push(Membership {
                        start: lo,
                        end: hi - 1,
                    });
            }
        }
        Ok(result)
    }
    /// Decode a daily float32 field; rejects fractional/negative offsets and truncation.
    pub fn field(&self, instrument: &str, name: &str) -> io::Result<DailyField> {
        safe_name(instrument)?;
        safe_name(name)?;
        let bytes = fs::read(
            self.root
                .join("features")
                .join(instrument.to_ascii_lowercase())
                .join(format!("{name}.day.bin")),
        )?;
        if bytes.len() < 4 || bytes.len() % 4 != 0 {
            return Err(invalid("truncated daily field"));
        }
        let start = f32::from_le_bytes(bytes[..4].try_into().unwrap());
        if !start.is_finite()
            || start < 0.0
            || start.fract() != 0.0
            || start as usize >= self.calendar.len()
        {
            return Err(invalid("invalid field calendar offset"));
        }
        let values = bytes[4..]
            .as_chunks::<4>()
            .0
            .iter()
            .map(|v| {
                let v = f32::from_le_bytes(*v) as f64;
                v.is_finite().then_some(v)
            })
            .collect::<Vec<_>>();
        if start as usize + values.len() > self.calendar.len() {
            return Err(invalid("field exceeds actual calendar"));
        }
        Ok(DailyField {
            start: start as usize,
            values,
        })
    }
}
fn safe_name(value: &str) -> io::Result<()> {
    if value.is_empty() || !value.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(invalid("invalid dataset identifier"));
    }
    Ok(())
}
/// Restore an unadjusted price from this release's normalized price and factor.
/// Both were divided by the same initial adjusted close upstream. The division
/// cancels that scale; float32 storage is rounded to the raw A-share cent tick.
pub fn raw_price(normalized: Option<f64>, factor: Option<f64>) -> Option<f64> {
    normalized.zip(factor).and_then(|(p, f)| {
        if !p.is_finite() || !f.is_finite() || p <= 0.0 || f <= 0.0 {
            return None;
        }
        let raw = p / f;
        (raw.is_finite() && raw >= 0.005).then(|| (raw * 100.0).round() / 100.0)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unadjusted_price_cancels_normalization_scale_and_rounds_float32_tick_noise() {
        let price = raw_price(Some(1.0), Some(0.1_f32 as f64));
        assert_eq!(price, Some(10.0));
        assert_eq!(raw_price(Some(2.0), Some(0.2_f32 as f64)), price);
        for (p, f) in [
            (None, Some(1.0)),
            (Some(1.0), Some(0.0)),
            (Some(f64::INFINITY), Some(1.0)),
            (Some(1.0), Some(-1.0)),
        ] {
            assert_eq!(raw_price(p, f), None);
        }
    }
    #[test]
    fn binary_offset_missing_values_and_membership_are_calendar_aligned() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("calendars")).unwrap();
        fs::create_dir_all(dir.path().join("instruments")).unwrap();
        fs::create_dir_all(dir.path().join("features/sh600000")).unwrap();
        fs::write(
            dir.path().join("calendars/day.txt"),
            "2020-01-02\n2020-01-03\n2020-01-06\n",
        )
        .unwrap();
        fs::write(
            dir.path().join("instruments/csi500.txt"),
            "SH600000\t2020-01-03\t2020-01-06\n",
        )
        .unwrap();
        let p = dir.path().join("features/sh600000/open.day.bin");
        fs::write(
            &p,
            [1.0_f32, 10.0, f32::NAN]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let data = QlibDataset::open(dir.path()).unwrap();
        let field = data.field("SH600000", "open").unwrap();
        assert_eq!(field.get(0), None);
        assert_eq!(field.get(1), Some(10.0));
        assert_eq!(field.get(2), None);
        assert_eq!(data.membership("csi500").unwrap()["SH600000"][0].start, 1);
        fs::write(
            &p,
            [-1.0_f32, 2.0]
                .into_iter()
                .flat_map(f32::to_le_bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(data.field("SH600000", "open").is_err());
        assert!(data.field("../bad", "open").is_err());
    }
}
