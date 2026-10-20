# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added - 工程化基础设施 (2026-10-20)

#### CI/CD 自动化
- **GitHub Actions CI workflow** (`.github/workflows/ci.yml`)
  - 自动格式检查（Rust + Python）
  - Clippy linting with `-D warnings`
  - 完整测试套件（cargo-nextest）
  - 文档构建验证
  - 架构规则检查
  - 在所有 PR 和 main 分支推送时运行

- **覆盖率报告 workflow** (`.github/workflows/coverage.yml`)
  - 使用 cargo-llvm-cov 生成 lcov 和 HTML 报告
  - 自动上传覆盖率报告为 artifact
  - 可手动触发或每次推送到 main 时运行

- **每日构建验证 workflow** (`.github/workflows/scheduled.yml`)
  - 每日 UTC 0:00（北京时间 08:00）自动运行
  - 执行完整 acceptance.sh 套件
  - 验证所有构建脚本和 golden tests
  - 及时发现依赖或环境问题

#### Git Hooks
- **Pre-commit hook** - 提交前自动格式检查
  - `cargo fmt --check` 验证 Rust 代码格式
  - `ruff format --check` 验证 Python 代码格式
  - 格式问题自动阻止提交并提示修复

- **Pre-push hook** - 推送前质量检查
  - `cargo clippy` 静态分析（warnings as errors）
  - 架构规则验证（`architecture.py`）
  - 可选的快速测试（默认注释，可手动开启）

- **cargo-husky 集成** - 自动安装 hooks
  - 首次 `cargo build` 时自动安装
  - 无需手动配置

- **手动安装脚本** (`scripts/install-hooks.sh`)
  - 备用安装方式
  - 适用于不使用 cargo-husky 的场景

#### 开发容器
- **VSCode DevContainer 配置** (`.devcontainer/devcontainer.json`)
  - Rust stable toolchain（Debian Bookworm base）
  - Python 3.12 + uv
  - cargo-nextest + cargo-llvm-cov 预装
  - rust-analyzer、LLDB、Ruff 扩展预配置
  - 保存时自动格式化
  - `.jfquant` 数据目录持久化

- **开发 Dockerfile** (`Dockerfile.dev`)
  - 基于 `rust:1-bookworm`
  - 包含所有构建依赖（CMake、Ninja、LLDB）
  - 非 root 用户（vscode）
  - 预配置 shell 环境

#### 文档和工具
- **工程化使用指南** (`docs/ENGINEERING.md`)
  - Git hooks 使用说明
  - 本地测试覆盖率生成指南
  - DevContainer 快速开始
  - CI/CD workflow 详解
  - 开发工作流最佳实践
  - 常见问题解答

- **验证脚本** (`scripts/verify-setup.sh`)
  - 一键检查所有工程化配置
  - 彩色输出，友好的错误提示
  - 验证 hooks、workflows、devcontainer、文档

- **工具安装脚本** (`scripts/install-dev-tools.sh`)
  - 自动安装 cargo-nextest 和 cargo-llvm-cov
  - 检测已安装工具避免重复安装
  - 提供使用示例

#### 项目配置更新
- **主 README 增强**
  - 添加 CI status badge
  - 添加 License badge
  - 添加 Rust edition badge
  - 新增"开发者工具"章节
  - DevContainer 快速开始指南
  - 工程化文档链接

- **Cargo.toml 更新**
  - 添加 cargo-husky 依赖（dev-dependencies）
  - 配置 pre-commit 和 pre-push hooks

- **.gitignore 更新**
  - 忽略 `lcov.info` 覆盖率文件
  - 忽略 `coverage-html/` 目录
  - 忽略 `junit.xml` 测试报告

### Changed
- 无破坏性变更，所有新增内容向后兼容

### Fixed
- 无修复内容（此版本为新增功能）

---

## 参照的优秀项目实践

本次工程化改进参照了以下优秀开源项目的最佳实践：

### Rust 生态
- **tokio** - 完善的 CI/CD、严格的 clippy 规则、覆盖率报告
- **serde** - 简洁的测试架构、文档构建验证
- **ripgrep** - 跨平台构建、性能基准测试

### 量化交易项目
- **Nautilus Trader** - Python + Rust 混合项目的测试策略
- **QuestDB** - 时间序列数据库的质量保障流程

### CI/CD 模式
- GitHub Actions 官方推荐的 Rust workflow
- cargo-nextest 作者推荐的 CI 配置
- LLVM 覆盖率工具的最佳实践

---

## 未来计划

以下优化项已在工程化路线图中，将根据需求逐步实施：

### Phase 2（可选）
- [ ] 安全扫描（cargo-audit、cargo-deny）
- [ ] 依赖更新自动化（Dependabot/Renovate）
- [ ] PR/Issue 模板
- [ ] CONTRIBUTING.md 贡献指南

### Phase 3（长期）
- [ ] 发布自动化（从 git tag 触发）
- [ ] 跨平台构建（Linux、Windows）
- [ ] 性能回归测试
- [ ] 文档网站（mdBook 或 GitHub Pages）

---

## 如何使用

1. **立即体验**：
   ```bash
   # 验证所有配置
   bash scripts/verify-setup.sh
   
   # 安装开发工具（可选）
   bash scripts/install-dev-tools.sh
   
   # 安装 Git hooks
   bash scripts/install-hooks.sh
   ```

2. **使用 DevContainer**（推荐）：
   - 在 VSCode 中打开项目
   - 点击 "Reopen in Container"
   - 等待容器构建完成（首次 5-10 分钟）

3. **查看 CI 运行**：
   - 推送代码到 GitHub
   - 访问 Actions 页面查看 workflow 运行状态

详细使用说明：[docs/ENGINEERING.md](docs/ENGINEERING.md)
