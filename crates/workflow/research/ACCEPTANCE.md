# AKQuant migration acceptance — 2026-10-15

本地环境：Apple M6 / arm64，macOS 27.0.1。部署目标为 macOS 14，
包内 CPython 3.12.15、AKQuant 0.3.66、LightGBM 4.7.0。
本机测试不等同于在另一台干净 macOS 14 机器完成验收。

## 维护面与实测

基线为 Git `63684e8`。统计包含空行、注释及模块内 Rust 测试，不包含文档、锁文件、
生成文件或依赖代码；迁移前统计 `crates/ml/**/*.rs` 加 core 的 execution.rs、
market_rules.rs；迁移后统计 workflow/src/research 下全部 Rust、私有 worker 四个
Python 文件和相同 core 文件。

| 项目 | 基线 | 本次 |
|---|---:|---:|
| 上述自维护源码行数 | 9151 | 6509 |
| 净减少 | — | 2642（28.9%） |
| 单独 ml crate | 1 | 0 |
| 本项目 LightGBM C FFI / 链接发现 / 源码构建 | 有 | 删除 |
| 自建撮合、资金、持仓、细分费用及延迟到账账本 | 有 | 删除 |
| 源码构建的辅助原生组件 | OpenMP + LightGBM | OpenMP |

保留 DuckDB 下载原生库、OpenMP 辅助构建及 Python wheel 原生库。
解释器、全部 Python 依赖和 OpenMP/CMake 源归档均固定版本及 SHA256。
Python 原生对象审核通过：242 个 Mach-O，arm64、macOS 14 目标与包内依赖路径。

Python 六个小型行为样例合计实测 0.92 秒，最大 RSS 209,698,816 字节（约 200 MiB）。
它们验证训练重载、缺失值、下一开盘、负分排名、费用、跨年持仓、无信号、资金不足拒单、分红和拆股。
该数字包含解释器启动，不代表全市场研究性能；没有同机器旧实现的可比运行基线，
不声称速度、内存或包体积下降。

最终开发签名安装包已完成包内验收并生成：App 的 `du -sk` 为 900,700 KiB
（约 880 MiB），其中 Python 为 723,892 KiB（约 707 MiB）；DMG 为
394,180,180 字节（约 376 MiB）。这些是本次绝对测量，没有可比旧包基线。
最终 DMG SHA256：`d43d677a6bee4c534a2ead359b36b30dd4e7494117c5ac768eae4d2de1b70c85`。

包内 `env -i` 验收通过完整研究、独立训练、账户参数覆盖后的模型复用回测、
GTJA191/Alpha158、运行中取消、停止服务后的离线读取，以及 53 项采集 fixture。

## 已运行的开发验收

- Rust workspace：253 项单元/集成测试和 2 项文档测试通过。
- Python 研究行为测试：6 项通过。
- 打包路径/加载命令回归：3 项通过。
- workspace 严格 Clippy、严格 rustdoc、格式与 diff 空白检查通过。
- 已有因子参考测试保留，通过 GTJA191 与 Alpha158 参考样例。
- 模型复用检查：改变账户参数后模型字节、训练签名不变，因子缓存复用；
  新执行结果冻结新的资金、持仓数与调仓周期。
- 取消检查：运行中的 Python 训练退出，进程被回收；后续任务使用新进程完成训练。

完整 workspace 测试同时启动多个独立服务及 Python 进程，使用两个测试线程避免
短等待窗口受构建/签名竞争影响。任务自身仍保持每工作区一个计算任务。

```bash
crates/app/packaging/build-collector.sh
DUCKDB_DOWNLOAD_LIB=1 cargo test --workspace -- --test-threads=2
DUCKDB_DOWNLOAD_LIB=1 cargo clippy --workspace --all-targets -- -D warnings
RUSTDOCFLAGS="-D warnings" DUCKDB_DOWNLOAD_LIB=1 cargo doc --workspace --no-deps
PYTHONPATH=crates/workflow/research /usr/bin/time -l target/collector-build/python/bin/python3 -B -m unittest discover -s crates/workflow/research/tests
python3 -m unittest discover -s crates/app/packaging -p 'test_*.py'
crates/app/packaging/package-macos.sh
```

安装包检查以 `env -i` 使用包内 CLI 和解释器，验证完整研究、独立训练/复用回测、
两类因子、断开服务后的历史读取、运行中取消及采集 fixture。
安装包、结果和日志只在忽略的 target 下保存。

## 上游能力边界

分红与拆股使用除权除息日语义。未知特别事件保留来源警告，已知不支持事件阻断回测。
费用只报告上游合计。默认整手增量不代表独立的板块最小买入量和涨跌停限制，
这些规则未建模；自定义旧规则无法映射时明确报错。市场数据并不因此获得额外保证。
跨机器安装、Developer ID/公证及干净 macOS 14 实机验收仍需目标环境完成。

## 公司行动核对与原实验复核 — 2026-10-15

公司行动发布格式6，采集协议10，服务和研究结果16；模板10、模型manifest2、训练描述3保持。
研究worker、AKQuant0.3.66、LightGBM4.7.0和Python依赖锁未修改。Rust使用JSON原数字字面量验证
Decimal文本；中央规则供在线采集和离线重放共同使用。App/CLI采用同一任务与准入。

原CSI300任务2012年起、5年训练＋1年验证、年度滚动15折，保留provider/qualified_only，
15个模型及manifest哈希全部匹配原训练回执。744证券公司行动重放合并30组同日组成项；
华能水电2020年实施公告核实公众股现金0.18元与除息参考0.15元，按同一证券/事件关闭原冲突。
重新重放保留该公告修订，未把参考金额、税后金额或接近金额当作公众股权益。

最终输入版本`actions-f4ba1f29390f095b`，账户复核任务`bcfe3624-1d80-4bc5-a48b-1f4b753f0b6e`。
测试期仍有292项普通事件阻断（98证券）：162项主来源历史缺记录，110项金额/对象/未登记精度
冲突，20项必要除权日缺失。97项已识别特殊事件仅警告。训练completed，预测not_started，
账户blocked；收益、订单统计为空，不能当作零成交。未进入因子/预测缓存，命中率为空。

CLI提交至终态121.11秒（冻结提交12.24秒），服务及子进程采样RSS峰值1,164,560KiB。
readiness摘要从114,988,398字节缩至230,410字节，完整准入/质量/问题仍保存在引用文件。
这不证明完整准入更快；新增原文及核对回执的完整性校验仍有成本。首次逐条扫描校验已改ID索引，
其取消后服务恢复为interrupted，未注册完整版本。证据按内部证券身份、来源映射及日期范围复用，
相同来源代码不能跨身份拼接。

仓库检查：Rust270项＋2项doctest，采集Python65项，研究Python8项通过；一个需显式真实系数
fixture的Rust验收默认忽略。strict Clippy、格式及无警告文档构建通过。实际账户边界fixture包括
100股派1元后拆为200股只增100元；公众股派0.18元、参考降价0.15元时100股权益净增3元。

本地可重放命令和按证券/日期/字段补证清单位于`.jfquant/experiments/csi300-actions-v6-20261015/`，
该目录及数据、模型、依赖、安装包不提交。仍需完成普通资料补证后才能交付该实验的完整账户结果；
本轮未在另一台干净macOS14机器执行安装包验收。
