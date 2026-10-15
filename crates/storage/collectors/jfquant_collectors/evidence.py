"""Capture decoded upstream HTTP response bodies inside the isolated collector process."""

import contextlib
import hashlib
import json
from pathlib import Path
from unittest.mock import patch
from urllib.parse import parse_qsl, urlencode, urlsplit, urlunsplit


class EvidenceCapture:
    def __init__(self, root):
        self.root = Path(root)
        self.root.mkdir(parents=True, exist_ok=True)
        self.receipts = []

    def response(self, response):
        original = str(response.url)
        parts = urlsplit(original)
        if "getcrumb" in parts.path.lower() or parts.hostname == "fc.yahoo.com":
            return  # Authentication receipts are neither research evidence nor safe diagnostics.
        query = [
            (k, v)
            for k, v in parse_qsl(parts.query)
            if k.lower() not in ("crumb", "token", "apikey")
        ]
        url = urlunsplit((parts.scheme, parts.netloc, parts.path, urlencode(query), ""))
        body = bytes(response.content)
        if parts.hostname == "api.tushare.pro":
            request_body = getattr(getattr(response, "request", None), "body", None)
            if request_body:
                try:
                    credential = json.loads(request_body).get("token", "")
                except (ValueError, TypeError):
                    credential = ""
                if credential and credential.encode() in body:
                    raise ValueError("authenticated_response_contains_credential")
        digest = hashlib.sha256(body).hexdigest()
        key = hashlib.sha256(url.encode()).hexdigest()[:16] + "-" + digest[:16]
        filename = "http-" + key + ".body"
        path = self.root / filename
        if not path.exists():
            path.write_bytes(body)
        self.receipts.append(
            {
                "url": url,
                "status": response.status_code,
                "filename": filename,
                "sha256": "sha256:" + digest,
                "kind": "http_response_body_decoded",
            }
        )

    @contextlib.contextmanager
    def enabled(self):
        import requests
        from curl_cffi import requests as curl_requests

        request_send = requests.Session.send
        curl_request = curl_requests.Session.request
        capture = self

        def send(session, *args, **kwargs):
            response = request_send(session, *args, **kwargs)
            capture.response(response)
            return response

        def request(session, *args, **kwargs):
            response = curl_request(session, *args, **kwargs)
            capture.response(response)
            return response

        with (
            patch.object(requests.Session, "send", send),
            patch.object(curl_requests.Session, "request", request),
        ):
            yield
