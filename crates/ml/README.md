# analytics-ml

目录导航：[全部组件](../README.md) · [src：公共模型接口、流水线与年度研究](src/README.md) · [examples：原生开发验收与完成结果绘图](examples/README.md)

## 职责

analytics-ml 是 LightGBM C API、训练/预测接口和端到端 pipeline 的边界。默认构建不链接原生库；desktop 和 native example 显式开启 lightgbm-native 与 duckdb-native。

## 公共接口

| 接口 | 契约 |
|---|---|
| DenseMatrix / TrainingSplit | 行主序特征、labels、70/15/15 三个矩阵，缺失特征用 NaN |
| Trainer | 写出 model、manifest、metrics、feature importance；逐轮发 TrainEvent |
| Predictor | 普通预测或 feature contributions |
| pipeline::run_factor_pipeline | market/universe -> factors -> labels -> split -> train -> predictions |
| pipeline::train_from_factor_cache | 从 factors.parquet 独立重训 |
| PipelineEvent | Loading、market rows、factor progress、training iterations、artifact writing |
| PipelineArtifact | 因子缓存、manifest、模型产物、预测路径和 split 行数 |
| backtest::ResearchRequest / run | 显式 JFQuant 下载目录，5年训练/1年验证/次年测试，年度模型、预测、市场费用预设与数据审计（lightgbm-native） |
| load_manifest | 读取模型 manifest JSON |

Pipeline manifest 保留 feature order、training schema hash、formula version、universe、date range、input snapshot hash。LightGBM Dataset、Booster 和 Predictor 都只在 worker 内创建。

## 数据流

```mermaid
flowchart TD
  PARQUET["Parquet与历史股票池"] --> PIPE["pipeline：按日期切分与因子缓存"]
  QLIB["Qlib行情与公司行动版本"] --> YEAR["backtest：年度滚动与严格准入"]
  PIPE --> TRAIN["共享LightGBM训练与预测接口"]
  YEAR --> TRAIN
  TRAIN --> MODEL["有序manifest、模型与样本外预测"]
  MODEL --> TABLE["流水线预览与重训产物"]
  MODEL --> LEDGER["年度原价股份账本与结果schema11"]
```

## 构建与环境

    cargo check -p analytics-ml
    brew install lightgbm
    cargo check -p analytics-ml --features lightgbm-native,duckdb-native
    LIGHTGBM_LIB_DIR=/path/to/lightgbm/lib cargo check -p analytics-ml --features lightgbm-native

`native_paths.rs` 由 ml 和 desktop 构建脚本共用。`LIGHTGBM_LIB_DIR` 非空时必须包含当前目标平台的 LightGBM 动态库；为空或未设置时自动探测 `brew --prefix lightgbm`、标准 Homebrew 目录和 `$HOME/homebrew` / `$HOME/.linuxbrew` 对应路径。不会硬编码开发者个人目录。非空无效路径直接报构建错误；无法探测库时 native 构建最终链接失败。macOS native example 可通过 `DYLD_LIBRARY_PATH` 配置 LightGBM/OpenMP，desktop 自动写入本机 rpath；仅在关闭 native feature 的构建中，trainer/predictor 返回 `BackendUnavailable`。

## 验收

    cargo nextest run -p analytics-ml
    cargo test --doc -p analytics-ml
    cargo nextest run -p analytics-ml --features lightgbm-native,duckdb-native
    jfquant factors --help

native 测试包含 synthetic market/universe 端到端 pipeline 和从 cache 重训；真实数据验收需检查 factors、model、manifest、metrics、importance、predictions 六类产物。

## 中证500年度滚动研究

默认使用**不复权撮合、真实整数股数和公司行动账本**。因子输入使用独立的复权价量视图，成交价用原价；两个视图不混用。先按[storage README](../storage/README.md)独立下载行情、公司行动及详情，核验/补齐缺失的实际支付与上市日期，再执行：

```bash
export JFQUANT_WORKSPACE="$HOME/JFQuantWorkspace"
jfquant templates default --output recipe.json
jfquant research check --template recipe.json --market MARKET_VERSION --actions ACTION_VERSION
jfquant jobs wait CHECK_TASK_UUID
jfquant research run --template recipe.json --market MARKET_VERSION --actions ACTION_VERSION
jfquant jobs wait RESEARCH_TASK_UUID
jfquant jobs results RESEARCH_TASK_UUID
```

`ResearchRequest`显式包含corporate_actions、费用、资金及dividend_withholding_rate，执行仅采用严格原价账本。源冲突或无法解释的训练复权变化会在训练前拒绝，到账日期缺失只在实际执行时校验，写execution_readiness.json。research check只审计而不训练；ready=false表示输入仍未就绪，不能生成完整真实回测。

### 账户与自定义费用

默认最新A股预设：全佣万2.5、每笔最低5元，另收过户费和卖出印花税；初始资金100万元。公开费用只用当前快照统一应用全区间，不恢复历史政策，不输出多档单边成本。账户佣金可配置，见[core README](../core/README.md)。

通过templates default导出完整模板后，编辑initial_cash_cny、market_rules.commission和dividend_withholding_rate；App提供同一字段的表单。以下账户配置用于完整模板对应字段：

```json
{"initial_cash_cny":2000000,"market_rules":{"commission":{"rate":0.0002,"minimum_cny":5,"mode":"gross"}},"dividend_withholding_rate":0}
```

该片段用于说明字段，执行时保存完整模板，并用templates validate检查。

生成预设拒绝覆盖已有文件。JSON允许只覆盖commission（缺省字段用默认），CLI覆盖JSON与参数位置无关。费用以实际CNY订单计，每笔按分四舍五入；整手预算包含最低佣金，未成交无费用。dividend-withholding-rate是显式固定研究扣税比例，默认0为税前股息；不是送红股所得税或个人FIFO持股期补税，不宣称个人税后收益。

### 原价账户和研究边界

模型按Y-6至Y-2训练、Y-1验证、Y测试；训练/验证及IC剔除跨截止日实现的标签。标签为`adjusted_open[t+2]/adjusted_open[t+1]-1`。最多200轮、早停20、学习率.05、31叶、min_data_in_leaf=200、feature_fraction=.8、lambda_l2=10、seed=17、4线程及确定性设置，测试集不选参；保存模型/RMSE/预测一致使用最佳轮数。

行情覆盖2000-01-04—2026-10-12；历史中证500从2007-01-31开始，样本外最早2013，2026为部分年度。不用今天成分回填历史。因子面板保留入选前756交易日预热，横截面按历史入选掩码。复权价量视图按每证券初始归一化factor锚定，成交量还原为股、成交额为元；DBM按原附录`max(open-low, open-prev_open)`，不擅自改成相反符号。早期成分历史不足时允许整列暂为null，finite_counts明确记录，不为了凑可用数使用部分窗口。缺失价格行会压缩因子时间序列，递归SMA预热截断历史；这些仍是研究局限。

v4隔离12条歧义公式，Alpha030另因真实MKT/SMB/HML缺失排除，当前178项用于研究。179个可执行AST通过独立NumPy算子fixture，不能因此宣称原始191式全部严格等价；详见[factor审计](../factor/FACTOR_AUDIT.md)。版本已变，旧189因子v3预测/模型必须重算，不能直接用于本版回测。

原价执行在完整交易日历逐日运行：日终信号下一开盘成交，每5日调预测前50目标等权，先卖后买，遵守整数股数、板块最小数量/增量、分价位及T+1。现金不足整手时留现金。账户与调仓时钟跨模型年份连续，最后一日信号可交给下一年；不年度重置、不估算虚拟强制平仓。收盘NAV包含应收权益，基准为同日中证500**收盘价格指数**收益（无分红）。

登记日收盘确定股数，除权日确认应收现金/红股，实际到账日增加可用现金，上市日增加可售整数股；登记后卖出旧股仍保留权益，除权日新买入没有旧权益。停牌无报价不成交，并在除权日调整旧估值，再计应收，避免重复收益。缺支付/上市日、来源冲突、配股/未知因子跳变、零碎股实际分配不明、重叠未上市分配均拒绝。因子变动按现金/送转理论除权参考值对账（容差0.2%，源float32及分价位），不从factor反推分红事件。

涨跌停仍为最新常规板块比例减0.5个百分点缓冲，缺精确交易所限价、上市特例、逐笔流动性、冲击、退市清算；不是完整实盘撮合复现。年化252日，Sharpe无风险0，回撤包含初值1；IC/RankIC为至少30只股票的每日均值、并列平均，标签期限不同于5日组合期限。

### 当前研究产物（schema_version=11）

backtest::RESEARCH_SCHEMA_VERSION=9；validate_result返回可识别的UnsupportedResultFormat，报告生成、平台读取与只读导入共用校验。仅接受当前公式、raw_integer_shares、来源/双签名、完整执行配置、年度报告及汇总字段。旧结果不转换、不重新计费；保留磁盘文件，读取时返回unsupported_result_format。

| 文件 | 契约 |
|---|---|
| execution.json | raw_integer_shares、完整费用预设、初始资金、固定股息税率、公司行动文件SHA256及研究配置 |
| execution_readiness.json | 公司行动/因子跳变校验ready、错误原因、区间缺支付/上市日和来源失败统计；训练前生成 |
| data_audit.json / factor_audit.json | 来源、覆盖、178项特征顺序、公式版本及双签名；191项审计、隔离原因和验证范围 |
| factor-cache/YYYY.json + .bin | 行/标签年份/有序特征/有效数；row-major little-endian float32，NaN为missing；raw成交另从独立行情恢复原价 |
| fold-NNNN/model.txt / manifest.json | schema hash、特征顺序、公式版本、来源签名、research_execution；同worker持有native句柄 |
| fold-NNNN/predictions.csv / metrics.json / feature_importance.json | 样本外预测、最佳轮指标及重要性 |
| account-through-DATE/ledger.json | 折末持续现金、整数股、可售股、应收权益和日期状态；只作审计，不作为任意断点恢复接口 |
| fold-NNNN/daily.json / report.json | 单预设日收益/费用/换手/持仓/受阻订单；费用仅来自实际模拟成交，不含估值平仓字段 |
| annual_reports.json / summary.json | 年度与连续账户汇总，schema11及双签名 |
| report.md / nav.csv | 可读报告及日期、策略净值、价格指数净值 |

特征签名覆盖数据、顺序、v4公式和价量适配约定；执行签名另含费用、初始资金、年份、模式、税率和完整公司行动文件哈希。改变金额/日期/来源会改变执行签名；已有不同签名输出拒绝覆盖。因子manifest的lookback由AST组合，requires_recursive_state单列说明，不能把递归历史表示成有限窗口。

```mermaid
flowchart TD
  D[独立行情及历史成分] --> F[复权因子视图和v4审计]
  F --> T[5年训练及1年验证]
  T --> P[逐年样本外预测]
  D --> R[恢复不复权价格]
  C[验证的登记除权到账上市事件] --> L[持续整数股份账本]
  R --> L
  P --> L
  U[用户费用及固定研究税率] --> L
  L --> A[收盘NAV及年度报告]
```

### 验收

```bash
export DUCKDB_LIB_DIR="$PWD/target/debug/deps"
export DYLD_LIBRARY_PATH="$DUCKDB_LIB_DIR:$(brew --prefix lightgbm)/lib:$(brew --prefix libomp)/lib"
cargo nextest run --workspace --features analytics-ml/lightgbm-native
cargo clippy --workspace --all-targets --features analytics-ml/lightgbm-native -- -D warnings
RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --features analytics-ml/lightgbm-native
cargo fmt --all -- --check
```

DuckDB未下载时先用`DUCKDB_DOWNLOAD_LIB=1 cargo build -p analytics-desktop`。不要用all-features把互斥的bundled-cmake源构建也打开。macOS直接运行workspace统一特性构建的example时需含DuckDB动态库路径。

测试覆盖跨年真实股数/连续资金、登记日/除权日差异、延迟现金和红股、T+1/整手与手续费、未知factor跳变拒绝、缺失日期/零碎股拒绝，及预测对齐、费用口径、早停和UI错误恢复。原价综合fixture手算：登记100股原价10元，每股现金1元加0.5股，除权价6元；应收现金100元+红股50股保持资产一致，不提前释放流动性。

绘图工具`python crates/ml/examples/plot_research.py RUN`只读取完成的nav.csv，不参与策略。数据/模型/报告/图片均写已忽略data/或.jfquant/，不提交。

## 平台任务接口与验收

ResearchConfig提供训练/验证年数、top_n、rebalance_days、有序factor_ids及crate根路径TrainingConfig；默认5/1/50/5和既有模型参数。模板拒绝重复、隔离及缺输入因子。run_with_context接受TaskContext；run是同一实现的默认上下文包装，供模块调用，不是旧命令入口。年度准备、逐因子、训练迭代、逐日撮合/对账检查取消；训练进度线程在返回前join。

App、jfquant research和序列化TaskRequest共用workflow适配器。流水线使用PipelineRequest，通过jfquant factors run --request pipeline.json提交；后台保存新UUID的factors.parquet、模型、manifest、指标、重要性与预测。缓存重训通过jfquant model train --cache PATH；参数文件仅含parameters映射与early_stopping_rounds，不保留旧类型重导出。

nextest覆盖标准CLI与公共API/egui的冻结参数、特征顺序、双签名、跨年账本、分红送转、费用及年度结果一致；旧格式/旧口径和不完整结果明确拒绝且文件不变，真实缺项停止在训练前。安装包运行不用外部Python或Cargo，绘图脚本仅作开发时的可选离线工具。

## 动态股票池与 Python 采集接口

当前模板schema 6、研究结果schema11、服务协议9；公式仍为v4。模板增加 `pool_version`、`benchmark` 和 `pool_exit_policy`；默认 `builtin:csi500` / `SH000905`，沪深300模板使用 `builtin:csi300` / `SH000300`。自选和集合组合引用不可变pool版本，冻结成员区间、人工日期调整及输入哈希；模型manifest和缓存签名包含这些研究输入。原schema结果明确拒绝，历史状态和日志仍可查看，不转换旧文件。

```mermaid
flowchart LR
  CLIENT["App / jfquant"] --> SERVICE["协议9公共服务"]
  SERVICE --> POOL["保存/预览不可变股票池"]
  SERVICE --> COLLECT["Rust监督内置Python双源采集"]
  COLLECT --> SNAPSHOT["验证后的Parquet版本与公司行动候选"]
  POOL --> FREEZE["模板6：池、基准、退出策略、签名"]
  SNAPSHOT --> READY["原价、历史覆盖、公司行动准入"]
  QLIB["现有Qlib读取器"] --> READY
  FREEZE --> READY
  READY --> RESEARCH["统一日行情、年度模型、持续整数股账户"]
  RESEARCH --> RESULT["结果11：成员变化、退出/阻碍、账本、费用"]
```

`pools create/validate/import/export/list/versions/preview` 使用公共定义及解析器；`data collect --provider akshare|yfinance --kind market|actions` 提交后台任务，使用 jobs wait 显式等待。`run --request` 可提交完整公共请求。App提供来源、采集日期/证券、股票池JSON编辑、日期预览、退出策略及失败采集恢复按钮。包内Python和依赖只用于采集，不需要外部Python；日常操作不调用Cargo。

```bash
jfquant templates default --universe csi300 --output csi300-template.json
jfquant pools validate --definition pool.json
jfquant pools create --definition pool.json
jfquant pools preview pool-0123456789abcdef --market market-0123456789abcdef --date 2026-10-12
jfquant data collect --provider akshare --start 2004-01-01 --end 2026-10-13 --instruments SH600000,SZ000001,SH000300 --benchmarks SH000300
jfquant data collect --kind actions --provider akshare --start 2004-01-01 --end 2026-10-13 --instruments SH600000,SZ000001
jfquant research demo-parquet
```

示例ID须换为实际版本。下载可恢复验证过的证券；重新提交时参数必须与owner一致。临时/缺失输入不会自动换源、补字段或启动训练。原价撮合不使用Yahoo拆股调整价；指数历史成员缺失停止训练，支付/上市日期在真实账户执行时校验。2010样本外的5+1窗口需要2004—2009完整历史池及行情，不能以当前成分回填。

开发验收先运行 `crates/app/packaging/build-collector.sh`，再执行workspace nextest、严格Clippy/Rustdoc和格式检查。解释器、依赖、数据、模型、缓存、日志、运行记录及DMG均留在忽略目录。另一台Apple Silicon/macOS14实机验收待完成。

年度准入在因子缓存和训练之前核验统一行情的独立系数、锚点、历史事件与执行权益；错误按证券进入 `execution_readiness.json.factor_basis_issues / adjustment_issues` 并列出 `affected_factor_ids`。供应商后复权报价仅用于留证，不从报价比值猜系数。

## 显式价量与schema11

年度研究令k=F/F0：OHLC乘k，研究量除k，成交额保持元，VWAP由实际金额/真实股数乘k。公式v4、默认178项与特征顺序不变。历史对账覆盖训练、验证、756交易日预热及测试；每个事件须有跳变，每次跳变须有解释。理论参考价对独立系数保留0.2%阈值，不放宽；显式源无事件跳变采用1e-12数值阈值，Qlib float32噪声仍为2e-5。缺参考价、逆向系数、配股或未解释特殊分派停止训练。

execution.price_inputs和price_input_sha256保存规则版本、系数文件哈希、来源、覆盖、锚点和精度，进入因子签名及每fold模型manifest。schema11读取、报告和只读导入共用校验：不仅要求哈希匹配，还要求合法来源、日期覆盖、正有限锚点和明确转换规则；重算哈希不能绕过缺字段校验。旧结果/缓存不转换。通用Parquet流水线按调用者提供的列计算，正式年度研究经DailyDataset生成价量，不能直接把供应商adj证据列当正式输入。

## 任意日期研究（当前接口）

ResearchRequest.schedule替代首末年份；ResearchConfig只保存模型/组合配置。single、rolling、manual解析为有序ResolvedFold，训练/验证按真实label_end日期剔除越界标签；没有验证集不建立验证矩阵且关闭早停。预测保留没有后续标签的尾日，IC和预测CSV不使用越界标签。

账户从首折测试日至末折测试日持续推进。间隔日不产生新选股信号，继续估值、权益结算和所选股票池退出规则；连续折可执行上一日已冻结信号，空档不保留过期信号。因子缓存仍按年度分文件以复用计算，但样本选择、模型训练和执行边界均按日期。未知历史成员的截面mask为null，不等同空池，也不回填当前成分。

结果schema11：fold_reports为每折实际测试日结果，annual_reports为自然年汇总（含折间账户日），daily.json/nav.csv为完整连续账户日；fold-NNNN/gap_daily.json单独保存空档。account-through-DATE/ledger.json冻结当期股数、现金和待交付权益。认购手续费独立列为corporate_action_cost_cny，普通买卖费用仍使用市场规则。日期调度、执行政策、公司行动、价量和来源映射签名进入缓存/模型/研究来源。

准入同时列出来源/成员、系数、历史事件、账户结算、后继证券、基准及样本问题。历史参考完整而结算缺失的特殊事件可声明Historical；若它进入实际账户区间则拒绝。换股对价、差异化参考、配股和退市按类型执行，缺少申购/上市/资格/零碎股/估值证据继续停止，不能自动删除股票或改用旧近似口径。

## 为什么需要本组件

将特征计算、模型和真实账户连接成可审计研究。研究结果需要区分预测效果与可交易收益。按折训练防止未来标签泄漏，账户跨折持续，原价撮合和公司行动结算避免复权碎股产生虚假收益；缺项时不能靠模型处理missing来掩盖。

设计依据：2026-10-14接受的当前组件职责、可复现实验与用户研究需求、组件接口及测试。未在代码或证据中确认的历史取舍不推定为既有设计理由；来源、法规或数据格式变化时重新评估。


## 三级研究模式：为什么允许筛选

决定日期2026-10-14，依据是用户接受的三级模式计划。正常预热空值、来源缺失和错误数据是不同问题：全部拒绝会浪费可用样本，全部放行又会混淆因子和真实权益。默认严格检查；另外提供“允许部分数据缺失”和“只用数据合格的股票”。模式是研究输入的一部分，不能作为训练失败后自动切换的兜底。

第二档保持原成员，纯缺失传播null；第三档派生每日合格成员，不改原始成员或证据。不能看到后来缺价而倒删早期股票。账户仍采用原价、整数股、T+1和连续权益；未裁定冲突、损坏、基准缺项及剩余必要依赖仍阻断。默认有效因子比例0表示至少一项有效，最低有标签样本1只表示可执行，不表示样本在统计上充分。所选因子整列无有效值不会被自动删除。

ResearchRequest.quality_policy使用公共策略。[quality.rs](src/backtest/quality.rs)生成quality.json：格式1、原成员、每日合格成员、日期压缩的处理记录及每日数量。因子计算前确定截面；训练/验证/预测的样本掩码随后生成，预测不要求未来标签。sample_masks/fold-NNNN-{train,validation,prediction}.json保存与因子缓存行序对应的bool掩码及仅含日期/数据代码的行序哈希；sample_selection.json保存数量、相对路径及内容签名。标签数值不进入行序标识，避免Qlib float32精度影响成员对照。

临时计算仍在.admission-factor-staging。准入、最低标签样本及每列有效值检查通过后才发布factor-cache和调用训练器。缓存签名、模型manifest及schema11结果冻结策略、资格、掩码及其哈希，离线读取重新校验；第三档报告明确写“经数据筛选的研究股票池”。执行检查仅涉及实际订单、持仓和待结算权益；候选缺资料不买入，实际权益无法解释时停止回测，保留已完成模型，不读取未来缺项倒删过去候选。

验收：`cargo nextest run -p analytics-workflow quality_tests`，覆盖原价股份账本、同输入三档、未来缺项、整列无效和源格式对照。

```mermaid
flowchart LR
  P[模式与门槛] --> C[结构化数据检查]
  C --> U[原股票池或每日合格池]
  U --> F[因子与nullable输入]
  F --> M[按折样本掩码]
  M --> G[训练剩余依赖门禁]
  G -->|通过| T[冻结签名后训练]
  G -->|未通过| E[原因及处理清单]
```

## 2026-10-14：直接代码读取与分阶段研究

为什么调整：普通市场代码无需先登记身份；独立目录、审核和权威名录下载增加了研究前置负担。特殊历史代码仍需在快照内明确区间，不能将旧证券行情或权益接到另一证券。训练使用价量和标签，到账／交付资料只在实际账户执行时检查。

当前格式：模板6、需求3、准入4、股票池3、公司行动4、规范行情5、系数3、采集6、服务9、研究结果11；快照内来源映射1、训练产物1。工作区和任务记录1、GTJA191公式v4及默认178项顺序保持。旧格式明确拒绝，不自动转换。

```mermaid
flowchart LR
  D[数据快照与少量来源例外] --> M[批量价量掩码与标签边界]
  M --> T[训练与独立模型产物]
  T --> P[测试期预测]
  P --> E[实际订单和持仓检查]
  E --> R[完整结果或未完成原因]
```

验收：nextest覆盖来源日期边界、三档掩码、训练／回测分离及模型保留；Clippy、Rustdoc、格式、组件文档和包内CLI共同检查。生成数据、模型、运行记录和安装包不提交。另一台Apple Silicon/macOS14实机验收继续待完成。

## 训练产物为什么可以复用

training.json格式1冻结原请求、实际折、特征顺序、因子与训练签名，以及各折model.txt/manifest.json字节哈希；artifact_signature核对描述本身。独立回测先重新核验模型、行情版本与训练期间除权参考，再加载模型预测，绝不调用训练器。支付日修复不改变历史研究价量，允许引用新执行版本；修改系数、现金分红除权参考或训练来源必须重训。

回测按实际订单检查资料，不扫描全部未来候选结算。拒单写order_events.jsonl；持仓或应收权益缺依据时保留partial_daily.json及account_checkpoint.json，summary.json仅在完整执行结束后生成。全程无成交时backtest_status=no_valid_trades，不能当作策略效果验收。
