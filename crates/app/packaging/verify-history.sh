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
(r/'history-v1.json').write_text(json.dumps(request))
PY
run_cli indices history migrate --request history-v1.json > "$check_root/submission.json"
wait_task "$check_root/migrated.json"
"$python" -I -B - "$check_root/migrated.json" "$check_root/workspace" <<'PY'
import json,pathlib,sys
result=json.load(open(sys.argv[1]))['results']['details']
r=pathlib.Path(sys.argv[2])
request=result['history_request']
assert request['schema_version']==2 and not request['transcriptions']
assert not request['history']['anchors'][0]['securities']
cite=request['history']['identity']
def proof(instrument):
    return dict(instrument=instrument,name=instrument[2:],membership=cite,identity=cite)
request['history']['anchors'][0]['securities']=[proof('SH600000')]
request['history']['events'][0]['securities']=[proof('SH600000'),proof('SH600001')]
(r/'history.json').write_text(json.dumps(request))
from PIL import Image,ImageDraw
import textwrap
image=Image.new('RGB',(640,100),'white')
ImageDraw.Draw(image).multiline_text((4,4),'\n'.join(textwrap.wrap((r/'original.txt').read_text(),80)),fill='black')
image.save(r/'image.png')
image.save(r/'image.pdf')
(r/'register-action.json').write_text(json.dumps(dict(action='register',id='pdf',path='image.pdf',url='https://example.com/image.pdf',format='pdf')))
(r/'extract-action.json').write_text(json.dumps(dict(action='extract',document='pdf',page=1,image=0,format='jpeg')))

PY
run_cli indices history preview --index builtin:csi300 --market "$market" --request history.json > "$check_root/submission.json"
wait_task "$check_root/preview.json"
[ "$(plutil -extract results.details.task_ready raw -o - "$check_root/preview.json")" = true ]
preview_job="$(plutil -extract record.id raw -o - "$check_root/preview.json")"
run_cli indices history compare --before "$preview_job" --after "$preview_job" > "$check_root/submission.json"
wait_task "$check_root/compare.json"
[ "$(plutil -extract results.details.after_unproven_days raw -o - "$check_root/compare.json")" = 0 ]
run_cli indices history material --request history.json --action register-action.json > "$check_root/submission.json"
wait_task "$check_root/register.json"
"$python" -I -B - "$check_root/register.json" "$check_root/workspace/material.json" <<'PY'
import json,sys
json.dump(json.load(open(sys.argv[1]))['results']['details']['history_request'],open(sys.argv[2],'w'))
PY
run_cli indices history material --request material.json --action extract-action.json > "$check_root/submission.json"
wait_task "$check_root/extract.json"
"$python" -I -B - "$check_root/extract.json" "$check_root/workspace" <<'PY'
import json,pathlib,sys
root=pathlib.Path(sys.argv[2]);r=json.load(open(sys.argv[1]))['results']['details']['history_request']
d=r['documents'][-1]
assert d['derived_from']==dict(document='pdf',page=1,image=0)
r['transcriptions']=[dict(id='reading',document=d['id'],sha256=d['sha256'],region=[0,0,640,100],text=(root/'original.txt').read_text(),reviewer='',reviewed_at=None,confirmed=False,receipt=None)]
(root/'reading.json').write_text(json.dumps(r))
(root/'review-action.json').write_text(json.dumps(dict(action='review',transcription='reading',reviewer='synthetic package reviewer')))
PY
run_cli indices history material --request reading.json --action review-action.json > "$check_root/submission.json"
wait_task "$check_root/review.json"
[ "$(plutil -extract results.details.history_request.transcriptions.0.confirmed raw -o - "$check_root/review.json")" = true ]
"$python" -I -B - "$check_root/review.json" "$check_root/workspace/history.json" <<'PY'
import json,sys
r=json.load(open(sys.argv[1]))['results']['details']['history_request']
t=r['transcriptions'][0]
cite=[dict(document=t['document'],excerpt=t['text'],transcription=t['id'])]
r['history']['anchors'][0]['citations']=cite
r['history']['anchors'][0]['securities'][0]['membership']=cite
json.dump(r,open(sys.argv[2],'w'))
PY
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
embedded_original="$(plutil -extract results.details.history_request.documents.2.path raw -o - "$check_root/review.json")"
rm "$check_root/workspace/original.txt" "$check_root/workspace/image.pdf" "$check_root/workspace/$embedded_original"
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
