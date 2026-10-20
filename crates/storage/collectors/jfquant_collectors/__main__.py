"""One supervised task per process. stdout is exclusively JSON Lines protocol v16."""

import contextlib
import hashlib
import importlib.metadata
import json
import os
import platform
import sys
import threading
from pathlib import Path

from . import PROTOCOL_VERSION
from .announcements import collect as announcements
from .evidence import EvidenceCapture
from .reconcile import replay
from .tdx import collect as tdx_collect
from .tdx import reference
from .tdx import fundamentals


def main():
    protocol = os.fdopen(os.dup(sys.stdout.fileno()), "w", buffering=1)
    os.dup2(sys.stderr.fileno(), sys.stdout.fileno())
    capture = None
    code_hash = hashlib.sha256()
    for name in (
        "__init__.py",
        "__main__.py",
        "providers.py",
        "evidence.py",
        "announcements.py",
        "tdx.py",
        "reconcile.py",
    ):
        code_hash.update(
            name.encode() + b"\0" + Path(__file__).with_name(name).read_bytes() + b"\0"
        )

    def report(event, **value):
        if event == "complete":
            value.setdefault("versions", versions)
            value["collector_signature"] = "sha256:" + code_hash.hexdigest()
            if capture is not None:
                value["evidence"] = {
                    "kind": "completion_sources",
                    "http_receipts": capture.receipts[:],
                }
        if capture is not None and event in ("shard", "action_shard", "adjustment_shard"):
            value["evidence"]["http_receipts"] = capture.receipts[:]
            capture.receipts.clear()
        protocol.write(
            json.dumps(
                {"version": PROTOCOL_VERSION, "event": event, **value},
                ensure_ascii=False,
                allow_nan=False,
                default=str,
            )
            + "\n"
        )
        protocol.flush()

    try:
        request = json.loads(sys.stdin.readline())
        if request["version"] != PROTOCOL_VERSION or request["operation"] not in (
            "market",
            "quotes",
            "actions",
            "announcements",
            "coefficients",
            "calendar",
            "members",
            "reconcile_actions",
            "fundamentals",
        ):
            raise ValueError("unsupported collector request")
        signature = "sha256:" + code_hash.hexdigest()
        if request["expected_collector_signature"] != signature:
            raise ValueError("collector code signature mismatch; rebuild the bundled runtime")
        versions = {
            "python": platform.python_version(),
            "tdxaidata": importlib.metadata.version("tdxaidata"),
        }
        report("ready", collector_signature=signature, versions=versions)
        cache = Path(request["cache_dir"])
        cache.mkdir(parents=True, exist_ok=True)
        os.environ["XDG_CACHE_HOME"] = str(cache)
        os.environ["TMPDIR"] = str(cache)

        def disconnected():
            # The Rust service owns this pipe until child completion, even after a client exits.
            sys.stdin.read()
            os._exit(130)

        threading.Thread(target=disconnected, daemon=True).start()
        capture = EvidenceCapture(Path(request["cache_dir"]).parent / "raw")
        with contextlib.redirect_stdout(sys.stderr), capture.enabled():
            operation = request["operation"]
            if operation == "reconcile_actions":
                replay(request, report)
                return 0
            if operation == "announcements" and request["provider"] == "cninfo":
                execute = announcements
            elif request["provider"] == "tdxaidata":
                execute = (
                    fundamentals
                    if operation == "fundamentals"
                    else reference
                    if operation in ("calendar", "members")
                    else tdx_collect
                )
            else:
                raise ValueError("unsupported_collector_provider")
            completed = []
            failures = {}

            def shard_report(event, **value):
                if event == "complete":
                    completed.append(value)
                else:
                    report(event, **value)

            for index, symbol in enumerate(request["instruments"]):
                shard = dict(request)
                shard["instruments"] = [symbol]
                shard["benchmarks"] = [s for s in request.get("benchmarks", []) if s == symbol]
                shard["universes"] = request.get("universes", []) if index == 0 else []
                try:
                    execute(shard, shard_report)
                except Exception as error:  # noqa: BLE001 - preserve independent securities
                    failures[symbol] = str(error)
                    report(
                        "shard_error", instrument=symbol, code="source_failed", message=str(error)
                    )
            calendar = []
            universes = {}
            for done in completed:
                if done.get("calendar"):
                    if calendar and calendar != done["calendar"]:
                        raise ValueError("inconsistent source calendars")
                    calendar = done["calendar"]
                universes.update(done.get("universes", {}))
            report(
                "complete",
                versions=versions,
                calendar=calendar,
                universes=universes,
                failures=failures,
            )
    except Exception as error:  # noqa: BLE001 - protocol boundary must report every provider failure
        report("error", code="collection_failed", message=str(error))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
