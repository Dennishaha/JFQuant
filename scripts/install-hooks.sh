#!/bin/bash
# Manual Git hooks installation script
# Use this if you prefer not to use cargo-husky automatic installation

set -euo pipefail

echo "🔧 Installing Git hooks manually..."

# Get the git hooks directory
hooks_dir="$(git rev-parse --git-dir)/hooks"

# Ensure hooks directory exists
mkdir -p "$hooks_dir"

# Copy pre-commit hook
echo "📋 Installing pre-commit hook..."
cp .cargo-husky/hooks/pre-commit "$hooks_dir/pre-commit"
chmod +x "$hooks_dir/pre-commit"

# Copy pre-push hook
echo "📋 Installing pre-push hook..."
cp .cargo-husky/hooks/pre-push "$hooks_dir/pre-push"
chmod +x "$hooks_dir/pre-push"

echo "✅ Git hooks installed successfully!"
echo ""
echo "Installed hooks:"
echo "  - pre-commit: Format checks"
echo "  - pre-push: Clippy + architecture validation"
echo ""
echo "To skip hooks temporarily, use:"
echo "  git commit --no-verify"
echo "  git push --no-verify"
