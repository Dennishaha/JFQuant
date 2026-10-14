#!/bin/bash
# Exercise the shipped CLI without development tools or environment libraries.
set -euo pipefail
app="${1:?App bundle path required}"
check_root="${2:?Generated acceptance output directory required}"
mkdir -p "$check_root/home"
cli="$app/Contents/MacOS/jfquant"
run_cli() { env -i HOME="$check_root/home" PATH=/usr/bin:/bin "$cli" --workspace "$check_root/workspace" "$@"; }
trap 'run_cli service stop >/dev/null 2>&1 || true' EXIT
run_cli --help > "$check_root/help.txt"
run_cli self-check --output "$check_root/native" > "$check_root/native.json"
run_cli workspace create "$check_root/workspace" > "$check_root/workspace.json"
run_cli research demo-parquet > "$check_root/submission.json"
demo_id="$(plutil -extract task_id raw -o - "$check_root/submission.json")"
run_cli jobs wait "$demo_id" --timeout 30 > "$check_root/demo-wait.json"
run_cli jobs results "$demo_id" > "$check_root/demo.json"
plutil -extract details.demo_template json -o "$check_root/recipe.json" "$check_root/demo.json"
market="$(plutil -extract data.id raw -o - "$check_root/demo.json")"
actions="$(plutil -extract details.demo_actions raw -o - "$check_root/demo.json")"
identity="$(plutil -extract details.demo_template.identity_version raw -o - "$check_root/demo.json")"
run_cli identities show "$identity" > "$check_root/identity-submission.json"
identity_task="$(plutil -extract task_id raw -o - "$check_root/identity-submission.json")"
run_cli jobs wait "$identity_task" --timeout 30 > "$check_root/identity-result.json"
[ "$(plutil -extract results.details.schema_version raw -o - "$check_root/identity-result.json")" = 1 ]
run_cli research run --template "$check_root/recipe.json" --market "$market" --actions "$actions" > "$check_root/research-submission.json"
research_id="$(plutil -extract task_id raw -o - "$check_root/research-submission.json")"
run_cli jobs wait "$research_id" --timeout 60 > "$check_root/research.json"
run_cli jobs results "$research_id" > "$check_root/online-results.json"
[ "$(plutil -extract schema_version raw -o - "$check_root/online-results.json")" = 9 ]
[ "$(plutil -extract execution.mode raw -o - "$check_root/online-results.json")" = raw_integer_shares ]
"$app/Contents/Resources/python/bin/python3" -I -B - "$check_root/online-results.json" <<'PY'
import hashlib
import json
import sys

with open(sys.argv[1], encoding="utf-8") as handle:
    report = json.load(handle)
inputs = report["execution"]["price_inputs"]
assert inputs["normalization"] == "explicit-hfq-first-raw-close-v1;shares;cny;nulls"
assert inputs["securities"]
encoded = json.dumps(inputs, sort_keys=True, ensure_ascii=False, separators=(",", ":")).encode()
assert "sha256:" + hashlib.sha256(encoded).hexdigest() == report["execution"]["price_input_sha256"]
for security in inputs["securities"].values():
    assert security["anchor"]["factor"]
    assert security["sha256"].startswith("sha256:")
PY
# All three user-facing modes run through the same shipped service and produce current results.
for strictness in strict allow_missing qualified_only; do
  run_cli research run --template "$check_root/recipe.json" --market "$market" --actions "$actions" \
    --strictness "$strictness" --min-valid-feature-ratio 0 --min-labeled-rows 1 > "$check_root/quality-$strictness-submission.json"
  quality_id="$(plutil -extract task_id raw -o - "$check_root/quality-$strictness-submission.json")"
  run_cli jobs wait "$quality_id" --timeout 60 > "$check_root/quality-$strictness.json"
  [ "$(plutil -extract results.execution.quality_policy.strictness raw -o - "$check_root/quality-$strictness.json")" = "$strictness" ]
  [ "$(plutil -extract results.admission.schema_version raw -o - "$check_root/quality-$strictness.json")" = 2 ]
done
# Exercise real date controls and absence of a validation set through the same service.
run_cli research preview --template "$check_root/recipe.json" --market "$market" \
  --train-start 2007-01-02 --train-end 2008-01-12 \
  --test-start 2009-01-05 --test-end 2009-01-12 --no-validation > "$check_root/date-preview-submission.json"
preview_id="$(plutil -extract task_id raw -o - "$check_root/date-preview-submission.json")"
run_cli jobs wait "$preview_id" --timeout 30 > "$check_root/date-preview.json"
run_cli research run --template "$check_root/recipe.json" --market "$market" --actions "$actions" \
  --train-start 2007-01-02 --train-end 2008-01-12 \
  --test-start 2009-01-05 --test-end 2009-01-12 --no-validation > "$check_root/date-run-submission.json"
date_id="$(plutil -extract task_id raw -o - "$check_root/date-run-submission.json")"
run_cli jobs wait "$date_id" --timeout 60 > "$check_root/date-run.json"
[ "$(plutil -extract results.fold_reports.0.validation_rows raw -o - "$check_root/date-run.json")" = 0 ]
# Build complete small Parquet inputs through the shared SQL exporter, then
# submit the standard pipeline request using only packaged/native facilities.
pipeline_dir=".jfquant/pipeline-$(uuidgen)"
run_cli query export --sql "SELECT 'SH600000' AS instrument_id, DATE '2020-01-01' + range::INTEGER AS trade_date, 10.0+range*.01 AS open, open+1 AS high, open-1 AS low, open+.2 AS close, 1000.0+range AS volume, volume*close AS amount, open AS adj_open, high AS adj_high, low AS adj_low, close AS adj_close, true AS is_trading, DATE '2000-01-01' AS listing_date FROM range(120)" \
  --output "$pipeline_dir/market.parquet" > "$check_root/market.json"
run_cli query export --sql "SELECT 'SH600000' AS instrument_id, 'csi500' AS universe_id, DATE '2000-01-01' AS trade_date, true AS is_member" \
  --output "$pipeline_dir/universe.parquet" > "$check_root/universe.json"
cat > "$check_root/pipeline-request.json" <<JSON
{"market_path":"$pipeline_dir/market.parquet","universe_path":"$pipeline_dir/universe.parquet","universe_id":"csi500","date_range":{"start":"2020-01-01","end":"2020-04-30"},"factor_ids":["alpha_002","alpha_001"],"output_dir":"$pipeline_dir/results","training":{"parameters":{"num_iterations":"100","seed":"17","min_data_in_leaf":"10"},"early_stopping_rounds":8}}
JSON
run_cli factors run --request "$check_root/pipeline-request.json" > "$check_root/pipeline-submission.json"
pipeline_id="$(plutil -extract task_id raw -o - "$check_root/pipeline-submission.json")"
run_cli jobs wait "$pipeline_id" --timeout 60 > "$check_root/pipeline.json"
[ "$(plutil -extract record.status raw -o - "$check_root/pipeline.json")" = succeeded ]
run_cli service status > "$check_root/status.json"
service_pid="$(plutil -extract pid raw -o - "$check_root/status.json")"
run_cli service stop > "$check_root/stop.json"
# Wait until the old listener is gone, then verify persisted offline results.
for attempt in {1..100}; do
  if ! kill -0 "$service_pid" 2>/dev/null; then break; fi
  sleep 0.02
done
run_cli jobs results "$research_id" > "$check_root/offline-results.json"
cmp "$check_root/online-results.json" "$check_root/offline-results.json"
[ "$(plutil -extract record.status raw -o - "$check_root/research.json")" = succeeded ]
[ "$(plutil -extract ok raw -o - "$check_root/native.json")" = true ]
echo "Packaged CLI research and offline results verified: $check_root"
