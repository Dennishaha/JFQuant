//! Presentation of completed native research artifacts; no strategy decisions.
use super::*;
use std::fmt::Write as _;

pub(super) fn render(output: &Path) -> Result<()> {
    let summary: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("summary.json"))?)?;
    validate_result(&summary)?;
    let audit: serde_json::Value =
        serde_json::from_slice(&fs::read(output.join("data_audit.json"))?)?;
    let reports: Vec<FoldReport> = serde_json::from_value(summary["annual_reports"].clone())?;
    let aggregate: portfolio::Performance = serde_json::from_value(summary["aggregate"].clone())?;
    let rules: MarketRulesPreset =
        serde_json::from_value(summary["execution"]["market_rules"].clone())?;
    rules.validate()?;
    let synthetic = audit["source"]["synthetic_fixture"] == true;
    let percent = |v: f64| format!("{:.2}%", v * 100.0);
    let rate = |v: f64| format!("{:.5}%", v * 100.0);
    let mut md = String::from("# JFQuant 中证500年度滚动研究\n\n");
    writeln!(
        md,
        "{}\n",
        if synthetic {
            "合成演示数据，仅验证流程和股份账本，不代表真实行情或市场收益。"
        } else {
            "数据由JFQuant独立下载、校验和读取；因子、LightGBM与组合回测使用本项目设施。没有使用JFQlib代码或缓存。"
        }
    )?;
    writeln!(
        md,
        "实际行情：{}—{}；历史中证500成分起日：{}。历史成分缺失的年份不构造回测；训练/验证窗口见执行快照research_config，次年为样本外。\n",
        audit["calendar_start"].as_str().unwrap_or("?"),
        audit["calendar_end"].as_str().unwrap_or("?"),
        audit["historical_csi500_start"].as_str().unwrap_or("?")
    )?;
    writeln!(
        md,
        "特征：{}项可用，排除项为`{}`。公式版本：`{}`。\n",
        audit["usable_factor_ids"].as_array().map_or(0, Vec::len),
        summary["excluded_factors"],
        summary["formula_version"].as_str().unwrap_or("?")
    )?;
    md.push_str("日终信号、下一开盘按不复权价格和整数股数成交，按执行快照的调仓间隔和持仓数量调整目标等权。账本跨年度连续，登记日确定权益，到账日释放现金，上市日释放红股，日终净值包含应收权益，不做年度虚拟平仓。训练/验证剔除跨截止日标签，仅验证集用于早停。\n\n");
    writeln!(
        md,
        "## 当前市场费率设定\n\n预设：`{}`，核验日期{}。所有回测日期统一使用本次最新快照，不还原历史费率。账户初始资金{}元。\n",
        rules.preset_id, rules.verified_on, summary["execution"]["initial_cash_cny"]
    )?;
    writeln!(
        md,
        "券商佣金：{}，每笔最低{:.2}元，口径`{:?}`。Gross含经手费和证管费；Net另收这些费用；AllInclusive还含过户费。默认万2.5是可修改的账户假设，不是全国统一费率。\n",
        rate(rules.commission.rate),
        rules.commission.minimum_cny,
        rules.commission.mode
    )?;
    md.push_str("| 市场 | 经手费（双向） | 证管费（双向） | 过户费（双向） | 印花税（仅卖出） |\n|---|---:|---:|---:|---:|\n");
    for f in &rules.exchanges {
        writeln!(
            md,
            "| {:?} | {} | {} | {} | {} |",
            f.exchange,
            rate(f.handling_rate),
            rate(f.regulatory_rate),
            rate(f.transfer_rate),
            rate(f.sell_stamp_rate)
        )?;
    }
    md.push_str("\n已含费用在分项中展示但不会重复叠加；零成交无最低佣金，单笔费用按人民币分四舍五入。买入在现金约束内预留实际费用；严格模式按有效整手数量成交，年度末不假设卖出。\n\n");
    md.push_str("## 样本外汇总\n\n| 年化收益 | 基准年化 | 年化相对超额 | 最大回撤 | Sharpe | 累计收益 |\n|---:|---:|---:|---:|---:|---:|\n");
    writeln!(
        md,
        "| {} | {} | {} | {} | {} | {} |",
        percent(aggregate.annualized_return),
        percent(aggregate.benchmark_annualized_return),
        percent(aggregate.annualized_excess),
        percent(aggregate.max_drawdown),
        aggregate
            .sharpe
            .map(|v| format!("{v:.2}"))
            .unwrap_or("—".into()),
        percent(aggregate.total_return)
    )?;
    let fees = aggregate.fee_totals_cny;
    writeln!(
        md,
        "\n费用合计{:.2}元：券商留存佣金{:.2}，经手费{:.2}，证管费{:.2}，过户费{:.2}，印花税{:.2}。费用来自持续账户实际模拟成交。\n",
        fees.total(),
        fees.broker_commission,
        fees.exchange_handling,
        fees.regulatory,
        fees.transfer,
        fees.stamp_duty
    )?;
    if output.join("nav.png").exists() {
        md.push_str("![净值与回撤](nav.png)\n\n");
    }
    md.push_str("## 逐年样本外\n\n| 年份 | 策略收益 | 基准收益 | 最大回撤 | 日均RankIC | 费用（元） | 预测行数 |\n|---|---:|---:|---:|---:|---:|---:|\n");
    for r in &reports {
        let partial = audit["coverage"].as_array().is_some_and(|years| {
            years
                .iter()
                .any(|y| y["year"] == r.year && y["partial_year"] == true)
        });
        let p = &r.performance;
        writeln!(
            md,
            "| {}{} | {} | {} | {} | {} | {:.2} | {} |",
            r.year,
            if partial { "（部分）" } else { "" },
            percent(p.total_return),
            percent(p.benchmark_total_return),
            percent(p.max_drawdown),
            r.rank_ic.map(|v| format!("{v:.4}")).unwrap_or("—".into()),
            p.fee_totals_cny.total(),
            r.test_rows
        )?;
    }
    md.push_str("\n## 解释边界\n\n因子审计覆盖191个注册项，尚未宣称全部与原报告严格等价；歧义公式已隔离，见factor_audit.json。公开历史中证500成分从2007年开始，不能构造2000年起的完整历史回测。费率与常规板块规则按当前快照用于所有日期。\n\n");
    {
        writeln!(
            md,
            "原始成交价由{}的归一化价格/factor恢复至分，持仓为整数股并执行T+1。公司行动快照哈希见execution.json；登记日之后买入不享旧权益，卖出旧股不丢失已登记权益。税率是显式固定研究假设{}，默认0表示税前分红，不模拟个人FIFO持股期补税。基准采用{}。",
            if synthetic {
                "合成演示输入"
            } else {
                "本项目独立下载行情"
            },
            summary["execution"]["dividend_withholding_rate"],
            if synthetic {
                "合成的恒定指数，仅用于验收"
            } else {
                "中证500价格指数收盘收益，不含分红"
            }
        )?;
    }
    md.push_str("\n停牌缺报价不成交并延用估值；严格模式在除权日调整停牌估值并单列权益。涨跌停仍用板块限制减0.5个百分点的保守缓冲，缺少精确涨跌停、ST、上市特例、逐笔流动性及退市清算数据；结果不是完整实盘成交复现。无法解释的复权因子变动、配股、缺失结算日、零碎股分配和相互重叠的未上市股权事件会阻止严格执行。因子采用756日预热；SMA初值、排名并列及样本标准差为显式约定。\n\n年化按252交易日，Sharpe无风险利率取0；部分年度不冒充全年。IC标签为下一开盘至再下一开盘收益，组合按配置间隔调仓，两者期限不同。\n\n## 复核与复现\n\n- [执行、费率及税率快照](execution.json)\n- [来源和覆盖](data_audit.json)\n- [191项因子审计](factor_audit.json)\n- [完整汇总](summary.json)\n- [年度报告](annual_reports.json)\n- [逐日净值](nav.csv)\n- 各fold保留模型、manifest、预测、daily.json；同时保存整数股账本ledger.json。复现见crates/ml/README.md。\n\n费用快照核验来源：\n\n");
    for source in &rules.sources {
        writeln!(md, "- <{source}>")?;
    }
    let mut csv = String::from("date,nav_after_fees,csi500_price_index_nav\n");
    let (mut nav, mut benchmark) = (1.0, 1.0);
    let mut previous = String::new();
    for r in &reports {
        let days: Vec<portfolio::DayResult> = serde_json::from_slice(&fs::read(
            output.join(format!("fold-{}/daily.json", r.year)),
        )?)?;
        for day in days {
            if day.date <= previous {
                return Err("return calendar alignment mismatch".into());
            }
            nav *= 1.0 + day.net_return;
            benchmark *= 1.0 + day.benchmark_return;
            writeln!(csv, "{},{},{}", day.date, nav, benchmark)?;
            previous = day.date;
        }
    }
    fs::write(output.join("nav.csv"), csv)?;
    fs::write(output.join("report.md"), md)?;
    Ok(())
}
