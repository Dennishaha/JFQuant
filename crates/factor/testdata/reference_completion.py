"""Direct NumPy formula oracle for the twelve explicit GTJA191 v5 interpretations.

No Rust AST is read. Uses the immutable v4 synthetic input panel and independent
NumPy time/cross-section helpers. Run: python reference_completion.py OUTPUT_JSON
"""
import hashlib
import json
import sys
from pathlib import Path

import numpy as np
import reference_ops as ops

source = Path(__file__).with_name("operator_golden.json")
fixture = json.loads(source.read_text())
shape = (fixture["days"], fixture["stocks"])
inputs = {name: np.array([np.nan if v is None else v for v in values]).reshape(shape)
          for name, values in fixture["columns"].items()}
close, open_, ret = (inputs[name] for name in ("close", "open", "ret"))
def rolling(x, n, fn):
    return ops.rolling(x, n, fn)
def mean(x, n): return rolling(x, n, np.mean)
def total(x, n): return rolling(x, n, np.sum)
def std(x, n): return rolling(x, n, lambda a: np.std(a, ddof=1))
def minimum(x, n): return rolling(x, n, np.min)
def maximum(x, n): return rolling(x, n, np.max)
def delay(x, n): return ops.delay(x, n)
def rank(x): return ops.rank(x)
def tsrank(x, n):
    return rolling(x, n, lambda a: np.count_nonzero((a <= a[-1]) |
        (abs(a-a[-1]) <= 1e-12*np.maximum(1, np.maximum(abs(a), abs(a[-1]))))) / len(a))
def corr(a, b, n):
    out = np.full(shape, np.nan)
    for t in range(n-1, shape[0]):
        for s in range(shape[1]):
            x, y = a[t+1-n:t+1, s], b[t+1-n:t+1, s]
            if np.isfinite(x).all() and np.isfinite(y).all():
                out[t, s] = np.corrcoef(x, y)[0, 1]
    return out
def sma(x, n, m):
    out = np.full(shape, np.nan)
    for s in range(shape[1]):
        state = None
        for t in range(shape[0]):
            if np.isfinite(x[t, s]):
                state = x[t, s] if state is None else m/n*x[t, s] + (1-m/n)*state
                out[t, s] = state
    return out
def rescaled_range(x, n):
    def value(a):
        cumulative = np.cumsum(a - np.mean(a))
        return np.ptp(cumulative) / np.std(a, ddof=1)
    return rolling(x, n, value)
def up_return_product(x):
    out = np.full(shape, np.nan)
    for s in range(shape[1]):
        state = None
        for t in range(shape[0]):
            current = x[t, s]
            if not np.isfinite(current) or current <= 0: continue
            if state is None:
                state = 1.0
            elif t == 0 or not np.isfinite(x[t-1, s]) or x[t-1, s] <= 0:
                continue
            elif current > x[t-1, s]:
                state *= current / x[t-1, s] - 1
            out[t, s] = state
    return out
def expected():
    smooth = sma(ret, 61, 2)
    benchmark = inputs["banchmarkindexclose"]
    centered_benchmark = benchmark - mean(benchmark, 20)
    threshold = (close / delay(close, 19)) ** (1/20) - 1
    up = np.where(np.isfinite(ret) & np.isfinite(threshold), (ret > threshold).astype(float), np.nan)
    down = np.where(np.isfinite(ret) & np.isfinite(threshold), (ret < threshold).astype(float), np.nan)
    square = (ret-threshold)**2
    double_rank = rank(rank(-rank((close-1)-delay(close-1, 5))))
    return {
        "alpha_010": rank(maximum(np.where(np.isfinite(ret), np.where(ret<0, std(ret, 20), close), np.nan)**2, 5)),
        "alpha_036": rank(total(corr(rank(inputs["volume"]), rank(inputs["vwap"]), 6), 2)),
        "alpha_054": -rank(std(abs(close-open_), 10) + close-open_ + corr(close, open_, 10)),
        "alpha_127": np.sqrt(mean((100*(close-maximum(close, 12))/maximum(close, 12))**2, 12)),
        "alpha_143": up_return_product(close),
        "alpha_146": mean(ret-smooth, 20)*(ret-smooth)/sma(smooth**2, 60, 2),
        "alpha_157": minimum(rank(rank(np.log(minimum(double_rank, 2)))), 5) + tsrank(delay(-ret, 6), 5),
        "alpha_165": rescaled_range(close, 48),
        "alpha_166": -20*19**1.5*total(ret-mean(ret, 20), 20)/(19*18*total((close/delay(close, 1))**2, 20)**1.5),
        "alpha_181": total((ret-mean(ret, 20))-centered_benchmark**2, 20)/total(centered_benchmark**3, 20),
        "alpha_183": rescaled_range(close, 24),
        "alpha_190": np.log((total(up, 20)-1)*total(np.where(np.isfinite(down), np.where(down!=0, square, 0), np.nan), 20)/
                            (total(down, 20)*total(np.where(np.isfinite(up), np.where(up!=0, square, 0), np.nan), 20))),
    }

if __name__ == "__main__":
    np.seterr(all="ignore")
    values = expected()
    result = {"formula_version": "gtja191-paper-2017+jfquant-audit-v5",
              "origin": "Direct NumPy formulas for the twelve documented v5 interpretations; no exported AST",
              "input_fixture_sha256": hashlib.sha256(source.read_bytes()).hexdigest(),
              "expected": {name: ops.clean(value[-3:]) for name, value in values.items()}}
    Path(sys.argv[1]).write_text(json.dumps(result, separators=(",", ":"), allow_nan=False)+"\n")
    print("direct formula goldens", len(values))
