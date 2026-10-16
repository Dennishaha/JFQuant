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
# Use only a fixture Key in the generated HOME. A signed bundle must share the
# same private-file interface without OS prompts or a development environment.
printf '%s' 'fixture-packaged-source-key' | run_cli data sources configure --provider tdxaidata --key-stdin > "$check_root/credential.json"
[ "$(plutil -extract storage raw -o - "$check_root/credential.json")" = private_local_file ]
[ "$(stat -f '%Lp' "$check_root/home/Library/Application Support/JFQuant/credentials")" = 700 ]
[ "$(stat -f '%Lp' "$check_root/home/Library/Application Support/JFQuant/credentials/tdxaidata.json")" = 600 ]
run_cli self-check --output "$check_root/native" > "$check_root/native.json"
run_cli workspace create "$check_root/workspace" > "$check_root/workspace.json"
run_cli research demo-parquet > "$check_root/submission.json"
demo_id="$(plutil -extract task_id raw -o - "$check_root/submission.json")"
run_cli jobs wait "$demo_id" --timeout 30 > "$check_root/demo-wait.json"
run_cli jobs results "$demo_id" > "$check_root/demo.json"
recipe="$(plutil -extract details.demo_template_path raw -o - "$check_root/demo.json")"
market="$(plutil -extract data.id raw -o - "$check_root/demo.json")"
actions="$(plutil -extract details.demo_actions raw -o - "$check_root/demo.json")"
run_cli data inspect-actions --actions "$actions" --instrument SH600000 > "$check_root/field-inspection-submit.json"
inspection_id="$(plutil -extract task_id raw -o - "$check_root/field-inspection-submit.json")"
run_cli jobs wait "$inspection_id" --timeout 30 > "$check_root/field-inspection.json"
"$app/Contents/Resources/python/bin/python3" -I -B - "$check_root/field-inspection.json" <<'PY_FIELDS'
import json, sys
with open(sys.argv[1]) as handle:
    details = json.load(handle)["results"]["details"]
assert details["instrument"] == "SH600000"
assert "structured_fields" in details and details["structured_fields"] is None
PY_FIELDS
run_cli data sources list > "$check_root/sources.json"
for role in quotes actions members calendar; do
  [ "$(plutil -extract "result.results.details.policy.priorities.$role.0" raw -o - "$check_root/sources.json")" = hithink ]
done
"$app/Contents/Resources/python/bin/python3" -I -B - "$check_root/sources.json" <<'PY_CHECK'
import json, sys
with open(sys.argv[1]) as handle:
    policy = json.load(handle)["result"]["results"]["details"]["policy"]
assert policy["schema_version"] == 2
assert policy["priorities"]["execution"] == []
PY_CHECK
# The same private stdin interface configures the Rust REST provider.
printf '%s' 'fixture-packaged-hithink-key' | run_cli data sources configure --provider hithink --key-stdin > "$check_root/hithink-credential.json"
[ "$(stat -f '%Lp' "$check_root/home/Library/Application Support/JFQuant/credentials/hithink.json")" = 600 ]
if run_cli data supplement > "$check_root/retired-command.json" 2> "$check_root/retired-command.stderr"; then
  echo "Retired action command unexpectedly accepted" >&2; exit 1
fi
run_cli data refresh --template "$recipe" --market "$market" --actions "$actions" --preview > "$check_root/refresh-preview.json"
[ "$(plutil -extract result.results.details.plan.schema_version raw -o - "$check_root/refresh-preview.json")" = 1 ]
[ "$(plutil -extract result.results.details.account.status raw -o - "$check_root/refresh-preview.json")" = not_run ]
"$app/Contents/Resources/python/bin/python3" -I -B -c 'import importlib.metadata; assert importlib.metadata.version("tdxaidata") == "1.2.2"'
run_cli research run --template "$recipe" --market "$market" --actions "$actions" > "$check_root/research-submission.json"
research_id="$(plutil -extract task_id raw -o - "$check_root/research-submission.json")"
run_cli jobs wait "$research_id" --timeout 60 > "$check_root/research.json"
run_cli jobs results "$research_id" > "$check_root/online-results.json"
[ "$(plutil -extract schema_version raw -o - "$check_root/online-results.json")" = 17 ]
[ "$(plutil -extract execution.mode raw -o - "$check_root/online-results.json")" = akquant_ex_date ]
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
  run_cli research run --template "$recipe" --market "$market" --actions "$actions" \
    --strictness "$strictness" --min-valid-feature-ratio 0 --min-labeled-rows 1 > "$check_root/quality-$strictness-submission.json"
  quality_id="$(plutil -extract task_id raw -o - "$check_root/quality-$strictness-submission.json")"
  run_cli jobs wait "$quality_id" --timeout 60 > "$check_root/quality-$strictness.json"
  [ "$(plutil -extract results.execution.quality_policy.strictness raw -o - "$check_root/quality-$strictness.json")" = "$strictness" ]
  [ "$(plutil -extract results.admission.schema_version raw -o - "$check_root/quality-$strictness.json")" = 5 ]
done
# Both account policies run through the packaged common backend; the complete demo
# provides audited coverage, so the stricter policy can execute the same account.
for event_check in provider audited; do
  run_cli research run --template "$recipe" --market "$market" --actions "$actions" \
    --event-check "$event_check" > "$check_root/event-$event_check-submission.json"
  event_id="$(plutil -extract task_id raw -o - "$check_root/event-$event_check-submission.json")"
  run_cli jobs wait "$event_id" --timeout 60 > "$check_root/event-$event_check.json"
  [ "$(plutil -extract results.execution.event_check_policy raw -o - "$check_root/event-$event_check.json")" = "$event_check" ]
done
# Exercise real date controls and absence of a validation set through the same service.
run_cli research preview --template "$recipe" --market "$market" \
  --train-start 2007-01-02 --train-end 2008-01-12 \
  --test-start 2009-01-05 --test-end 2009-01-12 --no-validation > "$check_root/date-preview-submission.json"
preview_id="$(plutil -extract task_id raw -o - "$check_root/date-preview-submission.json")"
run_cli jobs wait "$preview_id" --timeout 30 > "$check_root/date-preview.json"
run_cli research run --template "$recipe" --market "$market" --actions "$actions" \
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
{"factor_set":"gtja191","market_path":"$pipeline_dir/market.parquet","universe_path":"$pipeline_dir/universe.parquet","universe_id":"csi500","date_range":{"start":"2020-01-01","end":"2020-04-30"},"factor_ids":["alpha_002","alpha_001"],"output_dir":"$pipeline_dir/results","training":{"parameters":{"num_iterations":"100","seed":"17","min_data_in_leaf":"10"},"early_stopping_rounds":8}}
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

run_cli research train --template "$recipe" --market "$market" --actions "$actions" > "$check_root/train-submit.json"
training_id="$(plutil -extract task_id raw -o - "$check_root/train-submit.json")"
run_cli jobs wait "$training_id" --timeout 60 > "$check_root/train-wait.json"
run_cli jobs results "$training_id" > "$check_root/train-results.json"
[ "$(plutil -extract status raw -o - "$check_root/train-results.json")" = trained ]
run_cli research backtest --training "$training_id" --actions "$actions" --template "$recipe" > "$check_root/backtest-submit.json"
backtest_id="$(plutil -extract task_id raw -o - "$check_root/backtest-submit.json")"
run_cli jobs wait "$backtest_id" --timeout 60 > "$check_root/backtest-wait.json"
[ "$(plutil -extract results.schema_version raw -o - "$check_root/backtest-wait.json")" = 17 ]
run_cli research verify --task "$backtest_id" > "$check_root/backtest-verification.json"
[ "$(plutil -extract status raw -o - "$check_root/backtest-verification.json")" = consistent ]
[ "$(plutil -extract fills_on_valuation_only_rows raw -o - "$check_root/backtest-verification.json")" = 0 ]

# Published-score replay is separate from model loading and freezes explicit rights choices.
run_cli research replay-predictions --source "$backtest_id" --actions "$actions" --participation abandon > "$check_root/prediction-replay-submit.json"
replay_id="$(plutil -extract task_id raw -o - "$check_root/prediction-replay-submit.json")"
run_cli jobs wait "$replay_id" --timeout 60 > "$check_root/prediction-replay-wait.json"
run_cli research verify --task "$replay_id" > "$check_root/prediction-replay-verification.json"
[ "$(plutil -extract status raw -o - "$check_root/prediction-replay-verification.json")" = consistent ]

# Exercise provider training on the retained Qlib reader; HiThink has separate REST fixtures.
run_cli research demo > "$check_root/qlib-demo-submit.json"
qlib_demo_id="$(plutil -extract task_id raw -o - "$check_root/qlib-demo-submit.json")"
run_cli jobs wait "$qlib_demo_id" --timeout 30 > "$check_root/qlib-demo-wait.json"
run_cli jobs results "$qlib_demo_id" > "$check_root/qlib-demo.json"
qlib_market="$(plutil -extract data.id raw -o - "$check_root/qlib-demo.json")"
qlib_recipe="$(plutil -extract details.demo_template_path raw -o - "$check_root/qlib-demo.json")"
for family in gtja191 alpha158; do
  # Keep the synthetic acceptance feature subset meaningful; full catalogues have separate golden tests.
  "$app/Contents/Resources/python/bin/python3" -I -B - "$check_root/qlib-demo.json" "$check_root/$family-profile.json" "$family" <<'PYPROFILE'
import json,sys
r=json.load(open(sys.argv[1]))["details"]["demo_template"]
family=sys.argv[3]
profile={"schema_version":1,"factor_set":family,"price_basis":"provider","formula_version":r["formula_version"] if family=="gtja191" else "qlib-alpha158-b87a2c2+jfquant-units-v1","factor_ids":["alpha_002"] if family=="gtja191" else ["alpha158_001_KMID","alpha158_014_ROC5"]}
json.dump(profile,open(sys.argv[2],"w"))
PYPROFILE
  run_cli research train --template "$qlib_recipe" --market "$qlib_market" --factor-profile "$check_root/$family-profile.json" > "$check_root/$family-provider-submit.json"
  provider_id="$(plutil -extract task_id raw -o - "$check_root/$family-provider-submit.json")"
  run_cli jobs wait "$provider_id" --timeout 60 > "$check_root/$family-provider.json"
  run_cli jobs results "$provider_id" > "$check_root/$family-model.json"
  [ "$(plutil -extract price_basis raw -o - "$check_root/$family-model.json")" = provider ]
  [ "$(plutil -extract factor_set raw -o - "$check_root/$family-model.json")" = "$family" ]
  [ "$(plutil -extract schema_version raw -o - "$check_root/$family-model.json")" = 3 ]
done

# Cancel a running bundled research process, then verify partial work is not published.
"$app/Contents/Resources/python/bin/python3" -I -B - "$check_root/workspace/$recipe" "$check_root/cancel-case" <<'PYCANCEL'
import json, pathlib, shutil, sys
source=pathlib.Path(sys.argv[1]); root=pathlib.Path(sys.argv[2]); root.mkdir(exist_ok=True)
template=json.loads(source.read_text())
for key in ("factors", "target"):
    original=source.parent/template[key]
    shutil.copyfile(original, root/original.name)
    template[key]=original.name
template["training"]["parameters"]["num_iterations"]="1000000"
template["training"]["early_stopping_rounds"]=None
(root/"workflow.json").write_text(json.dumps(template))
PYCANCEL
run_cli research train --template "$check_root/cancel-case/workflow.json" --market "$market" --actions "$actions" > "$check_root/cancel-submit.json"
cancel_id="$(plutil -extract task_id raw -o - "$check_root/cancel-submit.json")"
started=false
for attempt in {1..150}; do
  run_cli jobs logs "$cancel_id" > "$check_root/cancel-progress.json"
  if /usr/bin/grep -q 'LightGBM iteration' "$check_root/cancel-progress.json"; then started=true; break; fi
  sleep 0.1
done
[ "$started" = true ]
run_cli jobs cancel "$cancel_id" > "$check_root/cancel.json"
set +e
run_cli jobs wait "$cancel_id" --timeout 10 > "$check_root/cancel-wait.json" 2> "$check_root/cancel-wait.stderr"
cancel_status=$?
set -e
[ "$cancel_status" = 130 ]
[ ! -f "$check_root/workspace/.jfquant/runs/$cancel_id/summary.json" ]

# Deterministic HTTP/response fixtures execute against the installed Python modules, not repo imports.
repo_root="$(cd "$(dirname "$0")/../../.." && pwd)"
"$app/Contents/Resources/python/bin/python3" -I -B -m unittest discover -s "$repo_root/crates/storage/collectors/tests" > "$check_root/collector-fixtures.log" 2>&1
[ -s "$app/Contents/Resources/Licenses/Qlib-Alpha158.txt" ]
echo "Packaged CLI research, two factor families, collector fixtures and offline results verified: $check_root"
