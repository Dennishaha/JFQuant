# JSON 拆表验证清单

## 立即验证（开发环境）

### 1. 编译检查
- [x] `cargo check --workspace` - 全部通过
- [x] `cargo clippy --workspace` - 待运行
- [ ] `cargo doc --no-deps --open` - 检查 API 文档

### 2. 单元测试
- [x] `cargo test -p analytics-storage --test catalog_store_tests` - 4/4 通过
- [ ] `cargo test -p analytics-storage catalog::` - 检查模块内测试
- [ ] `cargo test -p analytics-storage run_store::` - 检查模块内测试
- [ ] `cargo test -p analytics-storage package_store::` - 检查模块内测试

### 3. 迁移测试（测试 workspace）
```bash
# 准备测试环境
cd /tmp
mkdir -p json-migration-test/.jfquant/{catalog,runs,storage/packages}

# 创建测试数据
cat > json-migration-test/.jfquant/catalog/test-market.json <<'EOF'
{
  "id": "test-market-001",
  "kind": "market",
  "path": "data/test",
  "signature": "sha256:test",
  "start": "2020-01-01",
  "end": "2020-12-31",
  "details": {
    "evidence_sha256": {
      "response.json": "sha256:abc123"
    }
  }
}
EOF

# 执行迁移
jfquant --workspace json-migration-test storage migrate-json

# 验证结果
- [ ] SQLite 数据库创建成功
- [ ] 原始 JSON 文件移动到 archive
- [ ] journal.jsonl 记录完整
- [ ] 可以查询迁移的数据

# 测试回滚
jfquant --workspace json-migration-test storage migrate-json --rollback <uuid>

- [ ] 原始文件恢复
- [ ] SQLite 记录清理
```

## 真实 Workspace 验证

### 阶段 1: 只读验证（不执行迁移）

```bash
# 1. 检查当前数据规模
cd /path/to/real/workspace
find .jfquant/catalog -name "*.json" | wc -l
find .jfquant/runs -name "run.json" | wc -l
find .jfquant/storage/packages -name "*.json" | wc -l

# 2. 记录基线性能
time jfquant storage scan  # 记录当前扫描时间
du -sh .jfquant/storage/index.sqlite  # 记录当前数据库大小

# 3. 检查是否有不兼容的数据
- [ ] 所有 catalog 文件都有 schema_version 字段
- [ ] 所有 run.json 都可以解析
- [ ] package maps 格式正确
```

### 阶段 2: 备份和迁移

```bash
# 1. 完整备份
tar -czf workspace-backup-$(date +%Y%m%d).tar.gz .jfquant/

# 2. 执行迁移
jfquant storage migrate-json > migration-report.json

# 3. 验证迁移结果
- [ ] migration-report.json 显示成功
- [ ] 所有文件计数匹配
- [ ] journal.jsonl 完整
- [ ] 备份目录存在且完整

# 4. 检查数据库大小和性能
du -sh .jfquant/storage/index.sqlite  # 应该增加但合理
time jfquant storage scan  # 应该快于基线
```

### 阶段 3: 功能验证

```bash
# 1. Storage 扫描
jfquant storage scan
- [ ] 扫描完成无错误
- [ ] 所有 catalog 被识别
- [ ] 所有 run records 加载
- [ ] 诊断信息正常

# 2. Catalog 查询
jfquant catalog list
- [ ] 所有 catalog 可见
- [ ] 详细信息正确
- [ ] evidence 映射完整

# 3. 运行历史
jfquant runs list
- [ ] 所有历史 runs 可见
- [ ] 状态信息正确
- [ ] 归档 runs 可查询

# 4. 回测流程（最关键）
jfquant backtest run <existing-config>
- [ ] 可以访问 catalog
- [ ] 可以读取 market 数据
- [ ] 可以写入新的 run record
- [ ] 完整流程无错误
```

### 阶段 4: 性能基准测试

运行基准测试脚本：
```bash
cd crates/workflow/checks
python benchmark-json-storage.py

# 预期结果
- [ ] Catalog 查询 < 10ms (vs 基线 ~150ms)
- [ ] Run loading < 10ms (vs 基线 ~80ms)
- [ ] Evidence 反向查找使用索引
- [ ] 内存占用 < 500MB (vs 基线 ~1GB)
```

### 阶段 5: 并发测试

```bash
# 启动多个 storage 操作
jfquant storage scan &
jfquant runs list &
jfquant catalog list &
wait

- [ ] 所有操作正常完成
- [ ] 没有数据库锁错误
- [ ] WAL 模式正常工作
```

## 回滚准备

如果发现问题，立即回滚：

```bash
# 方案 1: 使用内置回滚
jfquant storage migrate-json --rollback <migration-uuid>

# 方案 2: 手动恢复
cd .jfquant
rm -rf catalog runs storage/packages
cp -r archive/json-migration-<uuid>/* ./

# 方案 3: 完整恢复备份
cd /path/to/workspace
rm -rf .jfquant
tar -xzf workspace-backup-YYYYMMDD.tar.gz
```

## 监控指标（生产使用后）

### Week 1
- [ ] Storage 扫描成功率 = 100%
- [ ] 无数据库锁超时
- [ ] 无数据丢失报告
- [ ] Catalog 查询延迟 < 10ms (p99)

### Week 2
- [ ] 所有回测任务正常完成
- [ ] 内存占用稳定
- [ ] 数据库文件大小合理增长
- [ ] 无 foreign key 约束错误

### Month 1
- [ ] 收集真实性能数据，更新文档
- [ ] 评估是否迁移剩余配置文件
- [ ] 考虑添加更多索引优化

## 文档更新检查

- [x] `crates/storage/README.md` - Schema Version 2 说明
- [ ] 用户手册 - 迁移指南
- [ ] 故障排查文档 - 常见问题
- [ ] API 文档 - 使用示例
- [ ] CHANGELOG - 版本变更说明

## 关键成功指标

| 指标 | 目标 | 实测 | 状态 |
|------|------|------|------|
| 编译通过 | ✓ | ✓ | ✅ |
| 单元测试通过率 | 100% | 100% (4/4) | ✅ |
| Catalog 查询性能 | < 10ms | 待测 | ⏳ |
| Run loading 性能 | < 10ms | 待测 | ⏳ |
| 内存占用 | < 500MB | 待测 | ⏳ |
| 数据完整性 | 100% | 待测 | ⏳ |
| 真实 workspace 迁移 | 成功 | 待执行 | ⏳ |
| 回测流程 | 正常 | 待测 | ⏳ |

## 风险管理

### 高风险场景

1. **大型数据库迁移失败**
   - 缓解：完整备份 + 回滚机制
   - 检测：迁移过程监控 + journal 验证
   
2. **并发访问死锁**
   - 缓解：WAL 模式 + 合理的超时设置
   - 检测：并发测试 + 生产监控

3. **数据丢失**
   - 缓解：备份到 archive + transaction journal
   - 检测：数据计数验证 + 完整性检查

4. **性能回退**
   - 缓解：索引优化 + 基准测试
   - 检测：性能监控 + 对比基线

### 中风险场景

1. **SQLite 文件损坏**
   - 缓解：定期 PRAGMA integrity_check
   - 恢复：从 JSON 备份重新迁移

2. **Schema version 冲突**
   - 缓解：明确的版本检查 + 错误信息
   - 恢复：降级数据库或升级代码

3. **磁盘空间不足**
   - 缓解：迁移前检查可用空间
   - 检测：磁盘使用率监控

## 完成标准

### Phase 4 完成条件

- [ ] 所有验证清单项目完成
- [ ] 真实 workspace 迁移成功
- [ ] 所有功能测试通过
- [ ] 性能指标达标（至少 3 个测量点）
- [ ] 文档更新完成
- [ ] 无已知阻塞性 bug

### 生产就绪条件

- [ ] Week 1 监控指标全部达标
- [ ] 至少 2 个真实 workspace 迁移成功
- [ ] 性能基准测试数据充分
- [ ] 回滚流程验证成功
- [ ] 用户文档完整
- [ ] 团队培训完成

## 后续优化方向

1. **配置文件迁移** (P1)
   - shared-ui-jobs.json
   - allocation-migration.json
   - source-policy.json

2. **索引优化** (P2)
   - 根据实际查询模式调整
   - 添加覆盖索引
   - 定期 ANALYZE 统计信息

3. **压缩和清理** (P3)
   - 定期 VACUUM
   - 旧 revision 清理策略
   - WAL checkpoint 策略

4. **监控和告警** (P3)
   - 数据库大小增长趋势
   - 查询性能监控
   - 锁等待时间监控
