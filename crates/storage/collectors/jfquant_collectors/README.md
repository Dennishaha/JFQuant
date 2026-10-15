# 内置 Python 采集模块

仅负责固定 `tdxaidata` 原生SDK、独立巨潮实施公告和离线行动核对。HiThink由Rust直接HTTP接入。公共任务、数据校验、不可变发布由Rust负责；完整接口与命令见[采集组件](../README.md)。

`__main__` 每进程一个任务，JSON Lines协议12；stdout专用于协议，原生printf和诊断进入stderr。启动及完成核对源码签名、CPython3.12.15和tdxaidata1.2.2。签名覆盖全部白名单模块，修改后必须重建包内运行时。

`tdx.py`在SDK填充前保留原生字节及哈希。原价强制none/fill_data=False，成交量为股，万元金额转元；ForwardFactor按共同观察日归一化并与送配参考核对。Key只在私有子进程环境及0700临时SDK目录使用，结束/取消清理。

`announcements.py`直接访问巨潮目录、分页搜索与PDF，无供应商SDK依赖。验证证券、机构ID、附件及完整分页，保留原文/文本/字段候选，不自动修改事件。`evidence.py`捕获requests响应正文，不把DataFrame冒充HTTP原文。

`reconcile.py`提供确定性Decimal规则和HiThink普通事件与通达信参考的离线核对；原始事件缺日期保持null。旧网络适配与旧生产者白名单已删除，纯规则fixture保留用于防止单位和去重规则退化。

```sh
crates/app/packaging/build-collector.sh
DUCKDB_DOWNLOAD_LIB=1 cargo test -p analytics-storage
uvx ruff check crates/storage/collectors
```
