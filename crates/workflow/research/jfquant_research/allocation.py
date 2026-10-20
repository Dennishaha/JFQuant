"""Holding-period allocation. No fallback, imputation, future labels or weight repair."""

import hashlib
import json
from bisect import bisect_left, bisect_right

import numpy as np
import pandas as pd

TOL = 1e-6
METHODS = {"equal", "minimum_variance", "risk_budgeting", "mean_variance"}


def check_cancel(cancelled):
    if cancelled.is_set():
        raise InterruptedError("allocation cancelled")


def digest(value):
    return (
        "sha256:"
        + hashlib.sha256(
            json.dumps(
                value, sort_keys=True, allow_nan=False, separators=(",", ":")
            ).encode()
        ).hexdigest()
    )


def validate_config(config):
    fields = {
        "candidate_policy",
        "solver_tolerance",
        "prediction_start",
        "reference_start",
        "execution_start",
        "method",
        "risk_window",
        "max_stock_weight",
        "max_strategy_weight",
        "max_turnover",
        "risk_aversion",
        "calibration_window",
        "calibration_min_days",
        "calibration_min_pairs",
    }
    if set(config) != fields or config["method"] not in METHODS:
        raise ValueError("invalid_allocation_config: fields/method")
    if (
        config["candidate_policy"] not in ("unfiltered", "complete_risk_window")
        or not np.isfinite(config["solver_tolerance"])
        or not 1e-12 <= config["solver_tolerance"] <= 1e-6
    ):
        raise ValueError("invalid_allocation_config: candidate policy/solver tolerance")
    for name in ("risk_window", "calibration_window"):
        if type(config[name]) is not int or not 2 <= config[name] <= 10000:
            raise ValueError(f"invalid_allocation_config: {name}")
    for name in ("max_stock_weight", "max_strategy_weight"):
        if not np.isfinite(config[name]) or not 0 < config[name] <= 1:
            raise ValueError(f"invalid_allocation_config: {name}")
    if (
        type(config["calibration_min_days"]) is not int
        or not 2 <= config["calibration_min_days"] <= config["calibration_window"]
        or type(config["calibration_min_pairs"]) is not int
        or config["calibration_min_pairs"] < 3
    ):
        raise ValueError("invalid_allocation_config: calibration")
    turnover, risk = config["max_turnover"], config["risk_aversion"]
    if turnover is not None and (not np.isfinite(turnover) or not 0 <= turnover <= 2):
        raise ValueError("invalid_allocation_config: turnover")
    if (risk is not None and (not np.isfinite(risk) or risk <= 0)) or (
        config["method"] == "mean_variance" and risk is None
    ):
        raise ValueError("invalid_allocation_config: risk_aversion required")


def independent_checks(
    weights,
    exposure,
    cap,
    previous,
    max_turnover,
    mapping=None,
    stock_cap=None,
    stock_exposure=None,
):
    """Validate original solver output, including unselected and exiting positions."""
    w = np.asarray(weights, dtype=float)
    if (
        w.ndim != 1
        or not np.isfinite(w).all()
        or (w < -TOL).any()
        or (w > cap + TOL).any()
        or abs(w.sum() - exposure) > TOL
    ):
        raise ValueError("allocation_constraint_failed: budget/weights")
    stock = w if mapping is None else np.asarray(mapping) @ w
    if mapping is not None and (
        (stock < -TOL).any()
        or (stock > stock_cap + TOL).any()
        or stock.sum() > stock_exposure + TOL
    ):
        raise ValueError("allocation_constraint_failed: underlying_stock_cap")
    previous = np.asarray(previous, dtype=float)
    if previous.shape != stock.shape or not np.isfinite(previous).all():
        raise ValueError("allocation_previous_weights_invalid")
    turnover = float(np.abs(stock - previous).sum())
    if max_turnover is not None and turnover > max_turnover + TOL:
        raise ValueError("allocation_constraint_failed: turnover")
    return {
        "budget_residual": float(abs(w.sum() - exposure)),
        "turnover": turnover,
        "stock_exposure": float(stock.sum()),
    }


def solve(
    config,
    names,
    returns,
    previous,
    expected=None,
    mapping=None,
    cancelled=None,
    stock_exposure=None,
):
    """Use skfolio estimators; verify every output independently without clipping."""
    validate_config(config)
    if cancelled is not None:
        check_cancel(cancelled)
    if (
        stock_exposure is None
        or not np.isfinite(stock_exposure)
        or not 0 < stock_exposure <= 1
    ):
        raise ValueError("allocation_stock_exposure_missing")
    strategy = mapping is not None
    exposure = 1.0 if strategy else stock_exposure
    cap = config["max_strategy_weight" if strategy else "max_stock_weight"]
    n = len(names)
    if len(set(names)) != n or not n or n * cap < exposure - TOL:
        raise ValueError("allocation_infeasible: candidate count * cap < budget")
    history = np.asarray(returns, dtype=float)
    covariance = None
    status = "analytic"
    solver_stats = None
    if config["method"] == "equal":
        w = np.full(n, exposure / n)
    else:
        from skfolio.moments import EmpiricalMu, LedoitWolf
        from skfolio.optimization import MeanRisk, ObjectiveFunction, RiskBudgeting
        from skfolio.prior import EmpiricalPrior

        if (
            history.ndim != 2
            or history.shape != (config["risk_window"], n)
            or not np.isfinite(history).all()
        ):
            raise ValueError("allocation_risk_history_incomplete")

        class FrozenMu(EmpiricalMu):
            def fit(self, X, y=None):
                self.mu_ = np.asarray(expected, dtype=float)
                if self.mu_.shape != (n,) or not np.isfinite(self.mu_).all():
                    raise ValueError("allocation_expected_returns_invalid")
                return self

        prior = EmpiricalPrior(
            covariance_estimator=LedoitWolf(),
            mu_estimator=FrozenMu()
            if config["method"] == "mean_variance"
            else EmpiricalMu(),
        )

        def constraints(w):
            import cvxpy as cp

            stock = w if mapping is None else mapping @ w
            result = []
            if strategy:
                result.extend(
                    [
                        stock >= 0,
                        stock <= config["max_stock_weight"],
                        cp.sum(stock) <= stock_exposure,
                    ]
                )
            if config["max_turnover"] is not None:
                result.append(cp.norm1(stock - previous) <= config["max_turnover"])
            return result

        kwargs = {
            "prior_estimator": prior,
            "min_weights": 0.0,
            "max_weights": cap,
            "budget": exposure,
            "solver": "CLARABEL",
            "solver_params": {
                "tol_gap_abs": config["solver_tolerance"],
                "tol_feas": config["solver_tolerance"],
                "tol_gap_rel": config["solver_tolerance"],
                "max_iter": 200,
            },
            "scale_objective": 1000.0,
            "scale_constraints": 100.0,
            "add_constraints": constraints,
            "save_problem": True,
            "raise_on_failure": True,
        }
        if config["method"] == "risk_budgeting":
            kwargs.pop("budget")
            kwargs["max_weights"] = cap / exposure
            kwargs["add_constraints"] = lambda w: constraints(w * exposure)
            model = RiskBudgeting(**kwargs)
        else:
            model = MeanRisk(
                objective_function=ObjectiveFunction.MAXIMIZE_UTILITY
                if config["method"] == "mean_variance"
                else ObjectiveFunction.MINIMIZE_RISK,
                risk_aversion=config["risk_aversion"]
                if config["method"] == "mean_variance"
                else 1.0,
                **kwargs,
            )
        model.fit(pd.DataFrame(history, columns=names))
        w = np.asarray(model.weights_, dtype=float) * (
            exposure if config["method"] == "risk_budgeting" else 1.0
        )
        problem = model.problem_
        status = problem.status
        if status not in ("optimal", "optimal_inaccurate"):
            raise ValueError(f"allocation_solver_status: {status}")
        solver_stats = {
            "solver": problem.solver_stats.solver_name,
            "iterations": problem.solver_stats.num_iters,
            "tolerance": config["solver_tolerance"],
            "objective_scale": 1000.0,
            "constraint_scale": 100.0,
            "objective_value": float(problem.value),
        }
        covariance = np.asarray(model.prior_estimator_.return_distribution_.covariance)
    residuals = independent_checks(
        w,
        exposure,
        cap,
        previous,
        config["max_turnover"],
        mapping,
        config["max_stock_weight"],
        stock_exposure,
    )
    risk = None
    if covariance is not None:
        variance = float(w @ covariance @ w)
        if not np.isfinite(variance) or variance <= 0:
            raise ValueError("allocation_risk_degenerate")
        risk = (w * (covariance @ w) / variance).tolist()
    if cancelled is not None:
        check_cancel(cancelled)
    receipt = {
        "schema_version": 1,
        "method": config["method"],
        "names": list(names),
        "weights": w.tolist(),
        "status": status,
        "solver": solver_stats,
        "risk_contributions": risk,
        "risk_budget_deviation": None
        if risk is None or config["method"] != "risk_budgeting"
        else [v - 1.0 / n for v in risk],
        "warnings": ["solver returned optimal_inaccurate; independent residuals passed"]
        if status == "optimal_inaccurate"
        else [],
        "constraints": residuals,
        "expected_returns": None if expected is None else np.asarray(expected).tolist(),
        "input_sha256": digest(
            {
                "config": config,
                "stock_exposure": stock_exposure,
                "names": names,
                "returns": history.tolist(),
                "previous": np.asarray(previous).tolist(),
                "mapping": None if mapping is None else mapping.tolist(),
                "expected": None if expected is None else np.asarray(expected).tolist(),
            }
        ),
    }
    return w, receipt


def percentiles(scores):
    values = pd.Series(scores, dtype=float)
    if not np.isfinite(values).all():
        raise ValueError("allocation_score_invalid")
    return (values.rank(method="average").to_numpy() - 0.5) / len(values)


class StockHistory:
    """Exact market-calendar indexing; no compression over missing security rows."""

    def __init__(
        self,
        calendar,
        prices,
        signals,
        horizon,
        config,
        provenance=None,
        missing=None,
        admission=None,
    ):
        self.calendar = list(calendar)
        if (
            len(set(calendar)) != len(calendar)
            or sorted(calendar) != list(calendar)
            or horizon < 1
        ):
            raise ValueError("allocation_calendar_invalid")
        self.index = {d: i for i, d in enumerate(calendar)}
        self.prices, self.signals, self.horizon, self.config = (
            prices,
            signals,
            horizon,
            config,
        )
        self.provenance = provenance
        self.missing = missing or {}
        self.admission = admission
        self.bad = {}
        self.candidate_cache = {}

    def candidates(self, date, scores):
        """As-of history eligibility precedes ranking. No future endpoint is inspected."""
        if date in self.candidate_cache:
            return self.candidate_cache[date]
        excluded, eligible = {}, {}
        end = self.index[date] - self.horizon
        start = end - self.config["risk_window"] + 1
        for symbol in sorted(scores):
            if self.config["candidate_policy"] == "unfiltered":
                eligible[symbol] = scores[symbol]
                continue
            if start < 1:
                excluded[symbol] = {"reason": "insufficient_calendar_history"}
                continue
            if symbol not in self.bad:
                observed = self.prices.get(symbol, {})
                self.bad[symbol] = [
                    i
                    for i, d in enumerate(self.calendar)
                    if d not in observed
                    or not np.isfinite(observed[d])
                    or observed[d] <= 0
                ]
            bad = self.bad[symbol]
            gaps = set()
            for a, b in ((start, end), (start + self.horizon, self.index[date])):
                gaps.update(bad[bisect_left(bad, a) : bisect_right(bad, b)])
            if gaps:
                dates = [self.calendar[i] for i in sorted(gaps)]
                excluded[symbol] = {
                    "reason": self.missing.get(symbol, {}).get(
                        dates[0], "unknown_price_or_tradeability"
                    ),
                    "first_missing_date": dates[0],
                    "missing_endpoints": dates,
                }
            else:
                eligible[symbol] = scores[symbol]
        if (
            self.admission is not None
            and self.config["candidate_policy"] == "complete_risk_window"
        ):
            row = self.admission["eligibility"].get(date)
            if row is None or sorted(eligible) != sorted(
                set(scores).intersection(row["eligible"])
            ):
                raise ValueError("allocation_eligibility_receipt_mismatch")
        receipt = {
            "policy": self.config["candidate_policy"],
            "as_of": date,
            "candidate_count": len(scores),
            "eligible_count": len(eligible),
            "eligible_sha256": digest(sorted(eligible)),
            "excluded_count": len(excluded),
            "excluded": excluded,
            "excluded_sha256": digest(excluded),
        }
        self.candidate_cache[date] = eligible, receipt
        return eligible, receipt

    def prepare(self, dates, reference_start, top_n, cancelled, formal_start=None):
        """Validate every scheduled prediction-based input before starting the account."""
        issues, clock, formal = [], 0, False
        formal_start = formal_start or reference_start
        for date in dates:
            check_cancel(cancelled)
            if date < reference_start or date not in self.signals:
                continue
            if date >= formal_start and not formal:
                clock, formal = 0, True
            if clock % self.horizon == 0:
                scores, admission = self.candidates(date, self.signals[date])
                names = sorted(scores, key=lambda s: (-scores[s], s))[:top_n]
                try:
                    if (
                        self.config["candidate_policy"] == "complete_risk_window"
                        and self.signals[date]
                        and len(names) < top_n
                    ):
                        raise ValueError("allocation_eligible_candidates_insufficient")
                    if names and self.config["method"] != "equal":
                        self.risk(date, names)
                    if names and self.config["method"] == "mean_variance":
                        self.calibrate(date, names, scores)
                except ValueError as error:
                    issues.append(
                        {"date": date, "error": str(error), "admission": admission}
                    )
            clock += 1
        return issues

    def label(self, signal_index, symbol):
        lo, hi = signal_index + 1, signal_index + 1 + self.horizon
        if lo < 0 or hi >= len(self.calendar):
            return None
        prices = self.prices.get(symbol, {})
        a, b = prices.get(self.calendar[lo]), prices.get(self.calendar[hi])
        if a is None or b is None or not np.isfinite([a, b]).all() or a <= 0 or b <= 0:
            return None
        return b / a - 1.0

    def risk(self, date, symbols):
        end = self.index[date] - 1 - self.horizon
        start = end - self.config["risk_window"] + 1
        if start < 0:
            raise ValueError(f"allocation_risk_warmup_insufficient: {date}")
        rows = [[self.label(i, s) for s in symbols] for i in range(start, end + 1)]
        if any(v is None for row in rows for v in row):
            raise ValueError(f"allocation_risk_history_gap: {date}")
        return rows, {
            "first_signal_date": self.calendar[start],
            "last_signal_date": self.calendar[end],
            "last_return_date": self.calendar[end + 1 + self.horizon],
            "horizon": self.horizon,
            "overlapping": self.horizon > 1,
        }

    def calibrate(self, date, symbols, scores):
        end = self.index[date] - 1 - self.horizon
        lo = max(0, end - self.config["calibration_window"] + 1)
        xx, yy, ww, used, records = [], [], [], [], []
        for i in range(lo, end + 1):
            signal = self.signals.get(self.calendar[i], {})
            if not signal:
                continue
            signal, eligibility = self.candidates(self.calendar[i], signal)
            names = sorted(signal)
            if not names:
                continue
            ranks = percentiles([signal[s] for s in names])
            pairs, excluded = [], {}
            for r, s in zip(ranks, names):
                y = self.label(i, s)
                if y is not None:
                    pairs.append((r, y))
                    continue
                endpoints = (self.calendar[i + 1], self.calendar[i + 1 + self.horizon])
                reasons = [
                    self.missing.get(s, {}).get(d, "unknown_price_or_tradeability")
                    for d in endpoints
                    if self.prices.get(s, {}).get(d) is None
                ]
                if not reasons or any(
                    reason != "verified_suspension" for reason in reasons
                ):
                    raise ValueError(
                        f"allocation_calibration_label_gap: {self.calendar[i]}:{s}"
                    )
                excluded[s] = {
                    "reason": "verified_suspension",
                    "endpoints": list(endpoints),
                }
            if len(pairs) < self.config["calibration_min_pairs"]:
                records.append(
                    {
                        "score_date": self.calendar[i],
                        "return_start": self.calendar[i + 1],
                        "return_end": self.calendar[i + 1 + self.horizon],
                        "pairs": len(pairs),
                        "used": False,
                        "excluded": excluded,
                        "eligibility": eligibility,
                    }
                )
                continue
            used.append(self.calendar[i])
            model = None
            if self.provenance is not None:
                candidates = [
                    m
                    for m in self.provenance["models"]
                    if m["range"]["start"] <= self.calendar[i] <= m["range"]["end"]
                ]
                if (
                    len(candidates) != 1
                    or candidates[0]["fit_cutoff"] >= self.calendar[i]
                ):
                    raise ValueError(
                        "allocation_calibration_prediction_not_out_of_sample"
                    )
                model = candidates[0]
            records.append(
                {
                    "score_date": self.calendar[i],
                    "return_start": self.calendar[i + 1],
                    "return_end": self.calendar[i + 1 + self.horizon],
                    "pairs": len(pairs),
                    "model": model,
                    "used": True,
                    "excluded": excluded,
                    "excluded_sha256": digest(excluded),
                    "eligibility": eligibility,
                }
            )
            for r, y in pairs:
                xx.append(r)
                yy.append(y)
                ww.append(1.0 / len(pairs))
        if len(used) < self.config["calibration_min_days"]:
            raise ValueError(
                f"allocation_calibration_warmup_insufficient: {date}; valid_days={len(used)}"
            )
        x, y, weights = np.array(xx), np.array(yy), np.array(ww)
        mx, my = np.average(x, weights=weights), np.average(y, weights=weights)
        denominator = np.sum(weights * (x - mx) ** 2)
        if denominator <= 1e-12:
            raise ValueError("allocation_calibration_degenerate")
        slope = np.sum(weights * (x - mx) * (y - my)) / denominator
        intercept = my - slope * mx
        ranks = dict(
            zip(sorted(scores), percentiles([scores[s] for s in sorted(scores)]))
        )
        mu = [float(intercept + slope * ranks[s]) for s in symbols]
        return mu, {
            "intercept": float(intercept),
            "slope": float(slope),
            "dates": used,
            "date_records": records,
            "strategy_contract": None
            if self.provenance is None
            else self.provenance["strategy_contract"],
            "last_return_date": self.calendar[end + 1 + self.horizon],
            "sample_sha256": digest({"x": xx, "y": yy, "weights": ww}),
        }
