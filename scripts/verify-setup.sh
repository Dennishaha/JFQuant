#!/bin/bash
# 验证工程化工具配置是否正确
set -euo pipefail

echo "🔍 验证 JFQuant 工程化工具配置..."
echo ""

# Color codes
GREEN='\033[0;32m'
RED='\033[0;31m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

passed=0
failed=0

check() {
    local name="$1"
    local command="$2"

    echo -n "检查 $name... "
    if eval "$command" > /dev/null 2>&1; then
        echo -e "${GREEN}✓${NC}"
        ((passed++))
    else
        echo -e "${RED}✗${NC}"
        ((failed++))
    fi
}

# 1. Git hooks
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "1️⃣  Git Hooks"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
check "Pre-commit hook 存在" "[ -f .cargo-husky/hooks/pre-commit ]"
check "Pre-push hook 存在" "[ -f .cargo-husky/hooks/pre-push ]"
check "安装脚本存在" "[ -f scripts/install-hooks.sh ]"
check "安装脚本可执行" "[ -x scripts/install-hooks.sh ]"
echo ""

# 2. CI/CD workflows
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "2️⃣  GitHub Actions Workflows"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
check "CI workflow 存在" "[ -f .github/workflows/ci.yml ]"
check "Coverage workflow 存在" "[ -f .github/workflows/coverage.yml ]"
check "Scheduled workflow 存在" "[ -f .github/workflows/scheduled.yml ]"
echo ""

# 3. DevContainer
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "3️⃣  DevContainer 配置"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
check "DevContainer 配置存在" "[ -f .devcontainer/devcontainer.json ]"
check "Dockerfile 存在" "[ -f Dockerfile.dev ]"
echo ""

# 4. 文档
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "4️⃣  文档"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
check "工程化指南存在" "[ -f docs/ENGINEERING.md ]"
check "主 README 已更新" "grep -q 'CI Status' README.md"
echo ""

# 5. 工具可用性（可选）
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "5️⃣  开发工具（可选）"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
if command -v cargo >/dev/null 2>&1; then
    check "Cargo 可用" "cargo --version"
    check "Rustfmt 可用" "cargo fmt --version"
    check "Clippy 可用" "cargo clippy --version"
else
    echo -e "${YELLOW}⚠${NC} Cargo 未安装（在 DevContainer 中会自动可用）"
fi

if command -v cargo-nextest >/dev/null 2>&1; then
    check "cargo-nextest 可用" "cargo nextest --version"
else
    echo -e "${YELLOW}⚠${NC} cargo-nextest 未安装（可选，CI 中会自动安装）"
fi

if command -v cargo-llvm-cov >/dev/null 2>&1; then
    check "cargo-llvm-cov 可用" "cargo-llvm-cov --version"
else
    echo -e "${YELLOW}⚠${NC} cargo-llvm-cov 未安装（可选，用于本地覆盖率）"
fi
echo ""

# 总结
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "📊 验证结果"
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo -e "通过: ${GREEN}$passed${NC}"
if [ $failed -gt 0 ]; then
    echo -e "失败: ${RED}$failed${NC}"
    echo ""
    echo -e "${RED}❌ 某些检查未通过${NC}"
    exit 1
else
    echo -e "失败: $failed"
    echo ""
    echo -e "${GREEN}✅ 所有核心配置验证通过！${NC}"
    echo ""
    echo "下一步："
    echo "  1. 提交所有新文件到 Git"
    echo "  2. 推送到 GitHub 触发首次 CI 运行"
    echo "  3. 查看 GitHub Actions 页面确认 workflows 正常运行"
    echo "  4. 可选：安装 Git hooks: bash scripts/install-hooks.sh"
    echo ""
    echo "详细使用指南：docs/ENGINEERING.md"
fi
