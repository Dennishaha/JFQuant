# 内置 Python 采集模块

仅负责固定 `tdxaidata` 原生SDK、独立巨潮实施公告和离线行动核对。HiThink由Rust直接HTTP接入。公共任务、数据校验、不可变发布由Rust负责；完整接口与命令见[采集组件](../README.md)。

`__main__` 每进程一个任务，JSON Lines协议13；stdout专用于协议，原生printf和诊断进入stderr。启动及完成核对源码签名、CPython3.12.15和tdxaidata1.2.2。签名覆盖全部白名单模块，修改后必须重建包内运行时。

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
