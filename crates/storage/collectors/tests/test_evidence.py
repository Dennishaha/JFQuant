"""Local HTTP transport fixtures exercise evidence, limits and timeout behavior."""

import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

import requests

from jfquant_collectors.evidence import EvidenceCapture
from jfquant_collectors.providers import request_call


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass

    def do_GET(self):
        if self.path.startswith("/slow"):
            time.sleep(0.1)
        self.send_response(429 if self.path.startswith("/limited") else 200)
        self.end_headers()
        try:
            self.wfile.write(b'{"value": 1}')
        except BrokenPipeError:
            pass


class HttpEvidence(unittest.TestCase):
    def setUp(self):
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = "http://127.0.0.1:" + str(self.server.server_port)

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join()

    def test_http_body_is_hashed_and_auth_query_is_redacted(self):
        with tempfile.TemporaryDirectory() as directory:
            capture = EvidenceCapture(directory)
            with capture.enabled():
                response = requests.get(
                    self.url + "/quote?token=private-fixture&symbol=600000", timeout=1
                )
            self.assertEqual(response.json(), {"value": 1})
            self.assertEqual(len(capture.receipts), 1)
            receipt = capture.receipts[0]
            self.assertNotIn("private-fixture", receipt["url"])
            self.assertNotIn("token", receipt["url"])
            self.assertEqual(receipt["kind"], "http_response_body_decoded")
            self.assertEqual(len(receipt["sha256"]), 71)

    def test_only_public_cninfo_form_is_bound_to_query_receipt(self):
        with tempfile.TemporaryDirectory() as directory:
            capture = EvidenceCapture(directory)
            response = requests.Response()
            response.status_code = 200
            response.url = "https://www.cninfo.com.cn/new/hisAnnouncement/query"
            response._content = b'{"totalAnnouncement":0,"announcements":[]}'
            form = {
                "stock": "600000,gssh0600000",
                "column": "sse",
                "seDate": "2024-01-01~2024-12-31",
                "searchkey": "",
                "tabName": "fulltext",
                "pageNum": "1",
                "pageSize": "30",
            }
            response.request = requests.Request("POST", response.url, data=form).prepare()
            capture.response(response)
            self.assertEqual(capture.receipts[-1]["query_request"], form)
            response.request = requests.Request(
                "POST", response.url, data={**form, "api_key": "private-key"}
            ).prepare()
            capture.response(response)
            self.assertNotIn("query_request", capture.receipts[-1])
            self.assertNotIn("private-key", str(capture.receipts))

    def test_rate_limits_retry_boundedly_and_timeout_is_not_empty_success(self):
        calls = []

        def fetch():
            calls.append(1)
            response = requests.get(self.url + "/limited", timeout=1)
            response.raise_for_status()

        with (
            patch("jfquant_collectors.providers.time.sleep"),
            self.assertRaises(requests.HTTPError),
        ):
            request_call(fetch)
        self.assertEqual(len(calls), 3)
        with self.assertRaises(requests.Timeout):
            requests.get(self.url + "/slow", timeout=0.02)
