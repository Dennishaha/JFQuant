"""Official LightGBM API, explicit validation-only early stopping and feature order."""

import json
from pathlib import Path

import lightgbm as lgb
import numpy as np
import pyarrow as pa
import pyarrow.parquet as pq


def matrix(path, columns):
    table = pq.read_table(path)
    if table.column_names != [*columns, "__label"]:
        raise ValueError("matrix feature order/schema mismatch")
    x = np.column_stack(
        [table[name].to_numpy(zero_copy_only=False) for name in columns]
    )
    return x, table["__label"].to_numpy(zero_copy_only=False)


def train(request, models, cancelled, emit):
    spec = request["request"]
    columns = spec["factor_ids"]
    x, y = matrix(request["train"], columns)
    params = dict(spec["parameters"])
    objective = spec["objective"]
    params["objective"] = (
        {"Regression": "regression", "Binary": "binary"}.get(objective, "multiclass")
        if isinstance(objective, str)
        else "multiclass"
    )
    if isinstance(objective, dict):
        params["num_class"] = objective["Multiclass"]["classes"]
    rounds = int(params.pop("num_iterations", 100))
    params.setdefault(
        "metric",
        "l2"
        if params["objective"] == "regression"
        else "binary_logloss"
        if params["objective"] == "binary"
        else "multi_logloss",
    )
    params.setdefault("verbosity", -1)
    params.setdefault("num_threads", min(8, __import__("os").cpu_count() or 1))
    # LightGBM's public API validates values; no key=value string concatenation.
    data = lgb.Dataset(x, label=y, feature_name=columns)
    valid_sets, valid_names = [data], ["train"]
    if request.get("validation"):
        vx, vy = matrix(request["validation"], columns)
        valid_sets.append(
            lgb.Dataset(vx, label=vy, reference=data, feature_name=columns)
        )
        valid_names.append("validation")
    history = []

    def callback(env):
        if cancelled.is_set():
            raise InterruptedError("training cancelled")
        values = {"iteration": env.iteration + 1, "train": [], "validation": []}
        for name, _metric, score, *_ in env.evaluation_result_list:
            values[name].append(float(score))
        history.append(values)
        emit("progress", message=f"LightGBM iteration {env.iteration + 1}")

    callback.order = 10
    callbacks = [callback]
    patience = spec.get("early_stopping_rounds")
    if patience and len(valid_sets) > 1:
        callbacks.append(
            lgb.early_stopping(int(patience), first_metric_only=True, verbose=False)
        )
    booster = lgb.train(
        params,
        data,
        num_boost_round=rounds,
        valid_sets=valid_sets,
        valid_names=valid_names,
        callbacks=callbacks,
    )
    best = booster.best_iteration or booster.current_iteration()
    output = Path(request["output"])
    model = output / "model.txt"
    booster.save_model(str(model), num_iteration=best)
    # Reload the saved best model; future predictions and test metrics use exactly this artifact.
    models.clear()
    models[str(model)] = lgb.Booster(model_file=str(model))
    rmse = None
    if request.get("test") and params["objective"] == "regression":
        tx, ty = matrix(request["test"], columns)
        rmse = float(np.sqrt(np.mean((models[str(model)].predict(tx) - ty) ** 2)))
    metrics = {
        "iterations": len(history),
        "best_iteration": best,
        "history": history,
        "test_rmse": rmse,
        "early_stopping_rounds": patience,
    }
    (output / "metrics.json").write_text(json.dumps(metrics, allow_nan=False))
    (output / "feature_importance.json").write_text(
        json.dumps(booster.feature_importance("gain", iteration=best).tolist())
    )
    return metrics


def predict(request, models):
    path = request["model"]
    if path not in models:
        models.clear()
        models[path] = lgb.Booster(model_file=path)
    booster = models[path]
    columns = booster.feature_name()
    x, _ = matrix(request["matrix"], columns)
    values = booster.predict(x, pred_contrib=request["contributions"])
    if values.ndim == 1:
        values = values.reshape(-1, 1)
    pq.write_table(
        pa.table({f"value_{i}": values[:, i] for i in range(values.shape[1])}),
        request["output"],
        compression="zstd",
        compression_level=1,
    )
    return {"path": request["output"], "rows": len(values), "columns": values.shape[1]}
