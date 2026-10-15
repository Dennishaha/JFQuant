"""One supervised process per task; stdout contains only protocol JSON Lines."""
import contextlib
import hashlib
import importlib.metadata
import json
import os
from pathlib import Path
import queue
import sys
import threading
import traceback

from . import PROTOCOL_VERSION


def signature():
    digest = hashlib.sha256()
    for name in ("__init__.py", "__main__.py", "training.py", "backtest.py"):
        digest.update(name.encode() + b"\0" + Path(__file__).with_name(name).read_bytes() + b"\0")
    return "sha256:" + digest.hexdigest()


def main():
    wire = os.fdopen(os.dup(sys.stdout.fileno()), "w", buffering=1)
    os.dup2(sys.stderr.fileno(), sys.stdout.fileno())
    cancelled = threading.Event()
    requests = queue.Queue()

    def receive():
        try:
            for line in sys.stdin:
                request = json.loads(line)
                if request.get("operation") == "cancel":
                    cancelled.set()
                else:
                    requests.put(request)
        finally:
            cancelled.set()
            requests.put(None)

    threading.Thread(target=receive, daemon=True).start()

    def emit(event, **values):
        wire.write(json.dumps({"version": PROTOCOL_VERSION, "event": event, **values}, allow_nan=False) + "\n")
        wire.flush()

    with contextlib.redirect_stdout(sys.stderr):
        from .training import train, predict
        from .backtest import backtest
        lock = Path(sys.prefix) / "requirements.lock"
        expected = {}
        for line in lock.read_text().splitlines():
            if line and not line[0].isspace() and "==" in line and not line.startswith("#"):
                name, version = line.split()[0].split("==", 1)
                expected[name] = version
        for name, version in expected.items():
            if importlib.metadata.version(name) != version:
                raise RuntimeError(f"dependency version mismatch: {name}; rebuild runtime")
        versions = {name: importlib.metadata.version(name) for name in ("akquant", "lightgbm", "numpy", "pyarrow", "polars")}
        versions["worker_signature"] = signature()
        versions["dependencies_sha256"] = "sha256:" + hashlib.sha256(lock.read_bytes()).hexdigest()
        versions["python"] = __import__("platform").python_version()
        emit("ready", versions=versions)
        models = {}
        while (request := requests.get()) is not None:
            try:
                if request.get("version") != PROTOCOL_VERSION or request.get("signature") != signature():
                    raise ValueError("research worker protocol/signature mismatch; rebuild runtime")
                if cancelled.is_set():
                    raise InterruptedError("research cancelled")
                operation = request["operation"]
                if operation == "shutdown":
                    return
                if operation == "train":
                    result = train(request, models, cancelled, emit)
                elif operation == "predict":
                    result = predict(request, models)
                elif operation == "backtest":
                    result = backtest(request, cancelled, emit)
                else:
                    raise ValueError("unsupported research operation")
                emit("complete", result=result, versions=versions)
            except Exception as error:
                traceback.print_exc(file=sys.stderr)
                emit("error", message=str(error), cancelled=cancelled.is_set())


if __name__ == "__main__":
    main()
