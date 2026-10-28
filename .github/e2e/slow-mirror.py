#!/usr/bin/env python3
"""Fake slow mirror for E2E tests.

Relays /ubuntu/ and /ubuntu-ports/ to the real archives, sending headers right
away and the body at a fixed low rate, like a degraded mirror.

usage: slow-mirror.py PORT [BYTES_PER_SECOND]
"""

import sys
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

UPSTREAMS = {
    "/ubuntu/": "http://archive.ubuntu.com/ubuntu/",
    "/ubuntu-ports/": "http://ports.ubuntu.com/ubuntu-ports/",
}
RATE = int(sys.argv[2]) if len(sys.argv) > 2 else 2048


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_GET(self):
        self.relay(send_body=True)

    def do_HEAD(self):
        self.relay(send_body=False)

    def relay(self, send_body):
        url = next(
            (base + self.path[len(p):] for p, base in UPSTREAMS.items() if self.path.startswith(p)),
            None,
        )
        if url is None:
            self.send_error(404)
            return
        try:
            resp = urllib.request.urlopen(urllib.request.Request(url, method=self.command), timeout=30)
        except urllib.error.HTTPError as e:
            resp = e
        except Exception:
            self.send_error(502)
            return
        self.send_response(resp.status)
        for name in ("Content-Length", "Content-Type", "Last-Modified"):
            if resp.headers.get(name):
                self.send_header(name, resp.headers[name])
        if not resp.headers.get("Content-Length"):
            self.close_connection = True
        self.end_headers()
        if not send_body:
            return
        step = max(RATE // 10, 1)
        try:
            while chunk := resp.read(step):
                self.wfile.write(chunk)
                self.wfile.flush()
                time.sleep(0.1)
        except (BrokenPipeError, ConnectionResetError):
            self.close_connection = True

    def log_message(self, fmt, *args):
        sys.stderr.write("slow-mirror: " + (fmt % args) + "\n")


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
