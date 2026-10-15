"""Fixed HTTPS APIs only. Credential and request bodies are never evidence."""

import datetime as dt
import os
import time
from decimal import Decimal, InvalidOperation

import requests

from .providers import identity

ENDPOINT = "https://api.tushare.pro"
FIELDS = {
    "dividend": "ts_code,end_date,ann_date,div_proc,stk_div,stk_bo_rate,stk_co_rate,cash_div_tax,record_date,ex_date,pay_date,div_listdate,imp_ann_date",
    "suspend_d": "ts_code,trade_date,suspend_type,suspend_timing",
    "stk_limit": "ts_code,trade_date,up_limit,down_limit",
    "adj_factor": "ts_code,trade_date,adj_factor",
    "anns_d": "ts_code,ann_date,title,url,rec_time",
}
DENIED = set()
LAST_REQUEST = 0.0
PROCESS_TOKEN = None
LIMITS = {
    "dividend": 2000,
    "suspend_d": 5000,
    "stk_limit": 5800,
    "adj_factor": 5000,
    "anns_d": 2000,
}


class SourceError(ValueError):
    """Stable errors avoid echoing provider errors, tokens or authenticated requests."""


class Api:
    def __init__(self, token=None, session=None, sleep=time.sleep):
        global PROCESS_TOKEN
        if token is None and PROCESS_TOKEN is None:
            PROCESS_TOKEN = os.environ.pop("JFQUANT_TUSHARE_TOKEN", "")
        self.token = token or PROCESS_TOKEN
        if not self.token:
            raise SourceError("credential_missing")
        self.session = session or requests.Session()
        self.sleep = sleep
        self.last = 0.0

    def call(self, name, params):
        if name not in FIELDS:
            raise SourceError("unsupported_tushare_api")
        global LAST_REQUEST
        if name in DENIED:
            raise SourceError("permission_denied:" + name)
        for attempt in range(3):
            self.sleep(max(0, 1.0 - (time.monotonic() - LAST_REQUEST)))
            LAST_REQUEST = time.monotonic()
            try:
                r = self.session.post(
                    ENDPOINT,
                    json={
                        "api_name": name,
                        "token": self.token,
                        "params": params,
                        "fields": FIELDS[name],
                    },
                    timeout=(10, 45),
                    allow_redirects=False,
                )
                if r.status_code == 429 or r.status_code >= 500:
                    raise SourceError("source_rate_or_server_error")
                if r.status_code != 200:
                    raise SourceError("source_http_error")
                v = r.json()
                code = v.get("code")
                if code in (2002, 40203) and "频率超限" in str(v.get("msg", "")):
                    raise SourceError("source_rate_limited:" + name)
                if code in (2002, 40203):
                    DENIED.add(name)
                    raise SourceError("permission_denied:" + name)
                if code != 0:
                    # Do not forward provider msg; it can contain request details.
                    raise SourceError("source_api_error:" + str(code))
                data = v.get("data")
                if not isinstance(data, dict) or not isinstance(data.get("items"), list):
                    raise SourceError("source_parse_error")
                fields = data.get("fields")
                if not isinstance(fields, list) or not set(FIELDS[name].split(",")).issubset(
                    fields
                ):
                    raise SourceError("source_fields_missing")
                rows = []
                for row in data["items"]:
                    if len(row) != len(fields):
                        raise SourceError("source_parse_error")
                    rows.append(dict(zip(fields, row, strict=True)))
                return rows
            except (requests.RequestException, ValueError, TypeError, KeyError) as error:
                if isinstance(error, SourceError) and str(error).startswith("permission_denied"):
                    raise
                if attempt == 2:
                    if isinstance(error, SourceError):
                        raise
                    raise SourceError("source_transport_or_parse_error") from None
                self.sleep(2**attempt)
        raise SourceError("source_failed")

    def ranged(self, name, code, start, end):
        """Split bounded date queries; a saturated single day fails instead of truncating."""
        params = {
            "ts_code": code,
            "start_date": start.replace("-", ""),
            "end_date": end.replace("-", ""),
        }
        rows = self.call(name, params)
        if len(rows) >= LIMITS[name]:
            lo, hi = dt.date.fromisoformat(start), dt.date.fromisoformat(end)
            if lo == hi:
                raise SourceError("source_truncated:" + name)
            mid = lo + (hi - lo) // 2
            return self.ranged(name, code, start, mid.isoformat()) + self.ranged(
                name, code, (mid + dt.timedelta(days=1)).isoformat(), end
            )
        return rows


def day(v):
    if v in (None, "", "None"):
        return None
    text = str(v)
    try:
        return dt.datetime.strptime(text, "%Y%m%d").date().isoformat()
    except ValueError:
        raise SourceError("invalid_event_date") from None


def amount(v):
    try:
        n = Decimal(str(v))
        if not n.is_finite() or n < 0:
            raise InvalidOperation
        return float(n)
    except (InvalidOperation, TypeError, ValueError):
        raise SourceError("invalid_event_amount") from None


def dated(rows, value):
    """No last-row-wins policy for conflicting daily execution data."""
    out = {}
    for r in rows:
        d = day(r["trade_date"])
        if d is None:
            raise SourceError("source_date_missing")
        v = value(r)
        if d in out and out[d] != v:
            raise SourceError("source_daily_conflict")
        out[d] = v
    return out


def dividends(symbol, rows, start, end):
    records, issues, seen = [], [], {}
    for r in rows:
        if r["ts_code"] != identity(symbol) + "." + symbol[:2]:
            raise SourceError("source_identity_conflict")
        if r["div_proc"] != "实施":
            continue
        announced = day(r.get("imp_ann_date"))
        ex = day(r.get("ex_date"))
        if ex is not None and not start <= ex <= end:
            continue
        try:
            record = day(r.get("record_date"))
            cash = amount(r.get("cash_div_tax"))
            shares = amount(r.get("stk_bo_rate")) + amount(r.get("stk_co_rate"))
            total = amount(r.get("stk_div"))
            if abs(total - shares) > 1e-8:
                raise SourceError("source_amount_conflict")
            if cash + shares == 0:
                continue
            if not announced or not ex or not record:
                raise SourceError("event_dates_missing")
            if announced > record or record >= ex:
                raise SourceError("event_chronology_conflict")
            a = {
                "id": symbol + ":" + ex,
                "instrument": symbol,
                "announced": announced,
                "record_date": record,
                "ex_date": ex,
                "pay_date": day(r.get("pay_date")),
                "listing_date": day(r.get("div_listdate")),
                "cash_per_share": cash,
                "shares_per_share": shares,
                "source": "https://tushare.pro/document/2?doc_id=103",
            }
            if ex in seen and seen[ex] != a:
                raise SourceError("source_event_conflict")
            if ex not in seen:
                seen[ex] = a
                records.append(a)
        except SourceError as error:
            known = announced or day(r.get("ann_date"))
            begin = day(r.get("record_date")) or ex or max(start, known or start)
            finish = ex or end
            begin = min(begin, finish)
            issues.append(
                {
                    "instrument": symbol,
                    "announced": known,
                    "dates": {"start": begin, "end": finish},
                    "code": str(error),
                    "message": "实施记录字段缺失、冲突或无法解释；查看结构化来源",
                }
            )
    return records, issues


def collect(request, report):
    api = Api()
    for symbol in request["instruments"]:
        code = identity(symbol) + "." + symbol[:2]
        report("progress", message="Tushare执行资料 " + symbol)
        rows = api.call("dividend", {"ts_code": code})
        if len(rows) >= LIMITS["dividend"]:
            # dividend has no documented interval parameters. Never pretend that a cap is full history.
            raise SourceError("source_truncated:dividend")
        records, issues = dividends(symbol, rows, request["start"], request["end"])
        aux = {
            "schema_version": 1,
            "suspensions": {},
            "limits": {},
            "coefficients": {},
            "capabilities": {"dividend": {"status": "available"}},
        }
        evidence = {
            "kind": "structured_api_response",
            "provider": "tushare",
            "dividends": rows,
            "request_range": {"start": request["start"], "end": request["end"]},
            "history_complete": True,
            "distribution_unit": "per_share_cny",
        }
        for name in ("suspend_d", "stk_limit", "adj_factor", "anns_d"):
            try:
                values = api.ranged(name, code, request["start"], request["end"])
                evidence[name] = values
                for r in values:
                    if r["ts_code"] != code:
                        raise SourceError("source_identity_conflict")
                aux["capabilities"][name] = {"status": "available"}
                if name == "suspend_d":
                    aux["suspensions"][symbol] = {
                        day(r["trade_date"]): "intraday" if r.get("suspend_timing") else "full_day"
                        for r in values
                        if r["suspend_type"] == "S"
                    }
                elif name == "stk_limit":
                    aux["limits"][symbol] = dated(
                        values, lambda r: [amount(r["down_limit"]), amount(r["up_limit"])]
                    )
                elif name == "adj_factor":
                    aux["coefficients"][symbol] = dated(values, lambda r: amount(r["adj_factor"]))
                else:
                    # Metadata is a candidate locator, never a parsed implementation or absence proof.
                    aux["capabilities"][name]["candidate_count"] = len(values)
                    for r in values:
                        if any(
                            word in r["title"]
                            for word in (
                                "差异化",
                                "配股",
                                "缩股",
                                "重整",
                                "股权分置",
                                "吸收合并",
                                "终止上市",
                                "换股",
                            )
                        ):
                            announced = day(r["ann_date"])
                            issues.append(
                                {
                                    "instrument": symbol,
                                    "announced": announced,
                                    "dates": {"start": announced, "end": request["end"]},
                                    "code": "special_event_candidate_unresolved",
                                    "message": "特殊事项公告候选须审查实施及结果，不能视为空事件："
                                    + r["title"],
                                }
                            )
            except SourceError as error:
                if not str(error).startswith("permission_denied"):
                    raise
                aux["capabilities"][name] = {"status": "permission_denied"}
        report(
            "action_shard",
            instrument=symbol,
            records=records,
            evidence=evidence,
            history_verified=True,
            execution_issues=issues,
            execution_data=aux,
        )
    report("complete")


def permissions(request, report):
    """Permissions are endpoint-specific, never inferred from account existence."""
    api = Api()
    symbol = request["instruments"][0]
    code = identity(symbol) + "." + symbol[:2]
    results = {}
    for name in FIELDS:
        params = {"ts_code": code}
        if name != "dividend":
            params["start_date"] = request["end"].replace("-", "")
            params["end_date"] = params["start_date"]
        try:
            rows = api.call(name, params)
            results[name] = {"status": "available", "rows": len(rows)}
        except SourceError as error:
            results[name] = {
                "status": "permission_denied"
                if str(error).startswith("permission_denied")
                else "failed",
                "code": str(error),
            }
        report("progress", message="Tushare权限：" + name + " " + results[name]["status"])
    report(
        "action_shard",
        instrument=symbol,
        records=[],
        evidence={"kind": "tushare_permissions", "capabilities": results},
    )
    report("complete")
