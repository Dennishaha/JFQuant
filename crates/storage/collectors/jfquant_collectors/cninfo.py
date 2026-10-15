"""CNINFO is the primary dividend source; Sina supplies explicit trading/listing facts."""

import datetime as dt
from decimal import Decimal, InvalidOperation
from unittest.mock import patch
from urllib.parse import parse_qs, urlsplit

import requests

SOURCE = "akshare.cninfo/implemented/per-ten-shares-v1"
ENDPOINT = "https://webapi.cninfo.com.cn/api/sysapi/p_sysapi1139"


def fetch(symbol, request_call):
    """Use the pinned AKShare client, but validate raw completeness before its coercions."""
    import akshare as ak

    responses = []
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
    return raw


def day(value):
    if value in (None, "", "--"):
        return None
    return dt.date.fromisoformat(str(value)[:10]).isoformat()


def amount(value, description, keyword):
    if value in (None, "", "--"):
        # CNINFO omits non-applicable components. The original plan text must
        # explicitly describe a distribution and must not mention that component.
        if (
            description
            and any(k in description for k in ("派", "送", "转"))
            and keyword not in description
        ):
            return Decimal(0)
        raise ValueError("cninfo_amount_missing")
    try:
        v = Decimal(str(value))
        if not v.is_finite() or v < 0:
            raise InvalidOperation
        return v
    except InvalidOperation:
        raise ValueError("cninfo_amount_invalid") from None


def parse(symbol, rows, start, end, sina_rows):
    records, issues, seen = [], [], {}

    def issue(code, date, announced, message):
        issues.append(
            {
                "instrument": symbol,
                "announced": announced,
                "dates": {"start": date or max(start, announced or start), "end": date or end},
                "code": code,
                "message": message,
            }
        )

    sina = {}
    for r in sina_rows:
        if r.get("进度") == "实施":
            ex = day(r.get("除权除息日"))
            if ex:
                if ex in sina and sina[ex] != r:
                    issue(
                        "source_event_conflict", ex, day(r.get("公告日期")), "新浪同日分红记录冲突"
                    )
                sina[ex] = r
    for r in rows:
        ex = day(r.get("F020D"))
        announced = day(r.get("F006D"))
        if ex and not start <= ex <= end:
            continue
        try:
            record = day(r.get("F018D"))
            description = r.get("F007V") or ""
            cash = amount(r.get("F012N"), description, "派") / 10
            shares = (
                amount(r.get("F010N"), description, "送")
                + amount(r.get("F011N"), description, "转")
            ) / 10
            if cash + shares == 0:
                continue
            if not ex or not record or not announced:
                raise ValueError("cninfo_event_date_missing")
            if announced > record or record >= ex:
                raise ValueError("cninfo_event_chronology_conflict")
            listing = None
            s = sina.get(ex)
            if s:
                scash = Decimal(str(s["派息"])) / 10
                sshares = (Decimal(str(s["送股"])) + Decimal(str(s["转增"]))) / 10
                if (
                    abs(scash - cash) > Decimal("0.000001")
                    or abs(sshares - shares) > Decimal("0.000001")
                    or day(s.get("股权登记日")) != record
                ):
                    raise ValueError("source_event_conflict")
                listing = day(s.get("红股上市日")) if shares > 0 else None
            a = {
                "id": symbol + ":" + ex,
                "instrument": symbol,
                "announced": announced,
                "record_date": record,
                "ex_date": ex,
                "pay_date": day(r.get("F023D")),
                "listing_date": listing,
                "cash_per_share": float(cash),
                "shares_per_share": float(shares),
                "source": ENDPOINT + "?scode=" + symbol[2:],
            }
            if ex in seen and seen[ex] != a:
                raise ValueError("source_event_conflict")
            if ex not in seen:
                seen[ex] = a
                records.append(a)
        except (ValueError, InvalidOperation, KeyError, TypeError) as error:
            code = str(error) if isinstance(error, ValueError) else "cninfo_event_parse_error"
            issue(code, ex, announced, "巨潮实施记录缺项或与新浪记录冲突；保留原始响应")
    # A nonempty independent history cannot be erased by a CNINFO empty/short reply.
    for ex, r in sina.items():
        if start <= ex <= end and ex not in seen:
            issue(
                "cninfo_history_gap",
                ex,
                day(r.get("公告日期")),
                "新浪有实施分红，巨潮未返回对应有效记录",
            )
    return records, issues


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
        raw = fetch(symbol, request_call)
        supplemental = []
        capabilities = {"cninfo": {"status": "available"}}
        try:
            supplemental = rows(
                request_call(ak.stock_history_dividend_detail, symbol=symbol[2:], indicator="分红")
            )
        except Exception:  # noqa: BLE001 - primary source stays usable; missing listing remains null
            capabilities["sina_listing"] = {"status": "unavailable"}
        records, issues = parse(
            symbol, raw["records"], request["start"], request["end"], supplemental
        )
        try:
            rights = rows(
                request_call(ak.stock_history_dividend_detail, symbol=symbol[2:], indicator="配股")
            )
            for r in rights:
                ex = day(r.get("除权日"))
                if ex and request["start"] <= ex <= request["end"]:
                    issues.append(
                        {
                            "instrument": symbol,
                            "announced": day(r.get("公告日期")),
                            "dates": {"start": day(r.get("股权登记日")) or ex, "end": ex},
                            "code": "rights_issue_candidate_unresolved",
                            "message": "配股须核实认购、交付及上市资料",
                        }
                    )
        except Exception:  # noqa: BLE001 - unknown special coverage is a warning, not a fabricated no-event claim
            rights = []
            capabilities["sina_rights"] = {"status": "unavailable"}
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
                "sina_dividends": supplemental,
                "sina_rights": rights,
                "adjustment": adjustment,
                "distribution_unit": "per_ten_shares",
            },
        )
    report("complete")
