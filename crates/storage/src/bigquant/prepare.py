"""Offline, checksum-verified Parquet candidates. Never registers a research dataset.

Run with an explicit jfquant binary and a local source index; no SDK import or network.
Raw evidence remains authoritative. In particular, financial NULLs are not zeros.
"""
import argparse
import bisect
import hashlib
import json
import math
import shutil
import subprocess
import uuid
from collections import Counter, defaultdict
from datetime import date
from pathlib import Path

VERSION = "bigquant-offline-candidates-v1"
DATE_COLUMNS = {
    "date", "start_date", "end_date", "report_date", "publish_date", "ex_date",
    "register_date", "exright_date", "allot_listdate", "list_date", "delist_date",
}
TEXT_COLUMNS = {"instrument", "name", "exchange", "security_type"}
INT_COLUMNS = {"observations", "distinct_dates", "change_type", "suspended"}


def day(value):
    if not isinstance(value, str) or len(value) != 19 or value[10:] != "T00:00:00":
        raise ValueError("invalid source midnight date")
    result = date.fromisoformat(value[:10])
    if result.isoformat() != value[:10]:
        raise ValueError("noncanonical source date")
    return result


def number(value):
    if value is None:
        return None
    if isinstance(value, bool) or not isinstance(value, (int, float, str)):
        raise ValueError("invalid numeric type")
    result = float(value)
    if not math.isfinite(result) or abs(result) > 1e25:
        raise ValueError("nonfinite number or sentinel")
    return result


def instrument_id(symbol):
    if not isinstance(symbol, str) or len(symbol) != 9 or symbol[6] != ".":
        raise ValueError("invalid BigQuant instrument")
    if not symbol[:6].isascii() or not symbol[:6].isdigit() or symbol[7:] not in {"SH", "SZ", "BJ"}:
        raise ValueError("invalid BigQuant instrument")
    return symbol[7:] + symbol[:6]


def run_indices(calendar, row):
    start, end = day(row["start_date"]), day(row["end_date"])
    a, b = bisect.bisect_left(calendar, start), bisect.bisect_left(calendar, end)
    if a >= len(calendar) or b >= len(calendar) or calendar[a] != start or calendar[b] != end:
        raise ValueError("run off source calendar")
    if a > b or row["observations"] != row["distinct_dates"] or row["observations"] != b - a + 1:
        raise ValueError("run contains duplicates or hidden gaps")
    return a, b


def equity_candidate(row):
    """Deduct aggregate once. Missing aggregate/components never imply zero.

    This numeric candidate is not a certification of units or ordinary equity scope.
    Preferred/perpetual subfields can overlap and are not added to the aggregate.
    """
    parent = number(row["total_equity_to_parent_shareholders"])
    other = number(row["other_equity_instruments"])
    parts = [number(row[c]) for c in (
        "preference_of_other_equity_instruments", "preference_shares", "perpetual_bonds"
    )]
    if parent is None:
        return None, "parent_equity_unknown"
    if other is None:
        return None, "other_equity_unknown"
    tolerance = max(1e-6, abs(other) * 1e-9)
    if other < 0 or any(v is not None and (v < 0 or v > other + tolerance) for v in parts):
        return None, "other_equity_components_inconsistent"
    return parent - other, None


def merged_runs(ranges):
    result = []
    for a, b in sorted(ranges):
        if result and a <= result[-1][1]:
            raise ValueError("overlapping source runs")
        if result and a == result[-1][1] + 1:
            result[-1] = (result[-1][0], b)
        else:
            result.append((a, b))
    return result


def verify_source(cli, base, item):
    snapshot = (base / item["snapshot"]).resolve()
    request_path = (base / item["request"]).resolve()
    raw = snapshot.read_bytes()
    value = json.loads(raw)
    request_raw = request_path.read_bytes()
    request = json.loads(request_raw)
    if value["payload"]["request"] != request or value["sha256"] != item["payload_sha256"]:
        raise ValueError("source index mismatch")
    command = item["command"]
    if command not in {"fetch", "extract"}:
        raise ValueError("invalid verification command")
    # Existing immutable cache path: Rust checks exact SQL/version/schema/hash offline.
    reply = subprocess.run([
        str(cli), "data", "bigquant", command, "--python", "/not-installed-sdk",
        "--request", str(request_path), "--output", str(snapshot),
    ], capture_output=True, text=True, check=True, timeout=90)
    checked = json.loads(reply.stdout)
    if not checked["cache_hit"] or not checked["complete"] or checked["sha256"] != item["payload_sha256"]:
        raise ValueError("source incomplete or cache verification failed")
    if snapshot.read_bytes() != raw or request_path.read_bytes() != request_raw:
        raise ValueError("source changed during validation")
    return value, {
        **item, "file_sha256": hashlib.sha256(raw).hexdigest(),
        "observed_at": value["observed_at"], "rows": checked["rows"],
    }


def prepare(cli, manifest_path, output):
    import pyarrow as pa
    import pyarrow.parquet as pq

    manifest_raw = manifest_path.read_bytes()
    implementation_sha256 = hashlib.sha256(Path(__file__).read_bytes()).hexdigest()
    manifest = json.loads(manifest_raw)
    if manifest["schema_version"] != 1 or manifest["shares_availability"] != "provider_dates" or manifest["shares_revision_risk"] is not True:
        raise ValueError("explicit provider historical shares and revision risk required")
    if output.exists():
        raise FileExistsError("candidate output is immutable; choose a new directory")
    base = manifest_path.parent
    sources = manifest["sources"]
    if not sources or len({str((base / s["snapshot"]).resolve()) for s in sources}) != len(sources):
        raise ValueError("empty or duplicate source index")
    output.parent.mkdir(parents=True, exist_ok=True)
    lock = output.with_name(output.name + ".lock")
    with lock.open("x") as handle:
        handle.write(str(output))
    stage = output.with_name(output.name + ".tmp-" + uuid.uuid4().hex)
    try:
        stage.mkdir()
        calendars, suspensions, classification = {}, set(), {}
        # Keep only small lookup sources in memory. Every large shard is verified at consumption.
        preload = {}
        for i, item in enumerate(sources):
            if item["kind"] not in {"share_calendar", "universe_calendar", "suspensions", "basic_info"}:
                continue
            value, receipt = verify_source(cli, base, item)
            rows = value["payload"]["rows"]
            kind = item["kind"]
            preload[i] = (value, receipt)
            if kind.endswith("calendar"):
                calendar = [day(r["date"]) for r in rows]
                if kind in calendars or not calendar or sorted(set(calendar)) != calendar:
                    raise ValueError("calendar missing, duplicate or unordered")
                calendars[kind] = calendar
            elif kind == "suspensions":
                for r in rows:
                    if r["suspended"] != 1:
                        raise ValueError("invalid explicit suspension flag")
                    key = (r["instrument"], day(r["date"]))
                    if key in suspensions:
                        raise ValueError("duplicate suspension key")
                    suspensions.add(key)
            else:
                for r in rows:
                    if r["instrument"] in classification:
                        raise ValueError("duplicate basic information")
                    classification[r["instrument"]] = r["security_type"]
        if set(calendars) != {"share_calendar", "universe_calendar"}:
            raise ValueError("both source calendars required")
        price_calendar = set(calendars["universe_calendar"])
        counts, issues, outputs, receipts = Counter(), Counter(), [], []
        keys = defaultdict(set)
        ranges = defaultdict(lambda: defaultdict(list))
        security_counts = defaultdict(set)
        for i, item in enumerate(sources):
            value, receipt = preload.pop(i) if i in preload else verify_source(cli, base, item)
            request = value["payload"]["request"]
            kind = item["kind"]
            expected_kind = request.get("kind") or {
                "cn_stock_basic_info": "basic_info", "cn_stock_dividend": "dividends",
                "cn_stock_allotment": "allotments",
            }.get(request.get("table"))
            if expected_kind != kind:
                raise ValueError("projection kind mismatch")
            receipts.append(receipt)
            rows = value["payload"]["rows"]
            if not rows:
                continue
            columns = list(rows[0])
            fields = [pa.field(c, pa.date32() if c in DATE_COLUMNS else pa.string() if c in TEXT_COLUMNS else pa.int64() if c in INT_COLUMNS else pa.float64()) for c in columns]
            fields += [pa.field("source_index", pa.int32())]
            has_instrument = "instrument" in columns
            if has_instrument:
                fields += [pa.field("instrument_id", pa.string()), pa.field("security_type", pa.string())] if "security_type" not in columns else [pa.field("instrument_id", pa.string())]
            if kind == "annual_equity":
                fields += [pa.field("parent_minus_other_equity", pa.float64()), pa.field("equity_issue", pa.string())]
            if kind in {"price_bars", "price_factors"}:
                fields += [pa.field("quote_state", pa.string())]
            normalized = []
            for row in rows:
                r = {}
                if set(row) != set(columns):
                    raise ValueError("inconsistent row schema")
                for c, v in row.items():
                    r[c] = None if v is None else day(v) if c in DATE_COLUMNS else v if c in TEXT_COLUMNS or c in INT_COLUMNS else number(v)
                r["source_index"] = i
                if has_instrument:
                    symbol = row["instrument"]
                    r["instrument_id"] = instrument_id(symbol)
                    if "security_type" not in columns:
                        r["security_type"] = classification.get(symbol)
                    security_counts[kind].add(symbol)
                if kind in {"share_runs", "universe_runs"}:
                    calendar = calendars["share_calendar" if kind == "share_runs" else "universe_calendar"]
                    a, b = run_indices(calendar, row)
                    if not date.fromisoformat(request["start"]) <= calendar[a] <= calendar[b] <= date.fromisoformat(request["end"]):
                        raise ValueError("run outside requested date range")
                    ranges[kind][symbol].append((a, b))
                    counts[kind + "_expanded_observations"] += b - a + 1
                    if kind == "share_runs" and r["total_shares"] is not None and r["total_shares"] <= 0:
                        raise ValueError("nonpositive shares")
                elif kind == "annual_equity":
                    key = (symbol, r["report_date"], r["date"])
                    if key in keys[kind]:
                        raise ValueError("duplicate annual disclosure key")
                    keys[kind].add(key)
                    if (r["report_date"].month, r["report_date"].day) != (12, 31) or r["report_date"] >= r["date"]:
                        raise ValueError("invalid annual disclosure dates")
                    r["parent_minus_other_equity"], r["equity_issue"] = equity_candidate(row)
                    issues[r["equity_issue"] or "equity_numeric_candidate"] += 1
                elif kind in {"price_bars", "price_factors"}:
                    key = (symbol, r["date"])
                    if key in keys["prices"]:
                        raise ValueError("duplicate price key across shards/projections")
                    keys["prices"].add(key)
                    if r["date"] not in price_calendar:
                        raise ValueError("price off source calendar")
                    for c in {"open", "high", "low", "close", "adjust_factor"} & r.keys():
                        if r[c] is not None and r[c] <= 0:
                            raise ValueError("nonpositive price or adjustment factor")
                    for c in {"volume", "amount"} & r.keys():
                        if r[c] is not None and r[c] < 0:
                            raise ValueError("negative volume or amount")
                    if kind == "price_bars" and all(r[c] is not None for c in ("open", "high", "low", "close")):
                        if not r["low"] <= min(r["open"], r["close"]) <= max(r["open"], r["close"]) <= r["high"]:
                            raise ValueError("invalid OHLC ordering")
                    r["quote_state"] = "priced" if r["close"] is not None else "missing_close_suspended" if key in suspensions else "missing_close_unexplained"
                    issues[r["quote_state"]] += 1
                    if r["adjust_factor"] is None:
                        issues["adjust_factor_unknown"] += 1
                # Validate partition dates, while other dates such as report end may be earlier.
                date_column = request.get("date_column", "date")
                if date_column in r and not date.fromisoformat(request["start"]) <= r[date_column] <= date.fromisoformat(request["end"]):
                    raise ValueError("row outside requested date range")
                if has_instrument and request["instruments"] and symbol not in request["instruments"]:
                    raise ValueError("unexpected instrument")
                normalized.append(r)
            dest = stage / kind / f"part-{i:04}.parquet"
            dest.parent.mkdir(exist_ok=True)
            schema = pa.schema(fields)
            pq.write_table(pa.Table.from_pylist(normalized, schema=schema), dest, compression="zstd")
            schema_fields = [{"name": f.name, "type": str(f.type), "nullable": f.nullable} for f in schema]
            outputs.append({
                "path": dest.relative_to(stage).as_posix(), "rows": len(rows), "source_index": i,
                "sha256": hashlib.sha256(dest.read_bytes()).hexdigest(), "fields": schema_fields,
                "schema_sha256": hashlib.sha256(json.dumps(schema_fields, separators=(",", ":")).encode()).hexdigest(),
            })
            counts[kind] += len(rows)
        merged = {k: {s: merged_runs(v) for s, v in by.items()} for k, by in ranges.items()}
        same_calendar = calendars["share_calendar"] == calendars["universe_calendar"]
        same_members = same_calendar and merged.get("share_runs") == merged.get("universe_runs")
        report = {
            "schema_version": 1, "normalization": VERSION, "research_admitted": False,
            "source_index_sha256": hashlib.sha256(manifest_raw).hexdigest(),
            "implementation_sha256": implementation_sha256, "pyarrow_version": pa.__version__,
            "shares_availability": "provider_dates", "shares_revision_risk": True,
            "financial_vintages": "preserved_date_report_date_change_type; nulls_not_zero",
            "equity_candidate_units": "supplier_native; unit_and_common_equity_scope_require_validation",
            "adjust_factor": "provider_cumulative_hfq_field; not_independent_tdx_verification",
            "calendars_equal": same_calendar, "share_universe_daily_keys_equal": same_members,
            "counts": dict(counts), "issues": dict(issues),
            "securities": {k: len(v) for k, v in security_counts.items()},
            "security_classifications": {
                k: dict(Counter(classification.get(s, "unknown") for s in v))
                for k, v in security_counts.items()
            },
            "sources": receipts, "outputs": outputs,
            "identity_bindings": manifest.get("identity_bindings", []),
            "scope": manifest["scope"],
        }
        (stage / "manifest.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
        # Immutable source list retained with original paths for offline replay.
        (stage / "source-index.json").write_bytes(manifest_raw)
        if output.exists():
            raise FileExistsError("candidate destination appeared during preparation")
        stage.rename(output)
        return report
    except BaseException:
        if stage.exists():
            shutil.rmtree(stage)
        raise
    finally:
        lock.unlink()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--sources", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = prepare(args.cli.resolve(), args.sources.resolve(), args.output.resolve())
    print(json.dumps({k: result[k] for k in ("normalization", "research_admitted", "counts", "issues", "securities", "share_universe_daily_keys_equal")}, ensure_ascii=False))
