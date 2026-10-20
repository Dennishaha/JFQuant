#!/bin/bash
# Storage Cleanup Script
# 安全清理已迁移的 JSON 文件和备份

set -euo pipefail

WORKSPACE_ROOT="${WORKSPACE_ROOT:-.jfquant}"
DRY_RUN="${DRY_RUN:-1}"

echo "🧹 Storage Cleanup Script"
echo "========================="
echo ""

# 颜色定义
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# 辅助函数
info() {
    echo -e "${GREEN}✓${NC} $1"
}

warn() {
    echo -e "${YELLOW}⚠${NC} $1"
}

error() {
    echo -e "${RED}✗${NC} $1"
}

format_size() {
    if command -v numfmt &> /dev/null; then
        numfmt --to=iec-i --suffix=B "$1" 2>/dev/null || echo "$1 bytes"
    else
        # macOS fallback
        local bytes=$1
        if [ "$bytes" -lt 1024 ]; then
            echo "${bytes}B"
        elif [ "$bytes" -lt 1048576 ]; then
            echo "$((bytes / 1024))KB"
        elif [ "$bytes" -lt 1073741824 ]; then
            echo "$((bytes / 1048576))MB"
        else
            echo "$((bytes / 1073741824))GB"
        fi
    fi
}

get_size() {
    # macOS and Linux compatible
    if [ -f "$1" ]; then
        stat -f%z "$1" 2>/dev/null || stat -c%s "$1" 2>/dev/null
    elif [ -d "$1" ]; then
        du -sk "$1" | cut -f1 | awk '{print $1 * 1024}'
    else
        echo "0"
    fi
}

# 检查目录
if [ ! -d "$WORKSPACE_ROOT" ]; then
    error "Workspace not found: $WORKSPACE_ROOT"
    exit 1
fi

cd "$WORKSPACE_ROOT"

# 计算空间
total_saved=0

echo "## 1. 检查 JSON 迁移备份"
echo ""

MIGRATION_BACKUP=$(find archive/ -type d -name "json-migration-*" 2>/dev/null | head -1)

if [ -n "$MIGRATION_BACKUP" ]; then
    BACKUP_SIZE=$(get_size "$MIGRATION_BACKUP")
    echo "找到备份: $MIGRATION_BACKUP"
    echo "大小: $(format_size $BACKUP_SIZE)"

    # 验证迁移成功（检查数据库中有数据）
    CATALOG_COUNT=$(sqlite3 storage/index.sqlite "SELECT COUNT(*) FROM catalog_entries;" 2>/dev/null || echo "0")

    if [ "$CATALOG_COUNT" -gt 0 ]; then
        info "迁移验证成功 (catalog_entries: $CATALOG_COUNT)"

        if [ "$DRY_RUN" = "1" ]; then
            warn "[DRY RUN] 将删除: $MIGRATION_BACKUP"
        else
            rm -rf "$MIGRATION_BACKUP"
            info "已删除备份"
            total_saved=$((total_saved + BACKUP_SIZE))
        fi
    else
        error "迁移验证失败，跳过删除"
    fi
else
    info "未找到迁移备份（可能已清理）"
fi

echo ""
echo "## 2. 检查旧的 JSON 文件"
echo ""

# inventory.json 和 snapshot.json 可能已被迁移
if [ -f "storage/inventory.json" ]; then
    INV_SIZE=$(get_size "storage/inventory.json")
    echo "找到: storage/inventory.json ($(format_size $INV_SIZE))"

    # 检查是否已迁移（catalog_entries 有数据）
    if [ "$CATALOG_COUNT" -gt 0 ]; then
        if [ "$DRY_RUN" = "1" ]; then
            warn "[DRY RUN] 将删除: storage/inventory.json"
        else
            rm "storage/inventory.json"
            info "已删除 inventory.json"
            total_saved=$((total_saved + INV_SIZE))
        fi
    fi
fi

if [ -f "storage/snapshot.json" ]; then
    SNAP_SIZE=$(get_size "storage/snapshot.json")
    echo "找到: storage/snapshot.json ($(format_size $SNAP_SIZE))"

    if [ "$CATALOG_COUNT" -gt 0 ]; then
        if [ "$DRY_RUN" = "1" ]; then
            warn "[DRY RUN] 将删除: storage/snapshot.json"
        else
            rm "storage/snapshot.json"
            info "已删除 snapshot.json"
            total_saved=$((total_saved + SNAP_SIZE))
        fi
    fi
fi

# 清理 storage/ 下的迁移目录
shopt -s nullglob
for dir in storage/json-migration-*; do
    if [ -d "$dir" ]; then
        DIR_SIZE=$(get_size "$dir")
        echo "找到: $dir ($(format_size $DIR_SIZE))"

        if [ "$DRY_RUN" = "1" ]; then
            warn "[DRY RUN] 将删除: $dir"
        else
            rm -rf "$dir"
            info "已删除 $dir"
            total_saved=$((total_saved + DIR_SIZE))
        fi
    fi
done
shopt -u nullglob

echo ""
echo "## 3. 数据库维护"
echo ""

DB_PATH="storage/index.sqlite"
DB_SIZE_BEFORE=$(get_size "$DB_PATH")
echo "数据库大小: $(format_size $DB_SIZE_BEFORE)"

# VACUUM 数据库
if [ "$DRY_RUN" = "1" ]; then
    warn "[DRY RUN] 将执行: VACUUM"
else
    echo "执行 VACUUM..."
    sqlite3 "$DB_PATH" "VACUUM;"

    DB_SIZE_AFTER=$(get_size "$DB_PATH")
    DB_SAVED=$((DB_SIZE_BEFORE - DB_SIZE_AFTER))

    if [ $DB_SAVED -gt 0 ]; then
        info "VACUUM 完成，节省 $(format_size $DB_SAVED)"
        total_saved=$((total_saved + DB_SAVED))
    else
        info "VACUUM 完成（无额外空间节省）"
    fi
fi

echo ""
echo "========================="
echo "清理总结"
echo "========================="

if [ "$DRY_RUN" = "1" ]; then
    echo ""
    warn "这是 DRY RUN 模式，未实际删除文件"
    echo ""
    echo "预计节省空间: $(format_size $total_saved)"
    echo ""
    echo "要执行实际清理，运行:"
    echo "  DRY_RUN=0 $0"
else
    echo ""
    info "清理完成！"
    echo ""
    echo "节省空间: $(format_size $total_saved)"
fi

echo ""
