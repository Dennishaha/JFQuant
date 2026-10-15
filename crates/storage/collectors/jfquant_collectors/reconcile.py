"""Deterministic action reconciliation. No network, prices, portfolio or user code."""

import bisect
import datetime as dt
import hashlib
import json
from collections import defaultdict
from decimal import Decimal, InvalidOperation, localcontext

RULE = "jfquant-actions-v1"
# Precision is deliberately not inferred from displayed decimal places. New
# source-specific rounding contracts must be reviewed with an evidence fixture.
PRECISION_RULES = {}
SPECIAL = {
    "配股": "rights_issue",
    "股改": "reform",
    "重整": "reorganization",
    "补偿": "compensation",
    "换股": "merger",
    "缩股": "reduction",
}


def fingerprint(value):
    return (
        "sha256:"
        + hashlib.sha256(
            json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()
    )


def day(value):
    if value in (None, "", "--"):
        return None
    return dt.date.fromisoformat(str(value)[:10]).isoformat()


def decimal(value):
    value = Decimal(str(value))
    if not value.is_finite() or value < 0:
        raise ValueError("event_amount_invalid")
    return value


def text(value):
    return format(value, "f")


def component_amount(value, description, keyword):
    if value in (None, "", "--"):
        if (
            description
            and any(k in description for k in ("派", "送", "转"))
            and keyword not in description
        ):
            return Decimal(0)
        raise ValueError("event_amount_missing")
    return decimal(value)


def compatible(exact, observed, source):
    if exact == observed:
        return True
    contract = PRECISION_RULES.get(source)
    if contract is None:
        return False
    quantum, rounding = contract
    return exact.quantize(Decimal(quantum), rounding=rounding) == observed


def classify(row):
    label = str(row.get("F044V") or "")
    description = str(row.get("F007V") or "")
    for keyword, category in SPECIAL.items():
        if keyword in label:
            return category
        if keyword in description:
            # A mixed description may contain ordinary cash/share entitlements.
            return (
                "unknown"
                if any(k in label for k in ("年度", "特别", "中期", "季度", "分红"))
                else category
            )
    if label and not any(k in label for k in ("年度", "特别", "中期", "季度", "分红", "利润分配")):
        return "unknown"
    return "distribution"


def reconcile(symbol, rows, start, end, sina_rows=(), native=(), calendar=(), rights=()):
    with localcontext() as context:
        context.prec = 64
        return _reconcile(symbol, rows, start, end, sina_rows, native, calendar, rights)


def _reconcile(symbol, rows, start, end, sina_rows, native, calendar, rights):
    """One identity-bound history; all amounts are per old share decimal text."""
    decisions, issues, groups = [], [], defaultdict(list)
    seen_rows, observed_dates = set(), set()

    def problem(code, ex, announced, message, row, category="distribution"):
        identity = fingerprint([symbol, code, row, category])
        if any(i["id"] == identity for i in issues):
            return
        issues.append(
            {
                "id": identity,
                "instrument": symbol,
                "announced": announced,
                "dates": {
                    "start": ex or min(end, max(start, announced or start)),
                    "end": ex or end,
                },
                "code": code,
                "event_category": category,
                "evidence": [fingerprint(row)],
                "message": message,
            }
        )

    def decide(rule, inputs, **extra):
        decisions.append({"rule": rule, "inputs": inputs, **extra})

    for row in rows:
        key = fingerprint(row)
        if key in seen_rows:
            decide("identical_source_row_deduplicated", [key])
            continue
        seen_rows.add(key)
        ex = announced = None
        category = classify(row)
        if category not in ("distribution", "unknown"):
            try:
                ex = day(row.get("F020D"))
                announced = day(row.get("F006D"))
            except (ValueError, TypeError):
                ex = announced = None
            if not ex or start <= ex <= end:
                problem(
                    "execution_event_unsupported_skipped",
                    ex,
                    announced,
                    "已识别特殊事件，AKQuant未模拟，继续回测",
                    row,
                    category,
                )
            if ex:
                observed_dates.add(ex)
            continue
        try:
            ex, announced = day(row.get("F020D")), day(row.get("F006D"))
            if ex:
                observed_dates.add(ex)
            if ex and not start <= ex <= end:
                decide("outside_effective_range", [key], ex_date=ex)
                continue
            if category == "unknown":
                raise ValueError("event_category_unresolved")
            record = day(row.get("F018D"))
            description = str(row.get("F007V") or "")
            if any(
                k in description for k in ("税后", "扣税后", "港元", "美元", "差异化", "特定股东")
            ):
                raise ValueError("distribution_basis_unresolved")
            cash = component_amount(row.get("F012N"), description, "派") / 10
            shares = (
                component_amount(row.get("F010N"), description, "送")
                + component_amount(row.get("F011N"), description, "转")
            ) / 10
            if not cash and not shares:
                decide("zero_distribution", [key])
                continue
            if not ex and record and calendar:
                index = bisect.bisect_right(calendar, record)
                next_day = calendar[index] if index < len(calendar) else None
                matches = [
                    r
                    for r in native
                    if r.get("type") == "1"
                    and r["date"] == next_day
                    and decimal(r["rights_per_ten"]) == 0
                    and decimal(r["bonus_per_ten"]) / 10 == cash
                    and decimal(r["shares_per_ten"]) / 10 == shares
                ]
                if len(matches) == 1:
                    ex = next_day
                    decide(
                        "unique_native_date_and_record_calendar",
                        [key, fingerprint(matches[0])],
                        ex_date=ex,
                    )
                    observed_dates.add(ex)
            if not ex:
                raise ValueError("cninfo_event_date_missing")
            if not start <= ex <= end:
                decide("outside_effective_range", [key], ex_date=ex)
                continue
            if (
                (announced and announced > ex)
                or (record and record >= ex)
                or (announced and record and announced > record)
            ):
                raise ValueError("cninfo_event_chronology_conflict")
            groups[ex].append(
                {
                    "key": key,
                    "row": row,
                    "label": str(row.get("F044V") or ""),
                    "cash": cash,
                    "shares": shares,
                    "announced": announced,
                    "record": record,
                }
            )
        except (ValueError, InvalidOperation, KeyError, TypeError) as error:
            code = str(error)
            if code not in {
                "event_amount_invalid",
                "event_amount_missing",
                "event_category_unresolved",
                "distribution_basis_unresolved",
                "cninfo_event_date_missing",
                "cninfo_event_chronology_conflict",
            }:
                code = "cninfo_event_parse_error"
            problem(
                code,
                ex,
                announced,
                "实施条款需补证；原始记录已保留",
                row,
                category,
            )

    sina = defaultdict(list)
    for row in sina_rows:
        if row.get("进度") != "实施":
            continue
        try:
            ex = day(row.get("除权除息日"))
            if ex and start <= ex <= end and row not in sina[ex]:
                sina[ex].append(row)
        except (ValueError, TypeError):
            problem("supplement_date_invalid", None, None, "补充来源日期无效", row)
    records = []
    for ex, components in sorted(groups.items()):
        primary = [c["key"] for c in components]
        labels = [c["label"] for c in components]
        if len(components) > 1 and (
            len(set(labels)) != len(labels)
            or not set(labels) <= {"年度分红", "特别分红", "中期分红", "季度分红"}
            or len({c["record"] for c in components}) != 1
            or len({c["announced"] for c in components}) != 1
        ):
            problem(
                "source_event_conflict",
                ex,
                None,
                "同日记录不能证明为独立组成项，更正关系需实施证据",
                primary,
            )
            continue
        cash = sum(c["cash"] for c in components)
        shares = sum(c["shares"] for c in components)
        # A source row's explicit CNY, pre-tax, per-ten-old-share semantics are
        # fixed by the reviewed endpoint rule, never by a user-supplied unit.
        conflict = False
        listing = None
        for s in sina[ex]:
            try:
                terms = (decimal(s["派息"]) / 10, (decimal(s["送股"]) + decimal(s["转增"])) / 10)
                same_record = (
                    not day(s.get("股权登记日"))
                    or not components[0]["record"]
                    or day(s.get("股权登记日")) == components[0]["record"]
                )
                if (
                    same_record
                    and compatible(cash, terms[0], "sina.cash.per_share")
                    and compatible(shares, terms[1], "sina.shares.per_share")
                ):
                    listing = day(s.get("红股上市日")) if shares else None
                    decide("supplement_total_agrees", primary + [fingerprint(s)])
                elif (
                    same_record
                    and len(components) > 1
                    and terms in [(c["cash"], c["shares"]) for c in components]
                ):
                    decide("supplement_partial_component", primary + [fingerprint(s)])
                else:
                    conflict = True
            except (InvalidOperation, ValueError, KeyError, TypeError):
                conflict = True
        native_matches = [
            n
            for n in native
            if n.get("type") == "1"
            and n.get("date") == ex
            and decimal(n["rights_per_ten"]) == 0
            and decimal(n["bonus_per_ten"]) / 10 == cash
            and decimal(n["shares_per_ten"]) / 10 == shares
        ]
        if native_matches:
            decide("native_total_agrees", primary + [fingerprint(n) for n in native_matches])
        if conflict:
            problem(
                "source_event_conflict",
                ex,
                components[0]["announced"],
                "普通分红实质冲突或未登记精度差异，需实施公告",
                primary + sina[ex],
            )
            continue
        try:
            pay_date = day(components[0]["row"].get("F023D"))
        except (ValueError, TypeError):
            problem(
                "cninfo_event_parse_error",
                ex,
                components[0]["announced"],
                "已提供的到账日期格式异常，需补证",
                primary,
            )
            continue
        if (pay_date and pay_date < ex) or (listing and listing < ex):
            problem(
                "cninfo_event_chronology_conflict",
                ex,
                components[0]["announced"],
                "已提供的到账/上市日期早于除权日",
                primary,
            )
            continue
        record = {
            "id": symbol + ":" + ex,
            "instrument": symbol,
            "announced": components[0]["announced"],
            "record_date": components[0]["record"],
            "ex_date": ex,
            "pay_date": pay_date,
            "listing_date": listing,
            "cash_per_share": text(cash),
            "shares_per_share": text(shares),
            "source": "https://webapi.cninfo.com.cn/api/sysapi/p_sysapi1139?scode=" + symbol[2:],
        }
        records.append(record)
        decide(
            "normalized_distribution"
            if len(components) == 1
            else "same_old_share_base_components_summed",
            primary,
            event=record,
            components=[c["row"] for c in components],
            source_unit="per_ten_old_shares",
            normalized_unit="per_old_share",
            cash_basis="gross_pre_tax_CNY",
            beneficiary="ordinary_A_share_holders",
            raw_amount_texts=[
                {
                    k: str(c["row"].get(k)) if c["row"].get(k) is not None else None
                    for k in ("F012N", "F010N", "F011N")
                }
                for c in components
            ],
        )
    # Presence is independent from eligibility: conflicts must not become spurious gaps.
    for ex, source_rows in sorted(sina.items()):
        if ex not in observed_dates:
            problem(
                "cninfo_history_gap",
                ex,
                day(source_rows[0].get("公告日期")),
                "补充来源有普通实施分红，主来源无对应原始记录",
                source_rows,
            )
    for r in rights:
        try:
            ex = day(r.get("除权日"))
            if ex and start <= ex <= end:
                problem(
                    "execution_event_unsupported_skipped",
                    ex,
                    day(r.get("公告日期")),
                    "配股参考已识别，AKQuant未模拟，继续回测",
                    r,
                    "rights_issue",
                )
        except (ValueError, TypeError):
            problem(
                "rights_issue_identity_unresolved",
                None,
                None,
                "配股候选日期或身份待核实",
                r,
                "unknown",
            )
    # The same identified special event from two sources is one skipped event.
    # All independent source references remain in the reconciliation decisions.
    roots = {}
    for issue in issues:
        dates = issue["dates"]
        if (
            issue["code"] == "execution_event_unsupported_skipped"
            and dates["start"] == dates["end"]
        ):
            key = fingerprint([symbol, dates["start"], issue["event_category"], issue["code"]])
            decide(
                "unsupported_event_source_reference",
                issue["evidence"],
                original_issue_id=issue["id"],
                root_issue_id=key,
            )
            if key in roots:
                roots[key]["evidence"] = sorted(set(roots[key]["evidence"] + issue["evidence"]))
            else:
                roots[key] = {**issue, "id": key}
        else:
            roots[issue["id"]] = issue
    return {
        "records": records,
        "issues": sorted(roots.values(), key=lambda i: i["id"]),
        "decisions": decisions,
        "rule_version": RULE,
    }


def replay(request, report):
    """Fixed offline operation over Rust-verified, frozen inputs."""
    for symbol in request["instruments"]:
        spec = request["reconciliation_inputs"][symbol]
        result = reconcile(**spec, calendar=request.get("reconciliation_calendar", []))
        evidence = {
            "kind": "offline_reconciliation",
            "provider": "akshare",
            "source_rule": "akshare.cninfo/implemented/per-ten-shares-v1",
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
