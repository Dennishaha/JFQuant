//! Configuration use cases.
use super::*;
pub(super) fn execute(
    w: &Workspace,
    request: &WorkflowRequest,
    run: &RunRecord,
    context: &TaskContext,
) -> Result<WorkflowOutcome> {
    let mut data = None;
    let mut details = Value::Null;
    match request {
        WorkflowRequest::Index { request: operation } => {
            use crate::indices::IndexOperation;
            details = match operation {
                IndexOperation::History { request } => {
                    crate::membership_history::execute(w, request, run, context)?
                }
                IndexOperation::Import { .. } => {
                    let config = serde_json::from_value(run.request["index_import"].clone())?;
                    context.check()?;
                    let v = w.save_index(config)?;
                    json!({"index_version":v,"index_versions":w.indices()?})
                }
                IndexOperation::List => json!({"index_versions":w.indices()?}),
                IndexOperation::Save { config } => {
                    context.check()?;
                    let v = w.save_index(*config.clone())?;
                    json!({"index_version":v,"index_versions":w.indices()?})
                }
                IndexOperation::Get { version } => json!({"index_version":w.index(version)?}),
                IndexOperation::Export { version, path } => {
                    let v = w.index(version)?;
                    if path.is_absolute()
                        || path
                            .components()
                            .any(|p| !matches!(p, std::path::Component::Normal(_)))
                    {
                        return Err("export requires a workspace-relative path".into());
                    }
                    if path
                        .components()
                        .next()
                        .is_some_and(|p| p.as_os_str() == ".jfquant")
                        || w.resolve(path).exists()
                    {
                        return Err(
                            "export destination exists or belongs to managed workspace state"
                                .into(),
                        );
                    }
                    context.check()?;
                    atomic_json(&w.resolve(path), &v.config)?;
                    json!({"output":path,"index_version":v})
                }
                IndexOperation::CollectMsci {
                    version,
                    calendar_market,
                    start,
                    end,
                } => {
                    let output = crate::indices::collect_msci(
                        w,
                        run,
                        version,
                        calendar_market,
                        analytics_core::DateRange {
                            start: *start,
                            end: *end,
                        },
                        context,
                    )?;
                    data = output.data;
                    output.details
                }
                IndexOperation::Verify {
                    provider,
                    start,
                    end,
                    current_members,
                    version,
                } => {
                    let v: crate::indices::IndexVersion =
                        serde_json::from_value(run.request["index_version"].clone())?;
                    if v.id != *version
                        || crate::signature(&v.config)? != v.signature
                        || crate::signature(&v.config.calculation_contract())?
                            != v.calculation_signature
                    {
                        return Err("frozen index version changed".into());
                    }
                    if run.request["index_adapter_signature"].as_str()
                        != Some(analytics_storage::collector::index_adapter_signature(
                            *provider,
                        ))
                    {
                        return Err("frozen index adapter changed".into());
                    }
                    let r = crate::indices::collection(
                        v.config.clone(),
                        *provider,
                        *start,
                        *end,
                        *current_members,
                    )?;
                    let output = super::acquisition::execute(
                        w,
                        &WorkflowRequest::CollectMarket {
                            request: r,
                            resume: None,
                        },
                        run,
                        context,
                    )?;
                    data = output.data;
                    let market = data.as_ref().ok_or("index collection produced no market")?;
                    let ds = DailyDataset::open(w.resolve(&market.path))?;
                    let actual_end = ds
                        .last_valid_close_date(&v.config.data_key)?
                        .ok_or("index has no valid close")?;
                    let close = ds.research_field(&v.config.data_key, "close", true)?;
                    let valid_dates: Vec<_> = ds
                        .calendar
                        .iter()
                        .enumerate()
                        .filter_map(|(i, d)| {
                            close
                                .get(i)
                                .filter(|x| x.is_finite() && *x > 0.)
                                .map(|_| *d)
                        })
                        .collect();
                    let missing_dates: Vec<_> = ds
                        .calendar
                        .iter()
                        .enumerate()
                        .filter_map(|(i, d)| {
                            if close.get(i).is_some_and(|v| v.is_finite() && v > 0.) {
                                None
                            } else {
                                Some(*d)
                            }
                        })
                        .collect();
                    json!({"schema_version":1,"availability":if missing_dates.is_empty() {"observed_values_available"}else{"partial"},"actual":{"start":valid_dates.first(),"end":actual_end},"valid_close_count":valid_dates.len(),"missing_close_dates":missing_dates,"adapter_signature":analytics_storage::collector::index_adapter_signature(*provider),"adapter_capabilities":analytics_storage::collector::index_capabilities(*provider),"identity_checks":{"provider_code":"checked_against_response","publisher_currency_and_return_basis":"configured_declaration_not_inferred"},"index_version":v,"provider":provider,"requested":{"start":start,"end":end},"actual_last_valid_close":actual_end,"calendar":ds.calendar,"current_members":if *current_members {Some(ds.pool(&v.config.definition.id)?)}else{None},"historical_members_supported":false,"scope":"collected observations only; no currency or publisher identity inferred from a successful response","collection":output.details})
                }
            };
        }
        WorkflowRequest::SavePool { definition } => {
            details = serde_json::to_value(w.save_pool(definition.clone())?)?;
        }
        WorkflowRequest::PreviewPool { id, market, day } => {
            details = w.preview_pool(id, market, *day)?;
        }
        WorkflowRequest::CreateCanonicalDemo {} => {
            let (m, a, t) = demo::create_canonical(w)?;
            let template_path = run.output.join("workflow.json");
            t.write(&w.resolve(&template_path))?;
            details =
                json!({"demo_actions":a.id,"demo_template":t,"demo_template_path":template_path});
            data = Some(m);
        }
        WorkflowRequest::CreateDemo {} => {
            context.report("创建合成演示输入（不是真实行情）")?;
            let (m, a, t) = demo::create(w)?;
            let template_path = run.output.join("workflow.json");
            t.write(&w.resolve(&template_path))?;
            details =
                json!({"demo_actions":a.id,"demo_template":t,"demo_template_path":template_path});
            data = Some(m);
        }
        WorkflowRequest::RegisterData { path } => {
            context.report("核验本地数据来源")?;
            data = Some(w.register(&w.resolve(path))?);
        }
        _ => unreachable!("domain dispatcher mismatch"),
    }
    Ok(WorkflowOutcome { data, details })
}
