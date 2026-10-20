#!/bin/bash
# 一键安装所有开发工具（可选）
set -euo pipefail

echo "🚀 安装 JFQuant 开发工具..."
echo ""

# Check if cargo is available
if ! command -v cargo &> /dev/null; then
    echo "❌ Cargo 未安装，请先安装 Rust: https://rustup.rs/"
    exit 1
fi

# Install Rust development tools
echo "📦 安装 Rust 开发工具..."
echo ""

install_tool() {
    local tool_name="$1"
    local install_cmd="$2"

    echo -n "检查 $tool_name... "
    if command -v "$tool_name" &> /dev/null; then
        echo "✓ 已安装"
    else
        echo "安装中..."
        eval "$install_cmd"
        echo "✓ $tool_name 安装完成"
    fi
}

# Install cargo tools
install_tool "cargo-nextest" "cargo install cargo-nextest"
install_tool "cargo-llvm-cov" "cargo install cargo-llvm-cov"

echo ""
echo "━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━"
echo "✅ 所有工具安装完成！"
echo ""
echo "已安装工具："
echo "  • cargo-nextest: 快速测试运行器"
echo "  • cargo-llvm-cov: 代码覆盖率工具"
echo ""
echo "使用示例："
echo "  cargo nextest run --workspace         # 运行测试"
echo "  cargo llvm-cov --html --open          # 生成覆盖率报告"
echo ""
echo "注意：这些工具在 DevContainer 中已预装，本地开发可选。"
