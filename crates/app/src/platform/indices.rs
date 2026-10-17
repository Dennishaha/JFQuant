//! Index catalogue controls; no provider calls or database handles on the UI thread.
use super::*;
use analytics_core::indices;
fn add_symbol(list: &mut String, symbol: &str) {
    if !list
        .split([',', '，', ' ', '\n'])
        .any(|s| s.eq_ignore_ascii_case(symbol))
    {
        if !list.trim().is_empty() {
            list.push(',');
        }
        list.push_str(symbol);
    }
}
pub(super) fn catalogue(ui: &mut egui::Ui, state: &mut PlatformState) {
    ui.collapsing("常见指数目录", |ui| {
        ui.label("宽基、规模、成长、红利与国际视角；目录不表示本地已具备行情或历史成分。");
        ui.horizontal(|ui| {ui.label("搜索名称、代码或分类");ui.text_edit_singleline(&mut state.index_query);});
        let query = state.index_query.to_lowercase();
        egui::ScrollArea::vertical().id_salt("index_catalogue").max_height(250.).show(ui, |ui| {
            for index in indices::all().iter().filter(|i| format!("{} {} {} {}",i.id,i.name,i.official_code,i.category).to_lowercase().replace(['_', ' '], "").contains(&query.replace(['_', ' '], ""))) {
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut state.index_selection, index.id.clone(), &index.name);
                    ui.label(format!("{} · {}",index.official_code,index.category));
                    ui.small(if index.symbol.is_some() {"行情接口已配置"} else {"待接数据源"});
                });
            }
        });
        if let Some(index) = indices::find(&state.index_selection) {
            ui.separator();
            ui.label(format!("{} · {} · {}",index.name,index.currency,if index.return_basis == "price" {"价格指数"} else {&index.return_basis}));
            ui.label(&index.description);
            ui.hyperlink_to("编制方资料", &index.source_url);
            if ui.button("配置此指数的数据源").clicked() {
                let config=indices::builtin_config(&index.id).unwrap_or_else(||indices::IndexConfig {schema_version:1,definition:index.clone(),data_key:format!("IDX_{}",index.id),mappings:Default::default()});
                state.index_editor=serde_json::to_string_pretty(&config).unwrap_or_default();
            }
            ui.small("映射可在下方维护指数配置中编辑；配置不会证明数据覆盖。");
            if let Some(symbol) = &index.symbol {
                ui.label(format!("行情代码：{symbol}；实际覆盖须经数据检查确认。"));
                if ui.button("加入行情采集列表").clicked() {add_symbol(&mut state.collection_symbols,symbol);add_symbol(&mut state.collection_benchmarks,symbol);}
                if ui.button("设为研究基准").clicked() {state.template.benchmark=symbol.clone();}
                if ui.button("设为研究股票池与基准").clicked() {state.template.benchmark=symbol.clone();state.template.pool_version=format!("builtin:{}",index.id);}
                ui.small("历史股票池必须来自所选数据版本；当前成分不能代替历史成分。选择指数保留当前因子、模型和研究日程。");
            } else {
                ui.label("待接数据源");
                ui.label("此条目尚无已核验的采集映射，暂不能设为可执行基准或内置股票池。不会用ETF、其他A50或当前成分替代。");
            }
        }
    });
}
/// Submit only plain worker operations; the render thread does not read files or call providers.
pub(super) fn maintenance(
    ui: &mut egui::Ui,
    state: &mut PlatformState,
    enabled: bool,
) -> Option<WorkflowRequest> {
    use analytics_workflow::indices::IndexOperation;
    let mut operation = None;
    ui.collapsing("维护指数配置", |ui| {
        ui.label("配置保存在当前研究目录。保存不代表行情或历史成分可用；验证会实际采集并发布所选范围的数据版本。");
        ui.add_enabled_ui(enabled, |ui| {
            if ui.button("刷新配置版本").clicked() {operation=Some(IndexOperation::List);}
            egui::ComboBox::from_id_salt("runtime_index_version").selected_text(if state.index_version.is_empty() {"选择配置版本"} else {&state.index_version}).show_ui(ui, |ui| {
                for v in &state.index_versions {
                    let id=if v.builtin {format!("builtin:{}",v.config.definition.id)}else{v.id.clone()};
                    ui.selectable_value(&mut state.index_version,id,format!("{} · {}",v.config.definition.name,if v.builtin {"内置"}else{&v.id}));
                }
            });
            let selected=state.index_versions.iter().find(|v|v.id==state.index_version || (v.builtin && state.index_version==format!("builtin:{}",v.config.definition.id))).cloned();
            if let Some(v)=selected {
                ui.label(format!("{} · {} · {}",v.config.data_key,v.config.definition.currency,v.config.definition.return_basis));
                ui.label("按所选适配器能力采集；MSCI 仅提供收盘，历史成分须由所选数据版本提供。");
                if ui.button("编辑此配置").clicked() {state.index_editor=serde_json::to_string_pretty(&v.config).unwrap_or_default();}
                if ui.add_enabled(v.config.mappings.contains_key(state.collection_provider.name()),egui::Button::new("加入行情采集列表")).clicked() {
                    add_symbol(&mut state.collection_symbols,&v.config.data_key);
                    add_symbol(&mut state.collection_benchmarks,&v.config.data_key);
                    state.collection_index_inputs.insert(v.config.data_key.clone(),v.config.clone());
                }
                if ui.button("设为研究基准").clicked() {state.template.benchmark=v.config.data_key.clone();}
                if ui.button("设为研究股票池与基准").clicked() {
                    state.template.benchmark=v.config.data_key.clone();
                    state.template.pool_version=if v.builtin {format!("builtin:{}",v.config.definition.id)} else {format!("index:{}",v.config.definition.id)};
                }
            }
            ui.horizontal(|ui| {ui.label("配置文件");ui.text_edit_singleline(&mut state.index_path);});
            if ui.button("导入配置并保存版本").clicked() {operation=Some(IndexOperation::Import {path:state.index_path.clone().into()});}
            if ui.button("导出所选版本").clicked() {operation=Some(IndexOperation::Export {version:state.index_version.clone(),path:state.index_path.clone().into()});}
            if ui.button("新建配置示例").clicked() {
                let mut c=indices::builtin_config("csi1000").expect("builtin seed");
                c.definition.id="my_index".into();c.definition.name="自定义指数".into();c.definition.official_code="请填写官方代码".into();c.definition.symbol=None;c.data_key="IDX_my_index".into();c.mappings.clear();
                state.index_editor=serde_json::to_string_pretty(&c).unwrap_or_default();
            }
            ui.label("配置 JSON（映射支持 hithink、tdxaidata、msci；MSCI 仅采集 CNY 价格指数收盘，不提供成员或成交行情）");
            ui.add(egui::TextEdit::multiline(&mut state.index_editor).code_editor().desired_rows(10));
            if ui.button("校验并保存新版本").clicked() {
                match serde_json::from_str(&state.index_editor) {Ok(config)=>operation=Some(IndexOperation::Save {config:Box::new(config)}),Err(e)=>state.error=Some(e.to_string())}
            }
            ui.horizontal(|ui| {ui.label("验证开始");ui.text_edit_singleline(&mut state.start);ui.label("结束");ui.text_edit_singleline(&mut state.end);});
            if state.index_versions.iter().any(|v| v.id == state.index_version && v.config.mappings.contains_key("msci")) {
                ui.label("MSCI 官方收盘：使用数据页选定市场的交易日历，保留原始响应与哈希；不会补造历史成员。");
                ui.label(format!("交易日历市场：{}", state.market));
                if ui.add_enabled(!state.market.is_empty(), egui::Button::new("从 MSCI 官方采集收盘行情")).clicked() {
                    match (state.start.parse(),state.end.parse()) {
                        (Ok(start),Ok(end))=>operation=Some(IndexOperation::CollectMsci {version:state.index_version.clone(),calendar_market:state.market.clone(),start,end}),
                        _=>state.error=Some("指数采集日期无效".into())
                    }
                }
            }
            ui.checkbox(&mut state.index_current_members,"同时采集当前成分（仅观察日）");
            let configured_provider = state.index_versions.iter().any(|v|v.id==state.index_version && v.config.mappings.contains_key(state.collection_provider.name()));
            if ui.add_enabled(configured_provider,egui::Button::new("使用当前采集供应商验证并采集")).clicked() {
                match (state.start.parse(),state.end.parse()) {
                    (Ok(start),Ok(end))=>operation=Some(IndexOperation::Verify {version:state.index_version.clone(),provider:state.collection_provider,start,end,current_members:state.index_current_members}),
                    _=>state.error=Some("指数验证日期无效".into())
                }
            }
            if state.details.get("requested").is_some() && (
                state.details["requested"]["start"].as_str()!=Some(state.start.as_str()) ||
                state.details["requested"]["end"].as_str()!=Some(state.end.as_str()) ||
                (state.details["provider"]!="msci" && state.details["provider"]!=serde_json::to_value(state.collection_provider).unwrap_or_default()) ||
                (state.details["provider"]=="msci" && state.details["calendar_market"].as_str()!=Some(state.market.as_str())) ||
                state.details["index_version"]["id"].as_str()!=Some(state.index_version.as_str())) {
                ui.colored_label(egui::Color32::YELLOW,"供应商、日期或所选版本已变化，原验证范围已过期。");
            }
            let expected_adapter = if state.details["provider"]=="msci" {analytics_storage::msci::signature()} else {analytics_storage::collector::index_adapter_signature(state.collection_provider).into()};
            if let Some(signature)=state.details["adapter_signature"].as_str()
                && signature!=expected_adapter {
                ui.colored_label(egui::Color32::YELLOW,"采集实现已变化，原验证报告已过期。");
            }
            if state.details.get("index_version").is_some() {
                if !state.index_editor.is_empty() && let Ok(c)=serde_json::from_str::<indices::IndexConfig>(&state.index_editor)
                    && state.details["index_version"]["config"]!=serde_json::to_value(c).unwrap_or_default() {ui.colored_label(egui::Color32::YELLOW,"编辑内容已变化，原验证报告不适用于新配置。");}
                ui.label("报告仅证明记录范围内的采集结果；研究准入仍检查任务所需数据与历史成员。");
            }
        });
    });
    let history = super::history::maintenance(ui, state, enabled);
    operation
        .map(|request| WorkflowRequest::Index { request })
        .or(history)
}
pub(super) fn research_selectors(
    ui: &mut egui::Ui,
    template: &mut ResearchTemplate,
    versions: &[analytics_workflow::indices::IndexVersion],
) {
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("builtin_index_pool")
            .selected_text("选择指数股票池")
            .show_ui(ui, |ui| {
                for v in versions.iter().filter(|v| !v.builtin) {
                    ui.selectable_value(
                        &mut template.pool_version,
                        format!("index:{}", v.config.definition.id),
                        &v.config.definition.name,
                    );
                }
                for index in indices::all().iter().filter(|i| i.symbol.is_some()) {
                    ui.selectable_value(
                        &mut template.pool_version,
                        format!("builtin:{}", index.id),
                        &index.name,
                    );
                }
            });
        egui::ComboBox::from_id_salt("index_benchmark")
            .selected_text(
                indices::by_symbol(&template.benchmark)
                    .map(|i| i.name.as_str())
                    .unwrap_or("自定义基准"),
            )
            .show_ui(ui, |ui| {
                for v in versions.iter().filter(|v| !v.builtin) {
                    ui.selectable_value(
                        &mut template.benchmark,
                        v.config.data_key.clone(),
                        &v.config.definition.name,
                    );
                }
                for index in indices::all() {
                    ui.add_enabled_ui(index.symbol.is_some(), |ui| {
                        if let Some(symbol) = &index.symbol {
                            ui.selectable_value(
                                &mut template.benchmark,
                                symbol.clone(),
                                &index.name,
                            );
                        } else {
                            ui.label(format!("{}（待接数据源）", index.name));
                        }
                    });
                }
            });
    });
    ui.small("指数选择不补造历史成分或行情；缺少所选区间时由共享检查明确提示。");
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn index_collection_routes_are_idempotent_and_not_stock_aliases() {
        let mut symbols = "SZ000001,SH600000".to_string();
        add_symbol(&mut symbols, "SH000001");
        add_symbol(&mut symbols, "SH000001");
        assert_eq!(symbols, "SZ000001,SH600000,SH000001");
        assert!(indices::by_symbol("SZ000001").is_none());
    }
}
