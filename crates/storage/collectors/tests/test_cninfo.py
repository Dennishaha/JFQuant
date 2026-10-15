"""Primary CNINFO amounts/dates; no implicit identity or date substitutions."""

import unittest
from unittest.mock import patch

from jfquant_collectors.cninfo import fetch, parse


def row(**kw):
    return {
        "F006D": "2024-06-01",
        "F018D": "2024-06-03",
        "F020D": "2024-06-04",
        "F023D": "2024-06-05",
        "F025D": None,
        "F012N": 0.5,
        "F010N": None,
        "F011N": None,
        "F007V": "10派0.5元（含税）",
        **kw,
    }


class CninfoTests(unittest.TestCase):
    def test_primary_payment_units_and_non_applicable_shares(self):
        records, issues = parse("SH600000", [row()], "2024-01-01", "2024-12-31", [])
        self.assertEqual(issues, [])
        self.assertEqual(records[0]["cash_per_share"], "0.05")
        self.assertEqual(records[0]["shares_per_share"], "0")
        self.assertEqual(records[0]["pay_date"], "2024-06-05")
        self.assertIsNone(records[0]["listing_date"])

    def test_arrival_never_substitutes_listing_and_null_payment_stays_null(self):
        r = row(F012N=0, F010N=2, F011N=0, F007V="10送2股", F023D=None, F025D="2024-06-05")
        records, issues = parse("SH600000", [r], "2024-01-01", "2024-12-31", [])
        self.assertEqual(issues, [])
        self.assertIsNone(records[0]["listing_date"])
        self.assertIsNone(records[0]["pay_date"])
        sina = {
            "进度": "实施",
            "除权除息日": "2024-06-04",
            "股权登记日": "2024-06-03",
            "派息": 0,
            "送股": 2,
            "转增": 0,
            "红股上市日": "2024-06-06",
        }
        records, issues = parse("SH600000", [r], "2024-01-01", "2024-12-31", [sina])
        self.assertEqual(issues, [])
        self.assertEqual(records[0]["listing_date"], "2024-06-06")

    def test_old_bad_row_does_not_invalidate_needed_period(self):
        old = row(F020D="1992-03-23", F018D=None)
        records, issues = parse("SZ000001", [old, row()], "2016-01-01", "2026-10-12", [])
        self.assertEqual(len(records), 1)
        self.assertEqual(issues, [])

    def test_missing_duplicate_and_cross_source_conflict_remain_issues(self):
        records, issues = parse("SH600000", [row(), row(F012N=1)], "2024-01-01", "2024-12-31", [])
        self.assertEqual(len(records), 0)
        self.assertIn("source_event_conflict", [i["code"] for i in issues])
        _, issues = parse("SH600000", [row(F018D=None)], "2024-01-01", "2024-12-31", [])
        self.assertEqual(issues, [])
        _, issues = parse(
            "SH600000",
            [],
            "2024-01-01",
            "2024-12-31",
            [{"进度": "实施", "除权除息日": "2024-06-04"}],
        )
        self.assertEqual(issues[0]["code"], "cninfo_history_gap")

    def test_numeric_lexemes_preserved_without_rewriting_raw_envelope(self):
        import requests

        response = requests.Response()
        response.status_code = 200
        response.url = "https://webapi.cninfo.com.cn/api/sysapi/p_sysapi1139?scode=600000"
        response._content = b'{"resultcode":200,"count":1,"total":1,"records":[{"F012N":9.7000,"F010N":1,"F011N":null}]}'

        def query(**kw):
            requests.Session().get(response.url)

        with (
            patch("akshare.stock_dividend_cninfo", query),
            patch.object(requests.Session, "send", return_value=response),
        ):
            raw, exact = fetch("SH600000", lambda f, **kw: f(**kw), include_decimal_rows=True)
        self.assertEqual(raw["records"][0]["F012N"], 9.7)
        self.assertEqual(exact[0]["F012N"], "9.7000")
        self.assertEqual(exact[0]["F010N"], 1)

    def test_empty_library_table_is_accepted_only_with_complete_raw_receipt(self):
        from types import SimpleNamespace

        import requests

        def query(**kw):
            requests.Session().get(
                "https://webapi.cninfo.com.cn/api/sysapi/p_sysapi1139?scode=" + kw["symbol"]
            )
            raise KeyError("实施方案公告日期")

        def response(
            total=0, url="https://webapi.cninfo.com.cn/api/sysapi/p_sysapi1139?scode=600000"
        ):
            return SimpleNamespace(
                url=url,
                raise_for_status=lambda: None,
                json=lambda **kw: {"resultcode": 200, "total": total, "count": 0, "records": []},
            )

        with (
            patch("akshare.stock_dividend_cninfo", query),
            patch.object(requests.Session, "send", return_value=response()),
        ):
            self.assertEqual(fetch("SH600000", lambda f, **kw: f(**kw))["records"], [])
        with (
            patch("akshare.stock_dividend_cninfo", query),
            patch.object(requests.Session, "send", return_value=response(2)),
            self.assertRaisesRegex(ValueError, "incomplete"),
        ):
            fetch("SH600000", lambda f, **kw: f(**kw))
        with (
            patch("akshare.stock_dividend_cninfo", query),
            patch.object(
                requests.Session,
                "send",
                return_value=response(
                    url="https://webapi.cninfo.com.cn/api/sysapi/p_sysapi1139?scode=600018"
                ),
            ),
            self.assertRaisesRegex(ValueError, "security_mismatch"),
        ):
            fetch("SH600000", lambda f, **kw: f(**kw))
