"""Capture decoded upstream HTTP response bodies inside the isolated collector process."""

import contextlib
import hashlib
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
        query = [
            (k, v)
            for k, v in parse_qsl(parts.query)
            if k.lower() not in ("token", "apikey", "api_key")
        ]
        url = urlunsplit((parts.scheme, parts.netloc, parts.path, urlencode(query), ""))
        body = bytes(response.content)
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

        request_send = requests.Session.send
        capture = self

        def send(session, *args, **kwargs):
            response = request_send(session, *args, **kwargs)
            capture.response(response)
            return response

        with patch.object(requests.Session, "send", send):
            yield
