"""Fixed CNINFO implementation evidence acquisition. Candidate extraction never overrides events."""

import datetime as dt
import hashlib
import io
import re
from functools import lru_cache
from itertools import pairwise
from urllib.parse import urlsplit

import requests
from pypdf import PdfReader

from .providers import identity, request_call

SEARCH = "https://www.cninfo.com.cn/new/hisAnnouncement/query"


@lru_cache(maxsize=1)
def security_directory():
    response = request_call(
        lambda: requests.get(
            "https://www.cninfo.com.cn/new/data/szse_stock.json", timeout=30, allow_redirects=False
        )
    )
    response.raise_for_status()
    if response.status_code != 200:
        raise ValueError("unexpected security-directory status/redirect")
    table = response.json().get("stockList")
    if not isinstance(table, list):
        raise TypeError("invalid disclosure security directory")
    return table


def security_identity(symbol):
    code = identity(symbol)
    matches = [
        r for r in security_directory() if r.get("code") == code and r.get("category") == "A股"
    ]
    if len(matches) != 1 or not re.fullmatch(r"[A-Za-z0-9_-]+", str(matches[0].get("orgId", ""))):
        raise ValueError("missing/ambiguous disclosure institution identity: " + symbol)
    # orgId is an opaque directory key. Recent issuers use numeric 99... IDs,
    # while historical gssh/gssz IDs additionally encode their venue.
    venue = symbol[:2]
    if not (
        (venue == "SH" and code.startswith("6"))
        or (venue == "SZ" and code.startswith(("0", "3")))
        or (venue == "BJ" and code.startswith(("4", "8", "920")))
    ):
        raise ValueError("disclosure security/exchange conflict")
    org = matches[0]["orgId"]
    if re.fullmatch(r"99[0-9]{8}", org):
        return org
    if (symbol.startswith("SH") and not org.startswith("gssh")) or (
        symbol.startswith("SZ") and not org.startswith("gssz")
    ):
        raise ValueError("disclosure institution/exchange conflict")
    if not org.startswith(("gssh", "gssz")):
        raise ValueError("unrecognized disclosure institution identity")
    return org


def attachment_url(path):
    if not isinstance(path, str) or not re.fullmatch(
        r"finalpage/[0-9-]+/[A-Za-z0-9_-]+\.PDF", path, re.IGNORECASE
    ):
        raise ValueError("untrusted announcement attachment")
    return "https://static.cninfo.com.cn/" + path


def extract_candidates(text, title):
    stage = (
        "implementation"
        if re.search(r"实施|清算|交割结果|换股结果|终止上市|摘牌|证券代码.*变更", title)
        else "unverified"
    )
    if re.search(r"预案|草案|摘要|提示性|风险提示|可能|拟|股东大会|董事会", title):
        stage = "unverified"
    labels = (
        ("record_date", "股权登记日(?:为)?"),
        ("ex_date", "(?:除权[（(]?息[）)]?日|除权除息日|除息日)(?:为)?"),
        ("pay_date", "现金红利发放日|现金红利派发日|现金红利到账日"),
        (
            "listing_date",
            "新增无限售条件流通股(?:份)?上市日|红股上市日|本次所(?:送|转|送转)的无限售流通股的起始交易日(?:为)?",
        ),
    )
    date_pattern = r"([0-9]{4})\s*(?:年|/|-)\s*([0-9]{1,2})\s*(?:月|/|-)\s*([0-9]{1,2})\s*日?"
    date_candidates = {field: [] for field, _ in labels}

    def capture_date(field, match, quote):
        try:
            value = dt.date(*(int(v) for v in match.groups())).isoformat()
        except ValueError:
            return
        item = {"value": value, "quote": quote}
        if item not in date_candidates[field]:
            date_candidates[field].append(item)

    for field, label in labels:
        for match in re.finditer(r"(?:" + label + r")[\s：:]*" + date_pattern, text):
            capture_date(field, match, match[0])
    # PDF tables often place all headers on one line and dates on the next.
    # Only align an exact-width row with an explicit share-class header. A dash
    # remains a missing cell, never a date borrowed from the adjacent column.
    lines = text.splitlines()
    for header, row in pairwise(lines):
        columns, cells = header.split(), row.split()
        if not columns or columns[0] != "股份类别" or len(columns) != len(cells):
            continue
        if cells[0] not in ("普通股", "A股", "A 股"):
            continue
        for index, column in enumerate(columns):
            for field, label in labels:
                match = re.fullmatch(date_pattern, cells[index])
                if re.fullmatch(label, column) and match:
                    capture_date(field, match, header + "\n" + row)
    # SSE's six-column ordinary-share table wraps two headings across lines.
    # Accept this exact layout, retaining the complete original block as evidence;
    # do not infer column positions from an arbitrary list of dates in prose.
    table_header = (
        r"股份类别\s+股权登记日\s+最后交易日\s+除权[（(]息[）)]日\s+"
        r"新增普通股无\s*限售条件流通\s*股份上市日\s+现金红利\s*发放日"
    )
    for match in re.finditer(table_header + r"\s*\n([^\n]+)", text):
        cells = match[1].split()
        if len(cells) != 6 or cells[0] not in ("Ａ股", "A股", "普通股"):
            continue
        for field, index in (
            ("record_date", 1),
            ("ex_date", 3),
            ("listing_date", 4),
            ("pay_date", 5),
        ):
            value = re.fullmatch(date_pattern, cells[index])
            if value:
                capture_date(field, value, match[0])
    dates = {}
    for field, candidates in date_candidates.items():
        values = sorted({item["value"] for item in candidates})
        dates[field] = values[0] if len(values) == 1 else None
    amounts = []
    for match in re.finditer(
        r"每\s*([0-9]+(?:\.[0-9]+)?)?\s*股\s*(?:派发|派|分配)?\s*(?:现金红利|现金|人民币)\s*(?:人民币)?\s*([0-9]+(?:\.[0-9]+)?)\s*元",
        text,
    ):
        amounts.append(
            {"original_shares": match[1] or "1", "original_cash_cny": match[2], "quote": match[0]}
        )
    return {
        "stage": stage,
        "dates": dates,
        "date_candidates": date_candidates,
        "amount_candidates": amounts,
        "requires_review": True,
    }


def collect(request, report):
    root = __import__("pathlib").Path(request["cache_dir"]).parent / "raw"
    for symbol in request["instruments"]:
        code = identity(symbol)
        org = security_identity(symbol)
        documents, seen, expected = [], set(), None
        all_announcements = []
        for page in range(1, 2001):
            report("progress", message=f"实施公告 {symbol} 第{page}页")
            payload = {
                "pageNum": page,
                "pageSize": 30,
                "column": {"SH": "sse", "SZ": "szse", "BJ": "bse"}[symbol[:2]],
                "tabName": "fulltext",
                "stock": code + "," + org,
                "searchkey": "",
                "seDate": request["start"] + "~" + request["end"],
                "sortName": "time",
                "sortType": "asc",
                "isHLtitle": "false",
            }
            response = request_call(
                lambda payload=payload: requests.post(
                    SEARCH, data=payload, timeout=30, allow_redirects=False
                )
            )
            response.raise_for_status()
            if response.status_code != 200:
                raise ValueError("unexpected announcement status/redirect")
            body = response.json()
            total = body.get("totalAnnouncement")
            rows = body.get("announcements")
            if type(total) is not int or total < 0 or total > 60000 or not isinstance(rows, list):
                raise ValueError("invalid announcement pagination")
            if expected is None:
                expected = total
            if total != expected:
                raise ValueError("announcement pagination changed during collection")
            for row in rows:
                if (
                    str(row.get("secCode")) != code
                    or row.get("orgId") != org
                    or row.get("announcementColumn", payload["column"]) != payload["column"]
                ):
                    raise ValueError("announcement security conflict")
                key = str(row["announcementId"])
                if not re.fullmatch(r"[A-Za-z0-9_-]+", key) or key in seen:
                    raise ValueError("invalid/duplicate announcement identity")
                seen.add(key)
                title = re.sub(r"<[^>]*>", "", row["announcementTitle"])
                all_announcements.append(
                    {
                        "id": key,
                        "title": title,
                        "institution_id": org,
                        "announcement_time": row["announcementTime"],
                        "attachment": row.get("adjunctUrl"),
                    }
                )
                if not re.search(
                    r"权益分派|利润分配|转增|送股|派息|股利|分红|配股|缩股|重整|股权分置|换股|合并|终止上市|摘牌|清偿|清算|交割|证券代码.*变更|现金选择权",
                    title,
                ):
                    continue
                url = attachment_url(row["adjunctUrl"])
                pdf = request_call(
                    lambda url=url: requests.get(url, timeout=30, allow_redirects=False)
                )
                pdf.raise_for_status()
                if (
                    pdf.status_code != 200
                    or urlsplit(pdf.url).hostname != "static.cninfo.com.cn"
                    or len(pdf.content) > 20 * 1024 * 1024
                    or not pdf.content.startswith(b"%PDF-")
                ):
                    raise ValueError("invalid announcement PDF/status")
                document = PdfReader(io.BytesIO(pdf.content), strict=True)
                if document.is_encrypted or len(document.pages) > 500:
                    raise ValueError("unsupported encrypted/oversized announcement")
                pages = [p.extract_text() or "" for p in document.pages]
                text = "\n".join(pages)
                if len(text.encode()) > 8 * 1024 * 1024:
                    raise ValueError("announcement text too large")
                name = symbol + "-" + key
                (root / (name + ".pdf")).write_bytes(pdf.content)
                (root / (name + ".txt")).write_text(text, encoding="utf-8")
                published = (
                    dt.datetime.fromtimestamp(
                        row["announcementTime"] / 1000, tz=dt.timezone(dt.timedelta(hours=8))
                    )
                    .date()
                    .isoformat()
                )
                if not request["start"] <= published <= request["end"]:
                    raise ValueError("announcement date outside request")
                documents.append(
                    {
                        "id": key,
                        "instrument": symbol,
                        "institution_id": org,
                        "announced": published,
                        "title": title,
                        "url": url,
                        "pdf": name + ".pdf",
                        "pdf_sha256": "sha256:" + hashlib.sha256(pdf.content).hexdigest(),
                        "text": name + ".txt",
                        "text_sha256": "sha256:" + hashlib.sha256(text.encode()).hexdigest(),
                        "parser_version": "cninfo-candidates-v3",
                        "candidates": extract_candidates(text, title),
                    }
                )
            if len(seen) == expected:
                break
            if len(rows) != 30 or len(seen) > expected:
                raise ValueError("truncated announcement pagination")
        else:
            raise ValueError("announcement pagination limit")
        report(
            "action_shard",
            instrument=symbol,
            records=[],
            history_verified=False,
            failure="implementation candidates require review",
            evidence={
                "kind": "implementation_announcements",
                "documents": documents,
                "all_announcements": all_announcements,
                "pagination_complete": len(seen) == expected,
                "requested_announcement_dates": {"start": request["start"], "end": request["end"]},
                "event_category_completeness": "not_reviewed",
            },
        )
    import platform

    import akshare
    import yfinance

    report(
        "complete",
        versions={
            "akshare": akshare.__version__,
            "yfinance": yfinance.__version__,
            "python": platform.python_version(),
        },
    )
