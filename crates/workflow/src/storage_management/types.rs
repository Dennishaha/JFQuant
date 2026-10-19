use analytics_storage::filesystem::FileStamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, path::PathBuf};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum StorageRequest {
    Scan { level: ScanLevel, threads: usize },
    Plan { resources: Vec<String> },
    Apply { plan: Uuid },
}
/// Capacity inspection does not imply dependency verification.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ScanLevel {
    #[default]
    Overview,
    Resources,
}
pub fn default_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get().min(4))
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScanSnapshot {
    pub schema_version: u32,
    pub workspace: PathBuf,
    pub scan_id: Uuid,
    pub level: ScanLevel,
    pub phase: String,
    pub started_at: String,
    pub updated_at: String,
    pub elapsed_ms: u64,
    pub finished: bool,
    pub complete: bool,
    pub diagnostics: Vec<String>,
    pub progress: analytics_storage::filesystem::WalkProgress,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ResourceCategory {
    Market,
    Evidence,
    FactorCache,
    Models,
    Predictions,
    Backtest,
    Metadata,
    Other,
}
impl ResourceCategory {
    pub fn label(self) -> &'static str {
        match self {
            Self::Market => "行情与基础数据",
            Self::Evidence => "公司行动与证据",
            Self::FactorCache => "因子共享缓存",
            Self::Models => "模型与训练证据",
            Self::Predictions => "预测与回放数据",
            Self::Backtest => "回测与日志",
            Self::Metadata => "配置与管理信息",
            Self::Other => "其他文件",
        }
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SpaceUsage {
    pub logical_bytes: u64,
    pub allocated_bytes: u64,
    pub files: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageFile {
    pub path: PathBuf,
    pub stamp: FileStamp,
    pub owners: Vec<String>,
    pub category: ResourceCategory,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceReference {
    pub source: String,
    pub target: String,
    pub kind: ReferenceKind,
    pub reason: String,
}
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReferenceKind {
    Integrity,
    Provenance,
    Unresolved,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageResource {
    pub id: String,
    pub name: String,
    pub category: ResourceCategory,
    pub path: PathBuf,
    pub task: Option<Uuid>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub modified_at: String,
    pub archived: bool,
    pub temporary: bool,
    pub released: bool,
    pub deletable: bool,
    pub reasons: Vec<String>,
    pub usage: SpaceUsage,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageInventory {
    pub scan_id: Uuid,
    pub level: ScanLevel,
    pub dependencies_checked: bool,
    pub stale: bool,
    pub directories: BTreeMap<PathBuf, analytics_storage::filesystem::DirectoryUsage>,
    pub schema_version: u32,
    pub scanned_at: String,
    pub workspace: PathBuf,
    pub complete: bool,
    pub diagnostics: Vec<String>,
    pub available_bytes: Option<u64>,
    pub total: SpaceUsage,
    pub categories: BTreeMap<ResourceCategory, SpaceUsage>,
    pub reclaimable_allocated_bytes_estimate: Option<u64>,
    pub resources: Vec<StorageResource>,
    pub references: Vec<ResourceReference>,
    pub external_references: Vec<PathBuf>,
    pub files: Vec<StorageFile>,
    pub metadata_fingerprint: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupPlan {
    pub schema_version: u32,
    pub id: Uuid,
    pub created_at: String,
    pub workspace: PathBuf,
    pub resources: Vec<StorageResource>,
    pub files: Vec<StorageFile>,
    pub metadata_fingerprint: String,
    pub fingerprint: String,
    pub blockers: Vec<String>,
    pub impacts: Vec<String>,
    pub estimated_release: SpaceUsage,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupResult {
    pub schema_version: u32,
    pub plan: Uuid,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub committed: bool,
    pub status: String,
    pub resources: Vec<String>,
    pub deleted: SpaceUsage,
    pub available_before: Option<u64>,
    pub available_after: Option<u64>,
    pub errors: BTreeMap<PathBuf, String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceAvailability {
    pub released_resources: Vec<String>,
    pub historical_view: bool,
    pub models_reusable: bool,
    pub predictions_replayable: bool,
    pub original_inputs_available: bool,
    pub full_verification_available: bool,
    pub reasons: Vec<String>,
}
pub type HistoricalDisplay = (Value, Vec<(String, f64, f64)>);
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HistoricalView {
    pub schema_version: u32,
    pub task: Uuid,
    pub parameters: Value,
    pub result: Value,
    pub nav: Vec<(String, f64, f64)>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ReleasedResource {
    pub plan: Uuid,
    pub resource: StorageResource,
}
