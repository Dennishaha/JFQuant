"""Offline reconciliation of normalized HiThink events and TDX references."""

import datetime as dt
import hashlib
import json
from decimal import Decimal, localcontext

RULE = "jfquant-actions-v2"


def fingerprint(value):
    return (
        "sha256:"
        + hashlib.sha256(
            json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()
    )


def decimal(value):
    if value is None or isinstance(value, bool):
        raise ValueError("event_amount_missing")
    result = Decimal(str(value))
    if not result.is_finite() or result < 0:
        raise ValueError("event_amount_invalid")
    return result


def reconcile(symbol, events, start, end, native=(), instrument=None):
    """Preserve missing dates; compare exact per-share terms without price inference."""
    with localcontext() as context:
        context.prec = 64
        start, end = dt.date.fromisoformat(start), dt.date.fromisoformat(end)
        if start > end:
            raise ValueError("invalid_reconciliation_range")
        records, issues, decisions, seen = [], {}, {}, {}
        for event in events:
            date = dt.date.fromisoformat(event["ex_date"])
            # Rust binds historical source codes to canonical instrument IDs.
            if event.get("instrument") != (instrument or symbol) or not start <= date <= end:
                raise ValueError("event_identity_or_coverage_invalid")
            cash = decimal(event["cash_per_share"])
            shares = decimal(event["shares_per_share"])
            identity = event["id"]
            if identity in seen:
                if seen[identity] != event:
                    raise ValueError("duplicate_event_conflict")
                continue
            seen[identity] = event
            records.append(event)
            for reference in native:
                if reference.get("date") != event["ex_date"] or str(reference.get("type")) != "1":
                    continue
                observed = (
                    decimal(reference["bonus_per_ten"]) / 10,
                    decimal(reference["shares_per_ten"]) / 10,
                )
                digest = fingerprint([event, reference])
                decisions[digest] = {
                    "rule": "hithink_tdx_reference_compared",
                    "inputs": [fingerprint(event), fingerprint(reference)],
                }
                if (cash, shares) != observed:
                    issues[digest] = {
                        "id": digest,
                        "instrument": event["instrument"],
                        "announced": None,
                        "dates": {"start": event["ex_date"], "end": event["ex_date"]},
                        "code": "distribution_conflict",
                        "event_category": "distribution",
                        "evidence": [digest],
                        "message": "HiThink distribution differs from TDX reference",
                    }
        return {
            "records": sorted(records, key=lambda e: (e["ex_date"], e["id"])),
            "issues": [issues[k] for k in sorted(issues)],
            "decisions": [decisions[k] for k in sorted(decisions)],
            "rule_version": RULE,
        }


def replay(request, report):
    """Fixed operation over Rust-verified, frozen inputs; no legacy row contracts."""
    for symbol in request["instruments"]:
        result = reconcile(**request["reconciliation_inputs"][symbol])
        evidence = {
            "kind": "offline_reconciliation",
            "provider": "hithink",
            "source_rule": "hithink.events/per-share-v1",
            "history_complete": True,
            "request_range": {"start": request["start"], "end": request["end"]},
            "reconciliation": result,
        }
        report(
            "action_shard",
            instrument=symbol,
            records=result["records"],
            execution_issues=result["issues"],
            history_verified=True,
            evidence=evidence,
        )
    report("complete")
