"""Independent numerical, constraint and time-boundary acceptance."""

import threading
import unittest

import numpy as np
from allocation_fixture import config, solve
from jfquant_research.allocation import StockHistory, independent_checks


class AllocationTests(unittest.TestCase):
    def test_boundary_minimum_variance_is_executable_without_weight_repair(self):
        # Correlated assets with different residual risks put many optimum weights
        # on zero. Unscaled constraints returned about -1e-11 for this fixture,
        # which passed residual checks but was rejected by AKQuant's long-only API.
        rng = np.random.default_rng(0)
        returns = rng.normal(size=(252, 1)) * 0.03 + rng.normal(
            size=(252, 50)
        ) * np.linspace(0.002, 0.08, 50)
        weights, receipt = solve(
            config(method="minimum_variance", max_stock_weight=0.1),
            [f"asset{i}" for i in range(50)],
            returns,
            [0.0] * 50,
        )
        self.assertTrue((weights >= 0).all())
        self.assertLess(weights.min(), 1e-8)
        self.assertLessEqual(weights.max(), 0.1 + 1e-6)
        self.assertAlmostEqual(weights.sum(), 0.98, places=6)
        np.testing.assert_array_equal(weights, receipt["weights"])
        self.assertEqual(receipt["solver"]["constraint_scale"], 100.0)

    def test_equal_and_exiting_holdings_turnover(self):
        w, r = solve(config(), ["a", "b"], [], [0.2, 0.3])
        np.testing.assert_allclose(w, [0.49, 0.49], atol=1e-12)
        self.assertAlmostEqual(r["constraints"]["turnover"], 0.48)
        with self.assertRaisesRegex(ValueError, "turnover"):
            solve(config(max_turnover=0.1), ["a", "b"], [], [0.2, 0.3])
        with self.assertRaisesRegex(ValueError, "candidate count"):
            solve(config(max_stock_weight=0.1), ["a", "b"], [], [0.0, 0.0])

    def test_covariance_and_risk_budgets_against_closed_form(self):
        # Independent covariance oracle; shrinkage is part of the intended contract.
        from sklearn.covariance import LedoitWolf

        x = np.random.default_rng(52).normal(size=(100, 2)) * [0.01, 0.03]
        cov = LedoitWolf().fit(x).covariance_
        expected = (cov[1, 1] - cov[0, 1]) / (cov[0, 0] + cov[1, 1] - 2 * cov[0, 1])
        w, _ = solve(
            config(method="minimum_variance", risk_window=100),
            ["a", "b"],
            x,
            [0.0, 0.0],
        )
        self.assertAlmostEqual(w[0] / 0.98, expected, places=4)
        w, r = solve(
            config(method="risk_budgeting", risk_window=100), ["a", "b"], x, [0.0, 0.0]
        )
        np.testing.assert_allclose(r["risk_contributions"], [0.5, 0.5], atol=1e-4)
        with self.assertRaisesRegex(ValueError, "incomplete"):
            solve(
                config(method="minimum_variance", risk_window=100),
                ["a", "b"],
                x[:-1],
                [0.0, 0.0],
            )

    def test_negative_expected_returns_preserve_exposure_and_cancellation(self):
        x = np.random.default_rng(5).normal(size=(40, 2)) * 0.01
        w, _ = solve(
            config(method="mean_variance", risk_window=40, risk_aversion=10.0),
            ["a", "b"],
            x,
            [0.0, 0.0],
            [-0.01, -0.02],
        )
        self.assertAlmostEqual(w.sum(), 0.98, places=6)
        self.assertGreater(w[0], w[1])
        flag = threading.Event()
        flag.set()
        with self.assertRaises(InterruptedError):
            solve(config(), ["a"], [], [0.0], cancelled=flag)

    def test_mean_variance_matches_constrained_two_asset_oracle(self):
        from sklearn.covariance import LedoitWolf

        x = np.random.default_rng(31).normal(size=(100, 2)) * [0.1, 0.2]
        cov = LedoitWolf().fit(x).covariance_
        mu = np.array([0.001, 0.002])
        lam = 10.0
        # d/dw1 of mu1*w1 + mu2*(.98-w1) - lam*w'Cov*w = 0.
        oracle = (mu[0] - mu[1] + 2 * lam * 0.98 * (cov[1, 1] - cov[0, 1])) / (
            2 * lam * (cov[0, 0] + cov[1, 1] - 2 * cov[0, 1])
        )
        w, _ = solve(
            config(method="mean_variance", risk_window=100, risk_aversion=lam),
            ["a", "b"],
            x,
            [0.0, 0.0],
            mu,
        )
        np.testing.assert_allclose(w, [oracle, 0.98 - oracle], atol=1e-6)
        from skfolio.exceptions import NonPositiveVarianceError

        with self.assertRaises(NonPositiveVarianceError):
            solve(
                config(method="minimum_variance", risk_window=100),
                ["a", "b"],
                np.zeros((100, 2)),
                [0.0, 0.0],
            )

    def test_future_data_cannot_change_risk_or_calibration(self):
        calendar = [f"{i:04}" for i in range(40)]
        prices = {
            s: {d: float(10 + i * k + 0.01 * i * i * k) for i, d in enumerate(calendar)}
            for s, k in [("a", 0.1), ("b", 0.2), ("c", 0.3)]
        }
        signals = {d: {"a": 1.0, "b": 2.0, "c": 3.0} for d in calendar}
        c = config(
            risk_window=5,
            calibration_window=10,
            calibration_min_days=3,
            calibration_min_pairs=3,
        )
        h = StockHistory(calendar, prices, signals, 3, c)
        before = h.risk(calendar[20], ["a", "b"])
        mu, receipt = h.calibrate(calendar[20], ["a", "b"], signals[calendar[20]])
        for price_dict in prices.values():
            for d in calendar[21:]:
                price_dict[d] *= 100
        self.assertEqual(before, h.risk(calendar[20], ["a", "b"]))
        for d in calendar[21:]:
            signals[d]["a"] = 1000.0
        self.assertEqual(
            (mu, receipt), h.calibrate(calendar[20], ["a", "b"], signals[calendar[20]])
        )
        self.assertEqual(receipt["last_return_date"], calendar[20])
        prices["a"].pop(calendar[18])
        with self.assertRaisesRegex(ValueError, "gap"):
            h.risk(calendar[20], ["a", "b"])
        with self.assertRaisesRegex(ValueError, "label_gap"):
            h.calibrate(calendar[20], ["a", "b"], signals[calendar[20]])

    def history_fixture(self, **changes):
        calendar = [f"{i:04}" for i in range(50)]
        prices = {
            s: {
                d: float(10 + j * 0.01 + j * j * 0.0001 * k)
                for j, d in enumerate(calendar)
            }
            for k, s in enumerate(["a", "b", "c", "d"], 1)
        }
        signals = {d: {"a": 10.0, "b": 3.0, "c": 2.0, "d": 1.0} for d in calendar}
        c = config(
            risk_window=5,
            calibration_window=12,
            calibration_min_days=3,
            calibration_min_pairs=3,
            **changes,
        )
        return calendar, prices, signals, c

    def test_eligibility_precedes_top_n_and_uses_only_realized_endpoints(self):
        calendar, prices, signals, c = self.history_fixture(
            candidate_policy="complete_risk_window"
        )
        del prices["a"][calendar[18]]
        h = StockHistory(calendar, prices, signals, 3, c)
        eligible, receipt = h.candidates(calendar[20], signals[calendar[20]])
        self.assertEqual(sorted(eligible), ["b", "c", "d"])
        self.assertEqual(sorted(eligible, key=lambda s: -eligible[s])[:2], ["b", "c"])
        self.assertEqual(receipt["excluded"]["a"]["first_missing_date"], calendar[18])
        for symbol in prices:
            for d in calendar[21:]:
                prices[symbol].pop(d, None)
        later = StockHistory(calendar, prices, signals, 3, c).candidates(
            calendar[20], signals[calendar[20]]
        )
        self.assertEqual((eligible, receipt), later)
        # A common shortened window is never substituted.
        self.assertEqual(len(h.risk(calendar[20], ["b", "c"])[0]), 5)

    def test_known_suspended_labels_are_disclosed_without_reranking(self):
        calendar, prices, signals, c = self.history_fixture()
        del prices["a"][calendar[18]]
        missing = {"a": {calendar[18]: "verified_suspension"}}
        h = StockHistory(calendar, prices, signals, 3, c, missing=missing)
        mu, receipt = h.calibrate(calendar[25], ["b", "c"], signals[calendar[25]])
        self.assertEqual(len(mu), 2)
        skipped = [r for r in receipt["date_records"] if r["excluded"]]
        self.assertTrue(skipped)
        self.assertTrue(all(r["pairs"] == 3 for r in skipped))
        # Percentiles stay those of the original scoring cross section.
        full_rank = 1.5 / 4.0
        self.assertAlmostEqual(
            mu[1], receipt["intercept"] + receipt["slope"] * full_rank
        )
        with self.assertRaisesRegex(ValueError, "label_gap"):
            StockHistory(calendar, prices, signals, 3, c).calibrate(
                calendar[25], ["b", "c"], signals[calendar[25]]
            )

    def test_preflight_reports_all_schedule_failures_and_preserves_empty_signals(self):
        calendar, prices, signals, c = self.history_fixture(
            candidate_policy="complete_risk_window"
        )
        del prices["a"][calendar[18]]
        h = StockHistory(calendar, prices, signals, 3, c)
        issues = h.prepare(calendar, calendar[20], 4, threading.Event())
        self.assertGreaterEqual(len(issues), 2)
        self.assertEqual(issues[0]["date"], calendar[20])
        self.assertEqual(
            issues[0]["error"], "allocation_eligible_candidates_insufficient"
        )
        signals[calendar[20]] = {}
        self.assertEqual(
            StockHistory(calendar, prices, signals, 3, c).prepare(
                [calendar[20]], calendar[20], 4, threading.Event()
            ),
            [],
        )

    def test_preflight_resets_schedule_at_formal_start_after_reference_warmup(self):
        calendar, prices, signals, c = self.history_fixture(
            candidate_policy="complete_risk_window"
        )
        del prices["a"][calendar[20]]
        h = StockHistory(calendar, prices, signals, 3, c)
        issues = h.prepare(
            calendar, calendar[20], 4, threading.Event(), formal_start=calendar[21]
        )
        self.assertEqual(
            [r["date"] for r in issues[:3]], [calendar[20], calendar[21], calendar[24]]
        )

    def test_flat_expected_returns_tolerance_keeps_closed_form_precision(self):
        from sklearn.covariance import LedoitWolf

        x = np.random.default_rng(52).normal(size=(100, 2)) * [0.01, 0.03]
        cov = LedoitWolf().fit(x).covariance_
        expected = (
            0.98 * (cov[1, 1] - cov[0, 1]) / (cov[0, 0] + cov[1, 1] - 2 * cov[0, 1])
        )
        for tolerance in (1e-10, 1e-9):
            weights, receipt = solve(
                config(
                    method="mean_variance",
                    risk_window=100,
                    risk_aversion=10.0,
                    solver_tolerance=tolerance,
                ),
                ["a", "b"],
                x,
                [0.0, 0.0],
                [-0.01, -0.01],
            )
            self.assertAlmostEqual(weights[0], expected, places=6)
            self.assertEqual(receipt["solver"]["tolerance"], tolerance)

    def test_combined_constraints_apply_to_underlying_stocks(self):
        mapping = np.array([[0.98, 0.98], [0.0, 0.0]])
        with self.assertRaisesRegex(ValueError, "underlying_stock_cap"):
            solve(
                config(max_stock_weight=0.1),
                ["one", "two"],
                [],
                [0.0, 0.0],
                mapping=mapping,
            )
        with self.assertRaisesRegex(ValueError, "budget/weights"):
            independent_checks([np.nan], 0.98, 1.0, [0.0], None)


class CombinedTests(unittest.TestCase):
    def fixture(self):
        from jfquant_research.combined import CombinedAllocator

        calendar = [f"{i:04}" for i in range(40)]
        sources = []
        for i, symbol in enumerate(["SH600000", "SZ000001"]):
            rows = [
                {
                    "date": d,
                    "nav": 1.0 + j * (0.01 if i == 0 else 0.002),
                    "open_nav": 1.0 + j * (0.01 if i == 0 else 0.002),
                    "weights": {symbol: 0.98},
                    "changed": False,
                }
                for j, d in enumerate(calendar)
            ]
            targets = [
                {
                    "date": calendar[22],
                    "effective_date": calendar[23],
                    "weights": {symbol: 0.98},
                }
            ]
            sources.append(
                {"id": str(i), "reference": {"rows": rows}, "targets": targets}
            )
        plan = {"execution": {"rebalance_days": 5, "allocation": config()}}
        return CombinedAllocator(
            plan, sources, calendar, threading.Event(), 0.98
        ), calendar

    def test_no_daily_rebalance_and_capital_drift_is_self_financing(self):
        model, calendar = self.fixture()
        weights, r = model.allocate(calendar[20], {})
        np.testing.assert_allclose(r["weights"], [0.5, 0.5], atol=1e-12)
        self.assertAlmostEqual(sum(weights.values()), 0.98)
        self.assertEqual(model.allocate(calendar[21], weights), (None, None))
        changed, r = model.allocate(calendar[22], weights)
        self.assertEqual(r["capital_kind"], "drifted")
        self.assertGreater(r["weights"][0], 0.5)
        self.assertAlmostEqual(sum(r["weights"]), 1.0)
        self.assertAlmostEqual(sum(changed.values()), 0.98)

    def test_risk_inputs_do_not_read_future_reference_nav(self):
        model, calendar = self.fixture()
        model.plan["execution"]["allocation"] = config(
            method="minimum_variance", risk_window=5
        )
        first = model.history(calendar[20])
        for rows in model.rows:
            for d in calendar[21:]:
                rows[d]["open_nav"] *= 1000
        later = model.history(calendar[20])
        np.testing.assert_equal(first[0], later[0])
        self.assertEqual(first[1], later[1])


class NativeDecisionTests(unittest.TestCase):
    def test_cancel_releases_pending_projection_before_replacement(self):
        import inspect

        import akquant as aq

        order = aq.Order(
            "old", "SH600000", aq.OrderSide.Buy, aq.OrderType.Limit, 100.0, price=10.0
        )
        values = {key: None for key in inspect.signature(aq.StrategyContext).parameters}
        values.update(
            cash=1000.0,
            previous_cash=1000.0,
            positions={},
            available_positions={},
            position_entry_prices={},
            current_time=0,
            active_orders=[order],
        )
        context = aq.StrategyContext(**values)
        self.assertEqual(context.get_projected_position("SH600000"), 100.0)
        context.cancel_order("old")
        self.assertEqual(context.get_projected_position("SH600000"), 0.0)
        state = __import__("json").loads(context.jfquant_decision_snapshot())
        self.assertEqual(state["orders"], [])
        self.assertEqual(state["cancel_intents"], ["old"])
        self.assertGreater(state["projected_buying_power"], 999.0)

    def test_native_risk_method_uses_next_qualified_stock_and_linear_journal(self):
        import json
        import tempfile
        from pathlib import Path
        from unittest.mock import patch

        import test_runtime
        from jfquant_research.backtest import backtest as execute

        captured = {}
        original = test_runtime.backtest

        def capture(request, *args):
            captured.update(request)
            return original(request, *args)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with patch.object(test_runtime, "backtest", capture):
                test_runtime.RuntimeTests().run_backtest(root)
            dates = captured["signal_dates"]
            prices = {
                s: {d: 10.0 + i * 0.1 + i * i * 0.002 * k for i, d in enumerate(dates)}
                for k, s in enumerate(["SH600000", "SZ000001"], 1)
            }
            del prices["SH600000"][dates[2]]
            c = config(
                method="minimum_variance",
                candidate_policy="complete_risk_window",
                risk_window=2,
            )
            payload = {
                "schema_version": 2,
                "stock_exposure": 0.98,
                "calendar": dates,
                "prices": prices,
                "config": c,
                "reference_range": {"start": dates[4], "end": dates[-1]},
                "missing_prices": {"SH600000": {dates[2]: "verified_suspension"}},
            }
            history = StockHistory(
                dates, prices, {}, 1, c, missing=payload["missing_prices"]
            )
            payload["admission"] = {
                "eligibility": {
                    d: {
                        "eligible": sorted(
                            history.candidates(d, {"SH600000": 1.0, "SZ000001": 0.0})[0]
                        )
                    }
                    for d in dates
                }
            }
            payload["prediction_provenance"] = None
            path = root / "allocation-input.json"
            path.write_text(json.dumps(payload))
            captured.update(
                allocation=c,
                allocation_input=str(path),
                formal_start=dates[4],
                reference_actions=[],
                reference_rights=None,
                rebalance_days=1,
            )
            execute(captured, threading.Event(), lambda *a, **k: None)
            report = json.loads((root / "allocation_report.json").read_text())
            self.assertEqual(report["status"], "completed")
            self.assertEqual(report["decisions"][0]["names"], ["SZ000001"])
            self.assertEqual(report["decisions"][0]["eligibility"]["excluded_count"], 1)
            journal = [
                json.loads(line)
                for line in (root / "allocation-decisions.jsonl")
                .read_text()
                .splitlines()
            ]
            self.assertEqual([r["decision"] for r in journal], report["decisions"])
            self.assertEqual([r["target"] for r in journal], report["targets"])
            # Reject insufficient qualified counts before starting the native account.
            captured["top_n"] = 2
            with patch("jfquant_research.backtest.aq.run_backtest") as engine:
                with self.assertRaisesRegex(
                    ValueError, "allocation_data_admission_failed"
                ):
                    execute(captured, threading.Event(), lambda *a, **k: None)
                engine.assert_not_called()
            self.assertEqual(
                json.loads((root / "allocation_report.json").read_text())["status"],
                "failed",
            )
            self.assertEqual(
                json.loads((root / "allocation-admission.json").read_text())["issues"][
                    0
                ]["date"],
                dates[4],
            )
            for mutation in ("old_version", "missing_admission"):
                changed = dict(payload)
                if mutation == "old_version":
                    changed["schema_version"] = 1
                else:
                    changed.pop("admission")
                path.write_text(json.dumps(changed))
                with patch("jfquant_research.backtest.aq.run_backtest") as engine:
                    with self.assertRaisesRegex(
                        ValueError, "unsupported_allocation_input_format"
                    ):
                        execute(captured, threading.Event(), lambda *a, **k: None)
                    engine.assert_not_called()
                self.assertEqual(json.loads(path.read_text()), changed)

    def test_pool_exit_is_an_explicit_target_instruction(self):
        import json
        import tempfile
        from pathlib import Path
        from unittest.mock import patch

        import test_runtime

        request = {}
        original = test_runtime.backtest

        def capture(value, *args):
            request.update(value)
            return original(value, *args)

        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            with patch.object(test_runtime, "backtest", capture):
                test_runtime.RuntimeTests().run_backtest(root)
            dates = request["signal_dates"]
            request["pool_exit_policy"] = "next_tradable_day"
            request["pool"] = {
                "intervals": {
                    "SH600000": [{"start": dates[0], "end": dates[2]}],
                    "SZ000001": [{"start": dates[0], "end": dates[-1]}],
                }
            }
            original(request, threading.Event(), lambda *args, **kwargs: None)
            report = json.loads((root / "allocation_report.json").read_text())
            exits = [
                d
                for d in report["decisions"]
                if d.get("instruction_kind") == "pool_exit"
            ]
            self.assertTrue(exits)
            self.assertEqual(exits[0]["date"], dates[3])
            self.assertEqual(exits[0]["names"], [])
            self.assertGreater(exits[0]["constraints"]["turnover"], 0.0)
            self.assertIn("submission_snapshot", exits[0])
