//! Versioned local workspace, built-in research recipes and persistent run history.
//! All execution is synchronous on the caller's worker; no native handle escapes.
#![warn(missing_docs)]
/// Reusable evidence capture, recovery, status and review drafts.
pub mod action_evidence;
/// Portfolio allocation configuration and validation.
pub mod allocation;
/// Same-input comparisons of allocation methods.
pub mod allocation_comparison;
/// Explicit offline migration into the allocation protocol.
pub mod allocation_migration;
/// Independent reference strategy accounts.
pub mod allocation_reference;
/// Shared computation worker and plain-data protocol.
pub mod backend;
/// Acquisition progress, historical coverage and explicit availability-pool proposals.
pub mod coverage;
pub mod custom_factors;
/// Explicit synthetic demo generator.
pub mod demo;
pub mod factor_evaluation;
pub mod factor_groups;
pub mod history_diagnostics;
pub mod history_material;
/// Runtime index configuration and resolution.
pub mod indices;
pub mod input_check;
pub mod maintenance;
/// Evidence-backed historical index members and immutable reconstruction artifacts.
pub mod membership_history;
/// Immutable date-effective stock pools.
pub mod pools;
/// Independently validated acquisition checkpoints.
pub mod preparation;
/// Independently saved workflow, factor and target configuration.
pub mod profiles;
/// Prioritized, independently resumable acquisition and refresh.
pub mod refresh;
/// Local service, task requests and transport client.
#[cfg(unix)]
pub mod service;
pub mod views;

/// Shared research orchestration and supervised Python runtime.
pub mod research;

mod contracts;
mod diagnostics;
mod persistence;
mod snapshot;
mod usecases;
mod workspace;

pub use contracts::*;
pub use diagnostics::self_check;
pub use persistence::signature;
pub(crate) use persistence::{
    atomic_json, invalid, market_content_hash, now, verify_action_evidence,
};
pub use snapshot::snapshot;
pub use usecases::{execute, research_requirements, research_requirements_with_actions};
pub(crate) use usecases::{research, version, version_metadata};
pub use workspace::{Workspace, WorkspaceLease};
/// Result for local workflow operations.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Synthetic integration fixture, only compiled for acceptance tests.
#[cfg(any(test, feature = "test-support"))]
pub mod testing;

#[cfg(test)]
mod quality_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
use analytics_core::task::TaskContext;
#[cfg(test)]
use analytics_storage::{corporate_actions::ActionBundle, daily::DailyDataset};
#[cfg(test)]
use serde_json::{Value, json};
#[cfg(test)]
use sha2::{Digest, Sha256};
#[cfg(test)]
use std::{collections::BTreeSet, fs, path::Path};
#[cfg(test)]
use uuid::Uuid;

mod market_revisions;
mod three_factor_build;
