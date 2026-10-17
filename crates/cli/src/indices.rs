//! Index CLI translation. Business validation and execution stay in shared workflow/storage services.
use super::*;
pub(super) enum Action {
    Read(Value),
    Task {
        request: WorkflowRequest,
        synchronous: bool,
    },
}
pub(super) fn offline(cli: &Cli) -> Result<Option<Value>, ServiceError> {
    if matches!(&cli.command, Command::Indices { command: None }) {
        let indices = analytics_core::indices::all().iter().map(|i| {
            let mut value = serde_json::to_value(i).expect("index metadata");
            value["quote_route"] = json!(if i.symbol.is_some() { "configured_requires_data_validation" } else { "unavailable" });
            value["current_members"] = json!({"hithink":analytics_storage::collector::builtin_members(&i.id,analytics_storage::collector::Provider::Hithink),"tdxaidata":analytics_storage::collector::builtin_members(&i.id,analytics_storage::collector::Provider::Tdxaidata),"historical":"requires_verified_membership_intervals"});
            value
        }).collect::<Vec<_>>();
        return Ok(Some(
            json!({"schema_version":analytics_core::indices::SCHEMA_VERSION,"verified_on":analytics_core::indices::VERIFIED_ON,"indices":indices}),
        ));
    }
    if let Command::Indices {
        command: Some(IndexCommand::Validate { path }),
    } = &cli.command
    {
        let root = cli
            .workspace
            .clone()
            .unwrap_or(std::env::current_dir().map_err(invalid)?);
        let config: analytics_core::indices::IndexConfig = read(&root, path)?;
        config.validate().map_err(invalid)?;
        for provider in config.mappings.keys() {
            analytics_storage::collector::validate_index_mapping(&config, provider)
                .map_err(invalid)?;
        }
        return Ok(Some(
            json!({"valid":true,"data_ready":false,"calculation_signature":analytics_workflow::signature(&config.calculation_contract()).map_err(invalid)?}),
        ));
    }
    Ok(None)
}
pub(super) fn prepare(client: &Client, command: IndexCommand) -> Result<Action, ServiceError> {
    use analytics_workflow::indices::IndexOperation;
    let mut synchronous = true;
    let operation = match command {
        IndexCommand::History(command) => {
            use analytics_workflow::membership_history::Operation as H;
            let op = match command {
                IndexHistoryCommand::Draft {
                    index,
                    market,
                    start,
                    end,
                } => H::Draft {
                    index,
                    market,
                    range: analytics_core::DateRange {
                        start: start.parse().map_err(invalid)?,
                        end: end.parse().map_err(invalid)?,
                    },
                },
                IndexHistoryCommand::Validate {
                    index,
                    market,
                    request,
                }
                | IndexHistoryCommand::Preview {
                    index,
                    market,
                    request,
                } => H::Preview {
                    index,
                    market,
                    request: Box::new(read(&client.root, &request)?),
                },
                IndexHistoryCommand::Publish {
                    index,
                    market,
                    request,
                    start,
                    end,
                } => H::Publish {
                    index,
                    market,
                    request: Box::new(read(&client.root, &request)?),
                    range: analytics_core::DateRange {
                        start: start.parse().map_err(invalid)?,
                        end: end.parse().map_err(invalid)?,
                    },
                },
                IndexHistoryCommand::Import {
                    index,
                    market,
                    path,
                } => H::Import {
                    index,
                    market,
                    path,
                },
                IndexHistoryCommand::Status { version } => H::Status { version },
                IndexHistoryCommand::Export { version, output } => H::Export {
                    version,
                    path: output,
                },
                IndexHistoryCommand::Capture {
                    url,
                    document_format,
                } => H::Capture {
                    url,
                    format: document_format,
                },
            };
            synchronous = false;
            IndexOperation::History {
                request: Box::new(op),
            }
        }
        IndexCommand::List => return Ok(Action::Read(client.call(Rpc::Indices)?)),
        IndexCommand::Import { path } => IndexOperation::Save {
            config: Box::new(read(&client.root, &path)?),
        },
        IndexCommand::Validate { .. } => {
            unreachable!("offline validation handled before connecting")
        }
        IndexCommand::Show { version } => {
            return Ok(Action::Read(client.call(Rpc::Index { id: version })?));
        }
        IndexCommand::Export { version, output } => IndexOperation::Export {
            version,
            path: output,
        },
        IndexCommand::Verify {
            version,
            provider,
            start,
            end,
            current_members,
        } => {
            synchronous = false;
            IndexOperation::Verify {
                version,
                provider: provider.into(),
                start: start.parse().map_err(invalid)?,
                end: end.parse().map_err(invalid)?,
                current_members,
            }
        }
        IndexCommand::CollectMsci {
            version,
            calendar_market,
            start,
            end,
        } => {
            synchronous = false;
            IndexOperation::CollectMsci {
                version,
                calendar_market,
                start: start.parse().map_err(invalid)?,
                end: end.parse().map_err(invalid)?,
            }
        }
    };
    Ok(Action::Task {
        request: WorkflowRequest::Index { request: operation },
        synchronous,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn history_cli_requires_explicit_inputs_and_publish_range() {
        assert!(
            Cli::try_parse_from([
                "jfquant",
                "indices",
                "history",
                "preview",
                "--index",
                "builtin:csi300",
                "--market",
                "market-fixture",
                "--request",
                "history.json"
            ])
            .is_ok()
        );
        assert!(
            Cli::try_parse_from([
                "jfquant",
                "indices",
                "history",
                "publish",
                "--index",
                "builtin:csi300",
                "--market",
                "market-fixture",
                "--request",
                "history.json"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "jfquant",
                "indices",
                "history",
                "capture",
                "--url",
                "https://example.com/source.pdf",
                "--document-format",
                "pdf"
            ])
            .is_ok()
        );
    }
    #[test]
    fn msci_cli_requires_explicit_calendar_and_has_no_arbitrary_url() {
        let base = [
            "jfquant",
            "indices",
            "collect-msci",
            "index-fixture",
            "--start",
            "2026-03-30",
            "--end",
            "2026-03-31",
        ];
        assert!(Cli::try_parse_from(base).is_err());
        let args: Vec<_> = base
            .into_iter()
            .chain(["--calendar-market", "market-fixture"])
            .collect();
        assert!(matches!(
            Cli::try_parse_from(&args).unwrap().command,
            Command::Indices {
                command: Some(IndexCommand::CollectMsci { .. })
            }
        ));
        assert!(
            Cli::try_parse_from(args.into_iter().chain(["--url", "https://example.com"])).is_err()
        );
    }
}
