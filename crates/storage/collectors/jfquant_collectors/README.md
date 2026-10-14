# 白名单采集实现

[上级：采集包](../README.md) · [测试](../tests/README.md)

`providers.py`调用固定AKShare/yfinance行情、指数、成员、日历和公司行动接口；`adjustment.py`从AKShare文档对应的固定新浪地址读取独立系数，核对证券、计数、日期唯一性和正有限十进制值，只接受JSON与注释。`evidence.py`在专用进程内记录requests/curl_cffi HTTP响应体及哈希，认证响应和URL crumb/token不留证。`__main__.py`处理协议4、身份握手、stdout隔离及父管道断开。

```mermaid
flowchart TD
  REQ["协议4请求与必需adjustment_source"] --> MAIN["签名和SDK/解释器ready握手"]
  MAIN --> PROVIDERS["白名单行情 / 日历 / 成分 / 公司行动"]
  PROVIDERS --> SDK["AKShare / yfinance"]
  PROVIDERS --> FACTOR["adjustment：固定新浪地址，1秒限速"]
  FACTOR --> PARSE["JSON唯一键、证券与历史计数校验"]
  SDK --> CAPTURE["HTTP原文与SDK表格分别留证"]
  PARSE --> CAPTURE
  CAPTURE --> SHARD["bars + adjustment + evidence证券分片"]
  SHARD --> RUST["Rust校验、恢复与原子发布"]
```

AKShare原价与供应商后复权表分别校验身份/日期，供应商后复权表不参与正式价格换算。股数=接口手数×100，成交额为元；缺失数值保持null。股票显式系数来源固定`sina_hfq`；经过验证的指数为price_index，Yahoo为unavailable。正式价量由Rust从原价和独立系数生成。

包内六个Python模块按文件名及内容计算SHA256，与Rust编译时签名核对。更改模块先用Ruff检查/格式化，再运行`crates/app/packaging/build-collector.sh`，最后重新构建CLI/App。代码、README和测试提交，运行缓存/HTTP证据不提交。接口、文件格式和验收边界见上级README。

announcements.py固定获取巨潮证券目录，以证券代码和已验证orgId联合获取实施证据；用pypdf提取文本并产生待审查日期/原始金额引用。cninfo-candidates-v2支持原文日期和严格对齐的普通股表格，逐字段保留引用，多候选不裁决。捕获PDF及HTTP证据，拒绝身份/分页/附件冲突。__main__.py协议4与六模块代码签名匹配后才能执行；依赖锁包含pypdf5.9.0，安装发生在开发打包阶段。SDK配股表不会被猜成完整可执行事件。

## 为什么需要本组件

只执行有边界的来源采集。固定接口和安全JSON解析阻止供应商响应变成代码。公告正文与表格候选明确区分，完整分页元数据保留供审核；一个证券的来源失败不应抹去其他证券已获取的证据。

设计依据：2026-10-14接受的证券身份与完整准入计划、组件接口及测试。未在代码或证据中确认的历史取舍不推定为既有设计理由；来源、法规或数据格式变化时重新评估。


## 独立证券与未裁定覆盖

Why：单个来源失败不应使其他证券的完整证据丢失，也不能让成功响应被解释为全事件完整。协议4只将来源定位符传给库；Rust保留内部SEC身份并重验源响应。主进程逐证券捕获失败，发送shard_error后继续；存在缺分片时不发布完整快照。

公告 evidence 保存 all_announcements、pagination_complete、requested_announcement_dates及event_category_completeness=not_reviewed。元数据保留未被标题过滤的公告，供类别覆盖审核；候选正文及结构化表格不能自动证明不存在其他公司行动。日期与类别裁定由公共审计接口完成，不执行供应商返回文本。
