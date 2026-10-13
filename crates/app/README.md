# analytics-desktop

## 职责

macOS-first egui shell for local Parquet analysis, SQL, Alpha191 factors, and LightGBM training. UI state is kept in state.rs, visual rules in theme.rs, panel views in panels.rs, and all long-running work in worker.rs.

## 层级结构

| 模块 | 责任 |
|---|---|
| main.rs | 创建 eframe、安装主题、布局导航/顶部/中央/底部 |
| theme.rs | system light/dark preference、语义色、间距、CJK font |
| state.rs | 表单状态、校验、分页 cursor、WorkerEvent reducer |
| panels.rs | 数据、查询、因子、建模四个面板 |
| worker.rs | 单线程持有 DuckDB connection 和 LightGBM handles |
| analytics-ml::pipeline | 组合市场加载、因子、训练、预测 |

## Worker 协议

| 方向 | 消息 | 作用 |
|---|---|---|
| UI -> worker | ScanDataset | Parquet footer 扫描 |
| UI -> worker | DescribeQuery | 查询 schema |
| UI -> worker | RunQuery | 临时结果快照上的隐藏行号分页 |
| UI -> worker | ExportQuery | Parquet/CSV 导出 |
| UI -> worker | RunPipeline | 完整因子训练流水线 |
| UI -> worker | TrainFromCache | 从已有因子缓存重训 |
| worker -> UI | Started/Progress/Finished/Failed | 任务生命周期 |
| worker -> UI | Unavailable | 后台退出、panic 或启动失败；禁用新任务，提示重启 |
| worker -> UI | DatasetScanned/QueryDescribed/QueryPaged/Exported | 数据 payload；扫描结果携带实际 root，避免被运行中编辑的路径误标注 |
| worker -> UI | PipelineFinished/TrainingFinished | 产物、metrics、importance、预测预览 |

worker 线程独占 DuckDB 和 LightGBM 句柄。UI 只发送可移动的 request、读取 plain-data event，并通过 Arc<AtomicBool> 请求取消。任务事件带 UUID，reducer 会忽略过期 payload。

`WorkerHandle::send` 返回 `Result<(), String>`，命令通道断开时由面板统一显示后台不可用提示。UI 同时检测事件通道断开，结束当前任务等待、将错误路由到所属面板并清除取消句柄；重复断开不重复写日志。

发送成功后 UI 立即登记任务并禁用任务按钮，不等待异步 `Started` 到达，防止快速重复点击入队。后台 `Started` 只更新同一任务的状态文字，不清空已有进度。

## 后台异常与恢复

| 错误类型 | 行为与恢复方式 |
|---|---|
| SQL、文件 I/O、Parquet/schema、因子、训练返回 `Err` | 回传 `Failed`，显示对应面板错误，后台继续接收任务；修改输入后重试 |
| 任务中的 Rust unwinding panic | 任务边界捕获，回传包含原因的 `Failed`；停止接收任务，在所属线程释放原生句柄，再回传 `Unavailable` |
| 任务边界以外的后台 panic | 线程入口捕获，回传 `Unavailable`；存在活动任务时按其类型显示错误并结束等待 |
| 线程启动失败、命令发送失败、事件通道断开 | 显示后台不可用，禁用扫描/执行/Describe/分页/导出/因子运行/训练操作 |

后台不可用时，顶部提供“重启后台”。点击后创建新的 worker 和新的命令/事件通道，清除不可用状态、分页游标与取消句柄；旧事件接收端被丢弃，不能污染新的任务。原有 SQL、路径、参数和已显示的结果保留，需要重新执行操作。后台不可用期间仍可切换面板和编辑表单；迟到的任务事件不会使已停止的后台重新变成“运行中”。

两层 panic 边界使用 `catch_unwind`/`AssertUnwindSafe`，捕获后不继续使用可能已不一致的 mutable/native 状态。普通错误仍使用 `Result` 传播。此兜底针对 Rust 的 unwinding panic，不覆盖 `panic=abort`、原生库段错误、OOM/系统强制终止或 UI 线程 panic；Rust panic hook 仍会输出诊断，调试器的 Rust panic 断点仍可暂停。[Rust catch_unwind 文档](https://doc.rust-lang.org/std/panic/fn.catch_unwind.html)说明了捕获边界。

恢复不会自动回滚或删除任务失败前写出的文件；重新运行因子/训练前请检查原输出目录，或指定新的输出目录。

## 四个面板

1. 数据管理：扫描文件、行数、列数、schema；对选中文件运行标准行情 schema 校验。
2. 查询工作台：SQL editor、Describe、100/500/2000 行分页、排序列、下一页 cursor、Parquet/CSV 导出。
3. 因子工作台：路径、universe、日期和 191 因子搜索/多选；运行流水线并显示迭代日志和产物。
4. 建模：选择 factors.parquet、设置迭代/学习率/叶节点/早停参数，查看 metrics、feature importance 和 predictions 预览。

### 查询与分页

首次执行将完整查询结果及隐藏行号保存在 DuckDB 连接的临时表中，后续页面在同一快照上使用行号游标。默认按所有结果列升序排序；选择排序列时该列优先，其余列用于同值排序，NULL 始终排在末尾。重复排序值、完全相同的行、NULL、中文/空格列名均可翻页；隐藏行号不出现在结果表和导出中。列名冲突时自动调整隐藏行号名称。

修改 SQL、每页行数、排序列或有效排序方向后，旧游标禁用，需重新执行查询；后台也校验游标所属的查询。开始新查询会清空旧结果和旧游标，查询失败不会继续显示旧结果。排序列可选“（未选择）”恢复默认，原排序列不存在时提供错误提示。重启后台也使旧游标失效。

临时快照保证源文件变化和 `random()` 等表达式不会在翻页时改变结果；新查询替换旧快照，后台退出时释放快照。代价是首次执行需要计算和保存完整结果，大数据查询建议显式限定日期、列和 `LIMIT`。导出按编辑器 SQL 重新执行，导出结果可能因源数据或随机表达式变化而不同于快照。SQL/导出/扫描当前不支持取消，取消按钮禁用并显示提示；因子流水线和训练支持协作式取消。

执行查询、Describe 和导出失败时，worker 通过 `Failed` 事件回传错误，查询面板顶部显示红色错误区域和可选择复制的原始诊断。找不到 Parquet 文件时提供检查路径/准备行情数据的提示；重试任务开始后清除旧错误。SQL 语法错误或文件缺失不终止 worker，可以修改 SQL 后继续执行。VS Code 的桌面启动配置会在启动前禁用 C++ throw/catch 断点，避免 DuckDB 内部已捕获的异常阻断错误回传；保留 Rust 源码和 panic 断点，详见 [.vscode README](../../.vscode/README.md)。

训练表单要求学习率为有限正数，迭代次数/叶节点/叶内样本数为正整数，叶节点至少为 2；早停留空关闭，非空值必须是 u32。过期任务的失败或完成事件不会改变当前任务状态或取消句柄。

取消提示依据 pipeline 的 `Cancelled` 类型或 LightGBM 的确切训练取消错误生成，不按任意错误中的 `cancelled` 子串判断；例如不存在的 `cancelled_table` 仍显示原始 SQL 错误。

## 数据流

```mermaid
flowchart LR
    UI[egui] -->|WorkerCommand| W[后台 worker]
    W --> N[DuckDB / storage / factor / ml]
    N -->|结果或普通错误| W
    W -->|WorkerEvent| R[状态 reducer]
    R --> UI
    W -->|panic / 退出| U[Unavailable]
    U --> R
    UI -->|重启：新线程与通道| W
```

## 视觉约定

- 主题跟随系统明暗模式。
- 8pt 间距、统一 6px 控件圆角、8px 窗口圆角。
- 蓝色表示主操作，绿色表示成功，琥珀表示提醒，红色表示错误。
- 数值、SQL、路径和 factor ID 使用等宽字体。
- 左侧导航、顶部任务栏、中央工作区、底部状态栏固定分工；日志折叠在底部。
- 数据表格采用条纹行和横向滚动，避免列内容互相遮挡。

## 构建与环境

默认依赖已在 Cargo.toml 中启用 analytics-engine 的 duckdb-native，以及 analytics-ml 的 lightgbm-native、duckdb-native。

桌面构建脚本从 Cargo `OUT_DIR` 定位当前 target/profile 的 `deps`，使用 libduckdb-sys 在该目录复制的匹配 DuckDB 库，不递归搜索下载缓存。相对/自定义 `CARGO_TARGET_DIR` 和交叉目标/profile 的路径都由 `OUT_DIR` 决定；也可显式设置包含实际动态库文件的 `DUCKDB_LIB_DIR`。空值对本地路径助手视为未设置，但上游 libduckdb-sys 将空 `DUCKDB_LIB_DIR` 当成显式路径，所以下载模式应 `unset DUCKDB_LIB_DIR`。

`build.rs`、`build_support.rs` 与 ml 的 `native_paths.rs` 负责库路径；app 和 ml 共用 LightGBM 探测，优先显式目录，再尝试 `brew --prefix lightgbm`、标准路径与当前用户 Homebrew 路径，不包含开发者个人绝对路径。显式目录缺少库文件时构建直接报错。macOS 写入绝对路径及 `@executable_path/deps` / `@executable_path` rpath，Linux 写入路径及 `$ORIGIN` 相对 rpath；Windows 不输出 Unix 链接选项，原生构建/运行验收目前限本机 macOS。

在本机已安装依赖或下载库的布局中，直接运行 desktop 无需额外 `DYLD_LIBRARY_PATH`；分发程序仍需打包原生库和 OpenMP 依赖，不能只复制可执行文件。

    export LIGHTGBM_LIB_DIR=/path/to/lightgbm/lib
    export DYLD_LIBRARY_PATH=$LIGHTGBM_LIB_DIR:/path/to/openmp/lib:$DYLD_LIBRARY_PATH
    export DUCKDB_DOWNLOAD_LIB=1
    unset DUCKDB_LIB_DIR
    brew install lightgbm
    cargo check -p analytics-desktop --all-targets
    cargo run -p analytics-desktop

没有 lib_lightgbm.dylib 时可以编译源码依赖，但最终链接和运行会失败；完整无原生检查见根 README。

## 自动化交互测试（nextest）

`src/interaction_tests.rs` 使用真实 egui 控件和鼠标/键盘事件，无需打开系统窗口；worker 测试在独立线程运行真实 DuckDB 与 LightGBM，数据和模型写入 `tempfile` 临时目录。

| 测试层 | 覆盖范围 |
|---|---|
| egui 交互 | 四面板导航、日志展开/收起、SQL 编辑、按钮点击、真实查询缺失文件/语法/表名错误显示及修正后恢复、Describe/导出错误及重试、panic 提示与重启后的真实查询恢复、命令/事件通道断开、全部任务按钮禁用、旧事件隔离、分页大小/排序/格式选择、文件选择与 schema 警告、因子筛选/全选/清空/勾选、训练参数、取消、明暗主题渲染 |
| 状态与校验 | SQL/日期/超参校验、分页累积与 schema 替换、错误路由、过期事件、任务完成、日志容量 |
| 真实 worker | Describe、升降序连续分页、NULL 与数值/日期类型、CSV/Parquet 导出及扫描、含引号 SQL/路径、缺失 Parquet glob 错误与失败恢复、扫描/导出/流水线/训练 I/O 失败后的复用、六类任务 panic 与任务外 panic、不同 panic 内容、取消、完整因子训练与缓存重训、指标/重要性/预测预览 |
| 分页与提交回归 | 默认 600 条重复行连续翻页、重复/NULL 排序键、中文列名/隐藏列冲突、源文件删除后翻页、随机表达式快照、修改查询禁用旧游标、失败查询清空旧结果、入队即禁用重复提交、basename 导出 |
| 原生库构建 | 实际编译并运行构建脚本，验证当前 target/profile、相对 target 配置、旧架构缓存隔离、无效显式目录拒绝、Homebrew 路径探测助手 |

```bash
cargo install cargo-nextest --locked
export DUCKDB_DOWNLOAD_LIB=1
# LightGBM 已安装时自动探测；自定义安装可设置 LIGHTGBM_LIB_DIR。
cargo nextest run -p analytics-desktop
cargo nextest run -p analytics-desktop -E 'test(interaction_tests::)'
# 故障注入与后台恢复（仅 test 构建含注入命令）
cargo nextest run -p analytics-desktop -E 'test(panic) | test(channel) | test(unavailable_worker) | test(file_scan_export)'
cargo nextest run --workspace --profile ci
```

VS Code 任务 `Rust: desktop interaction tests (nextest)` 运行同一套桌面测试。nextest 配置位于 `.config/nextest.toml`，默认不在首次失败时中止，超时会终止测试；CI 的 JUnit 报告在 `target/nextest/ci/junit.xml`，不提交生成文件。

这些测试验证交互逻辑和后台真实产物；构建脚本回归用例需要 `rustc` 在 PATH 中。macOS 原生窗口、系统外观跟随、字体视觉效果及实际数据规模下的体验仍需以下手动验收。

## 手动验收项目

- 切换 macOS 浅色/深色外观，重启 app 后主题跟随系统。
- 数据管理扫描 data/market，选中文件检查 footer schema 和行情契约提示。
- 查询工作台执行 SQL，选择排序列，连续点击下一页，导出 CSV 与 Parquet。
- 因子工作台选择至少一个因子，运行真实数据，观察加载、因子、训练和写出阶段。
- 建模面板从 factors.parquet 重训，查看 metrics、importance 和预测表。
- 任务运行中点击取消，确认 worker 回报失败/取消，界面恢复空闲。
- 操作失败后错误显示在所属面板；后台不可用时显示重启入口，重启后可重新查询且不会接收旧后台事件。
- 显式配置不含库文件的 LightGBM 目录，确认构建错误指出配置问题；不设置环境变量时确认自动探测生效。完全缺少 native library 时程序无法链接或加载，不能依赖界面显示启动前错误。

## 验收命令

    cargo fmt --all -- --check
    cargo check -p analytics-desktop --all-targets
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps

在设置好 LightGBM 动态库的机器上补跑：

    DUCKDB_DOWNLOAD_LIB=1 cargo nextest run -p analytics-desktop
    cargo run -p analytics-desktop

