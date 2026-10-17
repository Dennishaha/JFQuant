"""Native TDX tables are validated before SDK filling; these tests never contact TDX."""

import copy
import unittest
from contextlib import contextmanager
from unittest.mock import patch

from jfquant_collectors import tdx
from jfquant_collectors.tdx import (
    action_references,
    bars,
    merge_window,
    payload,
    symbol,
    verify_events,
)


def table():
    return {
        "Date": [20200102, 20200103],
        "Open": [10, 5],
        "High": [11, 6],
        "Low": [9, 4],
        "Close": [10, 5],
        "Volume": [1000, 2000],
        "Amount": [1, 1],
        "ForwardFactor": [1, 2],
    }


class TdxContracts(unittest.TestCase):
    def test_runtime_index_routes_use_explicit_mapping_and_do_not_guess_members(self):
        config = {"definition": {"id": "custom_index"}, "mappings": {"tdxaidata": "SH000903"}}
        request = {"source_bindings": {"IDX_custom_index": "SH000903"}, "index_inputs": {"IDX_custom_index": config}, "index_member_codes": {"custom_index": "SH000903"}}
        self.assertEqual(tdx.request_symbol(request, "IDX_custom_index"), "000903.SH")
        self.assertEqual(tdx.member_code(request, "custom_index"), "000903.SH")
        with self.assertRaises(ValueError):
            tdx.member_code(request, "unknown_index")
        with self.assertRaises(ValueError):
            symbol("IDX_custom_index")

    def test_empty_actions_still_query_and_capture_independent_gp29(self):
        calls, responses, emitted = [], [], []
        class SDK:
            def get_divid_factors(self, *args):
                calls.append("actions")
                responses.append({"ErrorId":"0","Value":[]})
            def get_trading_dates(self, *args, **kwargs):
                calls.append("calendar")
                responses.append({"Date":["20210104"]})
                return ["20210104"]
            def get_gpjy_value(self, *args):
                calls.append(args[1][0])
                responses.append({"ErrorId":"0","Value":{"GP29":[]}})
            def get_more_info(self, *args, **kwargs):
                calls.append("current_status")
                responses.append({"ErrorId":"0","Value":{"TPFlag":"1","HqDate":"20220104","StopJYDate_Recent":"20211230"}})
        @contextmanager
        def session(*args):
            yield SDK(), [], responses
        request = {"start":"2021-01-01","end":"2021-12-31","operation":"actions","instruments":["SH688981"]}
        with patch.object(tdx, "session", session):
            tdx.collect(request, lambda event, **value: emitted.append((event, value)))
        self.assertEqual(calls, ["actions", "GP41", "GP43", "calendar", "GP29", "current_status"])
        shard = next(value for event, value in emitted if event == "action_shard")
        self.assertEqual(shard["evidence"]["stock_status"]["status"], "captured")
        self.assertEqual(shard["evidence"]["stock_status"]["current_snapshot"], "captured")
        self.assertFalse(shard["history_verified"])
        self.assertEqual(shard["records"], [])

    def test_successful_empty_action_reference_does_not_suppress_auxiliary_status(self):
        self.assertEqual(action_references({"ErrorId":"0","Error":"","Value":[]}, "2021-01-01", "2021-12-31"), [])
        with self.assertRaises(ValueError):
            action_references({"ErrorId":"2","Value":[]}, "2021-01-01", "2021-12-31")

    def test_unfilled_units_and_explicit_factor_direction(self):
        data = table()
        raw, factors = bars("SH600000", data, "2020-01-02", "2020-01-03")
        self.assertEqual(raw[0]["volume"], 1000)
        self.assertEqual(raw[0]["amount"], 10000)
        self.assertIsNone(raw[0]["adj_close"])
        events = action_references(
            {"Date": [20200103], "Type": [1], "Value": [["0", "0", "10", "0"]]},
            "2020-01-02",
            "2020-01-03",
        )
        self.assertEqual(len(verify_events(raw, factors, events)), 1)
        bad = copy.deepcopy(factors)
        bad[1]["factor"] = "0.5"
        with self.assertRaisesRegex(ValueError, "conflict"):
            verify_events(raw, bad, events)
        with self.assertRaisesRegex(ValueError, "event_missing"):
            verify_events(raw, factors, [])
        events[0]["type"] = "15"
        with self.assertRaisesRegex(ValueError, "requires_review"):
            verify_events(raw, factors, events)

    def test_window_anchor_changes_are_rebased_but_shape_and_raw_conflicts_fail(self):
        raw, native = bars("SH600000", table(), "2020-01-02", "2020-01-03")
        output, points = [], []
        merge_window(output, points, raw, native)
        scaled = copy.deepcopy(native)
        for p in scaled:
            p["factor"] = str(float(p["factor"]) / 4)
        proof = merge_window(output, points, raw, scaled)
        self.assertEqual(proof["native_scale"], "4")
        self.assertEqual(len(output), 2)
        self.assertEqual([float(p["factor"]) for p in points], [1, 2])
        wrong = copy.deepcopy(scaled)
        wrong[0]["factor"] = "0.3"
        with self.assertRaisesRegex(ValueError, "shape_conflict"):
            merge_window(output, points, raw, wrong)
        raw = copy.deepcopy(raw)
        raw[0]["volume"] += 1
        with self.assertRaisesRegex(ValueError, "window_conflict"):
            merge_window(output, points, raw, scaled)

    def test_cash_dividend_reconciles_without_requiring_additive_front_price(self):
        raw, points = bars("SH600000", table(), "2020-01-02", "2020-01-03")
        # 1 CNY cash on a 10 CNY previous close: F_new/F_old = 10/9.
        # Vendor additive QFQ prices are not required to equal raw*F.
        points[1]["factor"] = "1.111111"
        events = action_references(
            {"Date": [20200103], "Type": [1], "Value": [["10", "0", "0", "0"]]},
            "2020-01-02",
            "2020-01-03",
        )
        self.assertEqual(verify_events(raw, points, events)[0]["cash_per_share"], "1")
        points[1]["factor"] = "1.12"
        with self.assertRaisesRegex(ValueError, "event_conflict"):
            verify_events(raw, points, events)

    def test_rights_reference_does_not_synthesize_account_subscription_dates(self):
        raw, points = bars("SH600030", table(), "2020-01-02", "2020-01-03")
        raw[0]["close"] = 25.7
        points[1]["factor"] = "1.060668592653735"
        events = action_references(
            {"Date": [20200103], "Type": [1], "Value": [["0", "14.43", "0", "1.5"]]},
            "2020-01-02",
            "2020-01-03",
        )
        proof = verify_events(raw, points, events)
        self.assertEqual(proof[0]["rights_per_share"], "0.15")
        self.assertNotIn("subscription_date", proof[0])

    def test_verified_index_and_independent_quotes_do_not_invent_stock_factors(self):
        data = table()
        del data["ForwardFactor"]
        _, factors = bars("SH000300", data, "2020-01-02", "2020-01-03", price_index=True)
        self.assertEqual([p["factor"] for p in factors], ["1", "1"])
        _, factors = bars("SH600000", data, "2020-01-02", "2020-01-03", require_factor=False)
        self.assertEqual(factors, [])
        with self.assertRaisesRegex(ValueError, "truncated"):
            bars("SH600000", data, "2020-01-02", "2020-01-03")

    def test_missing_stays_null_but_corruption_is_rejected(self):
        data = table()
        data["Open"][0] = None
        raw, _ = bars("SH600000", data, "2020-01-02", "2020-01-03")
        self.assertIsNone(raw[0]["open"])
        for key, value in [("Volume", -1), ("Close", float("nan")), ("ForwardFactor", 0)]:
            with self.subTest(key=key):
                bad = table()
                bad[key][0] = value
                with self.assertRaises(ValueError):
                    bars("SH600000", bad, "2020-01-02", "2020-01-03")

    def test_error_empty_duplicate_and_truncation_are_distinct(self):
        with self.assertRaisesRegex(ValueError, "tdx_request_failed"):
            payload({"ErrorId": 12, "Error": "sensitive vendor details"})
        for change, message in [
            (lambda d: d.update(Date=[]), "empty"),
            (lambda d: d["Date"].__setitem__(1, 20200102), "duplicate"),
            (lambda d: d["Close"].pop(), "truncated"),
        ]:
            data = table()
            change(data)
            with self.assertRaisesRegex(ValueError, message):
                bars("SH600000", data, "2020-01-02", "2020-01-03")
        self.assertEqual(symbol("SZ000001"), "000001.SZ")
        with self.assertRaises(ValueError):
            symbol("SHT00018")

    def test_event_reference_is_not_a_complete_account_event(self):
        data = {"Date": [20200103], "Type": [1], "Value": [["2", "0", "0", "0"]]}
        rows = action_references(data, "2020-01-01", "2020-01-04")
        self.assertEqual(rows[0]["bonus_per_ten_shares"], "2")
        self.assertEqual(rows[0]["account_dates"], "requires_supplement")
        self.assertNotIn("pay_date", rows[0])
        data["Value"][0].pop()
        with self.assertRaisesRegex(ValueError, "fields_missing"):
            action_references(data, "2020-01-01", "2020-01-04")


class TdxProcessBoundary(unittest.TestCase):
    def test_native_reply_authentication_cleanup_and_credential_isolation(self):
        import ctypes
        import hashlib
        import json
        import os
        import sys
        import tempfile
        from pathlib import Path
        from types import SimpleNamespace
        from unittest.mock import patch

        from jfquant_collectors import tdx

        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            package = root / "package"
            package.mkdir()
            (package / "libTdxAiData.dylib").write_bytes(b"fixture library")
            (root / "raw").mkdir()
            freed = []
            body = json.dumps(table()).encode("ascii")
            buffer = ctypes.create_string_buffer(body)

            def GetHISDATsInStr(*args):
                ctypes.cast(args[-1], ctypes.POINTER(ctypes.c_char_p))[0] = ctypes.cast(
                    buffer, ctypes.c_char_p
                )
                return 0

            class Native:
                _is_started = False
                _dll = SimpleNamespace(FreeResult=lambda p: freed.append(True), stop=lambda: None)
                _call_with_free = lambda *args: None

                def _ensure_started(self):
                    self._is_started = True

            native = Native()
            key = "fixture-only-private-key"
            tdx.PROCESS_KEY = None
            with (
                patch.dict(
                    sys.modules,
                    {"tdxaidata": SimpleNamespace(tqs=SimpleNamespace(_tdx=lambda: native))},
                ),
                patch("importlib.metadata.version", return_value="1.2.2"),
                patch(
                    "importlib.metadata.distribution",
                    return_value=SimpleNamespace(locate_file=lambda p: package),
                ),
                patch.dict(os.environ, {"JFQUANT_TDX_KEY": key}),
            ):
                with tdx.session({"cache_dir": str(root / "cache")}, lambda *a, **k: None) as (
                    _,
                    receipts,
                    _,
                ):
                    self.assertNotIn("JFQUANT_TDX_KEY", os.environ)
                    self.assertEqual(
                        (root / ".tdx-runtime/TdxAiData.ini").stat().st_mode & 0o777, 0o600
                    )
                    native._call_with_free(GetHISDATsInStr, b"600000.SH")
                    self.assertEqual(
                        receipts[0]["sha256"], "sha256:" + hashlib.sha256(body).hexdigest()
                    )
                    self.assertEqual(receipts[0]["arguments"], ["600000.SH"])
                    journal = (root / "raw/sdk-requests.jsonl").read_text()
                    self.assertNotIn(key, journal)
                    self.assertEqual(json.loads(journal)["function"], "GetHISDATsInStr")
                    self.assertNotIn(
                        key.encode(), (root / "raw" / receipts[0]["filename"]).read_bytes()
                    )
                self.assertFalse((root / ".tdx-runtime").exists())
                self.assertFalse(native._is_started)
                self.assertEqual(freed, [True])
            tdx.PROCESS_KEY = None

    def test_current_members_never_certify_historical_coverage(self):
        from jfquant_collectors.tdx import constituents

        pool = constituents(["600000.SH", "000001.SZ"], "2026-10-15")
        self.assertEqual(pool["coverage"], {"start": "2026-10-15", "end": "2026-10-15"})
        self.assertEqual(
            pool["intervals"]["SZ000001"], [{"start": "2026-10-15", "end": "2026-10-15"}]
        )
        with self.assertRaisesRegex(ValueError, "duplicate"):
            constituents(["600000.SH", "600000.SH"], "2026-10-15")
