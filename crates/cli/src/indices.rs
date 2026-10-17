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
            analytics_storage::collector::validate_index(
                &config,
                if provider == "hithink" {
                    analytics_storage::collector::Provider::Hithink
                } else {
                    analytics_storage::collector::Provider::Tdxaidata
                },
            )
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
    };
    Ok(Action::Task {
        request: WorkflowRequest::Index { request: operation },
        synchronous,
    })
}
