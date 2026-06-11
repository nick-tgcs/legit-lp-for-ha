#!/usr/bin/env python3
"""Tiny routing server for screenshotting the real panel frontend.

Serves the byte-for-byte production asset (scheduler/assets/index.html) and
routes its relative API calls to the committed demo report / SVG so the panel
renders exactly as it would in HA, with no live backend.
"""
import json
import pathlib
import sys
from http.server import BaseHTTPRequestHandler, HTTPServer

HERE = pathlib.Path(__file__).parent
INDEX = (HERE / "../../scheduler/assets/index.html").resolve()
REPORT = json.loads((HERE / "demo-report.json").read_text())
SVG = (HERE / "horizon.svg").read_text()


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def _send(self, body: bytes, ctype: str):
        self.send_response(200)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = self.path.split("?", 1)[0]
        if path in ("/", "/index.html"):
            self._send(INDEX.read_bytes(), "text/html")
        elif path == "/api/status":
            self._send(json.dumps(REPORT).encode(), "application/json")
        elif path == "/horizon.svg":
            self._send(SVG.encode(), "image/svg+xml")
        elif path == "/api/events":
            # SSE endpoint the panel subscribes to; keep it open & silent.
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.end_headers()
        else:
            self.send_error(404)


if __name__ == "__main__":
    port = int(sys.argv[1]) if len(sys.argv) > 1 else 8765
    HTTPServer(("127.0.0.1", port), H).serve_forever()
