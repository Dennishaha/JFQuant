//! Workspace data admission and artifact interpretation.
use super::*;
impl Workspace {
    /// Register independently acquired market data or a saved action bundle within this workspace.
    pub fn register(&self, path: &Path) -> Result<DataVersion> {
        let relative = self.relative(path)?;
        let (kind, details, start, end) = if path.is_dir() {
            let dataset = DailyDataset::open(path)?;
            let mut source: Value = serde_json::from_slice(&fs::read(
                path.parent()
                    .ok_or("market parent missing")?
                    .join("jfquant_source.json"),
            )?)?;
            if !(source["generator"] == "JFQuant analytics-workflow synthetic-demo-v1"
                && source["synthetic_fixture"] == true)
                && (source["downloader"] != "JFQuant analytics-storage"
                    || (source["archive_verified"] != true && source["snapshot_verified"] != true)
                    || source["independent_download"] != true)
            {
                return Err("unverified market provenance".into());
            }
            analytics_core::source_map::reject_removed_sources(&source)?;
            source["content_sha256"] = json!(market_content_hash(path)?);
            source["source_map"] = serde_json::to_value(dataset.source_map())?;
            (
                "market",
                source,
                dataset.calendar.first().map(ToString::to_string),
                dataset.calendar.last().map(ToString::to_string),
            )
        } else if path.file_name().is_some_and(|n| n == "coefficients.json") {
            let bundle = analytics_storage::collector::CoefficientBundle::read(path)?;
            (
                "coefficients",
                serde_json::to_value(&bundle)?,
                Some(bundle.request.start.to_string()),
                Some(bundle.request.end.to_string()),
            )
        } else {
            let book = ActionBundle::read(path)?;
            verify_action_evidence(path, &book)?;
            (
                "actions",
                ActionBundle::version_content(path)?,
                book.actions
                    .iter()
                    .map(|a| a.ex_date)
                    .min()
                    .map(|d| d.to_string()),
                book.actions
                    .iter()
                    .map(|a| a.ex_date)
                    .max()
                    .map(|d| d.to_string()),
            )
        };
        let hash = signature(&details)?;
        let details = if kind == "actions" {
            let book: ActionBundle = serde_json::from_value(details)?;
            json!({"events":book.actions.len(),"queried_symbols":book.queried_symbols.len(),"source_failures":book.failures,"downloaded_at":book.downloaded_at,"downloader":book.downloader,"missing_cash_dates":book.actions.iter().filter(|a|a.cash()>0.0 && a.pay_date.is_none()).count(),"missing_listing_dates":book.actions.iter().filter(|a|a.shares()>0.0 && a.listing_date.is_none()).count()})
        } else {
            details
        };
        let id = format!("{kind}-{}", &hash[7..23]);
        let data = DataVersion {
            id: id.clone(),
            kind: kind.into(),
            path: relative,
            signature: hash,
            start,
            end,
            details,
        };
        let dest = self
            .root
            .join(".jfquant/catalog")
            .join(format!("{id}.json"));
        if dest.exists() {
            let old: DataVersion = serde_json::from_slice(&fs::read(&dest)?)?;
            return Ok(old);
        }
        atomic_json(&dest, &data)?;
        Ok(data)
    }
}
