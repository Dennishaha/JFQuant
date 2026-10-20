//! Extended manifest metadata for efficient storage management.
//!
//! Producers record file metadata at publication time, enabling fast queries
//! without filesystem scanning. The SQLite index can be rebuilt from manifests
//! rather than traversing the entire workspace.

use crate::filesystem::FileStamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

/// File metadata recorded at publication time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileMetadata {
    /// Logical size in bytes.
    pub bytes: u64,
    /// Allocated space in bytes (block-aligned).
    pub allocated: u64,
    /// Modification timestamp (seconds, nanoseconds).
    pub modified: (i64, i64),
    /// SHA-256 content hash for immutable evidence files.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

impl From<&FileStamp> for FileMetadata {
    fn from(stamp: &FileStamp) -> Self {
        Self {
            bytes: stamp.bytes,
            allocated: stamp.allocated,
            modified: stamp.modified,
            sha256: None,
        }
    }
}

/// Extended manifest with file inventory for fast queries.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestInventory {
    /// Schema version for this inventory format (1).
    pub schema_version: u32,
    /// Resource category for bulk classification.
    pub category: String,
    /// Workspace-relative file paths and their metadata.
    pub files: BTreeMap<PathBuf, FileMetadata>,
    /// Total logical bytes across all files.
    pub total_bytes: u64,
    /// Total allocated bytes across all files.
    pub total_allocated: u64,
    /// Resource creation timestamp (RFC3339).
    pub created_at: String,
}

impl ManifestInventory {
    /// Create a new inventory for a set of files.
    pub fn new(category: impl Into<String>, files: BTreeMap<PathBuf, FileMetadata>) -> Self {
        let total_bytes = files.values().map(|m| m.bytes).sum();
        let total_allocated = files.values().map(|m| m.allocated).sum();
        Self {
            schema_version: 1,
            category: category.into(),
            files,
            total_bytes,
            total_allocated,
            created_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Validate inventory structure.
    pub fn validate(&self) -> io::Result<()> {
        if self.schema_version != 1 {
            return Err(io::Error::other("unsupported inventory schema"));
        }
        if self.files.is_empty() {
            return Err(io::Error::other("inventory must contain at least one file"));
        }
        let computed_bytes: u64 = self.files.values().map(|m| m.bytes).sum();
        let computed_allocated: u64 = self.files.values().map(|m| m.allocated).sum();
        if computed_bytes != self.total_bytes || computed_allocated != self.total_allocated {
            return Err(io::Error::other("inventory totals mismatch"));
        }
        Ok(())
    }

    /// Load inventory from a manifest file, if present.
    pub fn load(manifest_path: &Path) -> io::Result<Option<Self>> {
        let inventory_path = manifest_path.with_extension("inventory.json");
        if !inventory_path.exists() {
            return Ok(None);
        }
        let bytes = std::fs::read(&inventory_path)?;
        let inventory: Self = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::other(format!("invalid inventory: {e}")))?;
        inventory.validate()?;
        Ok(Some(inventory))
    }

    /// Save inventory alongside a manifest.
    pub fn save(&self, manifest_path: &Path) -> io::Result<()> {
        self.validate()?;
        let inventory_path = manifest_path.with_extension("inventory.json");
        let bytes = serde_json::to_vec_pretty(self)?;

        // Atomic write
        let temp = inventory_path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        std::fs::write(&temp, bytes)?;
        std::fs::rename(temp, inventory_path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inventory_validates_totals() {
        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from("data/file1.parquet"),
            FileMetadata {
                bytes: 1000,
                allocated: 4096,
                modified: (1700000000, 0),
                sha256: None,
            },
        );
        files.insert(
            PathBuf::from("data/file2.parquet"),
            FileMetadata {
                bytes: 2000,
                allocated: 4096,
                modified: (1700000001, 0),
                sha256: None,
            },
        );

        let inventory = ManifestInventory::new("market_data", files);
        assert_eq!(inventory.total_bytes, 3000);
        assert_eq!(inventory.total_allocated, 8192);
        assert!(inventory.validate().is_ok());
    }

    #[test]
    fn inventory_rejects_mismatched_totals() {
        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from("data/file1.parquet"),
            FileMetadata {
                bytes: 1000,
                allocated: 4096,
                modified: (1700000000, 0),
                sha256: None,
            },
        );

        let mut inventory = ManifestInventory::new("market_data", files);
        inventory.total_bytes = 999; // Wrong total

        assert!(inventory.validate().is_err());
    }

    #[test]
    fn inventory_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("manifest.json");

        let mut files = BTreeMap::new();
        files.insert(
            PathBuf::from("data/test.parquet"),
            FileMetadata {
                bytes: 5000,
                allocated: 8192,
                modified: (1700000000, 500000000),
                sha256: Some("sha256:abc123".into()),
            },
        );

        let inventory = ManifestInventory::new("predictions", files.clone());
        inventory.save(&manifest_path).unwrap();

        let loaded = ManifestInventory::load(&manifest_path).unwrap().unwrap();
        assert_eq!(loaded.category, "predictions");
        assert_eq!(loaded.files, files);
        assert_eq!(loaded.total_bytes, 5000);
        assert_eq!(loaded.total_allocated, 8192);
    }

    #[test]
    fn missing_inventory_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let manifest_path = dir.path().join("nonexistent.json");

        assert!(ManifestInventory::load(&manifest_path).unwrap().is_none());
    }
}
