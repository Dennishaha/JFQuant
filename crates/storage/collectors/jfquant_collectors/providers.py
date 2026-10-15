"""Shared strict scalar, identity and bounded HTTP utilities."""

import math
import time


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
