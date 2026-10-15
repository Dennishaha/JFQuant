"""Behavioral acceptance of upstream execution and model artifacts."""
import json
from pathlib import Path
import tempfile
import threading
import unittest

import numpy as np
import pandas as pd
import pyarrow as pa
import pyarrow.parquet as pq

from jfquant_research.backtest import backtest
from jfquant_research.training import train, predict


class RuntimeTests(unittest.TestCase):
    def test_validation_early_stop_saved_model_nulls_and_feature_order(self):
        with tempfile.TemporaryDirectory() as tmp:
            p = Path(tmp)
            x = np.linspace(-1, 1, 101)
            for name, sign in (("train", 1), ("validation", -1), ("test", 1)):
                pq.write_table(pa.table({"factor_b": x, "factor_a": [None] * len(x), "__label": sign*x}), p/f"{name}.parquet")
            models = {}
            request = {"request": {"factor_ids": ["factor_b", "factor_a"], "parameters": {"num_iterations": 20, "learning_rate": 0.3, "min_data_in_leaf": 1, "num_leaves": 4, "num_threads": 1}, "objective": "Regression", "early_stopping_rounds": 3}, "train": str(p/"train.parquet"), "validation": str(p/"validation.parquet"), "test": str(p/"test.parquet"), "output": tmp}
            metrics = train(request, models, threading.Event(), lambda *a, **k: None)
            self.assertLess(metrics["best_iteration"], metrics["iterations"])
            predict({"model": str(p/"model.txt"), "matrix": str(p/"test.parquet"), "output": str(p/"prediction.parquet"), "contributions": False}, {})
            predicted=pq.read_table(p/"prediction.parquet").to_pandas().to_numpy()
            self.assertAlmostEqual(metrics["test_rmse"], float(np.sqrt(np.mean((np.array(predicted).ravel()-x)**2))), places=12)
            pq.write_table(pa.table({"factor_a": x, "factor_b": x, "__label": x}), p/"wrong.parquet")
            with self.assertRaisesRegex(ValueError, "order/schema"):
                predict({"model": str(p/"model.txt"), "matrix": str(p/"wrong.parquet"), "output": str(p/"prediction.parquet"), "contributions": False}, {})

    def run_backtest(self, root, actions=(), signals=True, opening_gap=False):
        dates = pd.bdate_range("2020-12-28", periods=8).date
        rows = [{"date": str(d), "symbol": s, "open": 10., "high": 10., "low": 10., "close": 10., "volume": 10000000.} for d in dates for s in ("SH600000", "SZ000001")]
        if opening_gap:
            for row in rows:
                if row["date"] > str(dates[0]):
                    for field in ("open", "high", "low", "close"):
                        row[field] *= 100
        for event in actions:
            if event["kind"] == "Split":
                for row in rows:
                    if row["symbol"] == event["symbol"] and row["date"] >= event["date"]:
                        for field in ("open", "high", "low", "close"):
                            row[field] /= event["value"]
        pq.write_table(pa.Table.from_pylist(rows), root/"bars.parquet")
        scores = [{"date": str(d), "symbol": s, "score": -0.5 if s=="SH600000" else -1.} for d in dates for s in ("SH600000", "SZ000001")] if signals else []
        pq.write_table(pa.Table.from_pylist(scores, schema=pa.schema([("date",pa.string()),("symbol",pa.string()),("score",pa.float64())])), root/"scores.parquet")
        result = backtest({"bars": str(root/"bars.parquet"), "signals": str(root/"scores.parquet"), "signal_dates": [str(d) for d in dates], "output": str(root), "initial_cash": 100000., "top_n": 1, "rebalance_days": 20, "commission_rate": 0.00025, "minimum_commission": 5., "stamp_tax": 0.0005, "transfer_fee": 0.00001, "lot_sizes": {"SH600000":100,"SZ000001":100}, "actions": list(actions), "benchmark_returns": {str(d): 0. for d in dates}}, threading.Event(), lambda *a,**k: None)
        events = [json.loads(line) for line in (root/"order_events.jsonl").read_text().splitlines()]
        return result, events

    def test_next_open_negative_scores_fees_and_cross_year_holdings(self):
        with tempfile.TemporaryDirectory() as tmp:
            result, events = self.run_backtest(Path(tmp))
            fills = [e for e in events if e["status"]=="filled"]
            self.assertTrue(fills)
            self.assertEqual(fills[0]["date"], "2020-12-29")
            self.assertEqual(fills[0]["instrument"], "SH600000")
            self.assertEqual(fills[0]["filled_quantity"]%100, 0)
            self.assertTrue(all(d["holdings"]==1 for d in result["days"][1:]))
            self.assertAlmostEqual(sum(e["fee"] for e in fills), sum(d["execution_fee_cny"] for d in result["days"]))
            self.assertFalse(any(e["side"]=="sell" for e in fills))

    def test_no_signals_does_not_claim_trades(self):
        with tempfile.TemporaryDirectory() as tmp:
            result, events = self.run_backtest(Path(tmp), signals=False)
            self.assertEqual(events, [])
            self.assertTrue(all(d["net_return"]==0 for d in result["days"]))

    def test_ex_date_distribution_is_applied_to_continuous_position(self):
        with tempfile.TemporaryDirectory() as tmp:
            result, _ = self.run_backtest(Path(tmp), actions=[{"symbol":"SH600000","date":"2021-01-04","kind":"Dividend","value":0.5}])
            event_day = next(d for d in result["days"] if d["date"]=="2021-01-04")
            self.assertGreater(event_day["net_return"], 0.04)

    def test_split_doubles_held_shares_without_inventing_cash(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            result, _ = self.run_backtest(root, actions=[{"symbol":"SH600000","date":"2021-01-04","kind":"Split","value":2.0}])
            snapshots = json.loads((root/"portfolio_snapshots.json").read_text())
            before, after = snapshots["2021-01-01"], snapshots["2021-01-04"]
            self.assertEqual(after["positions"]["SH600000"], 2*before["positions"]["SH600000"])
            self.assertEqual(after["cash"], before["cash"])
            self.assertAlmostEqual(after["equity"], before["equity"])
            self.assertEqual(next(d for d in result["days"] if d["date"]=="2021-01-04")["net_return"], 0)

    def test_next_open_cash_shortage_is_rejected_without_fictitious_fill(self):
        with tempfile.TemporaryDirectory() as tmp:
            result, events = self.run_backtest(Path(tmp), opening_gap=True)
            self.assertTrue(any(event["status"]=="rejected" for event in events))
            self.assertFalse(any(event["status"]=="filled" for event in events))
            self.assertTrue(all(day["holdings"]==0 for day in result["days"]))
            self.assertTrue(all(day["net_return"]==0 for day in result["days"]))


if __name__ == "__main__":
    unittest.main()
