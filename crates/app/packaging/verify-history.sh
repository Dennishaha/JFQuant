#!/bin/bash
# Offline reconstruction using only packaged binaries/runtime; never fetch real data here.
set -euo pipefail
app="${1:?App bundle path required}"
check_root="${2:?Acceptance output root required}"
mkdir -p "$check_root"
check_root="$(mktemp -d "$check_root/run.XXXXXX")"
cli="$app/Contents/MacOS/jfquant"
python="$app/Contents/Resources/python/bin/python3"
mkdir -p "$check_root/home"
run_cli() { env -i HOME="$check_root/home" PATH=/usr/bin:/bin "$cli" --workspace "$check_root/workspace" "$@"; }
trap 'run_cli service stop >/dev/null 2>&1 || true' EXIT
wait_task() {
  local history_job
  history_job="$(plutil -extract task_id raw -o - "$check_root/submission.json")"
  run_cli jobs wait "$history_job" --timeout 60 > "$1"
  [ "$(plutil -extract record.status raw -o - "$1")" = succeeded ]
}
run_cli workspace create "$check_root/workspace" > "$check_root/created.json"
run_cli research demo > "$check_root/submission.json"
wait_task "$check_root/demo.json"
market="$(plutil -extract results.data.id raw -o - "$check_root/demo.json")"
"$python" -I -B - "$check_root/workspace" <<'PY'
import json,hashlib,pathlib,sys
r=pathlib.Path(sys.argv[1])
text='SYNTHETIC CSI300 000300 full roster 600000; 2007-01-03 before open remove 600000 add 600001; complete event delivery 2007-01-02 to 2007-01-04 review interim code_change merger spinoff.'
(r/'original.txt').write_text(text)
cite=[{'document':'source','excerpt':text}]
facts={'schema_version':1,'official_code':'000300','index_name':'CSI300','identity':cite,'requested':{'start':'2007-01-02','end':'2007-01-04'},
'anchors':[{'id':'roster','at':{'date':'2007-01-02','boundary':'before_open'},'known_at':'2007-01-01T00:00:00Z','members':['SH600000'],'citations':cite}],
'events':[{'id':'change','revision':1,'supersedes':None,'at':{'date':'2007-01-03','boundary':'before_open'},'known_at':'2007-01-01T00:00:00Z','category':'review','add':['SH600001'],'remove':['SH600000'],'citations':cite}],
'reviews':[{'id':'review','range':{'start':'2007-01-02','end':'2007-01-04'},'categories':['review','interim','code_change','merger','spinoff'],'reviewer':'synthetic package acceptance','basis':'explicit complete synthetic delivery reconciled with generated fixture','citations':cite}]}
request={'schema_version':1,'history':facts,'documents':[{'id':'source','path':'original.txt','sha256':'sha256:'+hashlib.sha256(text.encode()).hexdigest(),'source_url':'https://example.com/synthetic-history-fixture','format':'text','coverage_source':True}]}
(r/'history.json').write_text(json.dumps(request))
PY
run_cli indices history preview --index builtin:csi300 --market "$market" --request history.json > "$check_root/submission.json"
wait_task "$check_root/preview.json"
[ "$(plutil -extract results.details.task_ready raw -o - "$check_root/preview.json")" = true ]
run_cli indices history publish --index builtin:csi300 --market "$market" --request history.json --start 2007-01-02 --end 2007-01-04 > "$check_root/submission.json"
wait_task "$check_root/published.json"
version="$(plutil -extract results.details.history_version raw -o - "$check_root/published.json")"
"$python" -I -B - "$check_root/published.json" "$check_root/workspace/pool.json" <<'PY'
import json,sys
json.dump(json.load(open(sys.argv[1]))['results']['details']['pool_definition'],open(sys.argv[2],'w'))
PY
run_cli pools create --definition pool.json > "$check_root/pool.json"
pool="$(plutil -extract id raw -o - "$check_root/pool.json")"
run_cli pools preview "$pool" --market "$market" --date 2007-01-03 > "$check_root/members.json"
"$python" -I -B - "$check_root/members.json" <<'PY'
import json,sys
assert json.load(open(sys.argv[1]))['instruments']==['SH600001']
PY
rm "$check_root/workspace/original.txt"
run_cli indices history status "$version" > "$check_root/submission.json"
wait_task "$check_root/status.json"
run_cli indices history export "$version" exported > "$check_root/submission.json"
wait_task "$check_root/export.json"
run_cli indices history import --index builtin:csi300 --market "$market" exported/history.json > "$check_root/submission.json"
wait_task "$check_root/import.json"
"$python" -I -B - "$check_root/workspace/exported/evidence/0.txt" <<'PY'
import pathlib,sys
pathlib.Path(sys.argv[1]).write_text('tampered')
PY
run_cli indices history import --index builtin:csi300 --market "$market" exported/history.json > "$check_root/submission.json"
history_job="$(plutil -extract task_id raw -o - "$check_root/submission.json")"
run_cli jobs wait "$history_job" --timeout 60 > "$check_root/tampered.json"
[ "$(plutil -extract results.details.ready raw -o - "$check_root/tampered.json")" = false ]
echo "Packaged offline history reconstruction verified: $check_root"
