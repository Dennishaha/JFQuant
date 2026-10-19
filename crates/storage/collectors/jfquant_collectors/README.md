# 内置 Python 采集模块

仅负责固定 `tdxaidata` 原生SDK、独立巨潮实施公告和离线行动核对。HiThink由Rust直接HTTP接入。公共任务、数据校验、不可变发布由Rust负责；完整接口与命令见[采集组件](../README.md)。

`__main__` 每进程一个任务，JSON Lines协议16；stdout专用于协议，原生printf和诊断进入stderr。启动及完成核对源码签名、CPython3.12.15和tdxaidata1.2.2。签名覆盖全部白名单模块，修改后必须重建包内运行时。

`tdx.fundamentals`采集FN004/FN238/FN271/FN428/FN429，逐年加载日K后读取区间股本。
不使用实测返回Date=0的日期列表模式；action_shard传实际观察日与原生回执，数值
规范化由Rust完成。见[规范化组件](../../src/three_factor/acquire/README.md)。

`tdx.py`在SDK填充前保留原生字节及哈希。原价强制none/fill_data=False，成交量为股，万元金额转元；ForwardFactor按共同观察日归一化并与送配参考核对。Key只在私有子进程环境及0700临时SDK目录使用，结束/取消清理。

`announcements.py`直接访问巨潮目录、分页搜索与PDF，无供应商SDK依赖。验证证券、机构ID、附件及完整分页，保留原文/文本/字段候选，不自动修改事件。`evidence.py`捕获requests响应正文，不把DataFrame冒充HTTP原文。

巨潮目录的 `DR` 加七位数字机构ID与 `qsgn`、数字ID一样按不透明键处理（2026-10-17在线目录中603056对应DR0000021）；仍要求唯一A股目录记录、证券代码/市场一致，以及每页响应的机构ID精确匹配。命名格式支持不代表历史覆盖通过。修改后须重新构建签名匹配的包内采集器。

`reconcile.py`提供确定性Decimal规则和HiThink普通事件与通达信参考的离线核对；原始事件缺日期保持null。旧网络适配与旧生产者白名单已删除，纯规则fixture保留用于防止单位和去重规则退化。

```sh
crates/app/packaging/build-collector.sh
DUCKDB_DOWNLOAD_LIB=1 cargo test -p analytics-storage
uvx ruff check crates/storage/collectors
```

TDX行动分片增加独立GP41/GP43原文请求及获取时间，字段解释、每十股比例、总量单位和跨事件关联由Rust facts模块执行；不在Python自动生成交收条款。单字段失败不会覆盖普通事件或GP29。巨潮qsgn机构ID按目录身份接受，仍校验证券市场。验收：包内Python运行 `-m unittest discover -s crates/storage/collectors/tests`。

巨潮候选解析 v7：代码变更标题支持双向语序；全量分页最多三轮收齐总数对应的唯一ID，相同ID元数据冲突或总数变化仍拒绝。不会以去重后的较低条数替代接口总数。验收：`PYTHONPATH=crates/storage/collectors target/collector-build/python/bin/python3 -B -m unittest discover -s crates/storage/collectors/tests -p test_announcements.py`。

公告全文查询与选定文档捕获共用 `extract_pdf_text`：PDF 上限64 MiB、4096页，拒绝加密PDF，逐页累计文本上限8 MiB。此上限覆盖大型合并报告，超限仍失败，不跳过候选或宣告覆盖完整。错误区分HTTP状态/主机、PDF头、字节、页数、加密及文本限制。历史查询解析器版本为 `cninfo-candidates-v10`。

Large-document progress: report before each candidate download and after each32 extracted pages (including the final page). Progress reflects completed work; the native no-progress watchdog and cancellation remain enabled. The shared extractor accepts an optional progress callback; selected-document capture keeps its single JSON extraction response.
