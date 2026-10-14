"""Independent coefficient source fixtures and local HTTP; never depend on public network."""

import json
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

import requests

from jfquant_collectors import adjustment
from jfquant_collectors.evidence import EvidenceCapture
from jfquant_collectors.providers import request_call


def response(points):
    return (
        "var sh600000hfq="
        + json.dumps({"total": len(points), "data": points})
        + "\n/* inert source comment */"
    )


class SafeCoefficients(unittest.TestCase):
    def test_exact_decimal_and_source_order_are_preserved_without_eval(self):
        rows = [
            {"d": "2026-07-16", "f": "17.3916341988312000"},
            {"d": "1900-01-01", "f": "1.0000000000000000"},
        ]
        with patch("builtins.eval", side_effect=AssertionError("executed response")):
            parsed = adjustment.parse_response("SH600000", response(rows), "2026-10-13")
        self.assertEqual(parsed["basis"]["points"][0]["factor"], "1.0000000000000000")
        self.assertEqual(parsed["basis"]["points"][1]["factor"], "17.3916341988312000")
        self.assertEqual(rows[0]["d"], "2026-07-16")
        self.assertEqual(parsed["instrument_id"], "SH600000")

    def test_security_duplicate_invalid_value_and_executable_text_are_rejected(self):
        valid = response([{"d": "1900-01-01", "f": "1"}])
        for bad in [
            valid.replace("sh600000hfq", "sz000001hfq"),
            valid + "; alert(1)",
            valid + " /* unclosed",
            'var sh600000hfq=__import__("os")',
            'var sh600000hfq={"data":[],"data":[{"d":"1900-01-01","f":"1"}]}',
            response([{"d": "1900-01-01", "f": "1"}] * 2),
            response([]),
            valid.replace('"total": 1', '"total": 2'),
            valid.replace('"total": 1', '"total": 1, "extra": NaN'),
        ]:
            with self.subTest(bad=bad), self.assertRaises((ValueError, json.JSONDecodeError)):
                adjustment.parse_response("SH600000", bad, "2026-10-13")
        for coefficient in [
            "0",
            "-1",
            "NaN",
            "Infinity",
            "1e9999",
            "1e-9999",
            '__import__("os")',
            " 1",
        ]:
            with self.subTest(coefficient=coefficient), self.assertRaises(ValueError):
                adjustment.parse_response(
                    "SH600000", response([{"d": "1900-01-01", "f": coefficient}]), "2026-10-13"
                )
        with self.assertRaises(ValueError):
            adjustment.parse_response(
                "SH600000", response([{"d": "2020-02-30", "f": "1"}]), "2026-10-13"
            )


class FactorHttp(unittest.TestCase):
    def test_rate_limited_attempts_are_spaced_and_evidence_retains_each_body(self):
        starts = []
        attempts = []
        body = response([{"d": "1900-01-01", "f": "1"}]).encode()

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def do_GET(self):
                starts.append(time.monotonic())
                self.send_response(429 if len(starts) == 1 else 200)
                self.end_headers()
                self.wfile.write(body)

        server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        url = "http://127.0.0.1:" + str(server.server_port)
        actual_get = requests.get

        def attempt(*args, **kwargs):
            attempts.append(time.monotonic())
            return actual_get(url, **kwargs)

        try:
            with tempfile.TemporaryDirectory() as directory:
                capture = EvidenceCapture(directory)
                with (
                    patch("jfquant_collectors.adjustment.LAST_REQUEST", 0.0),
                    patch("requests.get", side_effect=attempt),
                    capture.enabled(),
                ):
                    result = adjustment.fetch(
                        "SH600000", "2026-10-13", lambda *a, **kw: None, request_call
                    )
                self.assertEqual(result["basis"]["source"], "sina_hfq")
                self.assertEqual(len(starts), 2)
                # Rate limiting constrains client attempt starts; server arrival also includes
                # thread scheduling/network latency and is not an accurate clock for this rule.
                self.assertGreaterEqual(attempts[1] - attempts[0], 0.98)
                self.assertEqual([r["status"] for r in capture.receipts], [429, 200])
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_connection_failure_and_timeout_are_bounded_without_fallback(self):
        for error in (requests.Timeout("fixture"), requests.ConnectionError("fixture")):
            with (
                patch("requests.get", side_effect=error) as get,
                patch("jfquant_collectors.providers.time.sleep"),
                self.assertRaises(type(error)),
            ):
                adjustment.fetch("SH600000", "2026-10-13", lambda *a, **kw: None, request_call)
            self.assertEqual(get.call_count, 3)
        redirect = requests.Response()
        redirect.status_code = 302
        with patch("requests.get", return_value=redirect) as get, self.assertRaises(ValueError):
            adjustment.fetch("SH600000", "2026-10-13", lambda *a, **kw: None, request_call)
        self.assertFalse(get.call_args.kwargs["allow_redirects"])
