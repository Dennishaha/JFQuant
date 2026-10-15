"""Implementation-document identity, safe locations and candidate-only extraction."""

import unittest
from unittest.mock import patch

from jfquant_collectors.announcements import attachment_url, collect, extract_candidates


class AnnouncementTests(unittest.TestCase):
    def test_pdf_spaces_and_share_listing_wording_keep_exact_evidence(self):
        text = "股权登记日：2016 年 6 月22 日\n新增无限售条件流通股份上市日：2016 年 6 月 24 日"
        result = extract_candidates(text, "利润分配及资本公积转增股本实施公告")
        self.assertEqual(result["dates"]["record_date"], "2016-06-22")
        self.assertEqual(result["dates"]["listing_date"], "2016-06-24")
        self.assertIn(result["date_candidates"]["listing_date"][0]["quote"], text)

    def test_exact_wrapped_sse_table_and_szse_start_trading_date(self):
        header = "股份类别 股权登记日 最后交易日 除权（息）日\n新增普通股无\n限售条件流通\n股份上市日\n现金红利\n发放日"
        row = "Ａ股 2020/5/24 － 2020/5/25 2020/5/26 2020/5/25"
        result = extract_candidates(header + "\n" + row, "权益分派实施公告")
        self.assertEqual(result["dates"]["listing_date"], "2020-05-26")
        result = extract_candidates(
            header + "\n" + row.replace("2020/5/26", "－"), "权益分派实施公告"
        )
        self.assertIsNone(result["dates"]["listing_date"])
        result = extract_candidates(
            "本次所转的无限售流通股的起始交易日为 2020 年 6 月 16 日。", "权益分派实施公告"
        )
        self.assertEqual(result["dates"]["listing_date"], "2020-06-16")

    def test_table_dates_keep_dash_column_and_amount_units_with_quotes(self):
        text = (
            "股份类别 股权登记日 最后交易日 除权（息）日 现金红利发放日\n"
            "普通股 2026/7/15 － 2026/7/16 2026/7/16\n"
            "普通股每股现金红利人民币0.42元（含税）\n"
            "现金红利发放日：2026-07-17"
        )
        candidate = extract_candidates(text, "年度权益分派实施公告")
        self.assertEqual(candidate["dates"]["record_date"], "2026-07-15")
        self.assertEqual(candidate["dates"]["ex_date"], "2026-07-16")
        self.assertIsNone(candidate["dates"]["pay_date"])
        self.assertEqual(len(candidate["date_candidates"]["pay_date"]), 2)
        self.assertIn("普通股", candidate["date_candidates"]["record_date"][0]["quote"])
        self.assertEqual(candidate["amount_candidates"][0]["original_shares"], "1")
        self.assertEqual(candidate["amount_candidates"][0]["original_cash_cny"], "0.42")
        self.assertTrue(candidate["requires_review"])

    def test_attachment_whitelist_and_proposal_is_never_implementation(self):
        self.assertTrue(
            attachment_url("finalpage/2026-07-01/12345.PDF").startswith(
                "https://static.cninfo.com.cn/"
            )
        )
        for path in ("https://evil/PDF", "../finalpage/a.PDF", "finalpage/a/b.PDF?token=1"):
            with self.assertRaises(ValueError):
                attachment_url(path)
        text = "股权登记日：2026年7月15日；现金红利发放日：2026年7月16日；每10股派发现金红利4.20元"
        candidate = extract_candidates(text, "2025年度权益分派实施公告")
        self.assertEqual(candidate["dates"]["record_date"], "2026-07-15")
        self.assertEqual(candidate["dates"]["pay_date"], "2026-07-16")
        self.assertEqual(candidate["amount_candidates"][0]["original_cash_cny"], "4.20")
        self.assertTrue(candidate["requires_review"])
        self.assertEqual(
            extract_candidates(text, "证券代码及证券简称变更公告")["stage"], "implementation"
        )
        self.assertEqual(extract_candidates(text, "权益分派预案摘要")["stage"], "unverified")

    def test_disclosure_institution_is_required_and_exchange_is_verified(self):
        from jfquant_collectors.announcements import security_identity

        table = [{"code": "600000", "category": "A股", "orgId": "gssh0600000"}]
        with patch("jfquant_collectors.announcements.security_directory", return_value=table):
            self.assertEqual(security_identity("SH600000"), "gssh0600000")
            with self.assertRaises(ValueError):
                security_identity("SZ600000")
            with self.assertRaises(ValueError):
                security_identity("SH600001")
        with (
            patch("jfquant_collectors.announcements.security_directory", return_value=table * 2),
            self.assertRaises(ValueError),
        ):
            security_identity("SH600000")

    def test_numeric_institution_id_is_opaque_but_security_and_venue_still_match(self):
        from jfquant_collectors.announcements import security_identity

        table = [{"code": "600025", "category": "A股", "orgId": "9900030360"}]
        with patch("jfquant_collectors.announcements.security_directory", return_value=table):
            self.assertEqual(security_identity("SH600025"), "9900030360")
            with self.assertRaises(ValueError):
                security_identity("SZ600025")
        for org in ["gssz0600025", "unreviewed-id"]:
            with patch(
                "jfquant_collectors.announcements.security_directory",
                return_value=[{**table[0], "orgId": org}],
            ), self.assertRaises(ValueError):
                security_identity("SH600025")

    def test_identity_and_truncated_pagination_fail_without_completing(self):
        request = {
            "cache_dir": "/unused/cache",
            "instruments": ["SH600000"],
            "start": "2020-01-01",
            "end": "2026-10-13",
        }
        for value, message in (
            ({"totalAnnouncement": 2, "announcements": []}, "truncated"),
            (
                {"totalAnnouncement": 1, "announcements": [{"secCode": "000001"}]},
                "security conflict",
            ),
        ):

            class Response:
                status_code = 200

                def raise_for_status(self):
                    pass

                def json(self, value=value):
                    return value

            emitted = []
            with (
                patch(
                    "jfquant_collectors.announcements.security_identity", return_value="gssh0600000"
                ),
                patch("jfquant_collectors.announcements.requests.post", return_value=Response()),
                patch("jfquant_collectors.providers.time.sleep"),
                self.assertRaisesRegex(ValueError, message),
            ):
                collect(request, lambda *a, emitted=emitted, **kw: emitted.append((a, kw)))
            self.assertFalse(any(a[0] == "complete" for a, _ in emitted))


class AnnouncementHttp(unittest.TestCase):
    def test_local_pdf_capture_pagination_and_empty_extraction_remain_candidates(self):
        import io
        import json
        import tempfile
        import threading
        from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
        from pathlib import Path

        import requests
        from pypdf import PdfWriter

        from jfquant_collectors.evidence import EvidenceCapture

        writer = PdfWriter()
        writer.add_blank_page(width=72, height=72)
        stream = io.BytesIO()
        writer.write(stream)
        pdf = stream.getvalue()

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_POST(self):
                self.rfile.read(int(self.headers.get("Content-Length", "0")))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(
                    json.dumps(
                        {
                            "totalAnnouncement": 1,
                            "announcements": [
                                {
                                    "secCode": "600000",
                                    "orgId": "gssh0600000",
                                    "announcementId": "a",
                                    "announcementTitle": "权益分派实施公告",
                                    "announcementTime": 1717200000000,
                                    "adjunctUrl": "finalpage/2024-06-01/a.PDF",
                                }
                            ],
                        }
                    ).encode()
                )

            def do_GET(self):
                self.send_response(200)
                self.end_headers()
                self.wfile.write(pdf)

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        address = "http://127.0.0.1:" + str(server.server_port)
        post, get = requests.post, requests.get

        def local_post(*args, **kwargs):
            return post(address, **kwargs)

        def local_get(url, **kwargs):
            result = get(address, **kwargs)
            result.url = url
            return result

        try:
            with tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                capture = EvidenceCapture(root / "raw")
                emitted = []
                with (
                    patch(
                        "jfquant_collectors.announcements.security_identity",
                        return_value="gssh0600000",
                    ),
                    patch("requests.post", side_effect=local_post),
                    patch("requests.get", side_effect=local_get),
                    patch("jfquant_collectors.providers.time.sleep"),
                    capture.enabled(),
                ):
                    collect(
                        {
                            "cache_dir": str(root / "cache"),
                            "instruments": ["SH600000"],
                            "start": "2024-01-01",
                            "end": "2024-12-31",
                        },
                        lambda *a, **kw: emitted.append((a, kw)),
                    )
                document = next(
                    kw["evidence"]["documents"][0] for a, kw in emitted if a[0] == "action_shard"
                )
                self.assertTrue(document["candidates"]["requires_review"])
                self.assertEqual((root / "raw" / document["text"]).read_text(), "")
                self.assertEqual(len(capture.receipts), 2)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()
