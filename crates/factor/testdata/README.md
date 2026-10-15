# 因子合成夹具与独立参考值

[上级：analytics-factor](../README.md) · [被测算子](../src/README.md) · [AST 导出](../examples/README.md)

本目录是可提交的确定性合成测试数据，不是下载行情或研究缓存。小面板 golden 和独立 NumPy 参考分别验证手算边界与大量 AST 的算子实现，不能替代原报告歧义审计。

## 文件格式与生成关系

| 文件 | 内容与边界 |
|---|---|
| [alpha191_expected.json](alpha191_expected.json) | `unit-panel-v1` 小面板的指定 expected 列、公式版本和绝对/相对容差；并非191项全公式真值 |
| [operator_golden.json](operator_golden.json) | 320日×6证券合成列；179项可执行AST的最后3日值；date-major平铺，非有限值写JSON null |
| [reference_ops.py](reference_ops.py) | 独立 NumPy 算子 oracle，固定随机种子191；读取导出的 AST，不调用 Rust 执行器、Qlib或真实行情 |

```mermaid
flowchart TD
  CATALOG["audit_catalog：带版本的 AST"] --> ORACLE["NumPy 独立算子实现"]
  SEED["固定种子与 320 日 × 6 证券输入"] --> ORACLE
  ORACLE --> GOLD["179 项末 3 日 golden 与 nullable 输入列"]
  GOLD --> TEST["Rust 读取夹具并执行同一请求"]
  SMALL["小面板手算 expected 与容差"] --> TEST
  TEST --> CHECK["null 位置、特征值、版本与顺序断言"]
```

`operator_golden.json` 的 `columns` 包含因子所需价量、基准及合成三因子；`expected` 每项18个尾值（3日×6证券）。这些三因子仅用来验证 Alpha030 算子，不能注入真实年度研究冒充可用市场输入。

## 重新生成与核验

从仓库根目录执行；需要开发用 Python/NumPy。App/CLI 运行不使用该脚本。先写入忽略目录，核对差异再更新受控 golden，避免直接覆盖已审核参考值。

```bash
mkdir -p .jfquant/audit
cargo run -p analytics-factor --example audit_catalog > .jfquant/audit/catalog.json
python3 crates/factor/testdata/reference_ops.py .jfquant/audit/catalog.json .jfquant/audit/operator_golden.json
cmp crates/factor/testdata/operator_golden.json .jfquant/audit/operator_golden.json
cargo nextest run -p analytics-factor
```

相同环境生成应可复现；`cmp` 失败时先查算子、公式版本、NumPy环境和浮点约定，不能为让测试变绿直接接受新文件。经解释的公式/实现修改才更新 fixture，并在 [FACTOR_AUDIT.md](../FACTOR_AUDIT.md) 记录范围。新增因子实现至少提供手算 fixture 或独立 golden；训练收益不是因子正确性的替代检验。

## 为什么需要本组件

避免因子测试与实现互相复制错误。独立输入和golden输出为公式定义提供外部核对。新增公式必须有可手算或独立计算依据；输出变化需要说明原因，不能只更新快照让测试变绿。

设计依据：2026-10-14接受的当前组件职责、可复现实验与用户研究需求、组件接口及测试。未在代码或证据中确认的历史取舍不推定为既有设计理由；来源、法规或数据格式变化时重新评估。

## 2026-10-14：直接代码读取与分阶段研究

为什么调整：普通市场代码无需先登记身份；独立目录、审核和权威名录下载增加了研究前置负担。特殊历史代码仍需在快照内明确区间，不能将旧证券行情或权益接到另一证券。训练使用价量和标签，到账／交付资料只在实际账户执行时检查。

当前格式：模板9、需求3、准入5、股票池3、公司行动5、规范行情5、系数3、采集7、服务11、研究结果13；快照内来源映射1、训练产物2。工作区和任务记录1、GTJA191公式v4及默认178项顺序保持。旧格式明确拒绝，不自动转换。

```mermaid
flowchart LR
  D[数据快照与少量来源例外] --> M[批量价量掩码与标签边界]
  M --> T[训练与独立模型产物]
  T --> P[测试期预测]
  P --> E[实际订单和持仓检查]
  E --> R[完整结果或未完成原因]
```

验收：nextest覆盖来源日期边界、三档掩码、训练／回测分离及模型保留；Clippy、Rustdoc、格式、组件文档和包内CLI共同检查。生成数据、模型、运行记录和安装包不提交。另一台Apple Silicon/macOS14实机验收继续待完成。

## Alpha158的独立全列参考

为什么另建参考：GTJA的rolling/null语义不能替代固定Qlib定义。alpha158_formulas.json保存固定loader的来源URL/哈希、158项顺序及数据AST；alpha158_expected.json包含85日×3证券全部158列，覆盖短窗口、缺失、零量、常量和并列。reference_alpha158.py用独立Pandas/NumPy实现固定语义，**未直接调用Qlib**，不能把它标为Qlib实际执行输出。运行时不依赖这个脚本或Qlib。

```mermaid
flowchart LR
  S[固定Qlib loader与算子文档] --> A[158项数据AST]
  A --> P[独立Pandas/NumPy参考]
  I[固定合成价量与边界] --> P
  P --> G[全158列golden]
  A --> R[Rust专用Alpha158算子]
  G --> V[逐行值/null/顺序比较]
  R --> V
```

开发重现：`target/collector-build/python/bin/python3 -I crates/factor/testdata/reference_alpha158.py target/alpha158-expected.json`，再`cmp crates/factor/testdata/alpha158_expected.json target/alpha158-expected.json`，最后`cargo nextest run -p analytics-factor`。输出不一致必须解释公式/算子或数值依赖变化，不能直接覆盖夹具让测试变绿。GTJA现有golden和公式v4保持。
