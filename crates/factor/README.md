# analytics-factor

目录导航：[全部组件](../README.md) · [src：目录、AST、执行与隔离](src/README.md) · [examples：带版本AST与审计目录导出](examples/README.md) · [testdata：合成fixture与独立NumPy参考](testdata/README.md)

## 职责与公共接口

注册国泰君安191个canonical ID及独立Alpha158组合；按组合解析公式并在单个worker的内存面板执行，输出保留缺失值的因子宽表。当前版本 `gtja191-paper-2017+jfquant-audit-v4`；版本变化必须重算缓存、重训模型，不能复用v3模型/预测冒充v4。

| 接口 | 契约 |
|---|---|
| FactorRegistry::for_set / FactorSpec | GTJA191或Alpha158的ID、原始目录公式、AST、输入列、版本；lookback由AST组合，requires_recursive_state说明递归历史 |
| unsupported_ids / audit::entries | 12项隔离原因；全部191项的原报告页码、执行状态和验证范围 |
| InMemoryFactorExecutor::compute / compute_streaming | 验证panel、保持请求特征顺序，核对请求公式版本、重复ID，检查取消并以错误停止，不产出部分成功；发送进度；streaming共享分组索引并逐因子回调 |
| Panel::validate | instrument/date/各数值列等长，instrument内按日期计算，重复instrument/date拒绝 |
| next_return_labels | 每证券下一条close收益，尾行null |
| FactorRunArtifact::write_parquet_with_labels | nullable因子、labels、feature order与manifest |

## GTJA191审计结论与执行约定

完整逐项记录见 [FACTOR_AUDIT.md](FACTOR_AUDIT.md)。191项注册不代表191项严格等价或可用：12项因公式/初值歧义隔离，179项可执行；真实研究还缺Alpha030的MKT/SMB/HML，当前选择178项。原始报告、DolphinDB注释目录和DolphinDB函数体之间本身有差异，不能互当数值真值。

v4修正WMA为归一化的`0.9^lag`权重，SIGN(0)=0，HIGHDAY/LOWDAY最新值距离为0，多元REGRESI保留全部预测变量和截距，SMA递推使用M/N。短窗口相关系数需稳定地保留数学上的±1，排名并列采用相对/绝对尺度`1e-12*max(1,abs(a),abs(b))`容差，避免浮点舍入制造名次。RANK/TSRANK输出最大并列名次/n；STD/COV使用样本分母n-1；这些是明确约定，原报告没有完整规定。

滚动算子要求完整且有限的窗口，历史不足或任一缺失输出null，不删除缺失点重排权重。FILTER条件回归例外：在最近252条交易行中选择同时有效的条件观测，保留其条件语义。条件为null时IF输出null，零除/非法幂/非有限结果输出null。模型适配器才将null映射为NaN。

SMA以首个有限输入为种子，缺失点输出null并保留递归状态；不是有限N日均值。lookback表示首个结果的最少观测数，递归项另外依赖种子之前的载入历史，不能把最长数字当完整嵌套窗口。SEQUENCE以持续时间位置实现，每个窗口斜率等价于1..N，不能周期回绕。

MAX/MIN第二参数为正整数时采用目录中滚动窗口的约定，0或表达式参数为逐元素比较；这是原附录MAX(A,B)与公式MAX(X,n)记法不一致时的公开解释。Alpha028保留印刷版第二分母TSMAX(LOW,9)，不偷偷替换成常见随机指标版本。SMEAN/COVIANCE/DELAT、字段VOL/HGIH/L等拼写及已确认标点修复保留在parser；缺窗口、多余参数和非法窗口不再被忽略或默认成identity/MEAN。

Panel可选`__universe_member`列：只有1参与当日横截面排名、缩放、标准化，时间历史仍保留；无此列时全体参与。研究适配器删除缺失价格行会压缩交易时间，该局限见ml README。

```mermaid
flowchart TD
  R[191项原始目录与审计] --> G[明确隔离歧义项]
  G --> A[AST与历史依赖元数据]
  P[nullable面板与历史成分掩码] --> E[worker内算子执行]
  A --> E
  E --> C[按请求顺序的因子缓存及manifest]
```

## 构建与验收

```bash
cargo check -p analytics-factor
cargo nextest run -p analytics-factor
cargo test --doc -p analytics-factor
cargo clippy -p analytics-factor --all-targets -- -D warnings
```

验收覆盖：179条可执行AST与独立NumPy算子参考输出、手算Alpha002、WMA/SIGN/空值/极值距离、多元OLS与奇异矩阵、FILTER回归、嵌套lookback、递归状态、未来值不影响过去、特征顺序、历史成分隔离和12项明确拒绝。参考生成器是合成测试工具，不参与回测，也不认证原始公式解读。

```bash
cargo run -p analytics-factor --example audit_catalog > .jfquant/audit-catalog.json
python -m pip install numpy
python crates/factor/testdata/reference_ops.py .jfquant/audit-catalog.json .jfquant/operator-golden.json
```

测试使用提交的合成fixture `testdata/operator_golden.json`，不联网。更改新因子或算子时必须独立重核期望值，不得用Rust执行器输出直接覆盖golden。

## 为什么需要本组件

将GTJA191定义与执行数据口径分开审计。公式v4和固定特征顺序可以独立于供应商复核。来源转换错误不能通过修改公式掩盖；隔离项保持明确理由，可用因子通过独立fixture或golden输出验证。

设计依据：2026-10-14接受的当前组件职责、可复现实验与用户研究需求、组件接口及测试。未在代码或证据中确认的历史取舍不推定为既有设计理由；来源、法规或数据格式变化时重新评估。

## 2026-10-14：直接代码读取与分阶段研究

为什么调整：普通市场代码无需先登记身份；独立目录、审核和权威名录下载增加了研究前置负担。特殊历史代码仍需在快照内明确区间，不能将旧证券行情或权益接到另一证券。训练使用价量和标签，到账／交付资料只在实际账户执行时检查。

当前格式：模板10、需求3、准入5、股票池3、公司行动5、规范行情5/HiThink6、系数3、采集7、服务14、研究结果14；快照内来源映射1、训练产物3。工作区和任务记录1、GTJA191公式v4及默认178项顺序保持。旧格式明确拒绝，不自动转换。

```mermaid
flowchart LR
  D[数据快照与少量来源例外] --> M[批量价量掩码与标签边界]
  M --> T[训练与独立模型产物]
  T --> P[测试期预测]
  P --> E[实际订单和持仓检查]
  E --> R[完整结果或未完成原因]
```

验收：nextest覆盖来源日期边界、三档掩码、训练／回测分离及模型保留；Clippy、Rustdoc、格式、组件文档和包内CLI共同检查。生成数据、模型、运行记录和安装包不提交。另一台Apple Silicon/macOS14实机验收继续待完成。

## Alpha158为什么使用独立实现

GTJA191的完整窗口、排名及空值语义不能直接替代Qlib Alpha158。`FactorRegistry::for_set`选择一个组合，`alpha158.rs`实现其独立AST及算子，不修改GTJA191 v4及默认178项。Alpha158锁定Qlib提交b87a2c294d364a33fb739359886acffe8ec907d1，158项按原loader顺序编号，公式版本`qlib-alpha158-b87a2c2+jfquant-units-v1`。JFQuant输入明确使用股/元；同一公式中的1e-12常数不因换供应商而改写。

定义依据：[固定loader](https://github.com/microsoft/qlib/blob/b87a2c294d364a33fb739359886acffe8ec907d1/qlib/contrib/data/loader.py)、同提交data/ops.py及data/_libs/rolling.pyx。滚动min_periods=1、样本标准差、线性分位数、平均并列排名、极值首位置、OLS及近零方差保护单独实现。零量、缺失、短窗口、常量与并列在全158列golden中覆盖。golden是独立Pandas/NumPy参考实现，不能称为直接运行Qlib所得；运行时不引入Qlib。

```mermaid
flowchart LR
  P[固定Qlib定义与顺序] --> A[Alpha158专用AST和算子]
  G[GTJA191 v4] --> R[按组合选择Registry]
  A --> R
  R --> F[nullable输出与独立公式版本]
  O[独立参考golden] --> V[158列逐值核验]
  F --> V
```

Alpha158派生定义保留[固定Qlib MIT许可](QLIB_LICENSE.txt)，打包时随App的Resources/Licenses分发；公式源提交及哈希见catalogs/alpha158.json。

## 自定义因子与统一算子（2026-10-16）

`operators::{OperatorRegistry,OperatorSpec,Expression,CompiledExpression,compile_formula}` 是统一入口。
注册表集中定义语义ID、参数类型、别名、输出类型、窗口/空值/历史规则、版本、示例和Rust内核绑定。
GTJA原始公式的历史修复由legacy parser完成，结果经统一注册表校验；新公式严格解析，不自动补括号或修复拼写。
旧GTJA AST与Qlib AST均转换为显式算子调用，再调用保留原数值语义的Rust内核；跨体系子表达式可互相嵌套。

`catalogs/{gtja191,alpha158}.json` 使用共同目录格式1并编译期嵌入，保存有序ID、名称、原文、syntax、可选源AST、
来源及原公式版本。191原字符串逐项迁移，158原AST逐项保留；既有golden不更新。目录原文与AST不是用户可覆盖配置。

用户公式示例：`CLOSE / GTJA_DELAY(CLOSE,20) - 1`；跨体系示例：`QLIB_MEAN(GTJA_SMA(CLOSE,3,1),2)`。
输入限OPEN/HIGH/LOW/CLOSE/VOLUME/AMOUNT/VWAP/RET；算子必须使用GTJA_或QLIB_前缀，大小写不敏感。
支持算术、比较、逻辑和括号；普通运算符沿用GTJA空值传播。窗口必须为1—10000的整数常量，
概率在[0,1]，数值必须有限；公式上限16384字节、用户表达式24层；拒绝未来窗口、脚本和因子引用。
`GTJA_RANK(X)`为当日成员横截面排名，`QLIB_RANK(X,N)`为平均并列的滚动排名；
`GTJA_MIN/MAX`为逐元素比较，滚动用GTJA_TSMIN/TSMAX；`GTJA_SUMIF(X,COND,N)`在统一语法中明确条件在窗口之前。

`selection::{CustomFactorDefinition,FactorRef}` 保存自定义修订的原文、AST、输入、历史依赖、算子版本和定义哈希。
`FactorRegistry::resolve`验证有序引用并支持混合；`selection_version`对混合定义和顺序生成签名，纯内置保留原版本。
格式2因子manifest记录逐因子原文与编译收据、载入历史起点和递归种子规则；正常缺失仍输出null。

```mermaid
flowchart LR
  B[两套只读JSON目录] --> A[统一显式算子AST]
  U[严格用户公式] --> A
  R[统一OperatorRegistry] --> A
  A --> K[保留各体系语义的Rust内核]
  F[固定修订的有序FactorRef] --> K
  K --> O[nullable列 / 原文与算子版本收据]
```

```bash
cargo test -p analytics-factor
cargo test --doc -p analytics-factor
cargo clippy -p analytics-factor --all-targets -- -D warnings
```

验收包括原GTJA/Alpha158全量golden、原AST无损往返、严格语法拒绝、跨体系嵌套和递归元数据。
`testdata/custom_operator_golden.json`为独立手算小序列，解释SMA状态与Qlib缺失处理，不能由执行器输出覆盖。

## 输入依赖窗口

`dependencies::resolve(&FactorExpr)` 从实际 AST 返回每字段的可分离观察窗口，包含递归种子和
横截面成员上下文的位置。嵌套 Delay、Delta、滚动和 Qlib 算子按本身规则传播，不以组合最大
lookback 扩展全部字段。Unsupported 明确报错。该元数据不改变算子数值、null 或递归初始化。
验收：`cargo test -p analytics-factor dependencies`，并保持既有两套 catalogue golden。
