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
    MigrationPlan { resources: Vec<String> },
    MigrationApply { plan: Uuid },
    Export { resource: String, output: PathBuf },
}
impl StorageRequest {
    pub(crate) fn exclusive(&self) -> bool {
        matches!(self, Self::Apply { .. } | Self::MigrationApply { .. })
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ResourceQuery {
    pub category: Option<ResourceCategory>,
    pub task: Option<Uuid>,
    pub status: Option<String>,
    pub search: Option<String>,
    pub modified_before: Option<String>,
    pub newest: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationMember {
    pub path: PathBuf,
    pub stamp: FileStamp,
    pub object: analytics_storage::evidence_store::ObjectRef,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPackage {
    pub map: analytics_storage::evidence_store::PackageMap,
    pub members: Vec<MigrationMember>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPlan {
    pub schema_version: u32,
    pub id: Uuid,
    pub workspace: PathBuf,
    pub scan_id: Uuid,
    pub created_at: String,
    pub packages: Vec<MigrationPackage>,
    pub blockers: Vec<String>,
    pub fingerprint: String,
    pub registry_sha256: String,
    pub original_reference_bytes: u64,
    pub original_file_usage: SpaceUsage,
    pub unique_original_bytes: u64,
    pub object_storage_bytes: u64,
    pub additional_space_required: u64,
    pub estimated_logical_savings: u64,
}
/// Small task result. Exact members remain in the durable plan outside the RPC/UI payload.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPreview {
    pub id: Uuid,
    pub workspace: PathBuf,
    pub scan_id: Uuid,
    pub created_at: String,
    pub members_manifest: PathBuf,
    pub packages: Vec<MigrationPackagePreview>,
    /// Distinct original-byte hashes, rather than file paths or filesystem identities.
    pub unique_objects: u64,
    pub blockers: Vec<String>,
    pub original_reference_bytes: u64,
    pub original_file_usage: SpaceUsage,
    pub unique_original_bytes: u64,
    pub object_storage_bytes: u64,
    pub additional_space_required: u64,
    pub estimated_logical_savings: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationPackagePreview {
    pub root: PathBuf,
    pub resources: Vec<String>,
    pub members: u64,
}
impl From<&MigrationPlan> for MigrationPreview {
    fn from(plan: &MigrationPlan) -> Self {
        Self {
            id: plan.id,
            workspace: plan.workspace.clone(),
            scan_id: plan.scan_id,
            created_at: plan.created_at.clone(),
            members_manifest: PathBuf::from(format!(
                ".jfquant/storage/migration-plans/{}.json",
                plan.id
            )),
            packages: plan
                .packages
                .iter()
                .map(|p| MigrationPackagePreview {
                    root: p.map.root.clone(),
                    resources: p.map.resources.clone(),
                    members: p.members.len() as u64,
                })
                .collect(),
            unique_objects: plan
                .packages
                .iter()
                .flat_map(|p| &p.members)
                .map(|m| &m.object.sha256)
                .collect::<std::collections::BTreeSet<_>>()
                .len() as u64,
            blockers: plan.blockers.clone(),
            original_reference_bytes: plan.original_reference_bytes,
            original_file_usage: plan.original_file_usage.clone(),
            unique_original_bytes: plan.unique_original_bytes,
            object_storage_bytes: plan.object_storage_bytes,
            additional_space_required: plan.additional_space_required,
            estimated_logical_savings: plan.estimated_logical_savings,
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MigrationResult {
    pub schema_version: u32,
    pub plan: Uuid,
    pub created_at: String,
    pub workspace: PathBuf,
    pub committed: bool,
    pub completed: bool,
    pub status: String,
    pub deleted: SpaceUsage,
    pub object_bytes_added: u64,
    /// Evidence files only: new object bytes minus removed original inode bytes.
    /// Mapping/journal/index overhead and filesystem shared extents are reported separately.
    pub evidence_logical_bytes_delta: i64,
    pub available_before: Option<u64>,
    pub available_after: Option<u64>,
    pub errors: BTreeMap<PathBuf, String>,
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
// Compact cache encoding avoids duplicating paths and field names per filesystem entry.
impl analytics_storage::index::CacheRecord for StorageFile {
    fn cache_bytes(&self) -> std::io::Result<Vec<u8>> {
        let s = &self.stamp;
        serde_json::to_vec(&(
            s.device,
            s.inode,
            s.bytes,
            s.allocated,
            s.links,
            s.modified,
            s.changed,
            &self.owners,
            self.category,
        ))
        .map_err(std::io::Error::other)
    }
    fn from_cache(path: PathBuf, bytes: &[u8]) -> std::io::Result<Self> {
        let (device, inode, bytes, allocated, links, modified, changed, owners, category) =
            serde_json::from_slice(bytes).map_err(std::io::Error::other)?;
        Ok(Self {
            path,
            stamp: FileStamp {
                device,
                inode,
                bytes,
                allocated,
                links,
                modified,
                changed,
            },
            owners,
            category,
        })
    }
    fn owned(&self) -> bool {
        !self.owners.is_empty()
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceReference {
    pub source: String,
    pub target: String,
    pub kind: ReferenceKind,
    pub reason: String,
}
impl analytics_storage::index::CacheRecord for ResourceReference {}
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EvidenceUsage {
    pub original_reference_bytes: u64,
    pub unique_original_bytes: u64,
    pub object_storage_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageInventory {
    pub evidence: Option<EvidenceUsage>,
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
