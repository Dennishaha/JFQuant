# JSON 存储迁移指南

## 概述

JFQuant 已将工作区元数据从分散的 JSON 文件迁移到统一的 SQLite 数据库，提升查询性能并降低内存占用。

**影响的文件**:
- `.jfquant/catalog/*.json` → `index.sqlite` 的 `catalog_entries` 表
- `.jfquant/runs/*/run.json` → `index.sqlite` 的 `run_records` 表
- `.jfquant/storage/packages/*.json` → `index.sqlite` 的 `package_maps` 表

**不影响**:
- Parquet 数据文件
- Evidence objects (.jfquant/objects/)
- 用户配置和凭据

## 迁移前准备

### 1. 备份工作区

```bash
cd /path/to/workspace
tar -czf jfquant-backup-$(date +%Y%m%d-%H%M%S).tar.gz .jfquant/
```

### 2. 检查数据规模

```bash
# 检查需要迁移的文件数量
find .jfquant/catalog -name "*.json" | wc -l
find .jfquant/runs -name "run.json" | wc -l
find .jfquant/storage/packages -name "*.json" | wc -l

# 检查磁盘空间（需要约 2x 当前大小）
df -h .jfquant/
```

### 3. 更新到最新版本

```bash
# 确保使用支持 Schema Version 2 的版本
jfquant --version  # 应该是包含迁移功能的版本
```

## 执行迁移

### 基本用法

```bash
jfquant storage migrate-json
```

迁移过程会：
1. 验证所有 JSON 文件格式
2. 创建备份到 `.jfquant/archive/json-migration-{uuid}/`
3. 插入数据到 SQLite
4. 移动原始文件到备份目录
5. 生成迁移报告

### 输出示例

```json
{
  "migration_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "started_at": "2026-10-20T10:00:00Z",
  "completed_at": "2026-10-20T10:00:05Z",
  "catalogs_migrated": 52,
  "runs_migrated": 18,
  "packages_migrated": 3,
  "configs_migrated": 0,
  "backup_path": ".jfquant/archive/json-migration-a1b2c3d4-e5f6-7890-abcd-ef1234567890",
  "journal_path": ".jfquant/storage/json-migration-a1b2c3d4-e5f6-7890-abcd-ef1234567890/journal.jsonl"
}
```

## 验证迁移

### 1. 检查数据完整性

```bash
# 验证 catalog 数量
jfquant catalog list | wc -l  # 应该等于迁移报告中的数量

# 验证 run records
jfquant runs list | wc -l

# 运行完整扫描
jfquant storage scan
```

### 2. 测试关键功能

```bash
# 查询 catalog
jfquant catalog list

# 查看运行历史
jfquant runs list

# 运行一个简单的回测（如果有配置）
jfquant backtest run <config-name>
```

### 3. 性能对比

迁移后，storage 扫描应该明显更快：

```bash
time jfquant storage scan
# 预期: Catalog 查询从 ~150ms 降至 < 10ms
```

## 回滚操作

如果迁移后发现问题，可以回滚到原始 JSON 文件：

### 自动回滚

```bash
# 使用迁移报告中的 migration_id
jfquant storage migrate-json --rollback a1b2c3d4-e5f6-7890-abcd-ef1234567890
```

回滚过程会：
1. 从备份目录恢复原始文件
2. 删除 SQLite 中插入的记录
3. 保留迁移日志用于诊断

### 手动回滚

```bash
cd .jfquant
# 恢复 catalog
cp -r archive/json-migration-{uuid}/catalog/* catalog/

# 恢复 run records
find archive/json-migration-{uuid}/runs -name "run.json" -exec cp --parents {} . \;

# 恢复 packages
cp -r archive/json-migration-{uuid}/storage/packages/* storage/packages/
```

### 完整恢复

如果需要完全恢复到迁移前状态：

```bash
cd /path/to/workspace
rm -rf .jfquant
tar -xzf jfquant-backup-YYYYMMDD-HHMMSS.tar.gz
```

## 故障排查

### 迁移失败

**症状**: 迁移命令返回错误

**可能原因**:
1. JSON 文件格式不兼容（缺少 schema_version 等）
2. 磁盘空间不足
3. 数据库文件锁定

**解决方法**:
```bash
# 检查错误详情
jfquant storage migrate-json 2>&1 | tee migration-error.log

# 检查磁盘空间
df -h .jfquant/

# 确保没有其他进程占用数据库
lsof .jfquant/storage/index.sqlite
```

### 数据缺失

**症状**: 迁移后某些 catalog 或 runs 不可见

**检查步骤**:
```bash
# 检查迁移报告
cat .jfquant/storage/json-migration-{uuid}/journal.jsonl

# 直接查询数据库
sqlite3 .jfquant/storage/index.sqlite "SELECT COUNT(*) FROM catalog_entries;"

# 对比原始文件数量
find .jfquant/archive/json-migration-{uuid}/catalog -name "*.json" | wc -l
```

### 性能未提升

**可能原因**:
1. 数据库文件在网络文件系统上
2. 缺少索引（数据库损坏）
3. WAL 模式未启用

**诊断**:
```bash
# 检查 WAL 模式
sqlite3 .jfquant/storage/index.sqlite "PRAGMA journal_mode;"
# 应该输出: wal

# 检查索引
sqlite3 .jfquant/storage/index.sqlite ".indexes"

# 检查完整性
sqlite3 .jfquant/storage/index.sqlite "PRAGMA integrity_check;"
```

## 迁移后清理

迁移成功并验证后，可以清理备份文件（建议保留至少 30 天）：

```bash
# 查看备份大小
du -sh .jfquant/archive/json-migration-*

# 删除特定迁移的备份（谨慎操作！）
rm -rf .jfquant/archive/json-migration-{uuid}

# 删除事务日志
rm -rf .jfquant/storage/json-migration-{uuid}
```

## Schema Version 说明

迁移后，数据库会从 Version 1 升级到 Version 2：

```bash
# 检查 schema version
sqlite3 .jfquant/storage/index.sqlite "PRAGMA user_version;"
# 应该输出: 2
```

**Version 1**: 文件系统扫描索引（records, resources, owners 表）  
**Version 2**: 增加元数据表（catalog_entries, run_records, package_maps 等）

两个版本共存，Version 1 的表仍然用于文件系统扫描。

## 常见问题

### Q: 迁移需要多长时间？

A: 取决于文件数量，通常：
- < 100 个文件: < 5 秒
- 100-500 个文件: 5-30 秒
- > 500 个文件: 30-120 秒

### Q: 迁移会中断正在运行的任务吗？

A: 不会。迁移只影响元数据读取，不影响已经运行的回测或训练任务。但建议在没有活动任务时执行迁移。

### Q: 可以多次运行迁移吗？

A: 可以，但第二次运行会跳过已迁移的文件。如果需要重新迁移，先执行回滚。

### Q: 迁移后可以继续使用旧版本吗？

A: 不可以。旧版本无法读取 SQLite 格式的元数据，必须回滚或升级所有工具到新版本。

### Q: 如何确认迁移完全成功？

A: 检查以下三点：
1. 迁移报告显示 "completed_at" 时间戳
2. `.jfquant/archive/json-migration-{uuid}/` 目录存在且包含原始文件
3. `jfquant catalog list` 显示所有预期的 catalog

## 获取帮助

如果遇到本指南未覆盖的问题：

1. 查看迁移日志: `.jfquant/storage/json-migration-{uuid}/journal.jsonl`
2. 检查完整性: `sqlite3 .jfquant/storage/index.sqlite "PRAGMA integrity_check;"`
3. 保留备份文件和错误日志，联系技术支持

## 相关文档

- [架构设计](json-to-sqlite-migration.md) - 详细的技术设计
- [实施总结](json-to-sqlite-implementation-summary.md) - 实现细节
- [验证清单](json-to-sqlite-verification-checklist.md) - 完整的测试清单
