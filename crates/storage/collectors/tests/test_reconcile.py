"""Deterministic financial action rules, with exact units and source evidence."""

import unittest
from decimal import ROUND_HALF_UP, Decimal
from unittest.mock import patch

from test_cninfo import row

from jfquant_collectors.reconcile import compatible, reconcile


class ReconciliationTests(unittest.TestCase):
    def run_rows(self, rows, **kw):
        return reconcile("SH600188", rows, "2012-01-01", "2026-10-12", **kw)

    def test_components_sum_and_partial_source_is_not_conflict(self):
        annual = row(F044V="年度分红", F012N="30.7", F010N="5", F011N="0")
        special = row(F044V="特别分红", F012N="12.3", F010N="0", F011N="0")
        sina = {
            "进度": "实施",
            "除权除息日": "2024-06-04",
            "股权登记日": "2024-06-03",
            "派息": "30.7",
            "送股": "5",
            "转增": "0",
        }
        result = self.run_rows([annual, special, annual], sina_rows=[sina])
        self.assertFalse(result["issues"])
        self.assertEqual(len(result["records"]), 1)
        self.assertEqual(result["records"][0]["cash_per_share"], "4.30")
        self.assertEqual(result["records"][0]["shares_per_share"], "0.5")
        self.assertIn("supplement_partial_component", [d["rule"] for d in result["decisions"]])
        self.assertEqual(result, self.run_rows([annual, special, annual], sina_rows=[sina]))

    def test_corrections_and_duplicate_components_not_added(self):
        r = row(F044V="年度分红")
        self.assertEqual(len(self.run_rows([r, r])["records"]), 1)
        conflict = self.run_rows([r, row(F044V="年度分红", F012N="0.6")])
        self.assertEqual(conflict["records"], [])
        self.assertEqual(conflict["issues"][0]["code"], "source_event_conflict")

    def test_missing_optional_dates_and_known_specials(self):
        result = self.run_rows([row(F006D=None, F018D=None, F023D=None)])
        self.assertFalse(result["issues"])
        self.assertIsNone(result["records"][0]["record_date"])
        for category in ("股改", "重整", "补偿", "配股", "换股", "缩股"):
            result = self.run_rows([row(F044V=category, F020D=None, F018D=None, F012N=None)])
            self.assertEqual(result["issues"][0]["code"], "execution_event_unsupported_skipped")
            self.assertTrue(result["issues"][0]["id"])
            self.assertTrue(result["issues"][0]["evidence"])

    def test_ambiguous_mixed_and_tax_basis_block(self):
        for r in [
            row(F044V="年度分红", F007V="重整并分红"),
            row(F007V="10派0.5元税后"),
            row(F007V="10派0.5港元"),
        ]:
            result = self.run_rows([r])
            self.assertFalse(result["records"])
            self.assertNotEqual(result["issues"][0]["code"], "execution_event_unsupported_skipped")

    def test_unique_date_requires_full_terms_and_calendar(self):
        n = {
            "type": "1",
            "date": "2024-06-04",
            "bonus_per_ten": "0.5",
            "shares_per_ten": "0",
            "rights_per_ten": "0",
        }
        r = row(F020D=None)
        result = self.run_rows([r], native=[n], calendar=["2024-06-03", "2024-06-04"])
        self.assertEqual(result["records"][0]["ex_date"], "2024-06-04")
        for native, calendar in [
            ([n, n], ["2024-06-03", "2024-06-04"]),
            ([{**n, "bonus_per_ten": "0.6"}], ["2024-06-03", "2024-06-04"]),
            ([n], []),
        ]:
            result = self.run_rows([r], native=native, calendar=calendar)
            self.assertFalse(result["records"])
            self.assertEqual(result["issues"][0]["code"], "cninfo_event_date_missing")

    def test_announcement_year_is_not_effective_date(self):
        result = self.run_rows([row(F006D="2007-01-01", F018D=None, F020D=None)])
        self.assertEqual(result["issues"][0]["code"], "cninfo_event_date_missing")

    def test_conflict_presence_not_reported_as_history_gap(self):
        s = {"进度": "实施", "除权除息日": "2024-06-04", "派息": "0.6", "送股": "0", "转增": "0"}
        result = self.run_rows([row()], sina_rows=[s])
        self.assertEqual([i["code"] for i in result["issues"]], ["source_event_conflict"])
        result = self.run_rows([], sina_rows=[s])
        self.assertEqual(result["issues"][0]["code"], "cninfo_history_gap")

    def test_unknown_precision_never_rounded_implicitly(self):
        self.assertFalse(compatible(Decimal("28.02423"), Decimal("28.0242"), "sina.cash.per_share"))
        with patch(
            "jfquant_collectors.reconcile.PRECISION_RULES", {"fixture": ("0.01", ROUND_HALF_UP)}
        ):
            self.assertTrue(compatible(Decimal("1.235"), Decimal("1.24"), "fixture"))
            self.assertFalse(compatible(Decimal("1.235"), Decimal("1.25"), "fixture"))
        s = {
            "进度": "实施",
            "除权除息日": "2024-06-04",
            "派息": "280.242",
            "送股": "0",
            "转增": "0",
        }
        result = self.run_rows([row(F012N="280.2423")], sina_rows=[s])
        self.assertFalse(result["records"])
        self.assertEqual(result["issues"][0]["code"], "source_event_conflict")

    def test_optional_but_malformed_date_is_a_localized_issue(self):
        result = self.run_rows([row(F023D="not-a-date")])
        self.assertEqual(result["issues"][0]["code"], "cninfo_event_parse_error")
        self.assertEqual(result["records"], [])

    def test_provided_invalid_chronology_still_blocks(self):
        result = self.run_rows([row(F023D="2024-06-02")])
        self.assertEqual(result["issues"][0]["code"], "cninfo_event_chronology_conflict")
