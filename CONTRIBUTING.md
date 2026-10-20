# 贡献指南

感谢你考虑为 JFQuant 做出贡献！这份指南将帮助你了解如何有效地参与项目。

## 快速开始

如果你是第一次贡献，请先阅读：

1. [快速入门指南](docs/QUICKSTART.md) - 5 分钟设置开发环境
2. [代码规范](AGENTS.md) - 必须遵守的代码规则
3. [工程化指南](docs/ENGINEERING.md) - 工程化实践

## 贡献方式

### 🐛 报告 Bug

发现 bug？请创建一个 Issue，包含：

- **清晰的标题**：简短描述问题
- **复现步骤**：如何触发这个 bug
- **期望行为**：你期望发生什么
- **实际行为**：实际发生了什么
- **环境信息**：
  - OS 版本（macOS 版本）
  - Rust 版本（`rustc --version`）
  - Python 版本（`python3 --version`）
  - 项目版本（`git rev-parse --short HEAD`）
- **相关日志**：如果有的话，附上错误日志

### ✨ 提出功能建议

有新想法？我们很乐意听到！创建一个 Issue，描述：

- **功能描述**：这个功能做什么
- **使用场景**：为什么需要这个功能
- **替代方案**：你考虑过的其他方法
- **参考实现**：如果有其他项目实现了类似功能

### 🔧 提交代码

#### 开发流程

1. **Fork 仓库**
   ```bash
   # 在 GitHub 上点击 "Fork" 按钮
   git clone https://github.com/YOUR_USERNAME/JFQuant.git
   cd JFQuant
   git remote add upstream https://github.com/Dennishaha/JFQuant.git
   ```

2. **创建分支**
   ```bash
   git checkout -b feature/my-feature
   # 或
   git checkout -b fix/my-bugfix
   ```

   分支命名约定：
   - `feature/` - 新功能
   - `fix/` - bug 修复
   - `docs/` - 文档更新
   - `refactor/` - 代码重构
   - `test/` - 测试改进
   - `chore/` - 工具和配置

3. **编写代码**
   ```bash
   # 监视模式下开发
   ./scripts/dev watch

   # 或手动运行测试
   cargo nextest run --workspace
   ```

4. **遵守代码规范**

   **必须**：
   - ✅ 代码格式化通过：`cargo fmt --all -- --check`
   - ✅ Clippy 检查无警告：`cargo clippy --workspace --all-targets -- -D warnings`
   - ✅ 架构验证通过：`python3 crates/workflow/checks/architecture.py`
   - ✅ 所有测试通过：`cargo nextest run --workspace`
   - ✅ 添加必要的测试覆盖新代码
   - ✅ 更新相关文档

   **推荐**：
   - 📝 运行完整检查：`./scripts/dev check`
   - 📊 查看测试覆盖率：`./scripts/dev cov`
   - 📚 更新组件 README（如果修改了公共接口）

5. **提交代码**
   ```bash
   git add -A
   git commit -m "feat: add new feature"
   ```

   提交消息规范（[Conventional Commits](https://www.conventionalcommits.org/)）：
   - `feat:` - 新功能
   - `fix:` - bug 修复
   - `docs:` - 文档更新
   - `style:` - 代码格式化（不影响功能）
   - `refactor:` - 代码重构
   - `test:` - 测试改进
   - `chore:` - 构建/工具配置
   - `perf:` - 性能优化

   示例：
   ```
   feat(storage): add SQLite caching layer

   - Implement SQLite-based metadata cache
   - Add migration from JSON to SQLite
   - Update storage tests

   Closes #123
   ```

6. **推送并创建 PR**
   ```bash
   git push -u origin feature/my-feature
   ```

   然后在 GitHub 上创建 Pull Request。

#### Pull Request 检查清单

在创建 PR 之前，请确保：

- [ ] 代码通过所有本地检查（`./scripts/dev check`）
- [ ] 添加了适当的测试
- [ ] 更新了相关文档
- [ ] 提交消息遵守规范
- [ ] PR 描述清晰地说明了改动
- [ ] 如果是重大改动，已经在 Issue 中讨论过
- [ ] 自行 review 了代码，确保没有遗留的调试代码或注释

#### Pull Request 模板

创建 PR 时，请包含以下信息：

```markdown
## 改动概述
简短描述这个 PR 做了什么。

## 改动类型
- [ ] 🐛 Bug 修复
- [ ] ✨ 新功能
- [ ] 📝 文档更新
- [ ] ♻️ 代码重构
- [ ] ⚡️ 性能优化
- [ ] ✅ 测试改进

## 相关 Issue
Closes #<issue-number>

## 测试
描述你如何测试了这些改动：
- [ ] 单元测试通过
- [ ] 集成测试通过
- [ ] 手动测试通过

## 截图（如果适用）
如果是 UI 改动，附上截图。

## 检查清单
- [ ] 代码格式化通过
- [ ] Clippy 检查通过
- [ ] 所有测试通过
- [ ] 文档已更新
```

## 代码规范

### Rust 代码

遵守 [AGENTS.md](AGENTS.md) 中的规则，特别注意：

1. **组件职责**
   - 每个 crate 有明确的职责边界
   - 不要在 crate 之间创建循环依赖
   - 使用 `crates/workflow/checks/architecture.py` 验证架构

2. **错误处理**
   - 使用 `anyhow::Result` 进行错误传播
   - 提供有意义的错误消息
   - 记录错误上下文

3. **异步代码**
   - DuckDB 和 LightGBM 必须在专用线程中运行
   - 不要在 UI 线程中调用阻塞操作
   - 使用 channels 进行线程间通信

4. **测试**
   - **新功能必须有测试**
   - 新因子实现需要 fixture 或 golden-output 测试
   - 测试应该独立，不依赖执行顺序

### Python 代码

1. **格式化**
   - 使用 `ruff format` 格式化代码
   - 使用 `ruff check` 检查代码质量

2. **类型注解**
   - 添加类型注解（Python 3.11+）
   - 使用 `from __future__ import annotations`

3. **测试**
   - 使用 `unittest` 框架
   - 测试文件放在 `tests/` 目录

### 文档

1. **代码文档**
   - 公共 API 必须有文档注释
   - 使用 Rust doc comments (`///`)
   - 复杂逻辑需要内联注释

2. **README 维护**
   - 每个 crate 必须有 `README.md`
   - 描述职责、接口、构建命令、验收标准
   - 使用 Mermaid 图表说明数据流

3. **示例代码**
   - 文档中的代码示例必须能运行
   - 使用 `cargo test --doc` 验证

## Git Hooks

项目使用 Git hooks 自动化检查：

### Pre-commit Hook（提交前）

自动运行：
- Rust 格式化检查
- Python 格式化检查
- Clippy 快速检查
- 架构验证

如果检查失败，提交会被阻止。修复问题后重新提交。

### Pre-push Hook（推送前）

自动运行：
- 快速单元测试
- 架构验证

推送前确保基本测试通过，避免破坏 CI。

### 跳过 Hooks（不推荐）

如果你有充分理由需要跳过 hooks：
```bash
git commit --no-verify
git push --no-verify
```

**警告**：跳过 hooks 可能导致 CI 失败，只在紧急情况下使用。

## CI/CD 流程

每次推送代码，GitHub Actions 会自动运行：

### 快速检查（~5 分钟）
- Rust 格式化检查
- Python 格式化检查
- Clippy lint
- 架构验证

### 单元测试（~10 分钟）
- Rust 单元测试
- 文档测试
- 文档构建

### 完整测试（仅 main 分支）
- 集成测试
- Python 测试
- 覆盖率报告

### 安全审计（每周/依赖变更时）
- Rust 依赖安全扫描
- Python 依赖安全扫描
- 许可证检查

如果 CI 失败，请：
1. 查看 GitHub Actions 日志
2. 在本地复现问题
3. 修复并推送更新

## 性能考虑

当你的改动可能影响性能时：

1. **运行基准测试**
   ```bash
   # 改动前
   cargo bench --bench my_bench > before.txt

   # 改动后
   cargo bench --bench my_bench > after.txt

   # 比较结果
   cargo benchcmp before.txt after.txt
   ```

2. **添加新的基准测试**
   - 对关键路径添加 benchmark
   - 使用 `criterion` 框架
   - 放在 `benches/` 目录

3. **性能回归**
   - 如果性能下降超过 10%，需要在 PR 中说明原因
   - 考虑是否有优化空间

## 发布流程

（仅维护者）

1. **更新版本号**
   - 修改所有 `Cargo.toml` 的版本号
   - 遵循 [Semantic Versioning](https://semver.org/)

2. **更新 CHANGELOG**
   - 记录所有重要改动
   - 按类型分组（Features, Fixes, Breaking Changes）

3. **创建 Git Tag**
   ```bash
   git tag -a v1.0.0 -m "Release v1.0.0"
   git push origin v1.0.0
   ```

4. **发布到 GitHub Releases**
   - 自动触发 release workflow
   - 上传构建产物

## 获取帮助

遇到问题？

- 💬 在 Issue 中提问
- 📖 阅读文档（`docs/` 目录）
- 🔍 搜索已有的 Issues 和 PRs
- 📧 联系维护者 @Dennishaha

## 行为准则

### 我们的承诺

为了营造一个开放和友好的环境，我们承诺：

- ✅ 使用友好和包容的语言
- ✅ 尊重不同的观点和经验
- ✅ 优雅地接受建设性批评
- ✅ 关注对社区最有利的事情
- ✅ 对其他社区成员表现出同理心

### 不可接受的行为

- ❌ 使用性化的语言或图像
- ❌ 人身攻击或政治攻击
- ❌ 公开或私下骚扰
- ❌ 未经许可发布他人的私人信息
- ❌ 其他在专业环境中被认为不适当的行为

## 感谢

感谢所有贡献者！你的努力让 JFQuant 变得更好。

---

再次感谢你的贡献！如果有任何疑问，随时在 Issue 中提问。
