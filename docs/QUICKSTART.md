# 快速入门指南

欢迎加入 JFQuant 开发！这份指南将帮助你在 5 分钟内完成环境设置并开始贡献代码。

## 前置要求

- macOS (推荐) 或 Linux
- [Rust](https://rustup.rs) (stable)
- Python 3.11+
- Git

## 一键设置

```bash
# 克隆仓库
git clone https://github.com/Dennishaha/JFQuant.git
cd JFQuant

# 运行设置脚本
bash scripts/dev-setup.sh
```

脚本会自动安装所有开发工具，配置 Git hooks，并执行首次构建。

## 手动设置

如果你更喜欢手动控制每一步：

### 1. 安装开发工具

```bash
# Rust 工具
cargo install cargo-nextest    # 更快的测试运行器
cargo install cargo-llvm-cov    # 覆盖率工具（可选）
cargo install cargo-watch       # 监视模式（可选）

# Python 工具
pip3 install ruff               # 格式化和 lint
```

### 2. 安装 Git Hooks

```bash
bash scripts/install-hooks.sh
```

### 3. 首次构建

```bash
cargo build --workspace
```

### 4. 运行测试验证

```bash
cargo nextest run --workspace
```

## 开发工作流

### 快速检查命令

我们提供了统一的开发脚本 `./scripts/dev`：

```bash
# 快速代码检查（格式化 + lint + 架构）
./scripts/dev check

# 运行所有测试
./scripts/dev test

# 仅运行特定 crate 的测试
./scripts/dev test storage

# 监视模式（自动重新运行）
./scripts/dev watch

# 查看测试覆盖率
./scripts/dev cov

# 发布前完整检查
./scripts/dev release
```

### 日常开发循环

```bash
# 1. 创建新分支
git checkout -b feature/my-feature

# 2. 编写代码
vim crates/storage/src/lib.rs

# 3. 监视模式下开发（自动重新测试）
./scripts/dev watch

# 4. 提交前检查
./scripts/dev check

# 5. 提交（Git hooks 会自动运行检查）
git add -A
git commit -m "feat: add new feature"

# 6. 推送到远程
git push -u origin feature/my-feature
```

### Git Hooks 说明

**Pre-commit Hook**（提交前）：
- 检查代码格式化
- 运行 Clippy lint
- 验证架构依赖

**Pre-push Hook**（推送前）：
- 运行快速单元测试
- 验证 Cargo.lock 已提交

如果你需要跳过 hooks（不推荐）：
```bash
git commit --no-verify
```

## 项目结构速览

```
JFQuant/
├── crates/
│   ├── cli/          # 命令行界面
│   ├── storage/      # 数据存储和收集
│   ├── factors/      # 因子计算
│   ├── models/       # 模型训练和预测
│   ├── api/          # HTTP API 服务
│   └── app/          # macOS 应用打包
├── scripts/
│   ├── dev           # 开发便捷脚本
│   ├── dev-setup.sh  # 环境设置脚本
│   └── verify-setup.sh  # 验证脚本
├── docs/
│   ├── ENGINEERING.md              # 工程化指南
│   └── ENGINEERING_OPTIMIZATION.md # 优化建议
├── AGENTS.md         # 代码规范（必读）
└── README.md
```

## 常见任务

### 添加新的 Rust 依赖

```bash
cd crates/<crate-name>
cargo add <dependency>

# 运行测试确保没有破坏任何东西
cargo nextest run -p jfquant-<crate-name>
```

### 添加新的 Python 依赖

```bash
# 编辑 requirements.txt
vim crates/storage/collectors/requirements.txt

# 重新构建 Python 环境
bash crates/app/packaging/build-collector.sh
```

### 运行特定测试

```bash
# 运行单个测试文件
cargo nextest run --test test_name

# 运行单个测试函数
cargo nextest run test_function_name

# 运行特定 crate 的测试
cargo nextest run -p jfquant-storage
```

### 查看测试覆盖率

```bash
# 生成 HTML 覆盖率报告
./scripts/dev cov

# 或手动运行
cargo llvm-cov nextest --html
open target/llvm-cov/html/index.html
```

### 性能基准测试

```bash
# 运行所有基准测试
cargo bench --workspace

# 运行特定 crate 的基准测试
cargo bench -p jfquant-factors
```

### 验证架构依赖

```bash
python3 crates/workflow/checks/architecture.py

# 更新依赖图
python3 crates/workflow/checks/architecture.py --update
```

## CI/CD 说明

### CI 流水线

每次推送代码时，GitHub Actions 会自动运行：

1. **快速检查**（5 分钟）：格式化、lint、架构验证
2. **单元测试**（10 分钟）：快速单元测试
3. **文档构建**：验证文档可以正确生成

完整测试（包括集成测试）仅在 `main` 分支运行。

### 查看 CI 状态

```bash
# 使用 GitHub CLI
gh run list

# 查看最新运行
gh run view

# 查看失败的日志
gh run view --log-failed
```

### PR 检查清单

在创建 PR 之前，请确保：

- [ ] 所有测试通过 (`./scripts/dev test`)
- [ ] 代码已格式化 (`./scripts/dev fmt`)
- [ ] Clippy 无警告 (`./scripts/dev lint`)
- [ ] 架构依赖验证通过 (`./scripts/dev arch`)
- [ ] 更新了相关文档
- [ ] 添加了必要的测试

## 故障排除

### 编译错误

```bash
# 清理并重新构建
cargo clean
cargo build --workspace
```

### 测试失败

```bash
# 查看详细测试输出
cargo nextest run --workspace --no-fail-fast

# 运行特定失败的测试
cargo test <test-name> -- --nocapture
```

### Git Hooks 问题

```bash
# 重新安装 hooks
bash scripts/install-hooks.sh

# 检查 hooks 是否可执行
ls -la .cargo-husky/hooks/

# 如果需要禁用 hooks
rm .cargo-husky/hooks/pre-commit
rm .cargo-husky/hooks/pre-push
```

### Python 环境问题

```bash
# 重新构建 Python 环境
rm -rf target/collector-build/
bash crates/app/packaging/build-collector.sh

# 验证 Python 环境
target/collector-build/python/bin/python3 --version
```

## 获取帮助

- **文档**: 查看 `docs/` 目录
- **Issue**: 在 GitHub 上创建 issue
- **代码规范**: 阅读 `AGENTS.md`
- **工程化指南**: 阅读 `docs/ENGINEERING.md`

## 下一步

- 阅读 [AGENTS.md](../AGENTS.md) 了解代码规范
- 阅读 [ENGINEERING.md](./ENGINEERING.md) 了解工程化实践
- 浏览现有代码熟悉项目结构
- 挑选一个 good-first-issue 开始贡献

Happy coding! 🚀
