#!/usr/bin/env python3
"""Fake Ubuntu mirror for testing apt-install-unstuck.

  slow  proxy requests to --upstream, but send the body at --rate bytes/s
  hang  accept the connection, read the request, never answer

Example:
  slow-mirror.py --mode slow --rate 2048 --upstream http://archive.ubuntu.com
  (then use http://127.0.0.1:8080/ubuntu as the mirror)
"""

import argparse
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

HOP_BY_HOP = {"connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade"}


def make_handler(args):
    class Handler(BaseHTTPRequestHandler):
        protocol_version = "HTTP/1.1"

        def log_message(self, fmt, *a):
            sys.stderr.write("[%s] %s\n" % (args.mode, fmt % a))
            sys.stderr.flush()

        def do_HEAD(self):
            self.handle_request(send_body=False)

        def do_GET(self):
            self.handle_request(send_body=True)

        def handle_request(self, send_body):
            if args.mode == "hang":
                self.log_message("hanging on %s", self.path)
                threading.Event().wait()
                return

            req = urllib.request.Request(args.upstream.rstrip("/") + self.path, method=self.command)
            req.add_header("User-Agent", self.headers.get("User-Agent", "slow-mirror"))
            try:
                resp = urllib.request.urlopen(req, timeout=30)
            except urllib.error.HTTPError as e:
                resp = e
            except OSError as e:
                self.send_error(502, str(e))
                return

            with resp:
                self.send_response(resp.status)
                for k, v in resp.headers.items():
                    if k.lower() not in HOP_BY_HOP:
                        self.send_header(k, v)
                self.end_headers()
                if not send_body:
                    return
                chunk = max(1, args.rate // 10)
                try:
                    while True:
                        data = resp.read(chunk)
                        if not data:
                            break
                        self.wfile.write(data)
                        self.wfile.flush()
                        time.sleep(len(data) / args.rate)
                except (BrokenPipeError, ConnectionResetError):
                    pass

    return Handler


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--mode", choices=["slow", "hang"], required=True)
    p.add_argument("--rate", type=int, default=2048, help="bytes per second in slow mode")
    p.add_argument("--upstream", default="http://archive.ubuntu.com", help="real mirror (scheme and host)")
    p.add_argument("--bind", default="127.0.0.1")
    p.add_argument("--port", type=int, default=8080)
    args = p.parse_args()
    if args.rate < 1:
        p.error("--rate must be positive")

    server = ThreadingHTTPServer((args.bind, args.port), make_handler(args))
    server.daemon_threads = True
    print("listening on http://%s:%d (%s)" % (args.bind, args.port, args.mode), flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
