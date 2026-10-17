//! Shared worker-local index of authenticated suspension facts.
use crate::corporate_actions::{ActionBundle, review::SuspensionRevision};
use analytics_core::Date;
use std::{collections::BTreeMap, io};
/// Sources are kept distinct; only announcements assert a full session without trading.
#[derive(Default)]
pub struct StatusIndex {
    native: BTreeMap<String, BTreeMap<Date, String>>,
    announcements: BTreeMap<String, Vec<SuspensionRevision>>,
}
impl StatusIndex {
    /// Build once from a bundle already authenticated with ActionBundle::read.
    pub fn build(book: &ActionBundle) -> io::Result<Self> {
        let mut result = Self::default();
        if let Some(data) = &book.execution_data {
            for (key, v) in &data.capabilities {
                if let Some(id) = key.strip_prefix(&format!("{}:", crate::tdx_status::RULE))
                    && v["status"] == "available"
                    && let Some(days) = data.suspensions.get(id)
                {
                    result.native.insert(id.into(), days.clone());
                }
                if key.starts_with("announcement-suspension-v1:") {
                    let s: SuspensionRevision =
                        serde_json::from_value(v.clone()).map_err(io::Error::other)?;
                    result
                        .announcements
                        .entry(s.instrument.clone())
                        .or_default()
                        .push(s);
                }
            }
        }
        for (id, intervals) in &result.announcements {
            for a in intervals {
                if intervals
                    .iter()
                    .any(|b| b.start <= a.resume && a.resume < b.resume)
                {
                    return Err(io::Error::other(format!(
                        "suspension_boundary_conflict:{id}:{}",
                        a.resume
                    )));
                }
            }
        }
        Ok(result)
    }
    /// Reject positive traded volume against a verified full-session suspension.
    pub fn check_quote(&self, id: &str, date: Date, volume: Option<f64>) -> io::Result<()> {
        if self.full_day(id, date) && volume.is_some_and(|v| v > 0.) {
            return Err(io::Error::other(format!(
                "suspension_quote_conflict:{id}:{date}"
            )));
        }
        Ok(())
    }
    /// At least one valid source confirms a suspension interval for this session.
    pub fn suspended(&self, id: &str, date: Date) -> bool {
        self.full_day(id, date)
            || self
                .native
                .get(id)
                .and_then(|v| v.get(&date))
                .is_some_and(|v| v == "suspension_day")
    }
    /// An explicit announcement states that no part of the session trades.
    pub fn full_day(&self, id: &str, date: Date) -> bool {
        self.announcements
            .get(id)
            .is_some_and(|v| v.iter().any(|s| s.start <= date && date < s.resume))
    }
    /// Original source labels, including the exact reviewed interval identifiers.
    pub fn evidence(&self, id: &str, date: Date) -> Vec<String> {
        let mut out = vec![];
        if self.native.get(id).is_some_and(|v| v.contains_key(&date)) {
            out.push(crate::tdx_status::RULE.into());
        }
        for s in self
            .announcements
            .get(id)
            .into_iter()
            .flatten()
            .filter(|s| s.start <= date && date < s.resume)
        {
            out.push(format!(
                "announcement:{}:{}",
                s.start_evidence.document_id, s.resume_evidence.document_id
            ));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_session_proof_blocks_trading_but_not_zero_volume_valuation() {
        let s: SuspensionRevision = serde_json::from_value(serde_json::json!({
            "instrument":"SH600068","start":"2021-08-11","resume":"2021-08-12",
            "reason":"fixture","start_evidence":{"document_id":"start","quote":"fixture","unit":"session"},
            "resume_evidence":{"document_id":"resume","quote":"fixture","unit":"session"}
        })).unwrap();
        let index = StatusIndex {
            native: Default::default(),
            announcements: BTreeMap::from([("SH600068".into(), vec![s])]),
        };
        let day = "2021-08-11".parse().unwrap();
        assert!(index.check_quote("SH600068", day, Some(1.)).is_err());
        assert!(index.check_quote("SH600068", day, Some(0.)).is_ok());
        assert!(index.check_quote("SH600068", day, None).is_ok());
        assert!(
            index
                .check_quote("SH600068", "2021-08-12".parse().unwrap(), Some(100.))
                .is_ok()
        );
    }
}
