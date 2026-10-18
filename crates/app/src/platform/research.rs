//! Research panel and interaction.
use super::*;
pub fn research_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
    _registry: &analytics_factor::FactorRegistry,
) {
    ui.heading("研究流程");
    if state.platform.details["input_kind"] == "synthetic_fixture" {
        ui.colored_label(
            crate::theme::semantic(ui.ctx()).warning,
            "合成演示数据，仅验证流程与账本，不代表真实市场收益。",
        );
    }
    crate::quality_ui::reasons(ui, &state.platform.details);
    if state.platform.workspace.is_none() {
        ui.label("先创建或打开工作区。");
        return;
    }
    let enabled = !state.busy() && state.worker_error.is_none();
    super::allocation::panel(ui, state, worker, cancel_slot, enabled);
    let mut save = false;
    let mut load = false;
    let mut export = false;
    let mut import = false;
    let (mut save_factors, mut load_factors, mut save_target, mut load_target) =
        (false, false, false, false);
    ui.horizontal(|ui| {
        ui.label("模板名称");
        ui.text_edit_singleline(&mut state.platform.template.name);
        save = ui
            .add_enabled(enabled, egui::Button::new("保存模板"))
            .clicked();
        load = ui
            .add_enabled(enabled, egui::Button::new("载入保存模板"))
            .clicked();
        export = ui
            .add_enabled(enabled, egui::Button::new("导出模板"))
            .clicked();
        import = ui
            .add_enabled(enabled, egui::Button::new("导入模板"))
            .clicked();
    });
    ui.horizontal(|ui| {
        ui.label("独立配置");
        save_factors = ui
            .add_enabled(enabled, egui::Button::new("保存因子配置"))
            .clicked();
        load_factors = ui
            .add_enabled(enabled, egui::Button::new("载入因子配置"))
            .clicked();
        save_target = ui
            .add_enabled(enabled, egui::Button::new("保存标的配置"))
            .clicked();
        load_target = ui
            .add_enabled(enabled, egui::Button::new("载入标的配置"))
            .clicked();
    });
    let w = state.platform.workspace.as_ref().unwrap().clone();
    let result = (|| -> analytics_workflow::Result<()> {
        use analytics_workflow::profiles::{FactorProfile, TargetProfile};
        if save_factors
            && let Some(p) = rfd::FileDialog::new()
                .set_file_name("factors.json")
                .save_file()
        {
            FactorProfile::from_template(&state.platform.template).write(&p)?;
        }
        if load_factors
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            FactorProfile::read(&p)?.apply(&mut state.platform.template)?;
        }
        if save_target
            && let Some(p) = rfd::FileDialog::new()
                .set_file_name("target.json")
                .save_file()
        {
            TargetProfile::from_template(&state.platform.template).write(&p)?;
        }
        if load_target
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            TargetProfile::read(&p)?.apply(&mut state.platform.template)?;
        }
        if save {
            if !state.platform.date_editor_valid {
                return Err("请修正日期输入后再保存".into());
            }
            let id = analytics_workflow::signature(&state.platform.template)?;
            state
                .platform
                .template
                .write(&w.resolve(format!(".jfquant/templates/{}.json", &id[7..23])))?;
        }
        if load
            && let Some(p) = rfd::FileDialog::new()
                .set_directory(w.resolve(".jfquant/templates"))
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            state.platform.template = ResearchTemplate::read(&p)?;
            state.platform.schedule_editor.clear();
            state.platform.date_editor_valid = true;
            crate::schedule_ui::reset(ui);
        }
        if export
            && let Some(p) = rfd::FileDialog::new()
                .set_file_name("research-template.json")
                .save_file()
        {
            state.platform.template.write(&p)?;
        }
        if import
            && let Some(p) = rfd::FileDialog::new()
                .add_filter("JSON", &["json"])
                .pick_file()
        {
            state.platform.template = ResearchTemplate::read(&p)?;
            state.platform.schedule_editor.clear();
            state.platform.date_editor_valid = true;
            crate::schedule_ui::reset(ui);
        }
        Ok(())
    })();
    if let Err(e) = result {
        state.platform.error = Some(e.to_string());
    }
    ui.horizontal(|ui| {
        ui.label("行情");
        select_data(
            ui,
            "研究行情选择",
            "market",
            &mut state.platform.market,
            &state.platform.datasets,
        );
        ui.label("公司行动");
        select_data(
            ui,
            "研究动作选择",
            "actions",
            &mut state.platform.actions,
            &state.platform.datasets,
        );
    });
    ui.collapsing("股票池、基准与日期调整", |ui| {
        ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal(|ui| {
                if ui.button("中证500默认模板").clicked(){state.platform.template=ResearchTemplate::builtin().expect("builtin");state.platform.schedule_editor.clear();state.platform.date_editor_valid=true;crate::schedule_ui::reset(ui);}
                if ui.button("沪深300默认模板").clicked(){state.platform.template=ResearchTemplate::builtin_csi300().expect("builtin");state.platform.schedule_editor.clear();state.platform.date_editor_valid=true;crate::schedule_ui::reset(ui);}
            });
            super::indices::research_selectors(ui, &mut state.platform.template, &state.platform.index_versions);
            ui.horizontal(|ui| {ui.label("股票池版本");ui.text_edit_singleline(&mut state.platform.template.pool_version);ui.label("基准证券");ui.text_edit_singleline(&mut state.platform.template.benchmark);});
            use analytics_core::pool::PoolExitPolicy;
            ui.horizontal(|ui| {ui.label("移除持仓");ui.selectable_value(&mut state.platform.template.pool_exit_policy,PoolExitPolicy::NextTradableDay,"下一可交易日退出");ui.selectable_value(&mut state.platform.template.pool_exit_policy,PoolExitPolicy::NextRebalance,"下次正常调仓退出");});
            ui.horizontal(|ui| {
                if ui.button("载入股票池JSON").clicked() && let Some(p)=rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file(){match fs::read_to_string(p){Ok(s)=>state.platform.pool_editor=s,Err(e)=>state.platform.error=Some(e.to_string())}}
                if ui.button("自选池示例").clicked(){state.platform.pool_editor=serde_json::json!({"schema_version":3,"name":"自选池","coverage":{"start":state.platform.start,"end":state.platform.end},"expression":{"kind":"symbols","instruments":["SH600000","SZ000001"]},"adjustments":[]}).to_string();}
                if ui.button("指数组合示例").clicked(){state.platform.pool_editor=serde_json::json!({"schema_version":3,"name":"沪深300与中证500并集","coverage":{"start":state.platform.start,"end":state.platform.end},"expression":{"kind":"union","inputs":[{"kind":"universe","version":state.platform.market,"universe":"csi300"},{"kind":"universe","version":state.platform.market,"universe":"csi500"}]},"adjustments":[]}).to_string();}
            });
            ui.label("股票池定义：支持 union / intersection / difference；adjustments 按 effective_date 设置 add / remove。");
            ui.add(egui::TextEdit::multiline(&mut state.platform.pool_editor).desired_rows(6).desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                if ui.button("校验并保存股票池新版本").clicked(){match serde_json::from_str::<analytics_core::pool::PoolDefinition>(&state.platform.pool_editor){Ok(d)=>{if let Err(e)=d.validate(){state.platform.error=Some(e);}else{submit(state,worker,cancel_slot,WorkflowRequest::SavePool{definition:d});}},Err(e)=>state.platform.error=Some(e.to_string())}}
                if ui.button("导出股票池JSON").clicked() && let Some(p)=rfd::FileDialog::new().set_file_name("stock-pool.json").save_file(){let result=(||->analytics_workflow::Result<()>{let d:analytics_core::pool::PoolDefinition=serde_json::from_str(&state.platform.pool_editor)?;d.validate()?;fs::write(p,serde_json::to_vec_pretty(&d)?)?;Ok(())})();if let Err(e)=result{state.platform.error=Some(e.to_string());}}
            });
            ui.horizontal(|ui| {ui.label("成员预览日期");ui.text_edit_singleline(&mut state.platform.pool_preview_date);if ui.button("预览成员").clicked(){match state.platform.pool_preview_date.parse(){Ok(day)=>submit(state,worker,cancel_slot,WorkflowRequest::PreviewPool{id:state.platform.template.pool_version.clone(),market:state.platform.market.clone(),day}),Err(_)=>state.platform.error=Some("预览日期无效".into())}}});
        });
    });
    ui.add_enabled_ui(enabled,|ui|{
        ui.horizontal(|ui|{
            for (label,announcements) in [("补采所需公司行动",false),("获取实施公告证据",true)] {
                if ui.button(label).clicked(){submit(state,worker,cancel_slot,WorkflowRequest::PrepareResearchData{actions:(!state.platform.actions.is_empty()).then(||state.platform.actions.clone()),template:Box::new(state.platform.template.clone()),market:state.platform.market.clone(),announcements,resume:None});}
            }
            if ui.button("预览研究日程").clicked(){submit(state,worker,cancel_slot,WorkflowRequest::PreviewResearch{actions:(!state.platform.actions.is_empty()).then(||state.platform.actions.clone()),template:Box::new(state.platform.template.clone()),market:state.platform.market.clone()});}
        });
        super::evidence::controls(ui,state,worker,cancel_slot);
        ui.collapsing("公司行动：查看证据与审计修订",|ui|{
            ui.horizontal(|ui| { ui.label("核对证据任务ID（逗号分隔，可空）"); ui.text_edit_singleline(&mut state.platform.action_evidence_tasks); });
            if ui.button("自动核对并发布新版本").clicked() {
                let ids = state.platform.action_evidence_tasks.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::parse).collect::<std::result::Result<Vec<uuid::Uuid>,_>>();
                match ids { Ok(evidence_tasks) => submit(state,worker,cancel_slot,WorkflowRequest::ReconcileActions { actions:state.platform.actions.clone(), evidence_tasks, market:(!state.platform.market.is_empty()).then(||state.platform.market.clone()) }),Err(e)=>state.platform.error=Some(e.to_string()) }
            }
            ui.label("从运行结果查看原值、候选字段及公告。修订JSON必须包含原事件哈希、逐字段原文引用及理由。");
            super::evidence::event_terms_editor(ui, &mut state.platform.action_review_editor, &mut state.platform.evidence_tools);
            ui.add(egui::TextEdit::multiline(&mut state.platform.action_review_editor).desired_rows(8).desired_width(f32::INFINITY));
            if ui.button("提交证据修订为新版本").clicked(){match serde_json::from_str(&state.platform.action_review_editor){Ok(request)=>submit(state,worker,cancel_slot,WorkflowRequest::ReviseActions{actions:state.platform.actions.clone(),request}),Err(e)=>state.platform.error=Some(e.to_string())}}
        });
    });
    ui.add_enabled_ui(enabled, |ui| {
            ui.horizontal(|ui|{ui.label("证券");ui.text_edit_singleline(&mut state.platform.action_review_instrument);ui.label("公告manifest");ui.text_edit_singleline(&mut state.platform.action_evidence_manifest);});
            if ui.button("加载事件、结构化字段与公告证据").clicked(){submit(state,worker,cancel_slot,WorkflowRequest::InspectActions{actions:state.platform.actions.clone(),instrument:state.platform.action_review_instrument.clone(),evidence_manifest:if state.platform.action_evidence_manifest.trim().is_empty(){None}else{Some(PathBuf::from(state.platform.action_evidence_manifest.trim()))}});}
        let t = &mut state.platform.template;
        ui.horizontal(|ui| {
            ui.label("训练价格口径");
            egui::ComboBox::from_id_salt("training_price_basis").selected_text(t.price_basis.label()).show_ui(ui,|ui| {
                for basis in [analytics_core::research_quality::PriceBasis::Verified,analytics_core::research_quality::PriceBasis::Provider] {ui.selectable_value(&mut t.price_basis,basis,basis.label());}
            });
            let old=t.factor_set;
            egui::ComboBox::from_id_salt("research_factor_set").selected_text(format!("{:?}",old)).show_ui(ui,|ui| {
                ui.selectable_value(&mut t.factor_set,analytics_core::research_quality::FactorSet::Gtja191,"GTJA191");
                ui.selectable_value(&mut t.factor_set,analytics_core::research_quality::FactorSet::Alpha158,"Alpha158");
            });
            if old!=t.factor_set {t.select_factor_set(t.factor_set);}
        });
        crate::quality_ui::controls(ui, &mut t.quality_policy);
        ui.label("AKQuant：分红与拆股按除权除息日处理；不支持的已知权益事件停止回测，保留模型。");
        ui.collapsing("日期调度：单次 / 滚动 / 手动多折", |ui| {
            let before=serde_json::to_string(&t.schedule).unwrap_or_default();
            crate::schedule_ui::draw(ui,&mut t.schedule);
            if before!=serde_json::to_string(&t.schedule).unwrap_or_default(){state.platform.schedule_editor.clear();}
            if state.platform.schedule_editor.is_empty(){state.platform.schedule_editor=serde_json::to_string_pretty(&t.schedule).unwrap_or_default();}
            ui.label("任意train / validation（可为null）/ test日期；滚动支持days/months/years/trading_days及expanding。");
            ui.add(egui::TextEdit::multiline(&mut state.platform.schedule_editor).desired_rows(14).desired_width(f32::INFINITY));
            if ui.button("应用日期设置").clicked(){match serde_json::from_str::<analytics_core::schedule::Schedule>(&state.platform.schedule_editor){Ok(s)=>match s.validate(){Ok(())=>t.schedule=s,Err(e)=>state.platform.error=Some(e)},Err(e)=>state.platform.error=Some(e.to_string())}}

        });
        ui.horizontal(|ui| {
            ui.label("持仓数量");
            ui.add(egui::DragValue::new(&mut t.config.top_n).range(1..=1000));
            ui.label("调仓间隔（交易日）");
            ui.add(egui::DragValue::new(&mut t.config.rebalance_days).range(1..=252));
            ui.label("初始资金（元）");
            ui.add(
                egui::DragValue::new(&mut t.initial_cash_cny)
                    .speed(10000.0)
                    .range(1.0..=1e13),
            );
        });
        ui.collapsing("组合优化（目标仓位98%）", |ui| {
            use analytics_workflow::allocation::AllocationMethod;
            let c = &mut t.config.allocation;
            egui::ComboBox::from_id_salt("allocation_method").selected_text(format!("{:?}",c.method)).show_ui(ui,|ui| {
                for (m,label) in [(AllocationMethod::Equal,"等权"),(AllocationMethod::MinimumVariance,"最小方差"),(AllocationMethod::RiskBudgeting,"风险预算"),(AllocationMethod::MeanVariance,"预测收益均值方差")] {ui.selectable_value(&mut c.method,m,label);}
            });
            ui.horizontal(|ui|{ui.label("单股权重上限");ui.add(egui::DragValue::new(&mut c.max_stock_weight).range(0.001..=1.).speed(0.01));ui.label("风险历史窗口");ui.add(egui::DragValue::new(&mut c.risk_window).range(2..=10000));});
            let mut turnover = c.max_turnover.is_some();
            if ui.checkbox(&mut turnover,"限制全账户目标换手（L1）").changed(){c.max_turnover=turnover.then_some(1.);}
            if let Some(v)=&mut c.max_turnover {ui.add(egui::DragValue::new(v).range(0.0..=2.).speed(0.01));}
            if c.method==AllocationMethod::MeanVariance {
                ui.label("必须显式设置风险厌恶参数；评分按已实现样本外收益校准。");
                let mut enabled=c.risk_aversion.is_some();if ui.checkbox(&mut enabled,"设置风险厌恶参数").changed(){c.risk_aversion=enabled.then_some(1.);}
                if let Some(v)=&mut c.risk_aversion {ui.add(egui::DragValue::new(v).range(0.000001..=1e8).speed(0.1));}
                ui.horizontal(|ui|{ui.label("校准窗口");ui.add(egui::DragValue::new(&mut c.calibration_window).range(2..=10000));ui.label("最少日期");ui.add(egui::DragValue::new(&mut c.calibration_min_days).range(2..=10000));ui.label("每日配对证券");ui.add(egui::DragValue::new(&mut c.calibration_min_pairs).range(3..=10000));});
            }
            ui.small("收益期限匹配调仓周期；历史不足、不可行和求解异常均失败，不自动降级。");
            if let Err(e)=c.validate_count(t.config.top_n){ui.colored_label(egui::Color32::YELLOW,e.to_string());}
        });
        ui.horizontal(|ui| {
            ui.label("佣金比例");
            ui.add(
                egui::DragValue::new(&mut t.market_rules.commission.rate)
                    .speed(0.00001)
                    .range(0.0..=0.1)
                    .max_decimals(6),
            );
            ui.label("最低佣金（元）");
            ui.add(
                egui::DragValue::new(&mut t.market_rules.commission.minimum_cny)
                    .range(0.0..=10000.0),
            );
            use analytics_core::market_rules::CommissionMode;
            egui::ComboBox::from_id_salt("research_commission")
                .selected_text(format!("{:?}", t.market_rules.commission.mode))
                .show_ui(ui, |ui| {
                    for (mode, label) in [
                        (CommissionMode::Gross, "全佣（含经手及证管费）"),
                        (CommissionMode::AllInclusive, "含过户费全佣"),
                    ] {
                        ui.selectable_value(&mut t.market_rules.commission.mode, mode, label);
                    }
                });
        });
        ui.label("分红使用税前金额；费用只展示上游合计，不模拟个人持股期税。");
        ui.collapsing("训练参数", |ui| {
            egui::Grid::new("research_training").show(ui, |ui| {
                for key in [
                    "num_iterations",
                    "learning_rate",
                    "num_leaves",
                    "min_data_in_leaf",
                    "feature_fraction",
                    "lambda_l2",
                    "num_threads",
                    "seed",
                ] {
                    ui.label(key);
                    ui.text_edit_singleline(
                        t.config.training.parameters.entry(key.into()).or_default(),
                    );
                    ui.end_row();
                }
            });
            let mut use_early = t.config.training.early_stopping_rounds.is_some();
            if ui.checkbox(&mut use_early, "启用早停").changed() {
                t.config.training.early_stopping_rounds = use_early.then_some(20);
            }
            if let Some(n) = &mut t.config.training.early_stopping_rounds {
                ui.add(egui::DragValue::new(n).range(1..=1000));
            }
        });
    });
    ui.collapsing(
        format!(
            "研究因子（已选{}）",
            state.platform.template.config.factor_ids.len()
        ),
        |ui| {
            if !state.platform.template.config.factor_refs.is_empty() {
                ui.label("已固定混合因子及其修订；请在因子页编辑后重新应用。");
                for r in &state.platform.template.config.factor_refs {
                    ui.monospace(match r {
                        analytics_factor::selection::FactorRef::Builtin {
                            factor_set, id, ..
                        } => format!("{} · {id}", factor_set.label()),
                        analytics_factor::selection::FactorRef::Custom { definition } => format!(
                            "{} · {} · 修订{}",
                            definition.name, definition.id, definition.revision
                        ),
                    });
                }
                return;
            }
            ui.text_edit_singleline(&mut state.platform.filter);
            let quarantined = analytics_factor::audit::quarantined_ids();
            egui::ScrollArea::vertical()
                .id_salt("research_factors")
                .max_height(240.0)
                .show(ui, |ui| {
                    let registry = analytics_factor::FactorRegistry::for_set(
                        state.platform.template.factor_set,
                    );
                    for s in registry
                        .all()
                        .filter(|s| s.id.contains(&state.platform.filter))
                    {
                        let available = s.id != "alpha_030" && !quarantined.contains(&s.id);
                        let mut selected =
                            state.platform.template.config.factor_ids.contains(&s.id);
                        let response = ui.add_enabled(
                            enabled && available,
                            egui::Checkbox::new(&mut selected, &s.id),
                        );
                        if response.changed() {
                            if selected {
                                state.platform.template.config.factor_ids.push(s.id.clone());
                            } else {
                                state
                                    .platform
                                    .template
                                    .config
                                    .factor_ids
                                    .retain(|id| id != &s.id);
                            }
                        }
                        if !available {
                            ui.label(if s.id == "alpha_030" {
                                "缺少真实MKT/SMB/HML输入"
                            } else {
                                "公式歧义已隔离，详见因子审计"
                            });
                        }
                    }
                });
        },
    );
    independent_panel(ui, state, worker, cancel_slot, enabled);
    ui.label("原研究流程：不复权成交、整数股、T+1、跨年持续账户。");
    let ready = enabled && !state.platform.market.is_empty();
    let execute_ready = ready && !state.platform.actions.is_empty();
    let mut run = None;
    ui.horizontal(|ui| {
        if ui
            .add_enabled(ready, egui::Button::new("检查研究输入"))
            .clicked()
        {
            run = Some(true);
        }
        if ui
            .add_enabled(execute_ready, egui::Button::new("执行年度研究"))
            .clicked()
        {
            run = Some(false);
        }
    });
    if let Some(audit_only) = run {
        match state.platform.template.validate() {
            Ok(()) => submit(
                state,
                worker,
                cancel_slot,
                WorkflowRequest::Research {
                    template: Box::new(state.platform.template.clone()),
                    market: state.platform.market.clone(),
                    actions: (!state.platform.actions.is_empty())
                        .then(|| state.platform.actions.clone()),
                    audit_only,
                },
            ),
            Err(e) => state.platform.error = Some(e.to_string()),
        }
    }
    ui.horizontal(|ui| {
        if ui
            .add_enabled(ready, egui::Button::new("仅训练模型"))
            .clicked()
        {
            submit(
                state,
                worker,
                cancel_slot,
                WorkflowRequest::TrainResearch {
                    template: Box::new(state.platform.template.clone()),
                    market: state.platform.market.clone(),
                    actions: (!state.platform.actions.is_empty())
                        .then(|| state.platform.actions.clone()),
                },
            );
        }
        ui.label("训练任务ID");
        ui.text_edit_singleline(&mut state.platform.training_job);
        if ui
            .add_enabled(
                enabled && !state.platform.actions.is_empty(),
                egui::Button::new("使用已有模型回测"),
            )
            .clicked()
        {
            match state.platform.training_job.parse() {
                Ok(training) => submit(
                    state,
                    worker,
                    cancel_slot,
                    WorkflowRequest::BacktestResearch {
                        execution: Some(Box::new(state.platform.template.execution_settings())),
                        event_check_policy: state.platform.template.event_check_policy,
                        training,
                        actions: state.platform.actions.clone(),
                    },
                ),
                Err(_) => state.platform.error = Some("请填写已完成训练的任务ID".into()),
            }
        }
    });
    ui.separator();
    egui::CollapsingHeader::new("配股与预测复用").show(ui, |ui| {
        ui.label("已发布预测的回测任务ID");
        ui.text_edit_singleline(&mut state.platform.prediction_job);
        use analytics_core::rights::{Attempt, InsufficientCash, Participation};
        ui.horizontal_wrapped(|ui| {
            ui.label("配股处理");
            ui.selectable_value(
                &mut state.platform.rights_policy.participation,
                Participation::Participate,
                "参与认购",
            );
            ui.selectable_value(
                &mut state.platform.rights_policy.participation,
                Participation::Abandon,
                "放弃认购",
            );
            ui.label("认购时点");
            ui.selectable_value(
                &mut state.platform.rights_policy.attempt,
                Attempt::FirstDay,
                "首日尝试一次",
            );
            ui.selectable_value(
                &mut state.platform.rights_policy.attempt,
                Attempt::LastDay,
                "截止日尝试一次",
            );
            ui.label("资金不足");
            ui.selectable_value(
                &mut state.platform.rights_policy.insufficient_cash,
                InsufficientCash::Partial,
                "部分认购",
            );
            ui.selectable_value(
                &mut state.platform.rights_policy.insufficient_cash,
                InsufficientCash::AllOrAbandon,
                "全额或放弃",
            );
            ui.selectable_value(
                &mut state.platform.rights_policy.insufficient_cash,
                InsufficientCash::Stop,
                "停止并报告",
            );
        });
        if ui
            .add_enabled(
                enabled && !state.platform.actions.is_empty(),
                egui::Button::new("复用已发布预测回测"),
            )
            .clicked()
        {
            match state.platform.prediction_job.parse() {
                Ok(source) => submit(
                    state,
                    worker,
                    cancel_slot,
                    WorkflowRequest::BacktestPredictions {
                        allocation: Some(Box::new(
                            state.platform.template.config.allocation.clone(),
                        )),
                        source,
                        actions: state.platform.actions.clone(),
                        rights_policy: state.platform.rights_policy.clone(),
                    },
                ),
                Err(_) => {
                    state.platform.error = Some("请填写含有已发布预测的成功回测任务ID".into())
                }
            }
        }
    });
    super::runs::show_results(ui, &state.platform.details);
}
fn independent_panel(
    ui: &mut egui::Ui,
    state: &mut AppState,
    worker: &WorkerHandle,
    cancel_slot: &mut Option<Cancellation>,
    enabled: bool,
) {
    use analytics_workflow::research::backtest::independent::{
        BacktestPlan, Job, ModelSelection, TrainingPlan,
    };
    egui::CollapsingHeader::new("独立训练与跨池回测").default_open(false).show(ui,|ui| {
        ui.label("训练模型：A 股票池与训练/验证周期，不需要测试日期。");
        ui.horizontal(|ui| {
            if ui.button("从当前配置生成训练计划").clicked() {state.platform.independent_training_editor=serde_json::to_string_pretty(&TrainingPlan::from_template(&state.platform.template)).unwrap();}
            if ui.button("导出独立训练JSON").clicked() && let Some(p)=rfd::FileDialog::new().set_file_name("fit.json").save_file() {match serde_json::from_str::<TrainingPlan>(&state.platform.independent_training_editor) {Ok(plan)=>{if let Err(e)=fs::write(p,serde_json::to_vec_pretty(&plan).unwrap()){state.platform.error=Some(e.to_string());}},Err(e)=>state.platform.error=Some(e.to_string())}}
            if ui.button("导入独立训练JSON").clicked() && let Some(p)=rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() {match fs::read_to_string(p) {Ok(v)=>state.platform.independent_training_editor=v,Err(e)=>state.platform.error=Some(e.to_string())}}
        });
        ui.collapsing("独立训练计划JSON", |ui| {
            ui.add(egui::TextEdit::multiline(&mut state.platform.independent_training_editor).id_salt("independent_fit_plan").desired_rows(9).desired_width(f32::INFINITY).hint_text("TrainingPlan JSON：single / manual / rolling；仅训练与验证日期"));
        });
        if let Ok(p)=serde_json::from_str::<TrainingPlan>(&state.platform.independent_training_editor) {ui.small(format!("训练A：{} · {:?}",p.pool_version,p.schedule));}
        let mut action=None;
        ui.add_enabled_ui(enabled && !state.platform.market.is_empty(),|ui|{ui.horizontal(|ui| {
            if ui.button("预检训练A输入").clicked(){action=Some((false,false));}
            if ui.button("补采训练A公司行动").clicked(){action=Some((true,false));}
            if ui.button("提交独立训练").clicked(){action=Some((false,true));}
        });});
        if let Some((prepare,train))=action {match serde_json::from_str::<TrainingPlan>(&state.platform.independent_training_editor) {
            Ok(plan)=>{let market=state.platform.market.clone();let actions=(!state.platform.actions.is_empty()).then(||state.platform.actions.clone());let job=if train {Job::Train {plan:Box::new(plan),market,actions}} else {Job::TrainingInputs {plan:Box::new(plan),market,actions,prepare,announcements:false}};submit(state,worker,cancel_slot,WorkflowRequest::Independent {request:Box::new(job)});},
            Err(e)=>state.platform.error=Some(e.to_string()),
        }}
        ui.separator();ui.label("模型回测：独立选择 B 股票池、行情版本、日期与账户；默认滚动使用已可用模型。");
        ui.horizontal(|ui| {
            ui.label("模型训练任务ID");ui.text_edit_singleline(&mut state.platform.training_job);
            if ui.add_enabled(enabled,egui::Button::new("查询已训练模型")).clicked() {match state.platform.training_job.parse() {Ok(training)=>submit(state,worker,cancel_slot,WorkflowRequest::Independent {request:Box::new(Job::Models {training})}),Err(_)=>state.platform.error=Some("训练任务ID无效".into())}}
        });
        ui.horizontal(|ui| {
            if ui.button("生成独立回测计划").clicked() {let result=(|| -> Result<BacktestPlan,String> {Ok(BacktestPlan {schema_version:4,training:state.platform.training_job.parse().map_err(|_|"请先填写训练任务ID")?,models:ModelSelection::Rolling,pool_version:state.platform.template.pool_version.clone(),range:analytics_core::DateRange {start:state.platform.start.parse().map_err(|_|"开始日期无效")?,end:state.platform.end.parse().map_err(|_|"结束日期无效")?},market:state.platform.market.clone(),actions:state.platform.actions.clone(),benchmark:state.platform.template.benchmark.clone(),quality_policy:state.platform.template.quality_policy.clone(),execution:state.platform.template.execution_settings(),event_check_policy:state.platform.template.event_check_policy})})();match result {Ok(p)=>state.platform.independent_backtest_editor=serde_json::to_string_pretty(&p).unwrap(),Err(e)=>state.platform.error=Some(e)}}
            if ui.button("导出独立回测JSON").clicked() && let Some(p)=rfd::FileDialog::new().set_file_name("backtest-B.json").save_file() {match serde_json::from_str::<BacktestPlan>(&state.platform.independent_backtest_editor) {Ok(plan)=>{if let Err(e)=fs::write(p,serde_json::to_vec_pretty(&plan).unwrap()){state.platform.error=Some(e.to_string());}},Err(e)=>state.platform.error=Some(e.to_string())}}
            if ui.button("导入独立回测JSON").clicked() && let Some(p)=rfd::FileDialog::new().add_filter("JSON", &["json"]).pick_file() {match fs::read_to_string(p) {Ok(v)=>state.platform.independent_backtest_editor=v,Err(e)=>state.platform.error=Some(e.to_string())}}
        });
        ui.small("回测 JSON 的 pool_version / range / market 独立于训练。models 默认 {\"mode\":\"rolling\"}；固定模型使用 {\"mode\":\"single\",\"model_id\":\"查询所得ID\"}。");
        if let Ok(mut p)=serde_json::from_str::<BacktestPlan>(&state.platform.independent_backtest_editor) {
            let mut changed=false;
            ui.horizontal(|ui| {ui.label("回测B股票池");changed|=ui.text_edit_singleline(&mut p.pool_version).changed();});
            ui.horizontal(|ui| {ui.label("回测B行情");let old=p.market.clone();select_data(ui,"independent_b_market","market",&mut p.market,&state.platform.datasets);changed|=p.market!=old;ui.label("回测B公司行动");let old=p.actions.clone();select_data(ui,"independent_b_actions","actions",&mut p.actions,&state.platform.datasets);changed|=p.actions!=old;});
            ui.small(format!("回测日期：{} → {}（在回测JSON中修改）",p.range.start,p.range.end));
            egui::ComboBox::from_id_salt("independent_model_selection").selected_text(match &p.models {ModelSelection::Rolling=>"默认滚动切换".to_owned(),ModelSelection::Single {model_id}=>format!("固定模型 {model_id}")}).show_ui(ui,|ui| {
                if ui.selectable_label(matches!(p.models,ModelSelection::Rolling),"默认滚动切换").clicked() {p.models=ModelSelection::Rolling;changed=true;}
                if state.platform.independent_models["training"]==p.training.to_string() {for model in state.platform.independent_models["catalogue"]["models"].as_array().into_iter().flatten() {let id=model["id"].as_str().unwrap_or("");if ui.selectable_label(matches!(&p.models,ModelSelection::Single {model_id} if model_id==id),format!("{} · 截止 {} · 可用 {}",id,model["fit_data_end"],model["available_from"])).clicked() {p.models=ModelSelection::Single {model_id:id.into()};changed=true;}}}
            });
            ui.horizontal(|ui| {
                use analytics_core::corporate_account::Policy;
                ui.label("公司事件模拟");
                changed |= ui.selectable_value(&mut p.execution.corporate_event_policy, Policy::EvidenceOnly, "仅证据执行").changed();
                changed |= ui.selectable_value(&mut p.execution.corporate_event_policy, Policy::ResearchTransitions, "退市收盘清算／换股迁码承接").changed();
            });
            if p.execution.corporate_event_policy == analytics_core::corporate_account::Policy::ResearchTransitions {
                ui.small("研究假设：最后实际交易日收盘清算，次日可用现金；换股舍零无补偿；待上市权益计净值、不计购买力。退市清算含事后信息。");
            }
            if changed {state.platform.independent_backtest_editor=serde_json::to_string_pretty(&p).unwrap();}
        }
        ui.collapsing("独立回测计划JSON",|ui| {ui.add(egui::TextEdit::multiline(&mut state.platform.independent_backtest_editor).id_salt("independent_backtest_plan").desired_rows(9).desired_width(f32::INFINITY));});
        let mut action=None;
        ui.add_enabled_ui(enabled,|ui| {ui.horizontal(|ui| {
            if ui.button("预检B与模型切换").clicked(){action=Some((false,false));}
            if ui.button("补采回测B公司行动").clicked(){action=Some((true,false));}
            if ui.button("提交独立模型回测").clicked(){action=Some((false,true));}
        });});
        if let Some((prepare,execute))=action {match serde_json::from_str::<BacktestPlan>(&state.platform.independent_backtest_editor) {
            Ok(plan)=>{let job=if execute {Job::Backtest {plan:Box::new(plan)}} else {Job::BacktestInputs {plan:Box::new(plan),prepare,announcements:false}};submit(state,worker,cancel_slot,WorkflowRequest::Independent {request:Box::new(job)});},Err(e)=>state.platform.error=Some(e.to_string()),
        }}
    });
}
