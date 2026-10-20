# 存储空间占用分析

**分析时间**: 2026-10-20  
**总空间**: 36 GB  
**数据库大小**: 2.5 GB (占比 7%)

---

## 📊 总体空间分布

| 目录 | 大小 | 占比 | 说明 |
|------|------|------|------|
| **archive/** | 20.2 GB | 56.1% | 历史归档数据 |
| **runs/** | 12.5 GB | 34.7% | 运行时数据（大 JSON） |
| **storage/** | 2.5 GB | 7.0% | SQLite 数据库 |
| experiments/ | 304 MB | 0.8% | 实验数据 |
| staging/ | 268 MB | 0.7% | 临时数据 |
| plot-deps/ | 100 MB | 0.3% | 绘图依赖 |
| audit/ | 71 MB | 0.2% | 审计日志 |
| identities/ | 56 MB | 0.2% | 身份数据 |
| **总计** | **36.0 GB** | **100%** | |

---

## 🔍 详细分析

### 1. Archive 目录（20.2 GB - 最大占用）

**内容**: 历史归档的 allocation 运行数据

```
allocation-e0134f5e-5256-40ca-aaae-fba8d32706c6/    17.3 GB
json-migration-2340c217-c6de-4bf5-9aaa-304b9096aff6/ 2.5 GB  ← JSON 迁移备份
protocol43-main-20261018/                            447 MB
allocation-7d4599ba-f9e3-4d3d-bd3c-c21e1cb78df9/    255 KB
allocation-e4727a91-8ec0-43c7-80c7-3adf4ed6af6d/      8 KB
```

**问题**:
- ❌ `allocation-e0134f5e` 占用 **17.3 GB**，极其庞大
- ❌ JSON 迁移备份 2.5 GB（已完成迁移，可以删除）
- ⚠️ 归档数据应该有保留策略

**建议**:
```bash
# 1. 删除 JSON 迁移备份（已验证迁移成功）
rm -rf .jfquant/archive/json-migration-2340c217-c6de-4bf5-9aaa-304b9096aff6/

# 2. 审查 allocation-e0134f5e 是否需要保留全部数据
# 可能的策略：
# - 压缩归档：tar -czf allocation-e0134f5e.tar.gz allocation-e0134f5e/
# - 移动到冷存储
# - 只保留摘要，删除详细数据
```

**潜在节省**: 2.5 GB（迁移备份）+ 可能 15+ GB（旧归档）

---

### 2. Runs 目录（12.5 GB - 大 JSON 文件）

**JSON 文件分布** (3,922 个文件，8.9 GB):

| 文件名 | 数量 | 总大小 | 平均大小 | 说明 |
|--------|------|--------|----------|------|
| **allocation-input.json** | 19 | **3.8 GB** | 202 MB | 分配输入数据 |
| **allocation-eligibility.json** | 32 | **3.2 GB** | 104 MB | 资格判定数据 |
| independent_predictions.json | 24 | 406 MB | 17 MB | 独立预测 |
| outcome.json | 165 | 232 MB | 1.4 MB | 结果数据 |
| reference-input.json | 19 | 193 MB | 10 MB | 参考输入 |
| input-check.json | 39 | 95 MB | 2.4 MB | 输入检查 |
| 2022.json, 2019.json 等 | 37 | 343 MB | 9 MB | 年度数据 |
| admission.json | 39 | 68 MB | 1.7 MB | 准入数据 |
| evidence-report.json | 18 | 61 MB | 3.4 MB | Evidence 报告 |
| summary.json | 12 | 60 MB | 5 MB | 运行摘要 |

**问题**:
- ❌ **allocation-input.json**: 19 个文件 × 202 MB = 3.8 GB
  - 单个文件最大 207 MB！
  - 这些是**运行时中间数据**，应该可以清理
  
- ❌ **allocation-eligibility.json**: 32 个文件 × 104 MB = 3.2 GB
  - 同样是巨大的中间计算结果

- ⚠️ **independent_predictions.json**: 24 × 17 MB = 406 MB
  - 预测数据可能需要保留

**这些文件是什么？**

典型的 allocation-input.json 内容（推测）：
```json
{
  "universe": [...],           // 股票池
  "factors": [...],            // 因子数据（时间序列）
  "constraints": {...},        // 约束条件
  "predictions": [...]         // 预测数据
}
```

每个文件 200+ MB 意味着包含了**完整的因子矩阵**，可能是：
- 数千只股票 × 数百个因子 × 数千个交易日 = 数百万个数据点
- 未压缩的 JSON 格式

**建议拆表方案** (runs/ 目录):

#### 方案 A: 清理策略（最简单）

```bash
# 保留最近 N 次运行，删除旧数据
# 假设保留最近 5 次
ls -t .jfquant/runs/ | tail -n +6 | xargs -I {} rm -rf .jfquant/runs/{}
```

**潜在节省**: 8-10 GB

#### 方案 B: 压缩旧数据

```bash
# 压缩 30 天前的运行数据
find .jfquant/runs/ -type d -mtime +30 -exec tar -czf {}.tar.gz {} \; -exec rm -rf {} \;
```

**压缩比预期**: JSON 文本 → gzip 通常 5-10x，节省 7-9 GB

#### 方案 C: 拆分到 SQLite（推荐但工作量大）

创建新表存储运行时大数据：

```sql
-- runs.sqlite (独立数据库)
CREATE TABLE allocation_inputs (
    run_id TEXT PRIMARY KEY,
    universe BLOB,              -- 二进制存储
    factors BLOB,               -- Parquet 或 DuckDB 格式
    constraints TEXT,           -- 小数据保留 JSON
    created_at INTEGER
);

CREATE TABLE allocation_eligibility (
    run_id TEXT PRIMARY KEY,
    data BLOB,                  -- 压缩二进制
    created_at INTEGER
);

CREATE TABLE predictions (
    run_id TEXT,
    model_id TEXT,
    data BLOB,
    PRIMARY KEY (run_id, model_id)
);
```

**优势**:
- 二进制存储比 JSON 小 3-5x
- 可以查询和索引
- 统一管理生命周期

**劣势**:
- 需要重写数据读取逻辑
- 迁移工作量较大

---

### 3. Storage 目录（2.5 GB - SQLite 数据库）

**文件结构**:

```
storage/
├── index.sqlite          2.5 GB   ← 主数据库
├── index.sqlite-shm        32 KB  ← WAL shared memory
├── index.sqlite-wal         0 B   ← WAL log (已 checkpoint)
├── inventory.json         1.4 MB  ← 可以删除（已迁移）
├── snapshot.json           22 KB  ← 可以删除（已迁移）
└── json-migration-xxx/           ← 已备份到 archive/
```

**数据库内容**:

| Schema | 表 | 记录数 | 用途 |
|--------|---|--------|------|
| Version 1 | records, resources, owners | ~数万条 | 文件扫描索引 |
| Version 2 | catalog_entries | 24 | Catalog 元数据 |
| Version 2 | catalog_evidence | 1,714 | Evidence 引用 |
| Version 2 | run_records | 257 | 运行记录 |

**分析**:

Version 1 索引占用了大部分空间（估计 2.4 GB），包含：
- **records 表**: 每个扫描到的文件的元数据（路径、hash、大小、修改时间）
- **resources 表**: 资源清单
- **owners 表**: 所有者关系

**问题**:
- ⚠️ 2.5 GB 似乎偏大，可能包含了历史扫描的冗余数据
- ⚠️ 没有自动清理过期扫描记录

**建议**:

```sql
-- 1. 查看数据库统计
sqlite3 .jfquant/storage/index.sqlite "
  SELECT 
    name,
    (SELECT COUNT(*) FROM pragma_table_info(name)) AS columns,
    (SELECT COUNT(*) FROM name) AS rows
  FROM sqlite_master 
  WHERE type='table';
"

-- 2. 清理旧扫描记录（保留最近 30 天）
sqlite3 .jfquant/storage/index.sqlite "
  DELETE FROM records 
  WHERE generation_id IN (
    SELECT id FROM generations 
    WHERE started < unixepoch('now', '-30 days')
  );
  
  VACUUM;
"

-- 3. 增量 vacuum
sqlite3 .jfquant/storage/index.sqlite "PRAGMA incremental_vacuum(10000);"
```

**潜在节省**: 500 MB - 1 GB（清理旧扫描记录）

---

## 🎯 优化建议总结

### 立即可执行（低风险）

| 操作 | 节省空间 | 命令 |
|------|----------|------|
| 删除 JSON 迁移备份 | 2.5 GB | `rm -rf .jfquant/archive/json-migration-*` |
| 删除 inventory.json | 1.4 MB | `rm .jfquant/storage/{inventory,snapshot}.json` |
| Vacuum SQLite | 500 MB | `sqlite3 index.sqlite "VACUUM;"` |
| **小计** | **3 GB** | |

### 短期优化（需要审查）

| 操作 | 节省空间 | 风险 |
|------|----------|------|
| 删除旧 runs | 8-10 GB | ⚠️ 确认不需要后删除 |
| 压缩旧归档 | 15 GB | ⚠️ 审查后移动到冷存储 |
| **小计** | **23-25 GB** | |

### 中期重构（工作量大）

| 操作 | 节省空间 | 复杂度 |
|------|----------|--------|
| runs/ 数据拆分到 SQLite | 5-7 GB | 高 |
| 实现自动清理策略 | 持续 | 中 |
| 归档数据压缩 | 10-15 GB | 中 |
| **小计** | **15-22 GB** | |

---

## 📊 优化后的预期占用

**现状**: 36 GB  
**立即优化**: -3 GB → **33 GB**  
**短期优化**: -25 GB → **8-11 GB**  
**中期优化**: -20 GB → **持续保持在 5-10 GB**

---

## 🔧 自动化清理策略（建议实现）

### 1. Runs 清理策略

```rust
// 在 crates/storage/src/cleanup.rs
pub struct RunRetentionPolicy {
    max_age_days: u32,
    max_total_size_gb: u32,
    min_keep_count: u32,
}

impl RunRetentionPolicy {
    pub fn cleanup(&self, runs_dir: &Path) -> io::Result<CleanupReport> {
        // 1. 列出所有 runs，按时间排序
        // 2. 保留最近的 min_keep_count 个
        // 3. 删除超过 max_age_days 的
        // 4. 如果总大小超过 max_total_size_gb，继续删除旧的
        // 5. 返回清理报告
    }
}
```

### 2. Archive 清理策略

```rust
pub enum ArchiveAction {
    Keep,
    Compress,
    Delete,
}

pub fn archive_policy(entry: &ArchiveEntry) -> ArchiveAction {
    match entry.age_days() {
        0..=30 => ArchiveAction::Keep,
        31..=90 => ArchiveAction::Compress,
        _ => ArchiveAction::Delete,
    }
}
```

### 3. SQLite 维护策略

```rust
pub fn maintain_database(db: &Connection) -> rusqlite::Result<()> {
    // 1. 删除 30 天前的扫描记录
    db.execute("DELETE FROM records WHERE generation_id IN (...)", [])?;
    
    // 2. 增量 vacuum
    db.pragma_update(None, "incremental_vacuum", 10000)?;
    
    // 3. 重建索引
    db.execute("REINDEX", [])?;
    
    Ok(())
}
```

### 4. CLI 集成

```bash
# 查看空间占用
jfquant storage usage

# 清理旧数据（交互式）
jfquant storage cleanup

# 自动清理（按策略）
jfquant storage cleanup --auto

# 归档数据
jfquant storage archive --older-than 30d
```

---

## 📈 监控指标

建议在 CLI 中添加 `jfquant storage stats` 命令显示：

```
Storage Statistics
==================

Total:              36.0 GB
├── Archive:        20.2 GB (56.1%) ⚠️  [可清理: 17.8 GB]
├── Runs:           12.5 GB (34.7%) ⚠️  [可清理: 8.0 GB]
└── Storage (DB):    2.5 GB ( 7.0%) ✓  [可优化: 0.5 GB]

Recommendations:
  • Run cleanup: jfquant storage cleanup --runs --older-than 30d
  • Archive old data: jfquant storage archive allocation-e0134f5e
  • Vacuum database: jfquant storage vacuum

Last cleanup: Never
Next scheduled: Not configured
```

---

## ✅ 行动计划

### 今天

1. ✅ 完成空间分析
2. ⏳ 删除 JSON 迁移备份（节省 2.5 GB）
3. ⏳ Vacuum SQLite（节省 0.5 GB）

### 本周

1. ⏳ 审查并清理 `allocation-e0134f5e` 归档（节省 17 GB）
2. ⏳ 删除旧 runs 数据（节省 8 GB）
3. ⏳ 实现 `jfquant storage usage` 命令

### 下周

1. ⏳ 实现自动清理策略
2. ⏳ 添加归档压缩功能
3. ⏳ 监控空间使用趋势

---

**文档版本**: 1.0  
**最后更新**: 2026-10-20  
**维护者**: Storage Team
