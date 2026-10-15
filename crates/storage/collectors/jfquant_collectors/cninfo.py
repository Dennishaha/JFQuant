"""CNINFO is the primary dividend source; Sina supplies explicit trading/listing facts."""

from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

import requests

SOURCE = "akshare.cninfo/implemented/per-ten-shares-v1"
ENDPOINT = "https://webapi.cninfo.com.cn/api/sysapi/p_sysapi1139"


def fetch(symbol, request_call, *, include_decimal_rows=False):
    """Use the pinned AKShare client, but validate raw completeness before its coercions."""
    import akshare as ak

    responses = []
    decimal_responses = []
    send = requests.Session.send

    def observe(session, request, **kwargs):
        if kwargs.get("timeout") is None:
            kwargs["timeout"] = (10, 30)
        response = send(session, request, **kwargs)
        u = urlsplit(response.url)
        if u.hostname == "webapi.cninfo.com.cn" and u.path == "/api/sysapi/p_sysapi1139":
            if u.scheme != "https" or parse_qs(u.query).get("scode") != [symbol[2:]]:
                raise ValueError("cninfo_security_mismatch")
            response.raise_for_status()
            responses.append(response.json())
            decimal_responses.append(response.json(parse_float=str))
        return response

    with patch.object(requests.Session, "send", observe):
        try:
            request_call(ak.stock_dividend_cninfo, symbol=symbol[2:])
        except KeyError:
            # AKShare indexes missing columns for a genuine empty table. The raw
            # success/count checks below distinguish it from an invalid response.
            if not responses or responses[-1].get("records") != []:
                raise
    if not responses:
        raise ValueError("cninfo_response_missing")
    raw = responses[-1]
    rows = raw.get("records")
    if (
        raw.get("resultcode") != 200
        or not isinstance(rows, list)
        or type(raw.get("total")) is not int
        or type(raw.get("count")) is not int
        or raw["total"] != raw["count"]
        or raw["count"] != len(rows)
    ):
        raise ValueError("cninfo_incomplete_response")
    if include_decimal_rows:
        precise_rows = []
        for row, literal in zip(rows, decimal_responses[-1]["records"], strict=True):
            precise_rows.append(
                {
                    **row,
                    **{
                        field: literal[field]
                        for field in ("F012N", "F010N", "F011N")
                        if isinstance(row.get(field), float)
                    },
                }
            )
        return raw, precise_rows
    return raw


def parse(symbol, rows, start, end, sina_rows):
    from .reconcile import reconcile

    result = reconcile(symbol, rows, start, end, sina_rows)
    return result["records"], result["issues"]


def collect(request, report):
    import akshare as ak
    import pandas as pd

    from .adjustment import fetch as factors
    from .providers import identity, request_call

    def rows(table):
        return table.astype(object).where(pd.notna(table), None).to_dict("records")

    for symbol in request["instruments"]:
        identity(symbol)
        report("progress", message="巨潮分红资料 " + symbol)
        raw, precise_rows = fetch(symbol, request_call, include_decimal_rows=True)
        supplemental = []
        capabilities = {"cninfo": {"status": "available"}}
        try:
            supplemental = rows(
                request_call(ak.stock_history_dividend_detail, symbol=symbol[2:], indicator="分红")
            )
        except Exception:  # noqa: BLE001 - primary source stays usable; missing listing remains null
            capabilities["sina_listing"] = {"status": "unavailable"}
        try:
            rights = rows(
                request_call(ak.stock_history_dividend_detail, symbol=symbol[2:], indicator="配股")
            )
        except Exception:  # noqa: BLE001 - unknown special coverage is a warning, not a fabricated no-event claim
            rights = []
            capabilities["sina_rights"] = {"status": "unavailable"}
        from .reconcile import reconcile

        reconciliation = reconcile(
            symbol, precise_rows, request["start"], request["end"], supplemental, rights=rights
        )
        records, issues = reconciliation["records"], reconciliation["issues"]
        adjustment = factors(symbol, request["end"], report, request_call)
        points = adjustment["basis"]["points"]
        coefficients = {p["effective_date"]: float(p["factor"]) for p in points}
        capabilities["independent_sina_factor"] = {
            "status": "available",
            "verified_through": request["end"],
        }
        report(
            "action_shard",
            instrument=symbol,
            records=records,
            history_verified=True,
            execution_issues=issues,
            execution_data={
                "schema_version": 1,
                "suspensions": {},
                "limits": {},
                "coefficients": {symbol: coefficients},
                "capabilities": capabilities,
            },
            evidence={
                "kind": "structured_api_response",
                "provider": "akshare",
                "source_rule": SOURCE,
                "history_complete": True,
                "request_range": {"start": request["start"], "end": request["end"]},
                "cninfo": raw,
                "cninfo_decimal_records": precise_rows,
                "sina_dividends": supplemental,
                "sina_rights": rights,
                "adjustment": adjustment,
                "distribution_unit": "per_ten_shares",
                "reconciliation": reconciliation,
            },
        )
    report("complete")
