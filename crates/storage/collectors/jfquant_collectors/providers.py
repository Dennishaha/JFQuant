"""Whitelisted providers. DataFrame evidence is distinct from an HTTP response."""

import datetime as dt
import math
import time
from pathlib import Path
from zoneinfo import ZoneInfo

from .adjustment import fetch as fetch_adjustment


def nullable(value):
    if value is None:
        return None
    try:
        number = float(value)
    except (ValueError, TypeError):
        return None
    return number if math.isfinite(number) else None


def date(value):
    import pandas as pd

    if value is None or pd.isna(value):
        return None
    return pd.Timestamp(value).date().isoformat()


def identity(symbol):
    if (
        not symbol.isascii()
        or len(symbol) != 8
        or symbol[:2] not in ("SH", "SZ", "BJ")
        or not symbol[2:].isdigit()
    ):
        raise ValueError("invalid instrument: " + symbol)
    return symbol[2:]


def canonical_akshare(symbol, raw, adjusted):
    """Hand -> actual shares; CNY turnover is never approximated from closing price."""
    required = {"日期", "开盘", "最高", "最低", "收盘", "成交量", "成交额"}
    if not required.issubset(raw.columns) or not {"日期", "开盘", "最高", "最低", "收盘"}.issubset(
        adjusted.columns
    ):
        raise ValueError("missing required daily columns")
    adjusted_rows = {}
    for row in adjusted.to_dict("records"):
        day = date(row["日期"])
        if day is None or day in adjusted_rows:
            raise ValueError("missing/duplicate adjusted trading date")
        if "股票代码" in row and str(row["股票代码"]).zfill(6) != identity(symbol):
            raise ValueError("returned adjusted security mismatch")
        adjusted_rows[day] = row
    out = []
    seen = set()
    for row in raw.to_dict("records"):
        day = date(row["日期"])
        if day is None or day in seen:
            raise ValueError("missing/duplicate trading date")
        seen.add(day)
        if "股票代码" in row and str(row["股票代码"]).zfill(6) != identity(symbol):
            raise ValueError("returned security mismatch")
        adj = adjusted_rows.get(day, {})
        bar = {"instrument_id": symbol, "trade_date": day}
        for local, name in (("open", "开盘"), ("high", "最高"), ("low", "最低"), ("close", "收盘")):
            bar[local] = nullable(row.get(name))
            bar["adj_" + local] = nullable(adj.get(name))
        volume = nullable(row.get("成交量"))
        bar["volume"] = None if volume is None else volume * 100
        bar["amount"] = nullable(row.get("成交额"))
        out.append(bar)
    return sorted(out, key=lambda r: r["trade_date"])


def canonical_yfinance(symbol, frame):
    """Yahoo OHLC are split-adjusted: preserve them as adjusted candidate fields only."""
    out = []
    for day, row in frame.iterrows():
        if getattr(day, "tzinfo", None) is not None:
            day = day.tz_convert("Asia/Shanghai")
        bar = {"instrument_id": symbol, "trade_date": day.date().isoformat()}
        for local, source in (
            ("open", "Open"),
            ("high", "High"),
            ("low", "Low"),
            ("close", "Close"),
        ):
            bar[local] = None
            bar["adj_" + local] = nullable(row.get(source))
        bar["volume"] = nullable(row.get("Volume"))
        bar["amount"] = None
        out.append(bar)
    return out


def request_call(call, **kwargs):
    """Bounded network retries. Parsing/validation exceptions are never retried."""
    for attempt in range(3):
        try:
            result = call(**kwargs)
            time.sleep(0.25)
            return result
        except Exception as error:
            status = getattr(getattr(error, "response", None), "status_code", None)
            transient = status in (429, 500, 502, 503, 504) or any(
                word in type(error).__name__.lower()
                for word in ("timeout", "connection", "ratelimit")
            )
            if not transient or attempt == 2:
                raise
            time.sleep(0.5 * (2**attempt))


def market(request, report):
    """Single-security chunks allow bounded retry and checkpointing in the Rust supervisor."""
    import akshare as ak
    import yfinance as yf

    now = dt.datetime.now(ZoneInfo("Asia/Shanghai"))
    closed_end = now.date() if now.hour >= 16 else now.date() - dt.timedelta(days=1)
    start, end = request["start"], min(request["end"], closed_end.isoformat())
    if start > end:
        raise ValueError("requested interval contains no completed trading date")
    symbols = request["instruments"]
    indices = set(request["benchmarks"])
    if not indices.issubset({"SH000300", "SH000905"}):
        raise ValueError("unverified price-index identity")
    expected_adjustment = "sina_hfq" if request["provider"] == "akshare" else "none"
    if request["adjustment_source"] != expected_adjustment:
        raise ValueError("unsupported provider/adjustment source combination")
    if request["provider"] == "yfinance":
        yf.set_tz_cache_location(str(Path(request["cache_dir"]) / "yfinance"))
        if hasattr(yf, "config"):
            yf.config.debug.hide_exceptions = False
    for symbol in symbols:
        code = identity(symbol)
        report("progress", message="采集 " + symbol)
        if request["provider"] == "akshare":
            if symbol in indices:
                raw = request_call(
                    ak.index_zh_a_hist,
                    symbol=code,
                    period="daily",
                    start_date=start.replace("-", ""),
                    end_date=end.replace("-", ""),
                )
                adjusted = raw.copy()
            else:
                args = {
                    "symbol": code,
                    "period": "daily",
                    "start_date": start.replace("-", ""),
                    "end_date": end.replace("-", ""),
                    "timeout": 30,
                }
                raw = request_call(ak.stock_zh_a_hist, **args, adjust="")
                adjusted = request_call(ak.stock_zh_a_hist, **args, adjust="hfq")
            if raw.empty or adjusted.empty:
                raise ValueError("empty provider response: " + symbol)
            rows = canonical_akshare(symbol, raw, adjusted)
            evidence = {
                "kind": "structured_library_response",
                "provider": "akshare",
                "raw": raw.to_json(orient="table", date_format="iso"),
                "adjusted": adjusted.to_json(orient="table", date_format="iso"),
            }
        elif request["provider"] == "yfinance":
            if symbol.startswith("BJ"):
                raise ValueError("Yahoo Beijing mapping is not verified")
            ticker = yf.Ticker(code + (".SS" if symbol.startswith("SH") else ".SZ"))
            frame = request_call(
                ticker.history,
                start=start,
                end=(dt.date.fromisoformat(end) + dt.timedelta(days=1)).isoformat(),
                interval="1d",
                auto_adjust=False,
                back_adjust=False,
                repair=False,
                keepna=True,
                actions=True,
                rounding=False,
                timeout=30,
            )
            if frame.empty:
                raise ValueError("empty provider response: " + symbol)
            metadata = ticker.get_history_metadata()
            if (
                metadata.get("currency") != "CNY"
                or metadata.get("exchangeTimezoneName") != "Asia/Shanghai"
            ):
                raise ValueError("unexpected Yahoo currency/timezone")
            rows = canonical_yfinance(symbol, frame)
            evidence = {
                "kind": "structured_library_response",
                "provider": "yfinance",
                "table": frame.to_json(orient="table", date_format="iso"),
                "metadata": metadata,
            }
        else:
            raise ValueError("unsupported provider")
        if request.get("operation") == "quotes":
            adjustment = {
                "schema_version": 3,
                "instrument_id": symbol,
                "source_symbol": symbol,
                "basis": {
                    "kind": "unavailable",
                    "reason": "quotes-only acquisition; compose with independently acquired coefficients before verified research",
                },
            }
        elif request["provider"] == "akshare":
            if symbol in indices:
                adjustment = {
                    "schema_version": 3,
                    "instrument_id": symbol,
                    "source_symbol": symbol,
                    "basis": {"kind": "price_index", "source": "akshare.index_zh_a_hist"},
                }
            else:
                adjustment = fetch_adjustment(symbol, end, report, request_call)
        else:
            adjustment = {
                "schema_version": 3,
                "instrument_id": symbol,
                "source_symbol": symbol,
                "basis": {
                    "kind": "unavailable",
                    "reason": "Yahoo raw price/adjustment basis unverified",
                },
            }
        report("shard", instrument=symbol, bars=rows, adjustment=adjustment, evidence=evidence)
    # An exchange calendar is a separate source, never inferred from surviving quotes.
    calendar = request_call(ak.tool_trade_date_hist_sina)
    dates = sorted(
        {date(d) for d in calendar["trade_date"] if date(d) is not None and start <= date(d) <= end}
    )
    if not dates:
        raise ValueError("empty exchange calendar")
    universes = {}
    for name in request["universes"]:
        if name not in ("csi300", "csi500"):
            raise ValueError("unsupported index constituent source")
        table = request_call(
            ak.index_stock_cons_csindex, symbol="000300" if name == "csi300" else "000905"
        )
        if table.empty:
            raise ValueError("empty constituent response")
        intervals = {}
        observed = {date(d) for d in table["日期"]}
        if len(observed) != 1 or None in observed:
            raise ValueError("ambiguous constituent observation date")
        day = observed.pop()
        for row in table.to_dict("records"):
            prefix = {"上海证券交易所": "SH", "深圳证券交易所": "SZ", "北京证券交易所": "BJ"}.get(
                row["交易所"]
            )
            if prefix is None:
                raise ValueError("unknown constituent exchange")
            symbol = prefix + str(row["成分券代码"]).zfill(6)
            identity(symbol)
            intervals[symbol] = [{"start": day, "end": day}]
        universes[name] = {"coverage": {"start": day, "end": day}, "intervals": intervals}
    report(
        "complete",
        calendar=dates,
        universes=universes,
        versions={
            "akshare": ak.__version__,
            "yfinance": yf.__version__,
            "python": __import__("platform").python_version(),
        },
    )


def corporate_actions(request, report):
    """Implemented distributions are candidates until all settlement dates are verified."""
    import akshare as ak
    import yfinance as yf

    for symbol in request["instruments"]:
        code = identity(symbol)
        report("progress", message="公司行动 " + symbol)
        records = []
        if request["provider"] == "akshare":
            table = request_call(ak.stock_history_dividend_detail, symbol=code, indicator="分红")
            rights = request_call(ak.stock_history_dividend_detail, symbol=code, indicator="配股")
            if table.empty:
                raise ValueError("empty dividend table cannot prove complete history: " + symbol)
            rights_review = not rights.empty
            # Preserve rights tables as candidates; no subscription/payment/listing date is guessed.
            for row in table.to_dict("records"):
                if row.get("进度") != "实施":
                    continue
                cash = nullable(row.get("派息"))
                shares, transfer = nullable(row.get("送股")), nullable(row.get("转增"))
                if cash is None or shares is None or transfer is None:
                    raise ValueError("missing distribution amount/unit: " + symbol)
                ex = date(row.get("除权除息日"))
                records.append(
                    {
                        "id": symbol + ":" + str(ex),
                        "instrument": symbol,
                        "announced": date(row.get("公告日期")),
                        "record_date": date(row.get("股权登记日")),
                        "ex_date": ex,
                        "pay_date": None,
                        "listing_date": date(row.get("红股上市日")),
                        "cash_per_share": cash / 10,
                        "shares_per_share": (shares + transfer) / 10,
                        "source": "https://vip.stock.finance.sina.com.cn/corp/go.php/vISSUE_ShareBonus/stockid/"
                        + code
                        + ".phtml",
                    }
                )
            evidence = {
                "kind": "structured_library_response",
                "provider": "akshare",
                "dividends": table.to_json(orient="table", date_format="iso"),
                "rights": rights.to_json(orient="table", date_format="iso"),
                "distribution_unit": "per_ten_shares",
            }
        elif request["provider"] == "yfinance":
            if symbol.startswith("BJ"):
                raise ValueError("unverified Yahoo symbol mapping")
            yf.set_tz_cache_location(str(Path(request["cache_dir"]) / "yfinance"))
            ticker = yf.Ticker(code + (".SS" if symbol.startswith("SH") else ".SZ"))
            table = request_call(ticker.get_actions, period="max")
            evidence = {
                "kind": "structured_library_response",
                "provider": "yfinance",
                "events": table.to_json(orient="table", date_format="iso"),
                "coverage_verified": False,
            }
            # These events lack record/payment/listing dates and cannot form a strict bundle.
        else:
            raise ValueError("unsupported provider")
        report(
            "action_shard",
            instrument=symbol,
            records=records,
            evidence=evidence,
            history_verified=request["provider"] == "akshare" and not rights_review,
            failure=None
            if request["provider"] == "akshare" and not rights_review
            else (
                "rights issue candidates require implementation review"
                if request["provider"] == "akshare"
                else "Yahoo events lack verified A-share history and settlement dates"
            ),
        )
    report(
        "complete",
        versions={
            "akshare": ak.__version__,
            "yfinance": yf.__version__,
            "python": __import__("platform").python_version(),
        },
    )


def coefficients(request, report):
    """Fetch independent dated factors without any quote or dividend dependency."""
    if request["provider"] != "akshare" or request["adjustment_source"] != "sina_hfq":
        raise ValueError("independent coefficient acquisition requires sina_hfq")
    for symbol in request["instruments"]:
        identity(symbol)
        if symbol in request["benchmarks"]:
            adjustment = {
                "schema_version": 3,
                "instrument_id": symbol,
                "source_symbol": symbol,
                "basis": {"kind": "price_index", "source": "akshare.index_zh_a_hist"},
            }
        else:
            adjustment = fetch_adjustment(symbol, request["end"], report, request_call)
        report(
            "adjustment_shard",
            instrument=symbol,
            adjustment=adjustment,
            evidence={"kind": "independent_coefficient_response", "source": "sina_hfq"},
        )
    report("complete")
