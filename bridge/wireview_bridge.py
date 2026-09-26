"""Local bridge: serves WireView Pro II readings as JSON on localhost.

The Xeneon Edge widgets (hosted on GitHub Pages or served from this bridge)
poll ``http://localhost:8765/api/wireview`` once a second. The bridge reads
HWiNFO shared memory on demand, caches for a short interval, and answers with
permissive CORS so an https page may fetch it.

Usage::

    python wireview_bridge.py               # port 8765, serves ../docs too
    python wireview_bridge.py --port 9000
    python wireview_bridge.py --no-static   # JSON only

Standard library only.
"""

from __future__ import annotations

import argparse
import json
import mimetypes
import os
import socket
import sys
import threading
import time
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent))
from hwinfo_wireview import read_wireview  # noqa: E402

DEFAULT_PORT = 8765
CACHE_SECONDS = 0.25
DOCS_DIR = Path(__file__).resolve().parent.parent / "docs"


class _Cache:
    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._at = 0.0
        self._data: dict = {}

    def get(self) -> dict:
        with self._lock:
            now = time.monotonic()
            if now - self._at > CACHE_SECONDS:
                self._data = read_wireview()
                self._data["served_at"] = time.time()
                self._at = now
            return self._data


_cache = _Cache()


class Handler(BaseHTTPRequestHandler):
    server_version = "WireViewBridge/1.0"
    static_root: Path | None = DOCS_DIR

    def log_message(self, fmt: str, *args) -> None:  # quiet by default
        if os.environ.get("WIREVIEW_BRIDGE_LOG"):
            super().log_message(fmt, *args)

    def _cors(self) -> None:
        self.send_header("Access-Control-Allow-Origin", "*")
        self.send_header("Access-Control-Allow-Methods", "GET, OPTIONS")
        self.send_header("Access-Control-Allow-Headers", "*")
        self.send_header("Access-Control-Allow-Private-Network", "true")
        self.send_header("Cache-Control", "no-store")

    def do_OPTIONS(self) -> None:
        self.send_response(HTTPStatus.NO_CONTENT)
        self._cors()
        self.end_headers()

    def do_GET(self) -> None:
        path = urlsplit(self.path).path
        if path in ("/api/wireview", "/api/wireview/"):
            body = json.dumps(_cache.get()).encode()
            self.send_response(HTTPStatus.OK)
            self._cors()
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if path == "/api/health":
            body = b'{"ok":true}'
            self.send_response(HTTPStatus.OK)
            self._cors()
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self._serve_static(path)

    def _serve_static(self, path: str) -> None:
        root = self.static_root
        if root is None:
            self.send_error(HTTPStatus.NOT_FOUND)
            return
        rel = path.lstrip("/")
        target = (root / rel).resolve() if rel else root.resolve()
        if root.resolve() not in target.parents and target != root.resolve():
            self.send_error(HTTPStatus.FORBIDDEN)
            return
        if target.is_dir():
            target = target / "index.html"
        if not target.is_file():
            self.send_error(HTTPStatus.NOT_FOUND)
            return
        ctype = mimetypes.guess_type(str(target))[0] or "application/octet-stream"
        data = target.read_bytes()
        self.send_response(HTTPStatus.OK)
        self._cors()
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="WireView Pro II localhost bridge")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--bind", default="127.0.0.1", help="interface to bind (default localhost only)")
    ap.add_argument("--no-static", action="store_true", help="do not serve the docs/ widgets, JSON only")
    args = ap.parse_args(argv)

    if args.no_static or not DOCS_DIR.is_dir():
        Handler.static_root = None

    # Browsers resolve "localhost" to ::1 first on Windows, so listen on both
    # loopback families when binding to the default address.
    binds = [args.bind]
    if args.bind in ("127.0.0.1", "localhost"):
        binds = ["127.0.0.1", "::1"]
    servers = []
    for b in binds:
        family = socket.AF_INET6 if ":" in b else socket.AF_INET
        try:
            srv = _Server((b, args.port), Handler, family)
        except OSError as e:
            if b == "::1":
                continue  # IPv6 loopback disabled on this machine
            raise SystemExit(f"cannot bind {b}:{args.port}: {e}")
        servers.append(srv)
        host = f"[{b}]" if ":" in b else b
        print(f"WireView bridge listening on http://{host}:{args.port}/api/wireview", flush=True)
    if Handler.static_root:
        print(f"Serving widgets from {Handler.static_root} at http://localhost:{args.port}/", flush=True)
    threads = [threading.Thread(target=s.serve_forever, daemon=True) for s in servers]
    for t in threads:
        t.start()
    try:
        while any(t.is_alive() for t in threads):
            time.sleep(0.5)
    except KeyboardInterrupt:
        pass
    finally:
        for s in servers:
            s.shutdown()
    return 0


class _Server(ThreadingHTTPServer):
    daemon_threads = True
    allow_reuse_address = True

    def __init__(self, addr, handler, family):
        self.address_family = family
        super().__init__(addr, handler)


if __name__ == "__main__":
    raise SystemExit(main())
