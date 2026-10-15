//! Reports describe AKQuant semantics, never the removed settlement ledger.
use super::*;
use std::fmt::Write;
pub(super) fn render(output: &Path) -> Result<()> {
    let value: serde_json::Value = serde_json::from_slice(&fs::read(output.join("summary.json"))?)?;
    validate_result(&value)?;
    let mut text =
        String::from("# JFQuant 研究报告\n\nRust 因子、Python LightGBM、AKQuant 连续回测。\n\n");
    writeln!(text, "状态：{}。", value["backtest_status"])?;
    text.push_str("\n日终信号，下一交易日开盘成交；跨年不重置账户，不在期末虚拟平仓。分红和拆股使用 AKQuant 除权除息日口径，不区分登记、到账及红股上市日。其他已知权益事件不支持时停止回测，保留模型。\n\n费用仅报告上游执行费用合计，不推断券商、经手费或税费明细。\n\n");
    if value["input_kind"] == "synthetic_fixture" {
        text.push_str("合成演示仅验证流程，不代表真实市场收益。\n\n");
    }
    writeln!(
        text,
        "公式版本：{}；因子签名：{}。\n",
        value["formula_version"], value["factor_signature"]
    )?;
    text.push_str("| 年份 | 收益 | 最大回撤 | 执行费用（元） |\n|---|---:|---:|---:|\n");
    for r in value["annual_reports"]
        .as_array()
        .ok_or("annual reports missing")?
    {
        writeln!(
            text,
            "| {} | {:.2}% | {:.2}% | {:.2} |",
            r["year"],
            r["performance"]["total_return"].as_f64().unwrap_or(0.) * 100.,
            r["performance"]["max_drawdown"].as_f64().unwrap_or(0.) * 100.,
            r["performance"]["execution_fee_cny"].as_f64().unwrap_or(0.)
        )?;
    }
    fs::write(output.join("report.md"), text)?;
    Ok(())
}
