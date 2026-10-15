#!/bin/bash
# Build-time only. Runtime does not invoke uv, pip, Cargo or a system interpreter.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/../../.." && pwd)"
collector_root="$repo_root/crates/storage/collectors"
build_root="$repo_root/target/collector-build"
python_root="$build_root/python"
mkdir -p "$build_root"
lock_dir="$build_root/build.lock"
mkdir "$lock_dir" 2>/dev/null || { echo "Collector environment build already running; see $lock_dir" >&2; exit 1; }
trap 'rmdir "$lock_dir"' EXIT
IFS=$'\t' read -r url sha < <(/usr/bin/plutil -extract url raw "$collector_root/python-runtime.lock.json" | tr '\n' '\t'; /usr/bin/plutil -extract sha256 raw "$collector_root/python-runtime.lock.json")
archive="$build_root/python.tar.gz"
if [ ! -f "$archive" ]; then curl --fail --location --retry 2 --max-time 180 "$url" -o "$archive.part"; mv "$archive.part" "$archive"; fi
actual="$(shasum -a 256 "$archive" | awk '{print $1}')"
[ "$actual" = "$sha" ] || { echo 'Python runtime archive hash mismatch' >&2; exit 1; }
# Reuse only an environment created from this exact interpreter archive.
identity_file="$build_root/runtime-archive.sha256"
if [ ! -x "$python_root/bin/python3" ] || [ "$(cat "$identity_file" 2>/dev/null || true)" != "$sha" ]; then
  if [ -d "$python_root" ]; then rm -rf "$python_root"; fi
  tar -xzf "$archive" -C "$build_root"
  printf '%s\n' "$sha" > "$identity_file"
fi
uv pip sync --python "$python_root/bin/python3" --require-hashes "$collector_root/requirements.lock"
site_root="$python_root/lib/python3.12/site-packages"
mkdir -p "$site_root"
rm -rf "$site_root/jfquant_collectors"
cp -R "$collector_root/jfquant_collectors" "$site_root/"
cp "$collector_root/requirements.lock" "$collector_root/python-runtime.lock.json" "$python_root/"
"$python_root/bin/python3" -I -B "$repo_root/crates/app/packaging/rewrite-python.py" "$python_root"
"$python_root/bin/python3" -I -B "$repo_root/crates/app/packaging/verify-python.py" "$python_root" "$build_root/native-audit.json"
"$python_root/bin/python3" -I -B -c 'import akshare, yfinance, importlib.metadata; assert importlib.metadata.version("tdxaidata") == "1.2.2"; assert akshare.__version__ == "1.19.1"; assert yfinance.__version__ == "1.7.0"'
