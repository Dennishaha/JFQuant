"""Structured fields, HTTP errors and credential safety without a live account."""

import json
import tempfile
import unittest
from types import SimpleNamespace

from jfquant_collectors.evidence import EvidenceCapture
from jfquant_collectors.tushare import FIELDS, Api, SourceError, dividends


class Session:
    def __init__(self, values):
        self.values = iter(values)
        self.calls = []

    def post(self, url, **kw):
        self.calls.append((url, kw))
        code, value = next(self.values)
        return SimpleNamespace(status_code=code, json=lambda: value)


def response(name, rows=()):
    return {"code": 0, "data": {"fields": FIELDS[name].split(","), "items": list(rows)}}


def row(**values):
    return {
        "ts_code": "600018.SH",
        "div_proc": "实施",
        "imp_ann_date": "20240601",
        "ann_date": "20240301",
        "record_date": "20240603",
        "ex_date": "20240604",
        "pay_date": "20240604",
        "div_listdate": "20240605",
        "cash_div_tax": "0.25",
        "stk_bo_rate": "0.1",
        "stk_co_rate": "0.2",
        "stk_div": "0.3",
        **values,
    }


class TushareTests(unittest.TestCase):
    def setUp(self):
        from jfquant_collectors.tushare import DENIED

        DENIED.clear()

    def test_permissions_are_not_empty_success_and_messages_do_not_echo_token(self):
        s = Session([(200, {"code": 40203, "msg": "secret-fixture"})])
        with self.assertRaisesRegex(SourceError, "permission_denied:dividend"):
            Api("secret-fixture", s, lambda _: None).call("dividend", {})
        self.assertTrue(s.calls[0][0].startswith("https://"))
        self.assertFalse(s.calls[0][1]["allow_redirects"])

    def test_empty_history_is_explicit_success_and_unknown_api_is_rejected(self):
        api = Api("secret-fixture", Session([(200, response("dividend"))]), lambda _: None)
        self.assertEqual(api.call("dividend", {}), [])
        with self.assertRaisesRegex(SourceError, "unsupported_tushare_api"):
            api.call("arbitrary", {})

    def test_rate_retry_and_schema_error_are_not_missing_data(self):
        s = Session([(429, {}), (200, response("dividend"))])
        self.assertEqual(Api("secret-fixture", s, lambda _: None).call("dividend", {}), [])
        self.assertEqual(len(s.calls), 2)
        api = Api("secret-fixture", Session([(200, {"code": 0, "data": None})] * 3), lambda _: None)
        with self.assertRaisesRegex(SourceError, "source_parse_error"):
            api.call("dividend", {})

    def test_frequency_error_sharing_permission_code_is_not_permission_denial(self):
        from jfquant_collectors.tushare import DENIED

        api = Api(
            "secret-fixture",
            Session([(200, {"code": 40203, "msg": "访问频率超限(1次/小时)"})] * 3),
            lambda _: None,
        )
        with self.assertRaisesRegex(SourceError, "source_rate_limited:dividend"):
            api.call("dividend", {})
        self.assertNotIn("dividend", DENIED)

    def test_private_credential_survives_securities_but_is_removed_from_environment(self):
        import os
        from unittest.mock import patch

        from jfquant_collectors import tushare

        with (
            patch.dict(os.environ, {"JFQUANT_TUSHARE_TOKEN": "secret-fixture"}),
            patch.object(tushare, "PROCESS_TOKEN", None),
        ):
            first = Api(session=Session([]), sleep=lambda _: None)
            self.assertNotIn("JFQUANT_TUSHARE_TOKEN", os.environ)
            self.assertEqual(first.token, Api(session=Session([]), sleep=lambda _: None).token)

    def test_per_share_units_and_implemented_records_only(self):
        records, issues = dividends(
            "SH600018", [row(), row(div_proc="预案")], "2024-01-01", "2024-12-31"
        )
        self.assertEqual(len(records), 1)
        self.assertEqual(issues, [])
        self.assertEqual(records[0]["cash_per_share"], 0.25)
        self.assertAlmostEqual(records[0]["shares_per_share"], 0.3)
        self.assertEqual(records[0]["announced"], "2024-06-01")

    def test_identity_duplicates_missing_and_conflict_remain_distinct(self):
        with self.assertRaisesRegex(SourceError, "source_identity_conflict"):
            dividends("SH600018", [row(ts_code="600000.SH")], "2024-01-01", "2024-12-31")
        records, issues = dividends("SH600018", [row(), row()], "2024-01-01", "2024-12-31")
        self.assertEqual((len(records), len(issues)), (1, 0))
        _, issues = dividends("SH600018", [row(record_date=None)], "2024-01-01", "2024-12-31")
        self.assertEqual(issues[0]["code"], "event_dates_missing")
        _, issues = dividends(
            "SH600018", [row(), row(cash_div_tax="0.5")], "2024-01-01", "2024-12-31"
        )
        self.assertEqual(issues[0]["code"], "source_event_conflict")

    def test_saturated_single_day_is_rejected(self):
        from unittest.mock import patch

        api = Api(
            "secret-fixture",
            Session([(200, response("adj_factor", [["600018.SH", "20240603", 1]]))]),
            lambda _: None,
        )
        with (
            patch("jfquant_collectors.tushare.LIMITS", {"adj_factor": 1}),
            self.assertRaisesRegex(SourceError, "source_truncated"),
        ):
            api.ranged("adj_factor", "600018.SH", "2024-06-03", "2024-06-03")

    def test_authenticated_echo_never_becomes_evidence(self):
        r = SimpleNamespace(
            url="https://api.tushare.pro",
            content=b'{"msg":"secret-fixture"}',
            request=SimpleNamespace(body=json.dumps({"token": "secret-fixture"})),
            status_code=200,
        )
        with tempfile.TemporaryDirectory() as directory:
            capture = EvidenceCapture(directory)
            with self.assertRaisesRegex(ValueError, "authenticated_response_contains_credential"):
                capture.response(r)
            self.assertEqual(capture.receipts, [])

    def test_daily_conflicts_cannot_silently_replace_earlier_rows(self):
        from jfquant_collectors.tushare import dated

        with self.assertRaisesRegex(SourceError, "source_daily_conflict"):
            dated(
                [{"trade_date": "20240603", "v": 1}, {"trade_date": "20240603", "v": 2}],
                lambda r: r["v"],
            )
