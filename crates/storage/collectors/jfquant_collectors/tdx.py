"""Pinned TdxAiData boundary: preserve native replies before SDK coercion or filling."""

import contextlib
import ctypes
import datetime as dt
import hashlib
import importlib.metadata
import json
import math
import os
import shutil
import time
from decimal import Decimal, localcontext
from pathlib import Path
from types import MethodType

VERSION = "1.2.2"
SOURCE = "tdxaidata.get_market_data/none-forward-event-reconciled-v2"
PROCESS_KEY = None


def symbol(code):
    from .providers import identity

    identity(code)
    return code[2:] + "." + code[:2]


def number(value, positive=False):
    if value in (None, "", "--"):
        return None
    v = float(value)
    if not math.isfinite(v) or v < 0 or (positive and v <= 0):
        raise ValueError("tdx_invalid_numeric_value")
    return v


def payload(value):
    if not isinstance(value, dict):
        raise TypeError("tdx_invalid_native_response")
    error = value.get("ErrorId", value.get("error_code", 0))
    if str(error) not in ("0", "0.0") or "error" in value:
        # Never include arbitrary vendor error text, which can echo authentication.
        raise ValueError("tdx_request_failed:" + str(error)[:16])
    return value["Value"] if isinstance(value.get("Value"), dict) and "Date" not in value else value


def bars(code, value, start, end, price_index=False, require_factor=True):
    data = payload(value)
    dates = data.get("Date") if isinstance(data, dict) else None
    if not isinstance(dates, list) or not dates:
        raise ValueError("tdx_empty_history")
    fields = ["Open", "High", "Low", "Close", "Volume", "Amount"]
    if require_factor and not price_index:
        fields.append("ForwardFactor")
    if any(not isinstance(data.get(k), list) or len(data[k]) != len(dates) for k in fields):
        raise ValueError("tdx_truncated_fields")
    output, points, seen = [], [], set()
    for i, day in enumerate(dates):
        day = dt.datetime.strptime(str(day), "%Y%m%d").date().isoformat()
        if day in seen:
            raise ValueError("tdx_duplicate_date")
        seen.add(day)
        if not start <= day <= end:
            continue
        row = {"instrument_id": code, "trade_date": day}
        for f in fields[:4]:
            row[f.lower()] = number(data[f][i], positive=True)
            row["adj_" + f.lower()] = None
        row["volume"] = number(data["Volume"][i])
        amount = number(data["Amount"][i])
        row["amount"] = amount * 10000 if amount is not None else None
        if require_factor:
            factor = 1.0 if price_index else number(data["ForwardFactor"][i], positive=True)
            if factor is None:
                raise ValueError("tdx_adjustment_missing")
            points.append(
                {
                    "effective_date": day,
                    "factor": "1" if price_index else str(data["ForwardFactor"][i]),
                }
            )
        output.append(row)
    if not output:
        raise ValueError("tdx_requested_history_not_covered")
    if [r["trade_date"] for r in output] != sorted(r["trade_date"] for r in output):
        raise ValueError("tdx_unsorted_dates")
    return output, points


def merge_window(output, points, raw, native_points):
    """Rebase declared coefficients, never adjusted price ratios, onto the first raw day.

    Native coefficients are rounded to six decimals and use a request-dependent
    anchor. Five overlapping observations prove a single scale and unchanged OHLCV.
    """
    old = {r["trade_date"]: (r, points[i] if points else None) for i, r in enumerate(output)}
    shared = [(i, r, old[r["trade_date"]]) for i, r in enumerate(raw) if r["trade_date"] in old]
    if output and not shared:
        raise ValueError("tdx_cross_window_overlap_missing")
    if any(row != previous[0] for _, row, previous in shared):
        raise ValueError("tdx_cross_window_conflict")
    with localcontext() as decimal_context:
        decimal_context.prec = 34
        scale = Decimal(1)
        if native_points:
            if shared:
                ratios = [
                    Decimal(previous[1]["factor"]) / Decimal(native_points[i]["factor"])
                    for i, _, previous in shared
                ]
                # Use the last common observation to preserve the exact join at
                # the next new row; a median can create a rounding-only jump
                # when the overlap straddles a real dividend date.
                scale = ratios[-1]
                if any(abs(r / scale - 1) > Decimal("0.00001") for r in ratios):
                    raise ValueError("tdx_cross_window_factor_shape_conflict")
            else:
                scale = Decimal(1) / Decimal(native_points[0]["factor"])
        rebased = [dict(p, factor=str(Decimal(p["factor"]) * scale)) for p in native_points]
    output.extend(r for r in raw if r["trade_date"] not in old)
    points.extend(p for p in rebased if p["effective_date"] not in old)
    return {
        "first": raw[0]["trade_date"],
        "last": raw[-1]["trade_date"],
        "overlap_dates": [r["trade_date"] for _, r, _ in shared],
        "native_scale": str(scale),
    }


def verify_events(raw, points, events):
    """Reconcile explicit coefficient changes against native event terms, not QFQ prices.

    Keep the existing 0.2% event threshold. Settlement dates are deliberately not
    needed here. Ordinary Type=1 rights terms use the same per-ten-share units;
    this validates only the ex-reference, not subscription or delivery eligibility.
    """
    proof = []
    if len(raw) != len(points):
        raise ValueError("tdx_coefficient_alignment_conflict")
    for i in range(1, len(raw)):
        previous, current = raw[i - 1], raw[i]
        actual = Decimal(points[i]["factor"]) / Decimal(points[i - 1]["factor"])
        applicable = [
            e for e in events if previous["trade_date"] < e["date"] <= current["trade_date"]
        ]
        if not applicable:
            if abs(actual - 1) > Decimal("0.00001"):
                raise ValueError("tdx_factor_event_missing:" + current["trade_date"])
            continue
        if len(applicable) != 1:
            raise ValueError("tdx_compound_event_requires_review:" + current["trade_date"])
        event = applicable[0]
        if event["type"] != "1":
            raise ValueError("tdx_special_action_requires_review:" + event["date"])
        if previous["close"] is None:
            raise ValueError("tdx_event_reference_price_missing:" + event["date"])
        close = Decimal(str(previous["close"]))
        cash = Decimal(event["bonus_per_ten_shares"]) / 10
        shares = Decimal(event["share_bonus_native"]) / 10
        rights = Decimal(event["allotment_native"]) / 10
        rights_price = Decimal(event["allot_price"])
        reference_numerator = close - cash + rights * rights_price
        if reference_numerator <= 0:
            raise ValueError("tdx_event_reference_price_invalid:" + event["date"])
        expected = close * (1 + shares + rights) / reference_numerator
        if abs(actual / expected - 1) > Decimal("0.002"):
            raise ValueError("tdx_factor_event_conflict:" + event["date"])
        proof.append(
            {
                "event_date": event["date"],
                "effective_date": current["trade_date"],
                "reference_date": previous["trade_date"],
                "previous_raw_close": str(close),
                "cash_per_share": str(cash),
                "shares_per_share": str(shares),
                "rights_per_share": str(rights),
                "rights_price": str(rights_price),
                "native_ratio": str(actual),
                "expected_ratio": str(expected),
            }
        )
    return proof


@contextlib.contextmanager
def session(request, report):
    """Only this worker changes directory. Never write a Key into the signed package."""
    if importlib.metadata.version("tdxaidata") != VERSION:
        raise ValueError("tdx_sdk_version_mismatch")
    global PROCESS_KEY
    if PROCESS_KEY is None:
        PROCESS_KEY = os.environ.pop("JFQUANT_TDX_KEY", "")
    key = PROCESS_KEY
    if not key:
        raise ValueError("credential_missing: configure tdxaidata")
    directory = Path(request["cache_dir"]).resolve().parent / ".tdx-runtime"
    directory.mkdir(mode=0o700, exist_ok=True)
    os.chmod(directory, 0o700)
    package = Path(importlib.metadata.distribution("tdxaidata").locate_file("tdxaidata/lib"))
    library = directory / "libTdxAiData.dylib"
    shutil.copyfile(package / library.name, library)
    os.chmod(library, 0o500)
    (directory / "TdxAiData.ini").write_text(
        "[TcHqHost]\nHostNum=2\nPrimaryHost=0\nHostName01=aihs\n"
        "IPAddress01=aihs.tdx.com.cn\nPort01=7709\nHostName02=aihs2\n"
        "IPAddress02=aihs2.tdx.com.cn\nPort02=7709\n[TcDsHost]\nHostNum=1\n"
        "HostName01=aids\nIPAddress01=aids.tdx.com.cn\nPort01=7727\n[Token]\ntoken=" + key + "\n"
    )
    os.chmod(directory / "TdxAiData.ini", 0o600)
    os.environ["TDX_AI_DATA_LIB"] = str(library)
    from tdxaidata import tqs

    native = tqs._tdx()
    original = native._call_with_free
    receipts = []
    responses = []
    raw_root = Path(request["cache_dir"]).resolve().parent / "raw"
    old = Path.cwd()

    def capture(self, function, *args):
        self._ensure_started()
        if self._dll is None or not self._is_started:
            raise ValueError("tdx_connection_or_authentication_failed")
        pointer = ctypes.c_char_p()
        function(*args, ctypes.byref(pointer))
        try:
            if not pointer.value:
                raise ValueError("tdx_empty_native_reply")
            body = pointer.value
            if len(body) > 32 * 1024 * 1024:
                raise ValueError("tdx_native_reply_too_large")
            if key.encode() in body:
                raise ValueError("tdx_response_contains_credential")
            # Decode strictly, before the SDK's errors=ignore and missing-value padding.
            text = body.decode("gb18030", errors="strict")
            value = json.loads(text)
            sha = hashlib.sha256(body).hexdigest()
            name = "sdk-" + sha + ".body"
            (raw_root / name).write_bytes(body)
            receipts.append(
                {
                    "kind": "sdk_native_response",
                    "sdk": "tdxaidata",
                    "version": VERSION,
                    "function": function.__name__,
                    "arguments": [a.decode("ascii") if isinstance(a, bytes) else a for a in args],
                    "filename": name,
                    "sha256": "sha256:" + sha,
                }
            )
            # Keep call metadata even when parsing/validation later rejects the
            # shard. It accounts for failed probes without exposing SDK secrets.
            with (raw_root / "sdk-requests.jsonl").open("a", encoding="utf-8") as journal:
                journal.write(json.dumps(receipts[-1], ensure_ascii=True) + "\n")
            responses.append(value)
            payload(value)
            return text
        finally:
            if pointer.value:
                self._dll.FreeResult(pointer)

    native._call_with_free = MethodType(capture, native)
    try:
        native._ensure_started()
        if native._dll is None or not native._is_started:
            raise ValueError("tdx_connection_or_authentication_failed")
        yield tqs, receipts, responses
    finally:
        native._call_with_free = original
        if native._is_started:
            native._dll.stop()
        native._is_started = False
        os.chdir(old)
        shutil.rmtree(directory, ignore_errors=True)


def collect(request, report):
    """One security per supervised shard; yearly windows bound native reply sizes."""
    start, end = request["start"], request["end"]
    now = dt.datetime.now(dt.timezone(dt.timedelta(hours=8)))
    closed = (now.date() if now.hour >= 16 else now.date() - dt.timedelta(days=1)).isoformat()
    if end > closed:
        raise ValueError("tdx_requested_date_not_complete")
    with session(request, report) as (tqs, receipts, responses):
        before = len(responses)
        calendar = (
            []
            if request["operation"] == "actions"
            else tqs.get_trading_dates("SH", start, end, count=-1)
        )
        if request["operation"] != "actions" and (
            len(responses) == before or not isinstance(calendar, list) or not calendar
        ):
            raise ValueError("tdx_calendar_unavailable")
        calendar = sorted(
            dt.datetime.strptime(str(d), "%Y%m%d").date().isoformat() for d in calendar
        )
        if len(calendar) != len(set(calendar)) or any(d < start or d > end for d in calendar):
            raise ValueError("tdx_calendar_invalid")
        universes = {}
        for name in request.get("universes", []):
            if name not in ("csi300", "csi500"):
                raise ValueError("unsupported_universe")
            values = tqs.get_zzgz_stocklist("000300.SH" if name == "csi300" else "000905.SH", 0)
            universes[name] = constituents(values, now.date().isoformat())
        for code in request["instruments"]:
            output, points, window_proofs = [], [], []
            if request["operation"] == "actions":
                tqs.get_divid_factors(symbol(code), start, end)
                candidates = action_references(responses[-1], start, end)
                rights_fields = {}
                for field in ("GP41", "GP43"):
                    before = len(responses)
                    try:
                        tqs.get_gpjy_value([symbol(code)], [field], start, end)
                        rights_fields[field] = "captured" if len(responses) > before else "unavailable"
                    except (ValueError, TypeError, OSError, RuntimeError, AttributeError, KeyError, IndexError, ctypes.ArgumentError):
                        rights_fields[field] = "failed"
                # Status is auxiliary evidence; unavailable status must not erase
                # a successfully captured ordinary-action history.
                status = {"status": "unavailable"}
                try:
                    calendar_start = (dt.date.fromisoformat(start) - dt.timedelta(days=3660)).isoformat()
                    tqs.get_trading_dates("SH", calendar_start, end, count=-1)
                    tqs.get_gpjy_value([symbol(code)], ["GP29"], start, end)
                    status = {"status": "captured", "rule": "tdxaidata.gp29-resumption-v1", "calendar_start": calendar_start}
                except (ValueError, TypeError, OSError, RuntimeError, AttributeError, KeyError, IndexError, ctypes.ArgumentError):
                    status = {"status": "unavailable"}
                # A dated current suspension can prove only the latest ongoing
                # episode retrospectively, including episodes without a resume.
                try:
                    tqs.get_more_info(symbol(code), field_list=["TPFlag", "HqDate", "StopJYDate_Recent"])
                    status["current_snapshot"] = "captured"
                except (ValueError, TypeError, OSError, RuntimeError, AttributeError, KeyError, IndexError, ctypes.ArgumentError):
                    status["current_snapshot"] = "unavailable"
                evidence = {
                    "provider": "tdxaidata",
                    "sdk_version": VERSION,
                    "sdk_receipts": receipts[:],
                    "source_rule": "tdxaidata.get_divid_factors/reference-v1",
                    "stock_status": status,
                    "rights_fields": rights_fields,
                    "acquired_at": dt.datetime.now(dt.UTC).isoformat(),
                    "request_range": {"start": start, "end": end},
                    "candidates": candidates,
                    "history_complete": False,
                }
                report(
                    "action_shard",
                    instrument=code,
                    records=[],
                    history_verified=False,
                    failure="tdx_settlement_dates_need_supplement",
                    evidence=evidence,
                )
                receipts.clear()
                continue
            tqs.get_stock_info(symbol(code), field_list=["J_start", "HSStockKind"])
            info = payload(responses[-1])
            price_index = code in request.get("benchmarks", [])
            if price_index and str(info.get("HSStockKind")) != "0":
                raise ValueError("tdx_index_kind_unverified")
            listed = info.get("J_start") if isinstance(info, dict) else None
            if str(listed) in ("0", "00000000", "", "None") or price_index:
                listed = None
            listed = (
                dt.datetime.strptime(str(listed), "%Y%m%d").date().isoformat() if listed else None
            )
            first_year = int(start[:4])
            for year in range(first_year, int(end[:4]) + 1):
                lo, hi = max(start, f"{year}-01-01"), min(end, f"{year}-12-31")
                if listed:
                    lo = max(lo, listed)
                if lo > hi or not any(lo <= d <= hi for d in calendar):
                    continue
                # Re-read the previous observed trading date in the next window.
                # A factor whose anchor changes with the requested window cannot be stitched.
                if output:
                    lo = min(lo, output[-min(5, len(output))]["trade_date"])
                report("progress", message=f"通达信日线 {code} {year}")
                before = len(responses)
                tqs.get_market_data(
                    stock_list=[symbol(code)],
                    period="1d",
                    start_time=lo,
                    end_time=hi,
                    count=-1,
                    dividend_type="none",
                    fill_data=False,
                )
                if len(responses) == before:
                    raise ValueError("tdx_native_receipt_missing")
                raw, ps = bars(
                    code,
                    responses[-1],
                    lo,
                    hi,
                    price_index=price_index,
                    require_factor=request["operation"] != "quotes",
                )
                window_proofs.append(merge_window(output, points, raw, ps))
                time.sleep(0.25)
            if not output:
                raise ValueError("tdx_empty_history")
            event_proofs = []
            if not price_index and request["operation"] != "quotes":
                tqs.get_divid_factors(symbol(code), start, end)
                events = action_references(responses[-1], start, end)
                event_proofs = verify_events(output, points, events)
            basis = (
                {"kind": "price_index", "source": "tdxaidata.get_market_data"}
                if code in request.get("benchmarks", [])
                else {
                    "kind": "explicit",
                    "source": "tdx_forward",
                    "source_url": "tdxaidata:get_market_data:none:ForwardFactor",
                    "verified_through": end,
                    "points": points,
                }
            )
            if request["operation"] == "quotes":
                basis = {"kind": "unavailable", "reason": "independent coefficients not collected"}
            adjustment = {
                "schema_version": 3,
                "instrument_id": code,
                "source_symbol": code,
                "basis": basis,
            }
            evidence = {
                "kind": "sdk_native_responses",
                "provider": "tdxaidata",
                "sdk_version": VERSION,
                "source_rule": SOURCE,
                "request_range": {"start": start, "end": end},
                "sdk_receipts": receipts[:],
                "price_rule_verified": request["operation"] != "quotes",
                "first_raw_date": output[0]["trade_date"],
                "coefficient_transform": "native-explicit-first-raw-anchor-v2",
                "window_proofs": window_proofs,
                "event_proofs": event_proofs,
                "units": {"price": "CNY", "volume": "shares", "amount": "10000 CNY"},
            }
            if request["operation"] == "coefficients":
                report(
                    "adjustment_shard", instrument=code, adjustment=adjustment, evidence=evidence
                )
            else:
                report(
                    "shard", instrument=code, bars=output, adjustment=adjustment, evidence=evidence
                )
            receipts.clear()
        report("complete", calendar=calendar, universes=universes)


def check(request, report):
    """A connection check is deliberately bounded; it does not start a full download."""
    collect(request, report)


def action_references(value, start, end):
    """Native dividend reference only; never synthesize settlement dates or pad fields."""
    data = payload(value)
    # A successful native empty array has no dividend references. It does not
    # establish account coverage, and must not suppress an independent GP29 query.
    if data.get("Value") == [] and "Date" not in data and "Type" not in data:
        return []
    dates, kinds, values = (data.get(k) for k in ("Date", "Type", "Value"))
    if not all(isinstance(v, list) for v in (dates, kinds, values)) or not len(dates) == len(
        kinds
    ) == len(values):
        raise ValueError("tdx_truncated_actions")
    out, seen = [], set()
    for date, kind, fields in zip(dates, kinds, values, strict=True):
        day = dt.datetime.strptime(str(date), "%Y%m%d").date().isoformat()
        if not start <= day <= end:
            continue
        if not isinstance(fields, list) or len(fields) != 4:
            raise ValueError("tdx_action_fields_missing")
        parsed = [number(v) for v in fields]
        if any(v is None for v in parsed):
            raise ValueError("tdx_action_fields_missing")
        key = (day, str(kind))
        if key in seen:
            raise ValueError("tdx_duplicate_event")
        seen.add(key)
        out.append(
            {
                "date": day,
                "type": str(kind),
                "bonus_per_ten_shares": str(fields[0]),
                "allot_price": str(fields[1]),
                "share_bonus_native": str(fields[2]),
                "allotment_native": str(fields[3]),
                "account_dates": "requires_supplement",
                "ratio_unit": "per_ten_shares" if str(kind) == "1" else "special_requires_review",
            }
        )
    return out


def constituents(values, observed):
    """A current list certifies one observation date, never the preceding history."""
    if not isinstance(values, list) or not values:
        raise ValueError("tdx_empty_members")
    intervals = {}
    for code in values:
        if not isinstance(code, str) or len(code) != 9 or code[6] != ".":
            raise ValueError("tdx_invalid_member_code")
        canonical = code[7:] + code[:6]
        symbol(canonical)
        if canonical in intervals:
            raise ValueError("tdx_duplicate_member")
        intervals[canonical] = [{"start": observed, "end": observed}]
    return {"coverage": {"start": observed, "end": observed}, "intervals": intervals}


def reference(request, report):
    """Small fixed reference endpoints, independent of quote/factor acquisition."""

    operation = request["operation"]
    provider = request["provider"]
    calendar, universes, receipts = [], {}, []
    if provider == "tdxaidata":
        with session(request, report) as (tqs, native_receipts, _):
            if operation == "calendar":
                dates = tqs.get_trading_dates("SH", request["start"], request["end"], -1)
                if not isinstance(dates, list) or not dates:
                    raise ValueError("tdx_calendar_unavailable")
                calendar = [
                    dt.datetime.strptime(str(d), "%Y%m%d").date().isoformat() for d in dates
                ]
            else:
                observed = dt.datetime.now(dt.timezone(dt.timedelta(hours=8))).date().isoformat()
                for name in request["universes"]:
                    if name not in ("csi300", "csi500"):
                        raise ValueError("unsupported_universe")
                    values = tqs.get_zzgz_stocklist(
                        "000300.SH" if name == "csi300" else "000905.SH", 0
                    )
                    universes[name] = constituents(values, observed)
            receipts = native_receipts[:]
    else:
        raise ValueError("reference_provider_unsupported")
    if operation == "calendar" and (
        not calendar
        or calendar != sorted(set(calendar))
        or any(not request["start"] <= d <= request["end"] for d in calendar)
    ):
        raise ValueError("invalid_reference_calendar")
    evidence = {
        "provider": provider,
        "sdk_version": VERSION if provider == "tdxaidata" else None,
        "sdk_receipts": receipts,
        "source_rule": provider + "." + operation,
        "request_range": {"start": request["start"], "end": request["end"]},
    }
    report(
        "action_shard",
        instrument=request["instruments"][0],
        reference={"calendar": calendar, "universes": universes},
        evidence=evidence,
    )
    report("complete", calendar=calendar, universes=universes)
