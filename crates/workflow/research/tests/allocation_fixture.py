"""Explicit current-protocol setup for the small upstream execution fixtures."""
import json
from pathlib import Path
import pyarrow.parquet as pq
from jfquant_research.backtest import backtest as run


def config(**changes):
    result = dict(prediction_start=None, reference_start=None, execution_start=None, method="equal", risk_window=252, max_stock_weight=1., max_strategy_weight=.6, max_turnover=None, risk_aversion=None, calibration_window=252, calibration_min_days=60, calibration_min_pairs=30)
    result.update(changes)
    return result


def backtest(request, *args):
    request = dict(request)
    request["allocation"] = config()
    bars = pq.read_table(request["bars"]).to_pandas()
    path = Path(request["output"]) / "allocation-input.json"
    path.write_text(json.dumps({"schema_version":1,"calendar":sorted(set(bars.date.astype(str))),"prices":{},"config":request["allocation"]}))
    payload=json.loads(path.read_text());payload["reference_range"]={"start":min(bars.date.astype(str)),"end":max(bars.date.astype(str))};path.write_text(json.dumps(payload))
    request["formal_start"]=min(bars.date.astype(str))
    request["reference_actions"]=request["actions"]
    request["reference_rights"]=request.get("rights")
    request["allocation_input"] = str(path)
    return run(request, *args)
