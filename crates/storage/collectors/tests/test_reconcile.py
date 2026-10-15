"""Current normalized-event fixtures: units, missing values, conflicts and replay."""

import unittest

from jfquant_collectors.reconcile import reconcile, replay


def event(**changes):
    return {
        "id": "SH600188:2024-06-04",
        "instrument": "SH600188",
        "ex_date": "2024-06-04",
        "cash_per_share": "0.05",
        "shares_per_share": "0",
        "record_date": None,
        "pay_date": None,
        "listing_date": None,
        **changes,
    }


class ReconciliationTests(unittest.TestCase):
    def run_events(self, events, native=()):
        return reconcile("SH600188", events, "2024-01-01", "2024-12-31", native)

    def test_exact_units_nulls_and_duplicate_rows(self):
        e = event()
        ref = {"date": e["ex_date"], "type": "1", "bonus_per_ten": "0.5", "shares_per_ten": "0"}
        result = self.run_events([e, e], [ref, ref])
        self.assertEqual(result["records"], [e])
        self.assertEqual(result["issues"], [])
        self.assertEqual(len(result["decisions"]), 1)
        for field in ("record_date", "pay_date", "listing_date"):
            self.assertIsNone(result["records"][0][field])

    def test_conflict_keeps_original_terms_and_evidence(self):
        ref = {"date": "2024-06-04", "type": "1", "bonus_per_ten": "0.50001", "shares_per_ten": "0"}
        result = self.run_events([event()], [ref])
        self.assertEqual(result["records"][0]["cash_per_share"], "0.05")
        self.assertEqual(result["issues"][0]["code"], "distribution_conflict")
        self.assertTrue(result["issues"][0]["evidence"])

    def test_empty_and_unrelated_references(self):
        self.assertEqual(self.run_events([])["records"], [])
        self.assertEqual(self.run_events([event()], [{"date": "2023-06-04"}])["issues"], [])

    def test_invalid_terms_and_duplicate_identity_fail(self):
        for value in (None, True, "NaN", "Infinity", "-1"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                self.run_events([event(cash_per_share=value)])
        with self.assertRaises(ValueError):
            self.run_events([event(), event(cash_per_share="1")])
        with self.assertRaises(ValueError):
            self.run_events([event(ex_date="2023-06-04")])

    def test_identity_mapping_and_mismatch(self):
        with self.assertRaises(ValueError):
            self.run_events([event(instrument="SH600000")])
        result = reconcile(
            "SH600188",
            [event(instrument="historical_issuer")],
            "2024-01-01",
            "2024-12-31",
            instrument="historical_issuer",
        )
        self.assertEqual(result["records"][0]["instrument"], "historical_issuer")

    def test_fixed_replay_and_unsupported_contract_rejected(self):
        request = {
            "instruments": ["SH600188"],
            "start": "2024-01-01",
            "end": "2024-12-31",
            "reconciliation_inputs": {
                "SH600188": {
                    "symbol": "SH600188",
                    "events": [event()],
                    "start": "2024-01-01",
                    "end": "2024-12-31",
                }
            },
        }
        reports = []
        replay(request, lambda kind, **v: reports.append((kind, v)))
        self.assertEqual([r[0] for r in reports], ["action_shard", "complete"])
        self.assertEqual(reports[0][1]["records"], [event()])
        with self.assertRaises(TypeError):
            reconcile("SH600188", rows=[], start="2024-01-01", end="2024-12-31")
