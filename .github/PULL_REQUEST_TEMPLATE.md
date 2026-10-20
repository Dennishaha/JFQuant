---
name: Pull Request
about: 提交代码改动
title: ''
labels: ''
assignees: ''
---

## 📝 改动概述

简短描述这个 PR 做了什么。

## 🔗 相关 Issue

Closes #<issue-number>

（如果没有相关 Issue，请先创建一个 Issue 讨论你的改动）

## 🎯 改动类型

请勾选相关的选项：

- [ ] 🐛 Bug 修复（non-breaking change 修复了一个问题）
- [ ] ✨ 新功能（non-breaking change 添加了功能）
- [ ] 💥 Breaking change（修复或功能导致现有功能无法正常工作）
- [ ] 📝 文档更新
- [ ] ♻️ 代码重构（不影响功能的代码改进）
- [ ] ⚡️ 性能优化
- [ ] ✅ 测试改进

## 🧪 测试

描述你如何测试了这些改动：

- [ ] 单元测试通过（`cargo nextest run --workspace`）
- [ ] 集成测试通过
- [ ] 手动测试通过
- [ ] 添加了新的测试覆盖新代码

### 测试步骤

1. ...
2. ...
3. ...

## 📊 性能影响

（如果适用）描述性能方面的改进或影响。

**基准测试结果：**
```
粘贴 cargo bench 的输出
```

## 🖼️ 截图/视频

（如果是 UI 改动）添加截图或视频展示改动。

**改动前：**
<!-- 截图 -->

**改动后：**
<!-- 截图 -->

## ✅ 检查清单

请确认以下事项：

- [ ] 我的代码遵循了项目的代码规范（[AGENTS.md](../AGENTS.md)）
- [ ] 我已经进行了自我 review
- [ ] 我已经添加了必要的注释，特别是复杂的逻辑
- [ ] 我已经更新了相关文档
- [ ] 我的改动没有产生新的警告
- [ ] 我已经添加了测试证明我的修复有效或功能工作正常
- [ ] 新的和现有的单元测试都通过（`./scripts/dev test`）
- [ ] 代码格式化通过（`cargo fmt --all -- --check`）
- [ ] Clippy 检查通过（`cargo clippy --workspace --all-targets -- -D warnings`）
- [ ] 架构验证通过（`python3 crates/workflow/checks/architecture.py`）

## 💭 其他信息

其他任何 reviewers 需要知道的信息。

## 📚 相关 PRs/Issues

（可选）链接到相关的 PRs 或 Issues。

---

<!-- 
感谢你的贡献！你的 PR 会由维护者 review。
如果有任何疑问，请在 PR 中留言。
-->
