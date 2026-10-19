# 工作区存储管理

## 职责与接口

`scan` 盘点工作区；`plan` 根据资源 ID 计算精确删除集合和依赖阻断；`apply` 仅执行已持久化计划。
App 与 CLI 使用 `WorkflowRequest::Storage { request: Scan { level, threads } | Plan | Apply }`，经过同一个服务任务槽、worker、取消和结果持久化流程。
`Rpc::StorageInventory` 读取上次扫描的资源摘要，`Rpc::StorageHistory` 读取清理收据。服务协议45；扫描清单格式2，进度快照、计划、日志和历史视图格式1。
从协议44升级需同步更新服务、App和CLI；已有数据与模型格式没有变化，无需数据转换。旧扫描缓存失效后重建，旧未执行计划重新预览，已有事务仍按格式1恢复。未知清单、计划、事务及收据格式会明确拒绝，不自动转换或兼容旧版本；工作区移动后旧缓存清单也必须重新扫描。

`types` 定义普通数据接口；`inventory` 根据现有生产者清单认领文件；`members` 限定生产者的保留文件名和清单成员；`dependencies` 解析完整性、追溯与未决引用；`cleanup` 管理事务；`history` 保存显示快照与能力状态。
底层 `analytics_storage::filesystem` 只读元数据，不依赖 DuckDB 或 Python。查询会话在执行清理前由原 worker 关闭；原生句柄不会传给客户端。

## 统计和保护

分类是互斥的空间归属，归档和临时状态是筛选标签。分类合计按 device/inode 去重；资源明细展示成员文件，因此共享资源明细不能直接相加。
逻辑大小和分配空间分开保存。释放估计按选择集合去重，硬链接未全部纳入删除时，不将其分配空间计为可释放。
文件系统共享块不能精确归属，估计不是磁盘可用空间增量承诺。权限错误、跨挂载点和扫描期间变化均使清单不完整，禁止据此清理。

只按已识别生产者和清单认领文件，不根据任意父目录扩展删除范围。配置、未知文件及未经确认的链接受保护；外部引用只显示，不计入工作区占用。
行情、训练折、样本掩码及年度缓存目录中的未识别文件独立保留。只有树哈希、没有文件成员清单的复合证据包，以及仅有归属标记的临时检查点，首版只统计并保护，不能按整个目录删除。
训练模型包含所需样本证据；共享预测包属于预测回放依赖。保留的预测身份需要训练描述时，只共享该描述，不要求保留已明确释放的模型字节。
市场视图的底层数据是完整性依赖；已完成任务的冻结来源是追溯关系。未决引用阻止清理，不自动级联扩大选择。

## 数据流与恢复

```mermaid
flowchart LR
 A[Scan: 元数据与manifest] --> B[Plan: 文件与依赖预览]
 B --> C[显式永久删除确认]
 C --> D[互斥重验与历史快照]
 D --> E[暂存文件]
 E --> F[单份journal提交资源状态]
 F --> G[永久删除与收据]
```

`.jfquant/storage/inventory.json`保存最近完成的轻量摘要（不含文件清单）；`files.json`保存资源扫描的完整文件索引，不能作为删除依据；`snapshot.json`保存当前或上次扫描进度；`plans/UUID.json` 保存计划；`transactions/UUID/journal.json` 是可用性提交的权威记录；`result.json` 是可查询收据；`history/TASK.json` 保存原参数、结果与收益曲线。
原始 catalog、run.json、签名 manifest、特征顺序、schema 哈希和公式版本不改写。数据释放后从可选版本中隐藏，原身份仍在 catalog；历史展示与模型复用、预测回放、原始输入和完整核验分别报告。

同内容重新采集或恢复后，登记为带新身份的可用版本；重复登记保持幂等。原已释放版本不会重新变成可用，旧研究仍指向原身份。已完成清理收据支持工作区移动；移动后重新扫描再生成新计划。

提交前可以取消并回滚。启动恢复在独占租约内执行：未提交暂存回滚，已提交暂存继续删除。
临时事务目录不是回收站。提交后不能取消永久删除；逐项失败保留 `pending_deletion`，任务报告失败，重试同一计划继续剩余删除。完成计划重复执行返回原收据。
永久删除后先同步暂存目录，再持久化完成状态；目录同步失败同样保留待处理状态，启动或重试继续完成。
计划绑定选择文件身份、大小、时间及相关元数据；文件或引用变化使计划失效。工作区迁移后应重新扫描与预览，不能执行原路径下的旧计划。

## 复现与验收

```bash
export DUCKDB_DOWNLOAD_LIB=1
cargo test -p analytics-workflow storage_ --lib
cargo test -p analytics-cli storage_
cargo test -p analytics-desktop storage_
python3 crates/workflow/checks/architecture.py
cargo fmt --all -- --check
cargo clippy -p analytics-storage -p analytics-workflow -p analytics-cli -p analytics-desktop --all-targets -- -D warnings

export JFQUANT_WORKSPACE=/absolute/workspace
jfquant storage scan # 默认overview
jfquant storage scan --level resources --threads 4
jfquant jobs wait SCAN_TASK
jfquant storage list --sort size
jfquant storage list --category models --status archived
jfquant storage plan --resource RESOURCE_ID --resource ANOTHER_RESOURCE_ID
jfquant jobs wait PLAN_TASK
jfquant jobs results PLAN_TASK # 查看 id、blockers、impacts、files 和释放估计
jfquant storage apply --plan PLAN_ID --confirm-permanent
jfquant jobs wait APPLY_TASK
jfquant storage history
```

验收覆盖分类合计、硬链接保留、共享材料、未决引用、数据视图依赖、来源追溯、摘要及曲线保留、计划过期、取消、服务写入互斥、提交前后恢复和待删除文件变化。只使用临时合成工作区；不清理用户真实数据。

## 分级、进度与性能

- `overview`仅遍历元数据并汇总整个工作区（包括target）；目录桶取父目录前两层，桶间互斥，不能假定target全部可清理。普通文件不保留明细，仅多链接文件记录去重身份。
- `resources`增加生产者manifest与成员识别，不执行引用分析。两级`dependencies_checked=false`，可释放空间为未评估；选择状态仅为候选，以plan核验结果为准。
- `plan/apply`仍完整扫描并验证引用与文件身份；未知JSON仍参与安全发现。元数据直接哈希原始字节，资源名/路径使用任务内索引；不新增数据库、文件监听或增量索引。清理后只将摘要标记stale，用户重新扫描更新占用。
- 默认`min(available_parallelism,4)`，可显式1—8线程。专用线程池只处理目录与文件metadata，生产者解析仍串行；不共享数据库/模型句柄。
- `Rpc::StorageSnapshot`返回可空的轻量快照：工作区、scan_id、level、phase、时间/耗时、finished/complete、目录计数、占用及诊断。每秒最多一次中间持久化，开始/结束额外写入最终状态。各阶段每秒报告进度，取消保留上次完整摘要。缓存查询不加载files.json；缺失缓存是正常空状态。
- `Rpc::StorageInventory`仅读取最近完成的摘要。`finished`与`complete`分别表示任务结束与扫描完整，不能互相替代。目录统计是扫描期间观测值，APFS共享块仍为估计，无法提供文件系统原子快照。

### 可复现基准

将旧提交导出到临时目录，分别构建旧/新发布CLI并保留各自二进制。运行：

```bash
DUCKDB_DOWNLOAD_LIB=1 cargo build --release -p analytics-cli
python3 crates/workflow/checks/storage_benchmark.py --root /tmp/jfquant-storage-million-unique --baseline /absolute/previous-release-jfquant --current /absolute/current-release-jfquant
```

脚本要求新建且不存在的目录，生成百万空文件及1000个JSON；只重置该生成样本中的工作区元数据。旧版完整扫描、新版1/4线程总览和完整清理核验均报告耗时及服务峰值RSS，不执行删除。首次与热运行分别报告；首次运行不是清空操作系统缓存后的冷测。严格冷测应在专用环境重启或由操作者清空缓存，记录操作，不在真实工作区自动清缓存。

### 2026-10-19 实测

Apple M6、macOS/APFS、发布构建；样本100万空文件、1000个约40KiB JSON。并发构建会影响结果，以下为观测范围，不是时延承诺；未执行清缓存冷测。

| 路径 | 耗时 | 服务峰值RSS |
|---|---:|---:|
| 旧版完整扫描 | 25.3—32.0秒 | 1037—1083MiB |
| 新版单线程总览 | 12.6秒 | 17MiB |
| 新版4线程总览 | 3.9—5.2秒 | 17—18MiB |
| 新版完整清理核验 | 18.3—19.3秒 | 627—657MiB |

真实工作区只读总览：1,789,661个文件、约187.3GiB分配空间，约23.9秒完成，诊断为空；进度查询约80ms，完成后的摘要查询约72ms。259份原有登记、任务及产物manifest扫描前后字节一致。容量统计包含本地构建产物，不等于可释放量。
