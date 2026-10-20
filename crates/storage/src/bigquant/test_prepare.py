"""Small independent accounting/calendar cases; never calls the vendor."""
import unittest
from datetime import date

from audit_annual import latest_version
from prepare import equity_candidate, instrument_id, merged_runs, run_indices


class PrepareTests(unittest.TestCase):
    def test_actual_disclosure_vintage_and_null_revision_are_preserved(self):
        period=date(2023,12,31)
        first={"report_date":"2023-12-31T00:00:00","date":"2024-04-20T00:00:00","equity":100}
        revised={"report_date":"2023-12-31T00:00:00","date":"2024-06-28T00:00:00","equity":None}
        future={"report_date":"2023-12-31T00:00:00","date":"2024-08-20T00:00:00","equity":200}
        self.assertIs(latest_version([future,revised,first],period,date(2024,6,28)),first)
        self.assertIs(latest_version([first,revised,future],period,date(2024,7,1)),revised)
    def test_bank_aggregate_is_deducted_once_and_unknown_is_not_zero(self):
        row = {
            "total_equity_to_parent_shareholders": 736329000000,
            "other_equity_instruments": 82701000000,
            "preference_of_other_equity_instruments": 40000000000,
            "preference_shares": 40000000000,
            "perpetual_bonds": 42701000000,
        }
        # Two preferred-share fields can describe the same component.
        self.assertEqual(equity_candidate(row), (653628000000, None))
        row["other_equity_instruments"] = None
        self.assertEqual(equity_candidate(row), (None, "other_equity_unknown"))
        row["other_equity_instruments"] = 0
        self.assertEqual(equity_candidate(row), (None, "other_equity_components_inconsistent"))
        for c in ("preference_of_other_equity_instruments", "preference_shares", "perpetual_bonds"):
            row[c] = None
        self.assertEqual(equity_candidate(row), (736329000000, None))
        row["total_equity_to_parent_shareholders"] = None
        self.assertEqual(equity_candidate(row), (None, "parent_equity_unknown"))

    def test_calendar_encoding_does_not_cross_missing_source_days(self):
        calendar = [date(2024, 1, 2), date(2024, 1, 3), date(2024, 1, 5)]
        row = {"start_date": "2024-01-02T00:00:00", "end_date": "2024-01-05T00:00:00", "observations": 3, "distinct_dates": 3}
        self.assertEqual(run_indices(calendar, row), (0, 2))
        row["observations"] = row["distinct_dates"] = 2
        with self.assertRaises(ValueError):
            run_indices(calendar, row)
        self.assertEqual(merged_runs([(5, 6), (0, 1), (2, 3)]), [(0, 3), (5, 6)])
        with self.assertRaises(ValueError):
            merged_runs([(0, 2), (2, 4)])

    def test_identifier_conversion_does_not_merge_code_changes(self):
        self.assertEqual(instrument_id("000022.SZ"), "SZ000022")
        self.assertEqual(instrument_id("001872.SZ"), "SZ001872")
        with self.assertRaises(ValueError):
            instrument_id("600000.HK")


if __name__ == "__main__":
    unittest.main()
