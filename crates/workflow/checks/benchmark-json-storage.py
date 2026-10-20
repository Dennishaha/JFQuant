#!/usr/bin/env python3
"""Compare legacy metadata discovery and SQLite projections on the same workspace.

Run --mode json before migration, then --mode sqlite afterward. Outputs JSON only;
no workspace writes, cache eviction, service startup or network requests.
"""

import argparse
import gc
import json
from pathlib import Path
import resource
import sqlite3
import statistics
import time

p = argparse.ArgumentParser(description=__doc__)
p.add_argument("--workspace", type=Path, required=True)
p.add_argument("--mode", choices=["json", "sqlite"], required=True)
p.add_argument("--iterations", type=int, default=5)
a = p.parse_args()
if a.iterations < 1:
    p.error("iterations must be positive")
root = a.workspace.resolve()
conn = None
if a.mode == "sqlite":
    conn = sqlite3.connect(
        (root / ".jfquant/storage/index.sqlite").as_uri() + "?mode=ro", uri=True
    )
    conn.execute("PRAGMA cache_size=-32768")


def catalog():
    if conn:
        return conn.execute(
            "SELECT id,kind,path,signature,start_date,end_date FROM catalog_entries ORDER BY id"
        ).fetchall()
    result = []
    for path in sorted((root / ".jfquant/catalog").glob("*.json")):
        v = json.loads(path.read_bytes())
        result.append(
            tuple(v.get(k) for k in ["id", "kind", "path", "signature", "start", "end"])
        )
    return sorted(result)


def runs():
    if conn:
        rows = conn.execute(
            "SELECT id,status,request_json,signature,output,operation,created_at,completed_at,stage,error,read_only FROM run_records WHERE path LIKE ? ORDER BY id",
            (".jfquant/runs/%",),
        ).fetchall()
        return [(r[0], r[1], json.loads(r[2]), *r[3:]) for r in rows]
    result = []
    for path in (root / ".jfquant/runs").glob("*/run.json"):
        if path.parent.is_symlink():
            continue
        v = json.loads(path.read_bytes())
        result.append(
            tuple(
                v[k]
                for k in [
                    "id",
                    "status",
                    "request",
                    "signature",
                    "output",
                    "operation",
                    "started_at",
                    "finished_at",
                    "stage",
                    "error",
                    "read_only",
                ]
            )
        )
    return sorted(result)


report = {
    "mode": a.mode,
    "iterations": a.iterations,
    "workspace": str(root),
    "queries": {},
}
for name, query in [("catalog_headers", catalog), ("active_generation_runs", runs)]:
    samples = []
    count = 0
    for _ in range(a.iterations):
        gc.collect()
        start = time.perf_counter()
        result = query()
        samples.append(time.perf_counter() - start)
        count = len(result)
        del result
    report["queries"][name] = {
        "rows": count,
        "median_seconds": statistics.median(samples),
        "samples_seconds": samples,
    }
# macOS reports bytes; Linux reports KiB. Keep the platform-native unit explicit.
import sys

report["peak_rss_bytes"] = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * (
    1 if sys.platform == "darwin" else 1024
)
print(json.dumps(report, indent=2))
