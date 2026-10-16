"""Native patch acceptance; ordinary execution is exercised by test_runtime."""
import json
import tempfile
import threading
import unittest
from pathlib import Path
from unittest.mock import patch

import akquant
import pyarrow as pa
import pyarrow.parquet as pq
import test_runtime


class RightsTests(unittest.TestCase):
    def fixture(self, root):
        request = {}
        original = test_runtime.backtest

        def capture(value, *args):
            request.update(value)
            return original(value, *args)

        with patch.object(test_runtime, "backtest", capture):
            test_runtime.RuntimeTests().run_backtest(root)
        known = lambda value: {"state": "confirmed", "value": value}
        terms = {k: known(v) for k, v in {
            "announced": "2020-12-28", "subscription_start": "2020-12-31",
            "subscription_end": "2021-01-01", "price": "1", "ratio": "0.1", "unit": 1,
            "fee_per_share": "0", "allotment_ratio": "1", "allotment_date": "2021-01-01",
            "listing_date": "2021-01-05",
        }.items()}
        terms["refund_date"] = {"state": "not_applicable", "reason": "fixture full successful allocation"}
        terms["conditional_refund"] = {"state": "unknown"}
        request["rights"] = {"policy": {"participation": "participate", "attempt": "first_day", "insufficient_cash": "partial"},
                             "events": [{"id": "SH600000:2021-01-01", "instrument": "SH600000", "record_date": "2020-12-30", "ex_date": "2021-01-01", "terms": terms, "sessions": request["signal_dates"]}],
                             "states": {}, "date": None, "closes": [], "audit": []}
        return request

    def test_native_registration_subscription_locked_asset_and_listing(self):
        self.assertTrue(hasattr(akquant.Engine(), "configure_jfquant_rights"))
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            request = self.fixture(root)
            test_runtime.backtest(request, threading.Event(), lambda *a, **k: None)
            ledger = json.loads((root / "rights_state.json").read_text())
            state = ledger["states"]["SH600000:2021-01-01"]
            self.assertGreater(float(state["registered"]), 0)
            self.assertEqual(float(state["subscribed"]), float(state["registered"]) * 0.1)
            self.assertTrue(state["listed"])
            audit = ledger["audit"]
            self.assertEqual(len([r for r in audit if r["kind"] == "subscribe"]), 1)
            self.assertEqual(len([r for r in audit if r["kind"] == "list"]), 1)
            self.assertEqual(next(r for r in audit if r["kind"] == "subscribe")["date"], "2020-12-31")
            orders = [json.loads(r) for r in (root / "order_events.jsonl").read_text().splitlines()]
            self.assertFalse(any(r["reason"] == "rights" for r in orders))

    def test_missing_fee_stops_participation_but_abandon_does_not_need_it(self):
        with tempfile.TemporaryDirectory() as tmp:
            request = self.fixture(Path(tmp))
            request["rights"]["events"][0]["terms"]["fee_per_share"] = {"state": "unknown"}
            with self.assertRaisesRegex(Exception, "fee_per_share"):
                test_runtime.backtest(request, threading.Event(), lambda *a, **k: None)
            request["rights"]["policy"]["participation"] = "abandon"
            test_runtime.backtest(request, threading.Event(), lambda *a, **k: None)
            state = json.loads((Path(tmp) / "rights_state.json").read_text())["states"]["SH600000:2021-01-01"]
            self.assertEqual(float(state["subscribed"]), 0)

    def test_payment_advances_on_market_calendar_without_security_bar(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            request = self.fixture(root)
            rows = pq.read_table(root / "bars.parquet").to_pylist()
            rows = [r for r in rows if not (r["symbol"] == "SH600000" and r["date"] == "2020-12-31")]
            pq.write_table(pa.Table.from_pylist(rows), root / "bars.parquet")
            test_runtime.backtest(request, threading.Event(), lambda *a, **k: None)
            ledger = json.loads((root / "rights_state.json").read_text())
            self.assertEqual(next(r for r in ledger["audit"] if r["kind"] == "subscribe")["date"], "2020-12-31")
