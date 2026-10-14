"""Independent Pandas/NumPy oracle for pinned Qlib Alpha158, not a runtime dependency.
Why: GTJA full-window/null/rank semantics cannot substitute Qlib's rolling contract.
Run with the bundled research Python: python reference_alpha158.py
"""

import json
import sys
from pathlib import Path

import numpy as np
import pandas as pd

HERE = Path(__file__).resolve().parent
catalog = json.loads((HERE / "alpha158_formulas.json").read_text())
days, stocks = 85, 3
inputs = {k: [] for k in ("open", "high", "low", "close", "volume", "vwap")}
for t in range(days):
    for s in range(stocks):
        close = 20.0 if s == 1 else 18 + s + 0.02 * t + (t % 7) * 0.13
        op = close + (0 if s == 1 else (t % 3 - 1) * 0.08)
        volume = 1000.0 if s == 1 else float(800 + t * 11 + (t % 4) * 30)
        if s == 2 and t in (5, 27):
            volume = 0.0
        for k, value in {
            "close": close,
            "open": op,
            "high": max(op, close) + 0.2,
            "low": min(op, close) - 0.2,
            "volume": volume,
            "vwap": (op + close) / 2,
        }.items():
            inputs[k].append(
                None
                if (s == 2 and t in (8, 21) and k in ("close", "volume"))
                else value
            )


def evaluate(expr, frame):
    name, value = next(iter(expr.items()))
    if name == "Input":
        return frame[value]
    if name == "Constant":
        return value
    if name == "Binary":
        a, b = evaluate(value["left"], frame), evaluate(value["right"], frame)
        return {
            "add": lambda: a + b,
            "sub": lambda: a - b,
            "mul": lambda: a * b,
            "div": lambda: a / b,
            "gt": lambda: a > b,
            "lt": lambda: a < b,
        }[value["op"]]()
    args = [evaluate(a, frame) for a in value["args"]]
    op = value["name"]
    a = args[0]
    if op == "Abs":
        return abs(a)
    if op == "Log":
        return np.log(a)
    if op == "Greater":
        return np.maximum(*args)
    if op == "Less":
        return np.minimum(*args)
    if op == "Ref":
        return a.shift(int(args[1]))
    if op == "Corr":
        w = int(args[2])
        r = a.rolling(w, min_periods=1).corr(args[1])
        r[
            np.isclose(a.rolling(w, min_periods=1).std(), 0, atol=2e-5)
            | np.isclose(args[1].rolling(w, min_periods=1).std(), 0, atol=2e-5)
        ] = np.nan
        return r
    w = int(args[1])
    roll = a.rolling(w, min_periods=1)
    if op in ("Mean", "Std", "Sum", "Max", "Min"):
        return getattr(roll, op.lower())()
    if op == "Rank":
        return roll.rank(pct=True)
    if op == "Quantile":
        return roll.quantile(args[2])
    if op in ("IdxMax", "IdxMin"):
        return roll.apply(
            lambda x: (x.argmax() if op == "IdxMax" else x.argmin()) + 1, raw=True
        )
    if op in ("Slope", "Rsquare", "Resi"):
        result = []
        for t in range(len(a)):
            y = a.iloc[max(0, t + 1 - w) : t + 1].to_numpy(dtype=float)
            x = np.arange(1, len(y) + 1, dtype=float)
            mask = np.isfinite(y)
            xx = x[mask]
            yy = y[mask]
            if len(xx) < 2:
                result.append(np.nan)
                continue
            dx, dy = xx - xx.mean(), yy - yy.mean()
            sx = np.dot(dx, dx)
            sy = np.dot(dy, dy)
            xy = np.dot(dx, dy)
            if op == "Slope":
                v = xy / sx
            elif op == "Rsquare":
                v = (
                    np.nan
                    if np.isclose(np.std(yy, ddof=1), 0, atol=2e-5)
                    else xy**2 / (sx * sy)
                )
            else:
                v = y[-1] - (yy.mean() + xy / sx * (len(y) - xx.mean()))
            result.append(v)
        return pd.Series(result, index=a.index)
    raise ValueError(op)


expected = {}
with np.errstate(all="ignore"):
    for i, f in enumerate(catalog["features"]):
        output = [None] * (days * stocks)
        for s in range(stocks):
            frame = pd.DataFrame({k: values[s::stocks] for k, values in inputs.items()})
            vals = evaluate(f["expression"], frame)
            for t, v in enumerate(vals):
                output[t * stocks + s] = float(v) if np.isfinite(v) else None
        expected[f"alpha158_{i + 1:03}_{f['name']}"] = output
fixture = {
    "formula_version": catalog["formula_version"],
    "days": days,
    "stocks": stocks,
    "inputs": inputs,
    "expected": expected,
}
(
    Path(sys.argv[1]) if len(sys.argv) > 1 else HERE / "alpha158_expected.json"
).write_text(json.dumps(fixture, indent=2, allow_nan=False) + "\n")
