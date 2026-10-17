#!/bin/bash
# Build-time only: verify upstream bytes, apply the reviewed patch, build a private wheel.
set -euo pipefail
repo_root="$(cd "$(dirname "$0")/../../.." && pwd)"
build_root="$repo_root/target/akquant-rights-build"
python="$repo_root/target/collector-build/python/bin/python3"
mkdir -p "$build_root"
"$python" -I -B - "$repo_root" "$build_root" <<'PY'
import hashlib, json, pathlib, sys, tarfile, urllib.request
repo, root = map(pathlib.Path, sys.argv[1:])
lock = json.loads((repo / 'crates/workflow/research/akquant-source.lock.json').read_text())
for name, expected in lock['inputs'].items():
    if hashlib.sha256((repo / name).read_bytes()).hexdigest() != expected:
        raise SystemExit('AKQuant reviewed input hash mismatch: ' + name)
archive = root / 'upstream.tar.gz'
if not archive.exists():
    with urllib.request.urlopen(lock['source_url'], timeout=90) as response:
        archive.write_bytes(response.read())
if hashlib.sha256(archive.read_bytes()).hexdigest() != lock['source_sha256']:
    raise SystemExit('AKQuant upstream source hash mismatch')
source = root / 'source'
source.mkdir(exist_ok=True)
with tarfile.open(archive) as tar:
    tar.extractall(source, filter='data')
source = source / ('akquant-' + lock['upstream_version'])
(source / 'jfquant-build-id').write_text('sha256:' + hashlib.sha256((repo / 'crates/workflow/research/akquant-source.lock.json').read_bytes()).hexdigest() + '\n')
if hashlib.sha256((source / 'Cargo.lock').read_bytes()).hexdigest() != lock['cargo_lock_sha256']:
    raise SystemExit('AKQuant Cargo lock mismatch')
for name in ('rights', 'rights_account', 'corporate_account'):
    (source / 'src' / (name + '.rs')).write_bytes((repo / 'crates/core/src' / (name + '.rs')).read_bytes())
PY
source="$build_root/source/akquant-0.3.66"
patch --batch --forward -p1 -d "$source" < "$repo_root/crates/app/packaging/akquant-rights.patch"
uv venv --python "$python" --allow-existing "$build_root/tools"
uv pip sync --python "$build_root/tools/bin/python" --require-hashes "$repo_root/crates/app/packaging/akquant-build-tools.lock"
PYO3_PYTHON="$python" MACOSX_DEPLOYMENT_TARGET=14.0 "$build_root/tools/bin/maturin" build --locked --release --manifest-path "$source/Cargo.toml" --interpreter "$python" --out "$build_root/patched-wheels"
"$python" -I -B - "$repo_root" "$build_root" <<'PY'
import hashlib, json, pathlib, sys
repo, root = map(pathlib.Path, sys.argv[1:])
wheel = next((root / 'patched-wheels').glob('akquant-0.3.66-*.whl'))
receipt = {'schema_version': 1, 'source_lock_sha256': hashlib.sha256((repo / 'crates/workflow/research/akquant-source.lock.json').read_bytes()).hexdigest(), 'wheel_sha256': hashlib.sha256(wheel.read_bytes()).hexdigest(), 'wheel': str(wheel)}
(root / 'build-receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
PY
