"""Offline June-formation audit from verified BigQuant annual vintages and membership runs."""
import argparse
import bisect
import hashlib
import itertools
import json
from collections import Counter, defaultdict
from datetime import date
from pathlib import Path

from prepare import day, equity_candidate, run_indices, verify_source


def latest_version(rows, period, formation):
    eligible = [r for r in rows if day(r["report_date"]) == period and day(r["date"]) < formation]
    return max(eligible, key=lambda r: day(r["date"]), default=None)


def audit(cli, sources_path):
    manifest = json.loads(sources_path.read_bytes())
    if manifest["shares_availability"] != "provider_dates" or manifest["shares_revision_risk"] is not True:
        raise ValueError("explicit historical shares assumption required")
    inputs = []
    calendar = None
    basic, annual, runs = {}, defaultdict(list), defaultdict(list)
    required = {"annual_equity", "universe_calendar", "universe_runs", "basic_info"}
    seen = set()
    for item in manifest["sources"]:
        kind = item["kind"]
        if kind not in required:
            continue
        value, receipt = verify_source(cli, sources_path.parent, item)
        inputs.append(receipt)
        rows = value["payload"]["rows"]
        if kind == "universe_calendar":
            if calendar is not None:
                raise ValueError("duplicate source calendar")
            calendar = [day(r["date"]) for r in rows]
            if sorted(set(calendar)) != calendar:
                raise ValueError("invalid source calendar")
        elif kind == "basic_info":
            for r in rows:
                if r["instrument"] in basic:
                    raise ValueError("duplicate security classification")
                basic[r["instrument"]] = r["security_type"]
        elif kind == "annual_equity":
            for r in rows:
                key = (r["instrument"], r["report_date"], r["date"])
                if key in seen:
                    raise ValueError("duplicate annual vintage")
                seen.add(key)
                annual[r["instrument"]].append(r)
        else:
            for r in rows:
                runs[r["instrument"]].append(r)
    if not calendar:
        raise ValueError("source calendar missing")
    indexed = {}
    for symbol, rows in runs.items():
        ranges = sorted(run_indices(calendar, r) for r in rows)
        if any(a[1] >= b[0] for a, b in itertools.pairwise(ranges)):
            raise ValueError("overlapping membership runs")
        indexed[symbol] = ranges
    years = range(calendar[0].year, calendar[-1].year + 1)
    formations = []
    for year in years:
        june = [d for d in calendar if d.year == year and d.month == 6]
        december = [d for d in calendar if d.year == year - 1 and d.month == 12]
        if not june or not december:
            formations.append({"year": year, "ready": False, "reason": "prior_december_or_june_calendar_missing"})
            continue
        formation, prior_december = june[-1], december[-1]
        j, d = bisect.bisect_left(calendar, formation), bisect.bisect_left(calendar, prior_december)
        counts, issues = Counter(), []
        period = date(year - 1, 12, 31)
        for symbol, intervals in sorted(indexed.items()):
            if not any(a <= j <= b for a, b in intervals):
                continue
            security_type = basic.get(symbol)
            if security_type is None:
                raise ValueError("unclassified universe security")
            if not security_type.endswith("A股"):
                counts["non_a_share_excluded"] += 1
                continue
            counts["june_a_share_members"] += 1
            if not any(a <= d <= b for a, b in intervals):
                counts["not_listed_at_prior_december"] += 1
                continue
            counts["requires_annual_equity"] += 1
            row = latest_version(annual[symbol], period, formation)
            candidate, issue = (None, "annual_report_not_available_before_formation") if row is None else equity_candidate(row)
            if issue:
                counts["missing_or_unconfirmed_book_equity"] += 1
                counts[issue] += 1
                issues.append({"instrument": symbol, "issue": issue,
                               "reported_on": row["date"] if row else None,
                               "report_period": period.isoformat()})
            elif candidate <= 0:
                counts["nonpositive_candidate_equity"] += 1
            else:
                counts["positive_numeric_equity_candidate"] += 1
        formations.append({"year": year, "formation_date": formation.isoformat(),
                           "prior_december": prior_december.isoformat(), "ready": False,
                           "reason": "book_equity_missing_or_common_equity_scope_unconfirmed",
                           "counts": dict(counts), "issues": issues,
                           "candidate_unit_and_scope_verified": False})
    return {"schema_version": 1, "audit_version": "bigquant-annual-june-v1",
            "source_index_sha256": hashlib.sha256(sources_path.read_bytes()).hexdigest(),
            "shares_revision_risk": True, "financial_same_day_disclosure_used": False,
            "unknown_aggregate_equity_replaced_with_zero": False,
            "all_required_formations_ready": False, "formations": formations, "sources": inputs}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cli", type=Path, required=True)
    parser.add_argument("--sources", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    result = audit(args.cli.resolve(), args.sources.resolve())
    with args.output.open("x") as handle:
        json.dump(result, handle, ensure_ascii=False, indent=2)
    print(json.dumps({"output": str(args.output), "years": len(result["formations"]), "smb_hml_ready": False}))
