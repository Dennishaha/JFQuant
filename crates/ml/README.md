# analytics-ml

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
| load_manifest | 读取模型 manifest JSON |

Pipeline manifest 保留 feature order、training schema hash、formula version、universe、date range、input snapshot hash。LightGBM Dataset、Booster 和 Predictor 都只在 worker 内创建。

## 数据流

    pipeline -> DuckDB -> MarketRow -> Alpha191 -> FactorRunArtifact -> LightGBM -> model/metrics/predictions

## 构建与环境

    cargo check -p analytics-ml
    brew install lightgbm
    cargo check -p analytics-ml --features lightgbm-native,duckdb-native
    LIGHTGBM_LIB_DIR=/path/to/lightgbm/lib cargo check -p analytics-ml --features lightgbm-native

`native_paths.rs` 由 ml 和 desktop 构建脚本共用。`LIGHTGBM_LIB_DIR` 非空时必须包含当前目标平台的 LightGBM 动态库；为空或未设置时自动探测 `brew --prefix lightgbm`、标准 Homebrew 目录和 `$HOME/homebrew` / `$HOME/.linuxbrew` 对应路径。不会硬编码开发者个人目录。非空无效路径直接报构建错误；无法探测库时 native 构建最终链接失败。macOS native example 可通过 `DYLD_LIBRARY_PATH` 配置 LightGBM/OpenMP，desktop 自动写入本机 rpath；仅在关闭 native feature 的构建中，trainer/predictor 返回 `BackendUnavailable`。

## 验收

    cargo test -p analytics-ml
    cargo test --doc -p analytics-ml
    cargo test -p analytics-ml --features lightgbm-native,duckdb-native
    LIGHTGBM_LIB_DIR=/path/to/lightgbm/lib cargo run -p analytics-ml --features lightgbm-native,duckdb-native --example factor_pipeline -- --help

native 测试包含 synthetic market/universe 端到端 pipeline 和从 cache 重训；真实数据验收需检查 factors、model、manifest、metrics、importance、predictions 六类产物。

