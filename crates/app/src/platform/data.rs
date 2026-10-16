//! Data panel and interaction.
use super::*;
pub fn data_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
) {
    if state.platform.workspace.is_none() {
        return;
    }
    ui.heading("数据更新与版本");
    if let Some(rx) = &state.platform.credential_save {
        match rx.try_recv() {
            Ok(result) => {
                state.platform.credential_save = None;
                if let Err(error) = result {
                    state.platform.error = Some(error);
                }
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                state.platform.credential_save = None;
                state.platform.error = Some("凭据配置进程未完成".into());
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                ui.label("正在保存本机凭据…");
                ui.ctx()
                    .request_repaint_after(std::time::Duration::from_millis(100));
            }
        }
    }
    ui.label(
        "更新保存新快照，已有研究继续使用原版本。全部数据默认通达信优先，不支持的字段按来源顺序补充。",
    );
    let enabled = !state.busy() && state.worker_error.is_none();
    let mut request = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(enabled, egui::Button::new("创建合成演示数据"))
            .clicked()
        {
            request = Some(WorkflowRequest::CreateCanonicalDemo {});
        }
        if ui
            .add_enabled(enabled, egui::Button::new("检查行情更新"))
            .clicked()
        {
            request = Some(WorkflowRequest::CheckUpdates {});
        }
        ui.label("发布日期");
        ui.text_edit_singleline(&mut state.platform.release_tag);
        if ui
            .add_enabled(
                enabled && !state.platform.release_tag.is_empty(),
                egui::Button::new("下载行情快照"),
            )
            .clicked()
        {
            request = Some(WorkflowRequest::DownloadMarket {
                tag: state.platform.release_tag.clone(),
                resume: state.platform.details["resume"]
                    .as_str()
                    .and_then(|s| s.parse().ok()),
            });
        }
    });
    ui.collapsing("HiThink / 通达信采集", |ui| {
        ui.horizontal(|ui| {
            ui.label("独立系数版本");
            select_data(
                ui,
                "coefficient_version",
                "coefficients",
                &mut state.platform.coefficients,
                &state.platform.datasets,
            );
            if ui
                .add_enabled(
                    enabled
                        && !state.platform.market.is_empty()
                        && !state.platform.coefficients.is_empty(),
                    egui::Button::new("生成研究数据视图"),
                )
                .clicked()
            {
                request = Some(WorkflowRequest::ComposeMarket {
                    market: state.platform.market.clone(),
                    coefficients: state.platform.coefficients.clone(),
                });
            }
        });
        ui.add_enabled_ui(enabled, |ui| {
            use analytics_storage::collector::{CollectionRequest, Provider};
            ui.collapsing("来源优先级与刷新", |ui| {
                ui.label("所有类别默认通达信优先，再用其他来源补缺。指定单一来源时不切换。历史成员和账户日期不能由当前快照代替。");
                if ui.button("读取已保存来源设置").clicked() { request=Some(WorkflowRequest::SourceStatus {}); }
                if let Some(policy)=state.platform.details.get("policy")
                    && ui.button("将读取结果载入编辑框").clicked() {state.platform.source_policy_editor=serde_json::to_string_pretty(policy).unwrap_or_default();}
                ui.add(egui::TextEdit::multiline(&mut state.platform.source_policy_editor).desired_rows(9));
                if ui.button("保存来源顺序").clicked() {
                    match serde_json::from_str(&state.platform.source_policy_editor) {
                        Ok(policy)=>request=Some(WorkflowRequest::SaveSourcePolicy {policy}),
                        Err(e)=>state.platform.error=Some(e.to_string()),
                    }
                }
                let mut refresh=None;
                ui.horizontal(|ui| {
                    for (label, mode, preview) in [
                        ("刷新预览",analytics_workflow::refresh::RefreshMode::Incremental,true),
                        ("更新行动",analytics_workflow::refresh::RefreshMode::Actions,false),
                        ("补新数据",analytics_workflow::refresh::RefreshMode::Incremental,false),
                        ("修复缺口",analytics_workflow::refresh::RefreshMode::Repair,false),
                        ("完整重取",analytics_workflow::refresh::RefreshMode::Full,false),
                    ] {
                        if ui.add_enabled(!state.platform.market.is_empty(),egui::Button::new(label)).clicked(){refresh=Some((mode,preview));}
                    }
                });
                if let Some((mode,preview))=refresh {
                    request=Some(WorkflowRequest::RefreshData {request:Box::new(analytics_workflow::refresh::RefreshRequest {
                        template:Box::new(state.platform.template.clone()),market:state.platform.market.clone(),actions:if mode==analytics_workflow::refresh::RefreshMode::Actions {None} else {(!state.platform.actions.is_empty()).then(||state.platform.actions.clone())},mode,provider:None,end:None,preview,resume:None,workers:3,reuse_shards_from:vec![],
                    })});
                }
            });
            ui.horizontal(|ui| {
                if ui.selectable_value(&mut state.platform.collection_provider,Provider::Tdxaidata,"通达信（备用）").clicked() {state.platform.adjustment_source=analytics_storage::adjustment::AdjustmentSource::TdxForward;}
                if ui
                    .selectable_value(
                        &mut state.platform.collection_provider,
                        Provider::Hithink,
                        "HiThink（主源）",
                    )
                    .clicked()
                {
                    state.platform.adjustment_source =
                        analytics_storage::adjustment::AdjustmentSource::HithinkForward;
                }
            });
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut state.platform.tdx_key).password(true).hint_text("通达信数据服务 Key，仅保存本机私有文件"));
                if ui.add_enabled(state.platform.credential_save.is_none(),egui::Button::new("保存通达信 Key")).clicked() {
                    let key=std::mem::take(&mut state.platform.tdx_key);
                    let (tx,rx)=std::sync::mpsc::channel();state.platform.credential_save=Some(rx);
                    std::thread::spawn(move || {let _=tx.send(analytics_workflow::service::configure_source("tdxaidata",key.trim()).map_err(|e|e.to_string()));});
                }
                if ui.button("通达信小样本验证（三轮）").clicked(){request=Some(WorkflowRequest::CheckSource {provider:Provider::Tdxaidata});}
            });
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut state.platform.hithink_key)
                        .password(true)
                        .hint_text("HiThink API Key，仅保存本机私有文件"),
                );
                if ui
                    .add_enabled(
                        state.platform.credential_save.is_none(),
                        egui::Button::new("保存凭据"),
                    )
                    .clicked()
                {
                    let token = std::mem::take(&mut state.platform.hithink_key);
                    let (tx, rx) = std::sync::mpsc::channel();
                    state.platform.credential_save = Some(rx);
                    std::thread::spawn(move || {
                        let _ = tx.send(
                            analytics_workflow::service::configure_source("hithink",token.trim())
                                .map_err(|e| e.to_string()),
                        );
                    });
                }
                if ui.button("检测接口权限").clicked() {
                    request = Some(WorkflowRequest::CheckSource { provider: Provider::Hithink });
                }
            });
            ui.label(match state.platform.adjustment_source {
                analytics_storage::adjustment::AdjustmentSource::TdxForward => "复权系数：通达信 ForwardFactor（方向与窗口一致性独立校验）",
                analytics_storage::adjustment::AdjustmentSource::HithinkForward => {
                    "HiThink 前复权 OHLC；原始量额；研究 VWAP 为 null"
                }
                analytics_storage::adjustment::AdjustmentSource::None => {
                    "复权系数：无（候选数据，不能用于严格研究）"
                }
            });
            ui.label("证券代码，以逗号分隔，如 SH600000,SZ000001,SH000300");
            ui.text_edit_singleline(&mut state.platform.collection_symbols);
            ui.label("其中的指数证券（不按股票接口采集）");
            ui.text_edit_singleline(&mut state.platform.collection_benchmarks);
            ui.horizontal(|ui| {
                ui.checkbox(&mut state.platform.collection_csi300, "登记当前沪深300成分");
                ui.checkbox(&mut state.platform.collection_csi500, "登记当前中证500成分");
            });
            ui.label("当前成分仅保存明确日期，不能回填历史；采集日期应包含训练、验证和因子预热。");
            ui.horizontal(|ui| {
                ui.label("开始");
                ui.text_edit_singleline(&mut state.platform.start);
                ui.label("结束");
                ui.text_edit_singleline(&mut state.platform.end);
            });
            let quote_source = true;
            let collect_quotes = ui
                .add_enabled(quote_source, egui::Button::new("仅采集行情（系数另采）"))
                .clicked();
            let collect_coefficients = ui
                .add_enabled(quote_source, egui::Button::new("仅采集独立复权系数"))
                .clicked();
            if ui
                .add_enabled(quote_source, egui::Button::new("采集日线并发布新版本"))
                .clicked()
                || collect_coefficients
                || collect_quotes
            {
                let parse = || -> Result<CollectionRequest, String> {
                    let ids = |v: &str| {
                        v.split([',', '，', ' ', '\n'])
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| s.trim().to_ascii_uppercase())
                            .collect()
                    };
                    let r = CollectionRequest {
                        source_bindings: Default::default(),
                        provider: state.platform.collection_provider,
                        adjustment_source: state.platform.adjustment_source,
                        start: state.platform.start.parse().map_err(|_| "开始日期无效")?,
                        end: state.platform.end.parse().map_err(|_| "结束日期无效")?,
                        instruments: ids(&state.platform.collection_symbols),
                        benchmarks: ids(&state.platform.collection_benchmarks),
                        universes: [
                            (state.platform.collection_csi300, "csi300"),
                            (state.platform.collection_csi500, "csi500"),
                        ]
                        .into_iter()
                        .filter(|(yes, _)| *yes)
                        .map(|(_, id)| id.to_string())
                        .collect(),
                    };
                    r.validate().map_err(|e| e.to_string())?;
                    Ok(r)
                };
                match parse() {
                    Ok(r) => {
                        request = Some(if collect_coefficients {
                            WorkflowRequest::CollectCoefficients {
                                request: r,
                                resume: None,
                            }
                        } else if collect_quotes {
                            WorkflowRequest::CollectQuotes {
                                request: r,
                                resume: None,
                            }
                        } else {
                            WorkflowRequest::CollectMarket {
                                request: r,
                                resume: None,
                            }
                        })
                    }
                    Err(e) => state.platform.error = Some(e),
                }
            }
            let announcements = ui.button("获取巨潮实施公告（按以上证券与日期）").clicked();
            if ui.button("按指定来源采集分红资料").clicked() || announcements {
                let parse = || -> Result<CollectionRequest, String> {
                    let ids = |v: &str| {
                        v.split([',', '，', ' ', '\n'])
                            .filter(|s| !s.trim().is_empty())
                            .map(|s| s.trim().to_ascii_uppercase())
                            .collect::<std::collections::BTreeSet<_>>()
                    };
                    let instruments = ids(&state.platform.collection_symbols)
                        .difference(&ids(&state.platform.collection_benchmarks))
                        .cloned()
                        .collect();
                    let r = CollectionRequest {
                        source_bindings: Default::default(),
                        provider: if announcements { Provider::Cninfo } else { state.platform.collection_provider },
                        adjustment_source: if announcements { analytics_storage::adjustment::AdjustmentSource::None } else { state.platform.adjustment_source },
                        start: state.platform.start.parse().map_err(|_| "开始日期无效")?,
                        end: state.platform.end.parse().map_err(|_| "结束日期无效")?,
                        instruments,
                        benchmarks: Default::default(),
                        universes: Default::default(),
                    };
                    r.validate().map_err(|e| e.to_string())?;
                    Ok(r)
                };
                match parse() {
                    Ok(r) => {
                        request = Some(if announcements { WorkflowRequest::CollectAnnouncements {
                            request: r,
                            resume: None,
                        }} else { WorkflowRequest::CollectActions {
                            request: r,
                            resume: None,
                        }})
                    }
                    Err(e) => state.platform.error = Some(e),
                }
            }
            ui.label("失败记录可恢复已验证证券；Yahoo价格不会自动作为不复权撮合价。");
        });
    });
    ui.horizontal(|ui| {
        ui.label("登记已有数据");
        ui.text_edit_singleline(&mut state.platform.register_path);
        if ui
            .add_enabled(enabled, egui::Button::new("数据目录"))
            .clicked()
            && let Some(p) = rfd::FileDialog::new().pick_folder()
        {
            state.platform.register_path = p.display().to_string();
        }
        if ui
            .add_enabled(enabled, egui::Button::new("公司行动文件"))
            .clicked()
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            state.platform.register_path = p.display().to_string();
        }
        if ui
            .add_enabled(
                enabled && !state.platform.register_path.is_empty(),
                egui::Button::new("核验并登记"),
            )
            .clicked()
        {
            request = Some(WorkflowRequest::RegisterData {
                path: state.platform.register_path.clone().into(),
            });
        }
    });
    ui.horizontal(|ui| {
        ui.label("行情版本");
        select_data(
            ui,
            "更新行情选择",
            "market",
            &mut state.platform.market,
            &state.platform.datasets,
        );
        if ui
            .add_enabled(
                enabled && !state.platform.market.is_empty(),
                egui::Button::new("下载公司行动历史"),
            )
            .clicked()
        {
            request = Some(WorkflowRequest::DownloadActions {
                market: state.platform.market.clone(),
                pool_version: state.platform.template.pool_version.clone(),
                resume: state.platform.details["resume_actions"]
                    .as_str()
                    .and_then(|s| s.parse().ok()),
            });
        }
    });
    if let Some(r) = request {
        submit(state, worker, cancel_slot, r);
    }
    egui::CollapsingHeader::new("已登记的数据版本")
        .default_open(true)
        .show(ui, |ui| {
            egui::Grid::new("dataset_versions")
                .striped(true)
                .show(ui, |ui| {
                    for d in &state.platform.datasets {
                        ui.label(&d.id);
                        ui.label(d.end.as_deref().unwrap_or("未知日期"));
                        ui.label(d.path.display().to_string());
                        ui.collapsing("详情", |ui| {
                            ui.label(serde_json::to_string_pretty(&d.details).unwrap_or_default());
                        });
                        ui.end_row();
                    }
                });
        });
    ui.separator();
}
