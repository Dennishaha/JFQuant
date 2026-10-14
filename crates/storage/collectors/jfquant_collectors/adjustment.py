"""Fixed Sina factor endpoint and safe data-only decoding; never execute response text."""

import datetime as dt
import json
import math
import re
import time
from decimal import Decimal, InvalidOperation

LAST_REQUEST = 0.0


def endpoint(symbol):
    if not re.fullmatch(r"(?:SH|SZ|BJ)[0-9]{6}", symbol):
        raise ValueError("invalid adjustment security")
    return "https://finance.sina.com.cn/realstock/company/" + symbol.lower() + "/hfq.js"


def unique_object(pairs):
    value = {}
    for key, entry in pairs:
        if key in value:
            raise ValueError("duplicate adjustment JSON key")
        value[key] = entry
    return value


def invalid_constant(value):
    raise ValueError("non-JSON adjustment constant: " + value)


def parse_response(symbol, text, verified_through):
    """Accept the exact variable assignment, one JSON object, and inert trailing comments."""
    endpoint(symbol)
    prefix = re.match(r"\s*var\s+" + symbol.lower() + r"hfq\s*=\s*", text)
    if prefix is None:
        raise ValueError("adjustment response security mismatch")
    body = text[prefix.end() :]
    value, length = json.JSONDecoder(
        object_pairs_hook=unique_object, parse_constant=invalid_constant
    ).raw_decode(body)
    tail = body[length:].strip()
    if tail.startswith(";"):
        tail = tail[1:].lstrip()
    if not re.fullmatch(r"(?:\s|/\*(?:(?!\*/)[\s\S])*\*/|//[^\n]*(?:\n|$))*", tail):
        raise ValueError("executable or invalid trailing adjustment response")
    if not isinstance(value, dict) or not isinstance(value.get("data"), list) or not value["data"]:
        raise ValueError("empty/missing adjustment history")
    if type(value.get("total")) is not int or value["total"] != len(value["data"]):
        raise ValueError("truncated/mismatched adjustment history count")
    points, seen = [], set()
    for row in value["data"]:
        if (
            not isinstance(row, dict)
            or set(row) != {"d", "f"}
            or not isinstance(row["f"], str)
            or not isinstance(row["d"], str)
            or not re.fullmatch(r"[0-9]{4}-[0-9]{2}-[0-9]{2}", row["d"])
        ):
            raise ValueError("invalid adjustment record")
        day = dt.date.fromisoformat(row["d"]).isoformat()
        if day in seen:
            raise ValueError("duplicate adjustment effective date")
        seen.add(day)
        number = row["f"]
        if not re.fullmatch(r"\+?(?:[0-9]+(?:\.[0-9]*)?|\.[0-9]+)(?:[eE][+-]?[0-9]+)?", number):
            raise ValueError("invalid adjustment decimal")
        try:
            decimal = Decimal(number)
            floating = float(decimal)
        except (InvalidOperation, OverflowError, ValueError) as error:
            raise ValueError("invalid adjustment coefficient") from error
        if not decimal.is_finite() or decimal <= 0 or not math.isfinite(floating) or floating <= 0:
            raise ValueError("adjustment coefficient must be positive and finite")
        points.append({"effective_date": day, "factor": number})
    points.sort(key=lambda p: p["effective_date"])
    return {
        "schema_version": 3,
        "instrument_id": symbol,
        "source_symbol": symbol,
        "basis": {
            "kind": "explicit",
            "source": "sina_hfq",
            "source_url": endpoint(symbol),
            "verified_through": dt.date.fromisoformat(verified_through).isoformat(),
            "points": points,
        },
    }


def fetch(symbol, end, report, request_call):
    """Enforce one-second spacing between every attempt, with bounded existing retries."""
    import requests

    def request():
        global LAST_REQUEST
        delay = max(0.0, 1.0 - (time.monotonic() - LAST_REQUEST))
        if delay:
            report("progress", message="新浪复权因子限速等待 " + symbol)
            time.sleep(delay)
        LAST_REQUEST = time.monotonic()
        response = requests.get(endpoint(symbol), timeout=30, allow_redirects=False)
        response.raise_for_status()
        if response.status_code != 200:
            raise ValueError("unexpected adjustment status or redirect")
        if len(response.content) > 1024 * 1024:
            raise ValueError("adjustment response too large")
        return response.text

    report("progress", message="独立复权因子 " + symbol)
    return parse_response(symbol, request_call(request), end)
