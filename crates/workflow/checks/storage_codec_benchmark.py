#!/usr/bin/env python3
"""Disposable codec comparison through production matrix/train/predict/backtest readers.
Run with the bundled Python and PYTHONPATH=crates/workflow/research:crates/workflow/research/tests.
All samples and outputs go into a new directory; first passes are not strict cold tests.
"""

import argparse
import json
from pathlib import Path
import resource
import sys
import threading
import time

import numpy as np
import pandas as pd
import pyarrow as pa
import pyarrow.parquet as pq

from allocation_fixture import backtest
from jfquant_research.training import matrix, predict, train


def measured(body):
    started = time.perf_counter()
    result = body()
    return result, (time.perf_counter() - started) * 1000


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--codec",
        choices=["all", "uncompressed", "snappy", "zstd1"],
        default="all",
        help="Isolate a codec in its own process for peak RSS measurement",
    )
    args = parser.parse_args()
    root = args.output.resolve()
    root.mkdir(parents=True, exist_ok=False)
    rng = np.random.default_rng(20261020)
    values = rng.normal(size=(16384, 32))
    columns = [f"factor_{i:02}" for i in range(values.shape[1])]
    tables = {
        "matrix": pa.table(
            {
                **{
                    name: pa.array(values[:, i], mask=np.arange(len(values)) % 17 == 0)
                    for i, name in enumerate(columns)
                },
                "__label": values[:, 0] + 0.2 * values[:, 1],
            }
        )
    }
    dates = [str(d.date()) for d in pd.bdate_range("2020-01-01", periods=504)]
    symbols = [f"SH{600000 + i}" for i in range(20)]
    bars, scores = [], []
    for day, date in enumerate(dates):
        for i, symbol in enumerate(symbols):
            price = 10.0 + i * 0.1 + day * 0.002
            bars.append(
                dict(
                    date=date,
                    symbol=symbol,
                    open=price,
                    high=price,
                    low=price,
                    close=price,
                    volume=10000000.0,
                )
            )
            scores.append(
                dict(date=date, symbol=symbol, score=float((i + day // 20) % 20))
            )
    tables["bars"] = pa.Table.from_pylist(bars)
    tables["signals"] = pa.Table.from_pylist(scores)
    codecs = {
        "uncompressed": dict(compression=None),
        "snappy": dict(compression="snappy"),
        "zstd1": dict(compression="zstd", compression_level=1),
    }
    if args.codec != "all":
        codecs = {args.codec: codecs[args.codec]}
    for codec, options in codecs.items():
        directory = root / codec
        directory.mkdir()
        for name, table in tables.items():
            path = directory / f"{name}.parquet"
            _, write_ms = measured(lambda: pq.write_table(table, path, **options))
            actual, read_ms = measured(lambda: pq.read_table(path))
            assert actual.equals(table)
            stat = path.stat()
            print(
                json.dumps(
                    dict(
                        codec=codec,
                        sample=name,
                        rows=len(table),
                        bytes=stat.st_size,
                        allocated_bytes=stat.st_blocks * 512,
                        write_ms=write_ms,
                        first_read_ms=read_ms,
                        values_equal=True,
                    )
                ),
                flush=True,
            )
    baseline_predictions, baseline_days = None, None
    for phase in ["first", "warm"]:
        for codec in codecs:
            directory = root / codec
            output = directory / phase
            output.mkdir()
            path = str(directory / "matrix.parquet")
            _, matrix_ms = measured(lambda: matrix(path, columns))
            models = {}
            spec = dict(
                factor_ids=columns,
                objective="Regression",
                early_stopping_rounds=None,
                parameters=dict(
                    num_iterations=30,
                    learning_rate=0.1,
                    num_leaves=15,
                    num_threads=1,
                    deterministic=True,
                    force_col_wise=True,
                    seed=7,
                ),
            )
            _, train_ms = measured(
                lambda: train(
                    dict(request=spec, train=path, output=str(output)),
                    models,
                    threading.Event(),
                    lambda *a, **k: None,
                )
            )
            prediction = output / "prediction.parquet"
            _, predict_ms = measured(
                lambda: predict(
                    dict(
                        model=str(output / "model.txt"),
                        matrix=path,
                        output=str(prediction),
                        contributions=False,
                    ),
                    models,
                )
            )
            predicted = pq.read_table(prediction)
            if baseline_predictions is None:
                baseline_predictions = predicted
            assert predicted.equals(baseline_predictions)
            request = dict(
                bars=str(directory / "bars.parquet"),
                signals=str(directory / "signals.parquet"),
                signal_dates=dates,
                output=str(output),
                initial_cash=1000000.0,
                top_n=5,
                rebalance_days=20,
                commission_rate=0.00025,
                minimum_commission=5.0,
                stamp_tax=0.0005,
                transfer_fee=0.00001,
                lot_sizes={s: 100 for s in symbols},
                actions=[],
                benchmark_returns={d: 0.0 for d in dates},
            )
            result, backtest_ms = measured(
                lambda: backtest(request, threading.Event(), lambda *a, **k: None)
            )
            if baseline_days is None:
                baseline_days = result["days"]
            assert result["days"] == baseline_days
            (output / "account-days.json").write_text(
                json.dumps(result["days"], sort_keys=True)
            )
            print(
                json.dumps(
                    dict(
                        codec=codec,
                        phase=phase,
                        matrix_ms=matrix_ms,
                        train_ms=train_ms,
                        predict_ms=predict_ms,
                        backtest_ms=backtest_ms,
                        predictions_equal=True,
                        account_days_equal=True,
                    )
                ),
                flush=True,
            )
    print(
        json.dumps(
            dict(
                peak_rss_bytes=resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
                * (1 if sys.platform == "darwin" else 1024),
                note="No OS cache purge: first/warm process passes, not strict cold measurements.",
            )
        ),
        flush=True,
    )


if __name__ == "__main__":
    main()
