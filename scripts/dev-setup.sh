#!/bin/bash
# JFQuant 开发环境设置脚本
# 自动安装和配置所有开发工具

set -euo pipefail

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
RED='\033[0;31m'
NC='\033[0m'

echo -e "${BLUE}🔧 设置 JFQuant 开发环境...${NC}\n"

# 检查 Rust 工具链
check_rust() {
    if ! command -v cargo &> /dev/null; then
        echo -e "${RED}❌ Rust 未安装${NC}"
        echo "请访问 https://rustup.rs 安装 Rust"
        exit 1
    fi
    echo -e "${GREEN}✓${NC} Rust $(rustc --version)"
}

# 检查 Python
check_python() {
    if ! command -v python3 &> /dev/null; then
        echo -e "${RED}❌ Python3 未安装${NC}"
        exit 1
    fi
    echo -e "${GREEN}✓${NC} Python $(python3 --version)"
}

# 安装 Rust 开发工具
install_rust_tools() {
    echo -e "\n${YELLOW}安装 Rust 开发工具...${NC}"

    local tools=(
        "cargo-nextest:cargo nextest"
        "cargo-llvm-cov:cargo llvm-cov"
        "cargo-watch:cargo watch"
    )

    for tool_spec in "${tools[@]}"; do
        IFS=':' read -r package command <<< "$tool_spec"

        if ! $command --version &> /dev/null; then
            echo -e "  安装 ${package}..."
            cargo install "$package"
        else
            echo -e "  ${GREEN}✓${NC} ${package} 已安装"
        fi
    done
}

# 安装 Python 开发工具
install_python_tools() {
    echo -e "\n${YELLOW}安装 Python 开发工具...${NC}"

    # 检查 ruff
    if ! python3 -m ruff --version &> /dev/null; then
        echo -e "  安装 ruff..."
        pip3 install ruff
    else
        echo -e "  ${GREEN}✓${NC} ruff 已安装"
    fi

    # 安装 Python 开发依赖（如果有 requirements-dev.txt）
    if [ -f "requirements-dev.txt" ]; then
        echo -e "  安装 Python 开发依赖..."
        pip3 install -r requirements-dev.txt
    fi

    # 安装 collector 依赖
    if [ -f "crates/storage/collectors/requirements.txt" ]; then
        echo -e "  安装 collector 依赖..."
        pip3 install -r crates/storage/collectors/requirements.txt
    fi

    # 安装 workflow 依赖
    if [ -f "crates/workflow/research/requirements.txt" ]; then
        echo -e "  安装 workflow 依赖..."
        pip3 install -r crates/workflow/research/requirements.txt
    fi
}

# 安装 Git hooks
install_git_hooks() {
    echo -e "\n${YELLOW}安装 Git hooks...${NC}"

    if [ -f "./scripts/install-hooks.sh" ]; then
        bash ./scripts/install-hooks.sh
        echo -e "  ${GREEN}✓${NC} Git hooks 已安装"
    else
        echo -e "  ${YELLOW}⚠${NC} install-hooks.sh 不存在，跳过"
    fi
}

# 下载原生依赖（如果需要）
download_native_deps() {
    echo -e "\n${YELLOW}检查原生依赖...${NC}"

    # 这里可以添加 DuckDB、LightGBM 等的下载脚本
    # 目前只是占位符
    echo -e "  ${GREEN}✓${NC} 原生依赖将在首次构建时自动下载"
}

# 首次构建
first_build() {
    echo -e "\n${YELLOW}执行首次构建（这可能需要几分钟）...${NC}"

    # 构建 workspace
    cargo build --workspace

    echo -e "  ${GREEN}✓${NC} 构建完成"
}

# 运行快速测试验证
quick_test() {
    echo -e "\n${YELLOW}运行快速测试验证...${NC}"

    if command -v cargo-nextest &> /dev/null; then
        cargo nextest run --lib --no-fail-fast
    else
        cargo test --lib --no-fail-fast
    fi

    echo -e "  ${GREEN}✓${NC} 测试通过"
}

# 显示后续步骤
show_next_steps() {
    cat << EOF

${GREEN}✅ 开发环境设置完成！${NC}

${BLUE}快速开始：${NC}
  ./scripts/dev check       运行快速检查
  ./scripts/dev test        运行所有测试
  ./scripts/dev watch       启动监视模式
  ./scripts/dev cov         查看测试覆盖率

${BLUE}常用命令：${NC}
  cargo build               构建项目
  cargo nextest run         运行测试（推荐）
  cargo clippy              运行 lint 检查
  cargo fmt                 格式化代码

${BLUE}详细文档：${NC}
  README.md                 项目介绍
  AGENTS.md                 代码规范
  docs/ENGINEERING.md       工程化指南
  docs/ENGINEERING_OPTIMIZATION.md  优化建议

${YELLOW}提示：${NC}
  运行 ./scripts/dev --help 查看所有可用命令

EOF
}

# 主流程
main() {
    check_rust
    check_python

    # 询问是否安装工具（可选）
    echo -e "\n${BLUE}是否安装开发工具？${NC}"
    read -p "这将安装 cargo-nextest, cargo-llvm-cov 等工具 (y/N): " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        install_rust_tools
        install_python_tools
    else
        echo -e "${YELLOW}跳过工具安装${NC}"
    fi

    install_git_hooks
    download_native_deps

    # 询问是否立即构建
    echo -e "\n${BLUE}是否执行首次构建？${NC}"
    read -p "这将编译整个项目 (y/N): " -n 1 -r
    echo
    if [[ $REPLY =~ ^[Yy]$ ]]; then
        first_build

        # 询问是否运行测试
        echo -e "\n${BLUE}是否运行快速测试验证？${NC}"
        read -p "这将验证环境配置是否正确 (y/N): " -n 1 -r
        echo
        if [[ $REPLY =~ ^[Yy]$ ]]; then
            quick_test
        fi
    else
        echo -e "${YELLOW}跳过构建，稍后可手动执行: cargo build${NC}"
    fi

    show_next_steps
}

# 执行
main
