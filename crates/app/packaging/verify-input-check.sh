#!/bin/bash
# Native-only offline acceptance. Works against an App on a mounted read-only DMG.
set -euo pipefail
app="${1:?absolute App bundle path required}"
check_root="${2:?absolute acceptance output directory required}"
mkdir -p "$check_root/home" "$check_root/working"
workspace="$check_root/workspace-$(uuidgen)"
cli="$app/Contents/MacOS/jfquant"
# Runtime starts outside every source checkout and sees no developer executables.
cd "$check_root/working"
run_cli() { env -i HOME="$check_root/home" PATH=/usr/bin:/bin "$cli" --workspace "$workspace" "$@"; }
trap 'run_cli service stop >/dev/null 2>&1 || true' EXIT
run_cli workspace create "$workspace" > "$check_root/workspace.json"
run_cli query export --sql "SELECT 'SH'||(600000+s)::VARCHAR AS instrument_id, DATE '2030-01-01'+d::INTEGER AS trade_date, (10+s+d*.01)::DOUBLE AS open, (open+1)::DOUBLE AS high, (open-1)::DOUBLE AS low, (open+.1)::DOUBLE AS close, 1000.0::DOUBLE AS volume, volume*close AS amount, true AS is_trading FROM range(2) a(s) CROSS JOIN range(60) b(d)" --output market.parquet > "$check_root/market.json"
run_cli query export --sql "SELECT 'SH600000' AS instrument_id,'test' AS universe_id,DATE '2030-01-05' AS trade_date,true AS is_member UNION ALL SELECT 'SH600001','test',DATE '2030-01-15',true" --output universe.parquet > "$check_root/universe.json"
cat > "$check_root/draft.json" <<'JSON'
{"name":"离线组合检查","description":"offline acceptance","formula":"CLOSE / GTJA_DELAY(CLOSE, 3) - 1"}
JSON
run_cli factors save --definition "$check_root/draft.json" > "$check_root/custom.json"
run_cli factors list --factor-set alpha158 > "$check_root/alpha158.json"
run_cli factors list --factor-set gtja191 > "$check_root/gtja191.json"
definition="$(plutil -extract saved_factor json -o - "$check_root/custom.json")"
custom_id="$(plutil -extract saved_factor.id raw -o - "$check_root/custom.json")"
alpha_version="$(plutil -extract formula_version raw -o - "$check_root/alpha158.json")"
gtja_version="$(plutil -extract formula_version raw -o - "$check_root/gtja191.json")"
cat > "$check_root/input.json" <<JSON
{"factor_set":"gtja191","market_path":"market.parquet","universe_path":"universe.parquet","universe_id":"test","date_range":{"start":"2030-01-10","end":"2030-02-20"},"factor_ids":["$custom_id","alpha158_001_KMID","alpha_002"],"factor_refs":[{"source":"custom","definition":$definition},{"source":"builtin","factor_set":"alpha158","id":"alpha158_001_KMID","formula_version":"$alpha_version"},{"source":"builtin","factor_set":"gtja191","id":"alpha_002","formula_version":"$gtja_version"}],"output_dir":"unused"}
JSON
input="$(cat "$check_root/input.json")"
cat > "$check_root/check.json" <<JSON
{"input":$input,"horizon":5}
JSON
cat > "$check_root/evaluate.json" <<JSON
{"input":$input,"horizon":5,"quantiles":2,"min_samples":3}
JSON
for iteration in 1 2; do
  run_cli factors check --request "$check_root/check.json" > "$check_root/check-$iteration-submit.json"
  task="$(plutil -extract task_id raw -o - "$check_root/check-$iteration-submit.json")"
  run_cli jobs wait "$task" --timeout 60 > "$check_root/check-$iteration.json"
  [ "$(plutil -extract results.ready raw -o - "$check_root/check-$iteration.json")" = true ]
  [ ! -f "$workspace/.jfquant/runs/$task/model.txt" ]
  [ ! -f "$workspace/.jfquant/runs/$task/fold-0000/model.txt" ]
done
[ "$(plutil -extract results.artifact.cache_hit raw -o - "$check_root/check-2.json")" = true ]
run_cli factors evaluate --request "$check_root/evaluate.json" > "$check_root/evaluate-submit.json"
evaluation_task="$(plutil -extract task_id raw -o - "$check_root/evaluate-submit.json")"
run_cli jobs wait "$evaluation_task" --timeout 60 > "$check_root/evaluate.json"
[ "$(plutil -extract results.computation_receipt.cache_hit raw -o - "$check_root/evaluate.json")" = true ]
run_cli jobs results "$task" > "$check_root/online.json"
run_cli service stop > "$check_root/stop.json"
run_cli jobs results "$task" > "$check_root/offline.json"
cmp "$check_root/online.json" "$check_root/offline.json"
echo "Offline native combination check and verified reuse passed: $check_root"
