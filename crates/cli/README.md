当前采集体系：HiThink（Rust REST主源）、通达信（Python SDK备用）、独立巨潮公告。服务协议24、采集协议13、来源策略2；接口、价格口径、构建和验收见[采集组件](../storage/collectors/README.md)。

# analytics-cli / jfquant

独立clap命令行客户端，和App共用同一工作区服务。stdout只输出JSON，长任务立即返回
`task_id`；jobs命令用于等待、读取状态、日志、结果和取消。无需打开GUI或运行Codex。

```mermaid
flowchart LR
 CLI[jfquant命令与JSON] --> C[workflow Client]
 C --> S[Unix socket工作区服务19]
 S --> W[数据 / Rust因子 / Python研究worker]
 W --> R[记录与结果16]
 R --> CLI
```

```bash
crates/app/packaging/build-collector.sh
DUCKDB_DOWNLOAD_LIB=1 cargo build -p analytics-cli
jfquant workspace create "$HOME/JFQuantWorkspace"
export JFQUANT_WORKSPACE="$HOME/JFQuantWorkspace"
jfquant research demo
jfquant jobs wait TASK_UUID
jfquant jobs results TASK_UUID
jfquant templates default --output recipe.json
jfquant research check --template recipe.json --market MARKET_VERSION --actions ACTION_VERSION
jfquant research train --template recipe.json --market MARKET_VERSION --actions ACTION_VERSION
jfquant research backtest --training TRAINING_TASK_UUID --actions ACTION_VERSION
jfquant query run --sql 'SELECT 42 AS answer'
```

`research run`执行完整研究；`train`和`backtest`分别运行独立阶段。因子、标的与工作流
仍可独立保存和引用，参数格式以`--help`及内置模板为准。`run --request`接受与App相同
的公共请求，不执行任意Python脚本。

研究使用包内LightGBM和AKQuant，不再链接Rust LightGBM。分红、拆股采用事件日口径，
仅报告执行费用合计；已识别特殊事件警告并跳过，继续发布含披露的收益；普通事件实质冲突或映射失败保留模型并返回阻断诊断。
无有效交易与数据阻断分别展示。详细能力边界见[研究worker](../workflow/research/README.md)。

退出码：成功0，参数错误2，数据阻断3，忙碌4，等待超时124，取消130，其他失败1。
关闭客户端不终止后台，取消由同一服务执行。安装包CLI位于
`/Applications/JFQuant.app/Contents/MacOS/jfquant`，无需Cargo、外部Python或Homebrew。

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-cli
DUCKDB_DOWNLOAD_LIB=1 cargo clippy -p analytics-cli --all-targets -- -D warnings
```

验收覆盖CLI与App公共结果、训练复用、格式隔离、取消、服务恢复及离线读取。
生成数据、模型、日志、凭据及安装包不提交。命令定义和接口目录见[src/README.md](src/README.md)。

`research backtest --training <ID> --actions <ID> --template <PATH>` 可复用模型并更改账户参数。
可选模板仅覆盖资金、费用、持仓数、调仓周期和退池策略，不改变模型或训练窗口。

## 回测复用与前置阻断

`research backtest --training ID --actions VERSION` 在因子/预测前完成静态执行准入，失败清单包含阶段、证券、日期和稳定错误码。`stages.json` 和预测清单区分已完成产物；结果格式16中的未执行折指标为null。命中模型与输入均相同的预测缓存时直接执行账户，费用与持仓数量调整不重训。历史任务日志仍可读，旧结果不按新格式解释。

验收：`DUCKDB_DOWNLOAD_LIB=1 cargo test -p analytics-cli`；真实复现命令及缓存数据流见workflow README。

`data reconcile-actions --actions VERSION --evidence-task UUID [--evidence-task UUID] --market VERSION`
重放已保存证据，发布新的格式6公司行动版本；可省略证据任务或行情，缺失依据保持未解决。
App调用同一`ReconcileActions`任务。`jobs results`提供普通问题数、未模拟事件警告数与核对回执路径。
特殊事件不弹确认；回测成功仍输出收益和`skipped_events`，普通冲突仍返回数据阻断。

## 原生证券分片与覆盖

`data refresh --workers 3` 使用Rust公共分片获取（1—8，默认3）；`--resume TASK`恢复相同计划，`--reuse-shards-from TASK`复用完成且窗口匹配的分片。复用后采集器重新核验身份和所有哈希。CLI不自行搬运或拼接两家行情。

`data coverage --refresh TASK [--inspect-bars]`是可在采集中调用的只读RPC。输出来源计数、缺失证券、剩余数；可选输出逐日/年度历史成员覆盖。`research_ready=null`，正常准入仍必需。全部行情查询完成后才提议明确减去缺失证券的股票池定义，调用者显式保存后才能使用，结果须披露可用样本偏差。

```sh
jfquant data coverage --refresh TASK_ID --inspect-bars
jfquant data refresh --template workflow.json --market MARKET_ID --workers 3 --reuse-shards-from TASK_ID
DUCKDB_DOWNLOAD_LIB=1 cargo test -p analytics-cli
```

服务协议24；升级时先等待或取消旧版本任务，再停止旧服务。旧协议客户端不能连接新协议服务。采集协议13与公司行动格式7同步升级；特征顺序和公式版本未改变。

分区位于不同本地工作区时，可用`--sources-manifest sources.json`汇总覆盖，或`--reuse-shards-manifest sources.json`复用；JSON数组元素为当前工作区任务UUID字符串，或`{"workspace":"/absolute/workspace","task_id":"UUID"}`。只读验证原生任务、同一不可变基础数据/窗口/基准及证券子集；不修改来源工作区。尚有分区待检查的证券不被误列为缺失。复用仍由采集器重新验证全部身份及哈希。

完整通达信检查点恢复由Rust监督器直接验证原始完成记录并重建派生元数据，不启动空待下载Python请求；原日历/完成证据保留。回归fixture验证无进程启动、日历不变及证据篡改拒绝。

仅补行动：`jfquant data refresh --mode actions --template workflow.json --market MARKET_ID --workers 3 --reuse-shards-from SOURCE_TASK`。该模式保持行情不变，覆盖报告将行情标为not_requested。


`jfquant research verify --task <UUID>`通过只读`VerifyResearch` RPC调用Rust结果一致性复核，覆盖收益复利、逐日权益、费用对账和估值行无成交；返回产物哈希及仍未模拟的事件计数，不修改任务或重训。`jfquant data inspect-actions --actions <id> --instrument SH600000`新增`stock_status`，展示已验证的停牌日期和能力/证据范围；null或空记录不代表完整历史或从未停牌。接口实现与验收见[工作流README](../workflow/README.md)。

`data inspect-actions --actions VERSION --instrument SH600030`沿用异步任务，`jobs wait TASK_UUID`的`results.details.structured_fields`给出GP41/43与配股参考的字段证据及缺口；旧数据未采字段为null。`data revise-actions`支持版本2结构化引用，完整请求契约见[公司行动README](../storage/src/corporate_actions/README.md)。

已发布预测对照入口：`jfquant research replay-predictions --source SUCCESSFUL_BACKTEST_UUID --actions ACTION_VERSION --participation abandon --attempt first_day --insufficient-cash partial`。可选 participate、last_day、all_or_abandon、stop；默认放弃认购；主动选择参与时，默认首日一次、现金内部分认购。该入口验证原任务及预测身份，不加载旧模型，不重训。参与认购时，实际持仓条款不全则停止并输出字段缺口。

依赖升级后的 `research verify` 保持旧 SHA-256 身份核验；`research replay-predictions` 可复用旧环境发布的预测并记录新执行环境，旧模型直接复用仍受依赖签名检查。默认配股策略继续为 abandon。

## 单因子有效性批量测试

`jfquant --workspace /path/to/workspace factors evaluate --request evaluation.json` 提交服务协议24
的独立测试任务，后续沿用jobs查询、取消和结果读取。请求示例（路径相对工作区）：

```json
{
  "input": {
    "factor_set": "alpha158",
    "market_path": "data/market",
    "universe_path": "data/universe",
    "universe_id": "csi500",
    "date_range": {"start": "2020-01-01", "end": "2025-12-31"},
    "factor_ids": ["alpha158_001_KMID"],
    "output_dir": ".jfquant/runs"
  },
  "horizon": 5,
  "quantiles": 5,
  "min_samples": 30
}
```

服务会把输出固定在本次任务目录；无需训练参数。报告的统计口径和null规则见
[workflow](../workflow/README.md)。构建及验收：
`DUCKDB_DOWNLOAD_LIB=1 cargo test -p analytics-cli`。

## 自定义公式与算子目录（协议24）

```bash
jfquant --workspace /path/to/workspace factors operators --query QLIB_MEAN
jfquant --workspace /path/to/workspace factors library
jfquant --workspace /path/to/workspace factors validate --definition draft.json
jfquant --workspace /path/to/workspace factors save --definition draft.json
jfquant --workspace /path/to/workspace factors preview --definition draft.json
jfquant --workspace /path/to/workspace factors archive --id custom_<UUID>
jfquant --workspace /path/to/workspace factors save-group --definition group.json
jfquant --workspace /path/to/workspace factors archive-group --id <组UUID>
```

`draft.json`示例：

```json
{
  "id": null,
  "expected_revision": null,
  "name": "20日动量",
  "description": "复权收盘价的20条观测变化",
  "formula": "CLOSE / GTJA_DELAY(CLOSE,20) - 1"
}
```

保存返回`saved_factor`和最新库列表。修改时使用返回ID和expected_revision；保存产生新修订。
Preview返回异步任务ID，沿用jobs wait/results/cancel，结果明确为合成样本。

混合评估/流水线请求在input/PipelineRequest中增加`factor_refs`；每项为
`{"source":"builtin","factor_set":"alpha158","id":"alpha158_001_KMID","formula_version":"qlib-alpha158-b87a2c2+jfquant-units-v1"}`
或`{"source":"custom","definition":<saved_factor对象>}`。其ID顺序必须与factor_ids完全一致。
自定义组的profile使用格式2及同样的factor_refs；请保留完整保存响应中的定义与哈希。
旧factor_set/factor_ids请求继续支持，原始因子目录不可通过库接口覆盖。

验收：`DUCKDB_DOWNLOAD_LIB=1 cargo test -p analytics-cli custom_formula_library_mixed_evaluation_training_and_revision_receipts`。

## 独立模型训练与 B 股票池回测

新命令使用服务协议24；保留原 `research train/backtest/run` 的配置与执行方式。
新训练计划格式1没有测试日期；回测计划独立指定 B、日期和行情。
App研究页“独立训练与跨池回测”可导入、编辑和导出相同 JSON。

```bash
# recipe.json 为已有研究配置；导出后编辑 fit.json 的 A 池和训练/验证日程。
jfquant templates fit --template recipe.json --output fit.json
jfquant research fit-inputs --plan fit.json --market A_MARKET --actions A_ACTIONS
jfquant research fit --plan fit.json --market A_MARKET --actions A_ACTIONS
jfquant jobs wait TRAIN_TASK_UUID
jfquant research models --training TRAIN_TASK_UUID

# 生成后编辑 B.json 的 pool_version；账户参数取自 recipe.json。
jfquant templates model-backtest --template recipe.json --training TRAIN_TASK_UUID \
  --market B_MARKET --actions B_ACTIONS --start 2020-01-01 --end 2021-12-31 --output B.json
jfquant research backtest-inputs --plan B.json
jfquant research model-backtest --plan B.json
jfquant jobs wait BACKTEST_TASK_UUID
jfquant jobs results BACKTEST_TASK_UUID
```

上述异步命令返回 `task_id`，预检/模型查询也通过 `jobs wait` 读取。
`fit-inputs` 和 `backtest-inputs` 添加 `--prepare` 可采集公司行动候选，添加
`--announcements` 改采公告；采集结果仍需原有来源/事件核验，并不自动代表数据就绪。
两侧预检返回证券、历史范围、依赖和质量诊断，用于行情采集；不会隐式覆盖已有行情版本。

`models` 默认 `{"mode":"rolling"}`：每天使用已结束训练/验证且最新的模型；
指定 `{"mode":"single","model_id":"查询所得ID"}` 固定一个模型。
模型切换计划在提交时冻结。更换 B、回测日期不重训，也不要求 A 原行情仍可访问。
首日无可用模型明确拒绝。报告区分“跨池（含曾见）”和“完全未见标的”，并提供同一账户分组贡献。
训练产物5、结果19；旧产物能否独立复用取决于原签名覆盖的样本和日历证据，不自动推断。
