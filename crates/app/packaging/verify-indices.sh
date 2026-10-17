#!/bin/bash
# Offline runtime configuration acceptance using only shipped binaries and Python.
set -euo pipefail
app="${1:?App bundle path required}"
check_root="${2:?Generated acceptance output directory required}"
mkdir -p "$check_root"
check_root="$(mktemp -d "$check_root/run.XXXXXX")"
mkdir -p "$check_root/home"
cli="$app/Contents/MacOS/jfquant"
python="$app/Contents/Resources/python/bin/python3"
run_cli() { env -i HOME="$check_root/home" PATH=/usr/bin:/bin "$cli" --workspace "$check_root/workspace" "$@"; }
trap 'run_cli service stop >/dev/null 2>&1 || true' EXIT
run_cli indices > "$check_root/builtin.json"
[ ! -e "$check_root/workspace" ]
run_cli workspace create "$check_root/workspace" > "$check_root/created.json"
run_cli indices export builtin:csi1000 seed.json > "$check_root/exported.json"
"$python" -I -B - "$check_root/workspace/seed.json" "$check_root/workspace/custom.json" <<'PY'
import json,sys
c=json.load(open(sys.argv[1]))
c['definition'].update(id='packaged_index',name='Packaged runtime fixture',official_code='TEST001',symbol=None)
c['data_key']='IDX_packaged_index'
c['mappings']={'hithink':'TEST001.CSI'}
json.dump(c,open(sys.argv[2],'w'))
PY
run_cli indices validate custom.json > "$check_root/validated.json"
run_cli indices import custom.json > "$check_root/imported.json"
run_cli indices list > "$check_root/versions.json"
version="$("$python" -I -B - "$check_root/versions.json" <<'PY'
import json,sys
v=[v for v in json.load(open(sys.argv[1])) if v['config']['definition']['id']=='packaged_index']
assert len(v)==1 and not v[0]['builtin']
print(v[0]['id'])
PY
)"
run_cli indices show "$version" > "$check_root/shown.json"
run_cli indices export "$version" roundtrip.json > "$check_root/roundtrip.json"
run_cli templates default --index-version "$version" > "$check_root/template.json"
"$python" -I -B - "$check_root" <<'PY'
import json,sys,pathlib
r=pathlib.Path(sys.argv[1])
assert json.load(open(r/'validated.json'))['data_ready'] is False
assert json.load(open(r/'workspace/custom.json'))==json.load(open(r/'workspace/roundtrip.json'))
t=json.load(open(r/'template.json'))
assert t['benchmark']=='IDX_packaged_index' and t['pool_version']=='index:packaged_index'
PY
run_cli indices verify --help > "$check_root/verify-help.txt"
"$python" -I -B - "$check_root/workspace/custom.json" "$check_root/workspace/msci.json" <<'PY'
import json,sys
c=json.load(open(sys.argv[1]))
c['definition'].update(id='packaged_msci',name='Packaged MSCI close fixture',official_code='746059')
c['data_key']='IDX_packaged_msci'
c['mappings']={'msci':'746059'}
json.dump(c,open(sys.argv[2],'w'))
PY
run_cli indices validate msci.json > "$check_root/msci-validated.json"
run_cli indices import msci.json > "$check_root/msci-imported.json"
run_cli indices collect-msci --help > "$check_root/msci-help.txt"
"$python" -I -B - "$check_root" <<'PY'
import json,sys,pathlib
r=pathlib.Path(sys.argv[1])
assert json.load(open(r/'msci-validated.json'))['data_ready'] is False
assert '--calendar-market' in (r/'msci-help.txt').read_text()
PY
echo "Packaged runtime index configuration verified: $check_root"
