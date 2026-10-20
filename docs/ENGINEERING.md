# 工程化工具使用指南

本项目已集成自动化 CI/CD、Git hooks 和开发容器，提升开发体验和代码质量。

## 🚀 快速开始

### 1. Git Hooks（自动质量检查）

**自动安装（推荐）：**
```bash
# 首次构建时自动安装 hooks
cargo build
```

**手动安装：**
```bash
# 如果自动安装失败，使用手动脚本
bash scripts/install-hooks.sh
```

**跳过 hooks（紧急情况）：**
```bash
git commit --no-verify -m "WIP: skip checks"
git push --no-verify
```

### 2. 本地测试覆盖率

```bash
# 安装 cargo-llvm-cov（一次性）
cargo install cargo-llvm-cov

# 生成覆盖率报告
export DUCKDB_DOWNLOAD_LIB=1
cargo llvm-cov --workspace --html --open
```

浏览器会自动打开 HTML 报告，显示每个文件的覆盖率热力图。

### 3. 使用 DevContainer（零配置开发环境）

**前提条件：**
- 安装 VSCode 和 "Dev Containers" 扩展

**步骤：**
1. 在 VSCode 中打开项目
2. 看到提示时点击 "Reopen in Container"
3. 首次构建 5-10 分钟，之后秒开
4. 所有开发工具已预装（Rust、Python、cargo-nextest 等）

**验证容器环境：**
```bash
rustc --version
python3 --version
cargo nextest --version
```

## 🔄 CI/CD 工作流

项目配置了三个 GitHub Actions workflow：

### 1. **CI** (`.github/workflows/ci.yml`)
- **触发条件**：PR 到 main、Push 到 main
- **包含检查**：
  - 格式检查（Rust + Python）
  - Clippy linting
  - 测试套件（Rust + Python）
  - 文档构建
  - 架构验证

### 2. **Coverage** (`.github/workflows/coverage.yml`)
- **触发条件**：Push 到 main、手动触发
- **生成内容**：
  - lcov 格式覆盖率数据
  - HTML 详细报告（可下载查看）
  - PR summary 中显示总覆盖率

### 3. **Scheduled Build** (`.github/workflows/scheduled.yml`)
- **触发条件**：每日 UTC 0:00（北京时间 08:00）
- **验证内容**：
  - 完整 acceptance.sh 套件
  - 构建脚本验证
  - Golden tests 回归测试

## 📋 开发工作流

### 典型的功能开发流程：

1. **创建分支**
   ```bash
   git checkout -b feature/new-feature
   ```

2. **进行开发**
   - 编辑代码
   - 提交时 pre-commit hook 自动检查格式

3. **本地测试**
   ```bash
   cargo nextest run --workspace
   ```

4. **推送代码**
   - Pre-push hook 自动运行 clippy 和架构验证
   ```bash
   git push origin feature/new-feature
   ```

5. **创建 PR**
   ```bash
   gh pr create --title "feat: add new feature" --body "Description..."
   ```

6. **CI 自动运行**
   - 所有 PR 检查必须通过才能合并
   - 查看 PR checks 了解失败原因

7. **合并后**
   - Main 分支 CI 运行
   - 覆盖率报告自动生成

## 🛠️ Git Hooks 详情

### Pre-commit Hook
- **检查内容**：Rust 格式（cargo fmt）、Python 格式（ruff）
- **何时触发**：每次 `git commit`
- **执行时间**：~5 秒
- **失败处理**：阻止提交，显示修复建议

### Pre-push Hook
- **检查内容**：Clippy linting、架构规则验证
- **何时触发**：每次 `git push`
- **执行时间**：~30 秒（取决于改动范围）
- **失败处理**：阻止推送，显示错误详情

## 🐳 DevContainer 详情

### 已安装工具
- Rust stable toolchain
- Python 3.12 + uv
- cargo-nextest
- cargo-llvm-cov
- CMake + Ninja
- Git + LLDB

### 数据持久化
- `.jfquant/` 目录挂载到容器，数据永久保存
- 容器重启后数据不丢失

### VSCode 集成
- rust-analyzer（自动配置 clippy 检查）
- LLDB 调试器
- Ruff Python formatter
- 自动格式化（保存时）

## 💡 常见问题

### Q: Hooks 太慢，能否跳过部分检查？

**A:** 可以编辑 `.cargo-husky/hooks/pre-push`，注释掉测试部分：
```bash
# if ! cargo nextest run --workspace --no-fail-fast; then
#     echo "❌ Tests failed"
#     exit 1
# fi
```

### Q: 如何在 CI 中查看失败详情？

**A:** 
1. 打开 PR 页面，点击 "Checks" 标签
2. 点击失败的 job（例如 "format-and-lint"）
3. 展开步骤查看详细日志
4. 复制错误信息本地修复

### Q: 覆盖率报告在哪里下载？

**A:**
1. 进入 Actions 页面
2. 点击最新的 "Coverage" workflow run
3. 下载 "coverage-html" artifact
4. 解压后用浏览器打开 `index.html`

### Q: DevContainer 构建失败怎么办？

**A:**
1. 检查 Docker 是否正常运行
2. 查看 "Dev Containers" 输出面板的错误日志
3. 尝试 "Rebuild Container"
4. 如果仍失败，可以继续使用本地开发环境

## 📚 参考资料

- [GitHub Actions 文档](https://docs.github.com/en/actions)
- [cargo-nextest 文档](https://nexte.st/)
- [cargo-llvm-cov 文档](https://github.com/taiki-e/cargo-llvm-cov)
- [VSCode Dev Containers](https://code.visualstudio.com/docs/devcontainers/containers)
- [Git Hooks 文档](https://git-scm.com/book/en/v2/Customizing-Git-Git-Hooks)

## 🎯 下一步优化（可选）

以下优化项已在计划中，可按需实施：

1. **发布自动化** - 从 git tag 自动构建和发布 .app/.dmg
2. **安全扫描** - 集成 cargo-audit 和 cargo-deny
3. **贡献指南** - 创建 CONTRIBUTING.md 和 PR/Issue 模板
4. **依赖更新自动化** - 配置 Dependabot 或 Renovate

详见 `/Users/jiangfan/.claude/plans/serene-gathering-deer.md` 完整方案。
