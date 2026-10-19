//! Storage CLI translation; ownership, accounting and deletion live in workflow.
use super::*;
use analytics_workflow::storage_management::{StorageInventory, StorageRequest};

#[derive(Debug, Subcommand)]
pub enum StorageCommand {
    /// Submit a metadata-only workspace scan; inspect using jobs wait/results.
    Scan {
        #[arg(long, value_enum, default_value = "overview")]
        level: ScanLevelArg,
        #[arg(long, default_value_t = analytics_workflow::storage_management::default_threads() as u8, value_parser = clap::value_parser!(u8).range(1..=8))]
        threads: u8,
    },
    /// Read the last completed scan without traversing the filesystem.
    List {
        #[arg(long)]
        category: Option<String>,
        #[arg(long)]
        task: Option<Uuid>,
        #[arg(long, value_enum, default_value = "all")]
        status: ResourceStatus,
        #[arg(long, value_enum, default_value = "size")]
        sort: ResourceSort,
        #[arg(long)]
        search: Option<String>,
        /// ISO date, compared with resource modification date.
        #[arg(long)]
        modified_before: Option<String>,
    },
    /// Preview exact files, retained dependencies and irreversible effects.
    Plan {
        #[arg(long = "resource", required = true)]
        resources: Vec<String>,
    },
    /// Permanently delete a previously previewed plan. Repeating resumes pending deletion.
    Apply {
        #[arg(long)]
        plan: Uuid,
        #[arg(long, required = true)]
        confirm_permanent: bool,
    },
    /// Read durable cleanup receipts, including partial failures.
    History,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ScanLevelArg {
    Overview,
    Resources,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ResourceStatus {
    All,
    Archived,
    Temporary,
    Released,
    Deletable,
    Protected,
}
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum ResourceSort {
    Size,
    Date,
}
pub(super) enum Action {
    Read(Value),
    Task(StorageRequest),
}
pub(super) fn prepare(client: &Client, command: StorageCommand) -> Result<Action, ServiceError> {
    let task = match command {
        StorageCommand::Scan { level, threads } => StorageRequest::Scan {
            level: match level {
                ScanLevelArg::Overview => {
                    analytics_workflow::storage_management::ScanLevel::Overview
                }
                ScanLevelArg::Resources => {
                    analytics_workflow::storage_management::ScanLevel::Resources
                }
            },
            threads: usize::from(threads),
        },
        StorageCommand::Plan { resources } => StorageRequest::Plan { resources },
        StorageCommand::Apply {
            plan,
            confirm_permanent,
        } => {
            if !confirm_permanent {
                return Err(invalid("永久删除必须提供 --confirm-permanent"));
            }
            StorageRequest::Apply { plan }
        }
        StorageCommand::History => return Ok(Action::Read(client.call(Rpc::StorageHistory)?)),
        StorageCommand::List {
            category,
            task,
            status,
            sort,
            search,
            modified_before,
        } => {
            let snapshot = client.call(Rpc::StorageSnapshot)?;
            let cached = match client.call(Rpc::StorageInventory) {
                Ok(value) => value,
                Err(e)
                    if e.message.contains("No such file")
                        || e.message.contains("storage_inventory_format") =>
                {
                    return Ok(Action::Read(
                        serde_json::json!({"inventory":null,"snapshot":snapshot,"diagnostic":"尚无有效扫描缓存，请运行 storage scan"}),
                    ));
                }
                Err(e) => return Err(e),
            };
            let mut inventory: StorageInventory =
                serde_json::from_value(cached).map_err(invalid)?;
            let category = category
                .map(|c| {
                    serde_json::from_value::<
                            analytics_workflow::storage_management::ResourceCategory,
                        >(Value::String(c))
                        .map_err(invalid)
                })
                .transpose()?;
            let before = modified_before
                .map(|d| {
                    d.parse::<analytics_core::Date>()
                        .map(|d| d.to_string())
                        .map_err(invalid)
                })
                .transpose()?;
            inventory.resources.retain(|r| {
                category.is_none_or(|c| r.category == c)
                    && task.is_none_or(|t| r.task == Some(t))
                    && match status {
                        ResourceStatus::All => true,
                        ResourceStatus::Archived => r.archived,
                        ResourceStatus::Temporary => r.temporary,
                        ResourceStatus::Released => r.released,
                        ResourceStatus::Deletable => r.deletable,
                        ResourceStatus::Protected => !r.deletable && !r.released,
                    }
                    && search
                        .as_ref()
                        .is_none_or(|s| format!("{} {}", r.name, r.path.display()).contains(s))
                    && before.as_ref().is_none_or(|d| {
                        r.modified_at
                            .get(..10)
                            .is_some_and(|date| date < d.as_str())
                    })
            });
            match sort {
                ResourceSort::Size => inventory
                    .resources
                    .sort_by_key(|r| std::cmp::Reverse(r.usage.allocated_bytes)),
                ResourceSort::Date => inventory
                    .resources
                    .sort_by(|a, b| b.modified_at.cmp(&a.modified_at)),
            }
            let mut value = serde_json::to_value(inventory).map_err(invalid)?;
            value["snapshot"] = snapshot;
            return Ok(Action::Read(value));
        }
    };
    Ok(Action::Task(task))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storage_permanent_release_requires_explicit_flag_and_only_accepts_plan_identity() {
        let id = Uuid::new_v4().to_string();
        assert!(Cli::try_parse_from(["jfquant", "storage", "apply", "--plan", &id]).is_err());
        assert!(
            Cli::try_parse_from([
                "jfquant",
                "storage",
                "apply",
                "--plan",
                &id,
                "--confirm-permanent"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "jfquant",
                "storage",
                "apply",
                "--plan",
                "../../data",
                "--confirm-permanent"
            ])
            .is_err()
        );
    }
}

#[cfg(test)]
mod scan_tests {
    use super::*;
    #[test]
    fn storage_scan_defaults_to_overview_and_rejects_unbounded_threads() {
        let cli = Cli::try_parse_from(["jfquant", "storage", "scan"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Storage(StorageCommand::Scan {
                level: ScanLevelArg::Overview,
                threads: 1..=4
            })
        ));
        assert!(
            Cli::try_parse_from([
                "jfquant",
                "storage",
                "scan",
                "--level",
                "resources",
                "--threads",
                "8"
            ])
            .is_ok()
        );
        for threads in ["0", "9", "-1"] {
            assert!(
                Cli::try_parse_from(["jfquant", "storage", "scan", "--threads", threads]).is_err()
            );
        }
    }
}
