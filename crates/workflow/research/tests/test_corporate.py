"""Native corporate transitions: real account mutations, never synthetic exchange fills."""
import json
import os
import threading
import unittest
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

import pyarrow as pa
import pyarrow.parquet as pq
import test_runtime
from allocation_fixture import backtest


def fixture(root, kind, final=False):
    request = {}
    original = test_runtime.backtest
    def capture(value, *args):
        request.update(value)
        return original(value, *args)
    with patch.object(test_runtime, "backtest", capture):
        test_runtime.RuntimeTests().run_backtest(root)
    dates = request["signal_dates"]
    effective = dates[-1] if final else dates[4]
    terms = {"kind": kind}
    if kind == "liquidation":
        terms.update(commission_rate=str(request["commission_rate"]), minimum_commission=str(request["minimum_commission"]), other_sell_rate=str(request["stamp_tax"] + request["transfer_fee"]))
    elif kind == "merger":
        terms.update(successor="SZ000001", ratio="0.6647", listing_date=dates[-1])
    else:
        terms.update(successor="SZ000001")
    event = {"id": "fixture:transition", "instrument": "SH600000", "date": effective,
             "last_trade_date": effective if kind == "liquidation" else dates[3], "last_close": "10",
             "announced": effective, "evidence": ["synthetic:fixture"], "terms": terms,
             "successor_closes": {d: "10" for d in dates if d >= effective}}
    request["rights"] = {"policy": {"participation": "abandon", "attempt": "first_day", "insufficient_cash": "partial"}, "events": [], "states": {}, "date": None, "closes": [], "audit": [],
        "corporate": {"policy": "research_transitions", "events": [event], "states": {}, "opens": [], "closes": [], "audit": []}}
    # Keep the converted holding, so the fixture exercises locked asset valuation and delivery.
    request["rebalance_days"] = 100
    request["pool_exit_policy"] = "hold"
    rows = pq.read_table(request["bars"]).to_pylist()
    if kind != "liquidation":
        for r in rows:
            if r["symbol"] == "SH600000" and r["date"] >= effective:
                r["volume"] = 0.
    pq.write_table(pa.Table.from_pylist(rows), request["bars"])
    return request


class CorporateTests(unittest.TestCase):
    def run_case(self, root, kind, final=False):
        request = fixture(root, kind, final)
        (root / "rights_input.json").write_text(json.dumps(request["rights"]))
        (root / "fixture.json").write_text(json.dumps({"calendar": request["signal_dates"], "initial": request["initial_cash"], "closes": {s: [10.] * len(request["signal_dates"]) for s in request["lot_sizes"]}}))
        backtest(request, threading.Event(), lambda *a, **k: None)
        state = json.loads((root / "rights_state.json").read_text())["corporate"]
        terminal = json.loads((root / "native_account_daily.json").read_text())["terminal"]
        fills = [json.loads(r) for r in (root / "order_events.jsonl").read_text().splitlines()]
        self.assertGreater(float(state["states"]["fixture:transition"]["old_quantity"]), 0)
        self.assertEqual(state["closes"][-1], request["signal_dates"][-1])
        self.assertEqual(float(terminal["positions"].get("SH600000", 0)), 0)
        if kind == "liquidation":
            self.assertGreater(terminal["cash"], 99000.)
            self.assertEqual(len([r for r in state["audit"] if r["phase"] == "research_liquidation"]), 1)
            self.assertFalse(any(r["side"] == "sell" and r["filled_quantity"] > 0 for r in fills))
        else:
            self.assertGreater(terminal["positions"]["SZ000001"], 0)
            self.assertTrue(state["states"]["fixture:transition"]["delivered"])
            self.assertEqual(len([r for r in state["audit"] if r["phase"] == "deliver"]), 1)
        return state

    def test_liquidation_midrun_and_final_day(self):
        for final in (False, True):
            with self.subTest(final=final), TemporaryDirectory() as tmp:
                self.run_case(Path(tmp), "liquidation", final)

    def test_migration_and_locked_merger(self):
        for kind in ("migration", "merger"):
            with self.subTest(kind=kind), TemporaryDirectory() as tmp:
                self.run_case(Path(tmp), kind)

    def test_locked_claim_cannot_fund_rebalance_orders(self):
        with TemporaryDirectory() as tmp:
            root = Path(tmp)
            request = fixture(root, "merger")
            request["rebalance_days"] = 1
            backtest(request, threading.Event(), lambda *a, **k: None)
            days = json.loads((root / "native_account_daily.json").read_text())["days"]
            self.assertTrue(all(d["cash"] >= -1e-8 for d in days.values()))
            ledger = json.loads((root / "rights_state.json").read_text())
            state = ledger["corporate"]["states"]["fixture:transition"]
            self.assertGreater(float(state["quantity"]), 0)
            fills = [json.loads(r) for r in (root / "order_events.jsonl").read_text().splitlines()]
            start = request["rights"]["corporate"]["events"][0]["date"]
            listing = request["rights"]["corporate"]["events"][0]["terms"]["listing_date"]
            early = [r for r in fills if start <= r["date"] < listing and r["instrument"] == "SZ000001" and r["side"] == "buy" and r["filled_quantity"] > 0]
            # Only the small remaining cash balance may finance newly bought shares.
            self.assertLess(sum(r["filled_quantity"] * r["price"] for r in early), 3000.)

    def test_export_reproducible_cross_language_fixtures(self):
        if not os.environ.get("JFQUANT_CORPORATE_FIXTURE_DIR"):
            self.skipTest("set JFQUANT_CORPORATE_FIXTURE_DIR to persist synthetic acceptance inputs")
        for kind in ("liquidation", "migration", "merger"):
            root = Path(os.environ["JFQUANT_CORPORATE_FIXTURE_DIR"]) / kind
            root.mkdir(parents=True, exist_ok=True)
            self.run_case(root, kind, final=(kind == "liquidation"))
