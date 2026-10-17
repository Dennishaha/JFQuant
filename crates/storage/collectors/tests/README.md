# 采集验收

Python unittest测试独立巨潮目录/分页/实施字段、HTTP证据、Decimal事件核对和通达信未填充原生表。HiThink REST测试在Rust `hithink.rs`，覆盖本地HTTP限流/权限/凭据回显、代码日期、null、量额单位、窗口冲突、原价/前复权隔离和检查点篡改。

```sh
crates/app/packaging/build-collector.sh
DUCKDB_DOWNLOAD_LIB=1 cargo test -p analytics-storage
```

Rust验收还覆盖受监管子进程的取消、超时、签名及恢复、公告证据修订。线上服务权限、历史覆盖和长期稳定性必须通过账户实际请求验证，不能从fixture通过推断。完整接口与环境见[采集组件](../README.md)。

运行时指数映射 fixture 位于 `test_tdx.py`：显式供应商映射、当前成员路线及未知路线拒绝。
不访问真实 SDK 网络，不将当前成员作为历史覆盖。
