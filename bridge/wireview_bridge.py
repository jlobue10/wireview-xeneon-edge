"""Local bridge: serves WireView Pro II readings as JSON on localhost.

The Xeneon Edge widgets (hosted on GitHub Pages or served from this bridge)
poll ``http://localhost:8765/api/wireview`` once a second. The bridge reads
the WireView directly over USB serial (falling back to HWiNFO shared memory),
caches for a short interval, and answers CORS requests only from the widget
origins it knows (its own loopback origin and the GitHub Pages copy), so an
arbitrary website open in a browser cannot read the readings. Requests whose
``Host`` header is not a loopback name are refused, which also defeats DNS
rebinding. The device's hardware UID is not served.

Programs that share the device through this bridge (wireview-nexus) send a
``nonce`` query parameter; the reply carries ``X-WireView-Auth``, an HMAC over
nonce and body keyed with a per-user secret file, so a client can tell this
bridge from any other process that happens to own the port.

Usage::

    python wireview_bridge.py               # port 8765, serves ../docs too
    python wireview_bridge.py --port 9000
    python wireview_bridge.py --no-static   # JSON only
    python wireview_bridge.py --source hwinfo
    python wireview_bridge.py --allow-origin https://example.github.io   # another widget host

Requires pyserial for the direct USB path; standard library otherwise.
"""

from __future__ import annotations

import argparse
import errno
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
from urllib.parse import parse_qs, urlsplit

sys.path.insert(0, str(Path(__file__).resolve().parent))
from wireview_source import (  # noqa: E402
    AUTH_HEADER, SOURCES, bridge_secret, bridge_secret_path, bridge_sign, read_wireview, valid_nonce,
)

VERSION = "1.0.1"
DEFAULT_PORT = 8765
CACHE_SECONDS = 0.25
DOCS_DIR = Path(__file__).resolve().parent.parent / "docs"
PAGES_ORIGIN = "https://jlobue10.github.io"   # the hosted copy of docs/
LOOPBACK_HOSTS = ("localhost", "127.0.0.1", "[::1]")
PRIVATE_KEYS = ("uid",)                       # device fields never served
REQUEST_TIMEOUT_S = 10                        # idle socket timeout per request
MAX_WORKERS = 32                              # concurrent connections
MAX_STATIC_BYTES = 8 * 1024 * 1024

# errno values meaning "this address family is not available here", for which
# skipping the IPv6 listener is correct. Anything else (address in use!) is not.
_AF_UNAVAILABLE = {
    getattr(errno, name) for name in ("EAFNOSUPPORT", "EADDRNOTAVAIL", "EPFNOSUPPORT", "EPROTONOSUPPORT") if hasattr(errno, name)
} | {10047, 10049, 10043}   # WSAEAFNOSUPPORT, WSAEADDRNOTAVAIL, WSAEPROTONOSUPPORT


class _Cache:
    source = "auto"
    serial_port: str | None = None

    def __init__(self) -> None:
        self._lock = threading.Lock()
        self._at = 0.0
        self._data: dict = {}
        self._last_source: str | None = None

    def get(self) -> dict:
        with self._lock:
            now = time.monotonic()
            if now - self._at > CACHE_SECONDS:
                try:
                    data = read_wireview(self.source, self.serial_port, bridge_url=None)
                except Exception as e:  # a reader bug must not take the server down
                    data = {"ok": False, "source": self.source, "status": "Reader error", "hint": str(e)[:200], "error": str(e)}
                dev = data.get("device")
                if isinstance(dev, dict):
                    data["device"] = {k: v for k, v in dev.items() if k not in PRIVATE_KEYS}
                self._data = data
                src = self._data["source"] if self._data["ok"] else f"none ({self._data.get('status')}: {self._data.get('hint')})"
                if src != self._last_source:
                    dev = self._data.get("device") or {}
                    extra = f" on {dev.get('port')} fw v{dev.get('fw')}" if dev else ""
                    print(f"readings: {src}{extra}", flush=True)
                    self._last_source = src
                self._data["served_at"] = time.time()
                self._at = now
            return self._data


_cache = _Cache()


class Handler(BaseHTTPRequestHandler):
    server_version = f"WireViewBridge/{VERSION}"
    sys_version = ""                       # do not advertise the Python version
    timeout = REQUEST_TIMEOUT_S            # a peer that stops sending is dropped
    static_root: Path | None = DOCS_DIR
    allowed_origins: frozenset[str] = frozenset()   # filled in by main()
    allowed_hosts: frozenset[str] | None = None     # None = any (non-loopback bind)
    secret: bytes | None = None                     # for X-WireView-Auth

    def log_message(self, fmt: str, *args) -> None:  # quiet by default
        if os.environ.get("WIREVIEW_BRIDGE_LOG"):
            super().log_message(fmt, *args)

    # -- access control -------------------------------------------------------
    def _host_ok(self) -> bool:
        """The Host header names this machine (blocks DNS rebinding)."""
        if self.allowed_hosts is None:
            return True
        host = (self.headers.get("Host") or "").strip().lower()
        return host in self.allowed_hosts

    def _origin(self) -> str | None:
        o = self.headers.get("Origin")
        return o.strip().rstrip("/").lower() if o else None

    def _origin_ok(self) -> bool:
        """No Origin (same-origin page, curl, the Nexus daemon) or a known one."""
        o = self._origin()
        return o is None or o in self.allowed_origins

    def _refuse(self, why: str) -> None:
        self.send_error(HTTPStatus.FORBIDDEN, why)

    def _cors(self) -> None:
        o = self._origin()
        if o is not None:   # only ever reached for an allowed origin
            self.send_header("Access-Control-Allow-Origin", o)
            self.send_header("Vary", "Origin")
            self.send_header("Access-Control-Allow-Methods", "GET, OPTIONS")
            self.send_header("Access-Control-Allow-Headers", "*")
            self.send_header("Access-Control-Allow-Private-Network", "true")
        self.send_header("Cache-Control", "no-store")

    def _gate(self) -> bool:
        if not self._host_ok():
            self._refuse("unexpected Host header")
            return False
        if not self._origin_ok():
            self._refuse("origin not allowed")
            return False
        return True

    def do_OPTIONS(self) -> None:
        if not self._gate():
            return
        self.send_response(HTTPStatus.NO_CONTENT)
        self._cors()
        self.end_headers()

    # -- responses ------------------------------------------------------------
    def _send_json(self, body: bytes, nonce: str | None = None) -> None:
        self.send_response(HTTPStatus.OK)
        self._cors()
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        if nonce is not None and self.secret is not None and valid_nonce(nonce):
            self.send_header(AUTH_HEADER, bridge_sign(self.secret, nonce, body))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self) -> None:
        if not self._gate():
            return
        parts = urlsplit(self.path)
        path = parts.path
        if path in ("/api/wireview", "/api/wireview/"):
            nonce = parse_qs(parts.query).get("nonce", [None])[0]
            self._send_json(json.dumps(_cache.get()).encode(), nonce)
            return
        if path == "/api/health":
            self._send_json(b'{"ok":true}')
            return
        self._serve_static(path)

    def _serve_static(self, path: str) -> None:
        root = self.static_root
        if root is None:
            self.send_error(HTTPStatus.NOT_FOUND)
            return
        rel = path.lstrip("/")
        target = root / rel if rel else root
        if target.is_dir():
            target = target / "index.html"
        # Resolve the *final* file (after the index was appended) and require
        # it to live under docs/, so neither ".." nor a symlink escapes.
        target = target.resolve()
        root_r = root.resolve()
        if root_r not in target.parents:
            self.send_error(HTTPStatus.FORBIDDEN)
            return
        if not target.is_file():
            self.send_error(HTTPStatus.NOT_FOUND)
            return
        if target.stat().st_size > MAX_STATIC_BYTES:
            self.send_error(HTTPStatus.REQUEST_ENTITY_TOO_LARGE)
            return
        ctype = mimetypes.guess_type(str(target))[0] or "application/octet-stream"
        data = target.read_bytes()
        self.send_response(HTTPStatus.OK)
        self._cors()
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


class _Server(ThreadingHTTPServer):
    daemon_threads = True
    # On Windows SO_REUSEADDR lets a second copy bind the same port silently,
    # so insist on exclusive use there; elsewhere keep the usual fast restart.
    allow_reuse_address = sys.platform != "win32"
    _slots = threading.BoundedSemaphore(MAX_WORKERS)   # shared by both listeners

    def __init__(self, addr, handler, family):
        self.address_family = family
        super().__init__(addr, handler)

    def server_bind(self) -> None:
        if sys.platform == "win32":
            self.socket.setsockopt(socket.SOL_SOCKET, socket.SO_EXCLUSIVEADDRUSE, 1)
        super().server_bind()

    # Cap concurrent workers: a flood of half-open connections cannot spawn
    # threads without limit. Over the cap, the connection is simply closed.
    def process_request(self, request, client_address) -> None:
        if not self._slots.acquire(blocking=False):
            self.shutdown_request(request)
            return
        try:
            super().process_request(request, client_address)
        except BaseException:
            self._slots.release()
            raise

    def process_request_thread(self, request, client_address) -> None:
        try:
            super().process_request_thread(request, client_address)
        finally:
            self._slots.release()


def main(argv: list[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="WireView Pro II localhost bridge")
    ap.add_argument("--port", type=int, default=DEFAULT_PORT)
    ap.add_argument("--bind", default="127.0.0.1",
                    help="interface to bind (default localhost only; anything else exposes the readings to that network)")
    ap.add_argument("--allow-origin", action="append", default=[], metavar="ORIGIN",
                    help=f"extra web origin allowed to read the API, e.g. https://you.github.io (loopback and {PAGES_ORIGIN} are always allowed; repeatable)")
    ap.add_argument("--no-static", action="store_true", help="do not serve the docs/ widgets, JSON only")
    ap.add_argument("--source", choices=[s for s in SOURCES if s != "bridge"], default="auto", help="serial (direct USB), hwinfo, or auto (default)")
    ap.add_argument("--serial-port", metavar="COMx", default=None, help="WireView COM port (default: auto-detect)")
    ap.add_argument("--version", action="version", version=f"wireview-bridge {VERSION}")
    args = ap.parse_args(argv)

    _Cache.source = args.source
    _Cache.serial_port = args.serial_port

    if args.no_static or not DOCS_DIR.is_dir():
        Handler.static_root = None

    Handler.secret = bridge_secret(create=True)
    if Handler.secret is None:
        print(f"WARNING: cannot create the bridge secret at {bridge_secret_path()}; "
              "other programs will not trust this bridge and will keep the COM port to themselves.", flush=True)

    # Browsers resolve "localhost" to ::1 first on Windows, so listen on both
    # loopback families when binding to the default address.
    binds = [args.bind]
    loopback = args.bind in ("127.0.0.1", "localhost", "::1")
    if args.bind in ("127.0.0.1", "localhost"):
        binds = ["127.0.0.1", "::1"]

    origins = {f"http://{h}:{args.port}" for h in LOOPBACK_HOSTS} | {PAGES_ORIGIN}
    if args.port == 80:
        origins |= {f"http://{h}" for h in LOOPBACK_HOSTS}
    origins |= {o.strip().rstrip("/").lower() for o in args.allow_origin if o.strip()}
    Handler.allowed_origins = frozenset(origins)
    if loopback:
        hosts = {f"{h}:{args.port}" for h in LOOPBACK_HOSTS}
        if args.port == 80:
            hosts |= set(LOOPBACK_HOSTS)
        Handler.allowed_hosts = frozenset(hosts)
    else:
        Handler.allowed_hosts = None
        print(f"WARNING: --bind {args.bind} makes the readings and widgets reachable from that network; "
              "the Host check is off. Prefer the default loopback bind.", flush=True)

    servers = []
    for b in binds:
        family = socket.AF_INET6 if ":" in b else socket.AF_INET
        try:
            srv = _Server((b, args.port), Handler, family)
        except OSError as e:
            if b == "::1" and e.errno in _AF_UNAVAILABLE:
                continue  # IPv6 loopback really is unavailable on this machine
            # Anything else, in particular "address in use" on either family,
            # is a collision: another process would receive some of the
            # traffic meant for this bridge. Refuse to start half-bound.
            for s in servers:
                s.server_close()
            raise SystemExit(f"cannot bind {b}:{args.port}: {e}. Is another bridge (or another program) already listening there?")
        servers.append(srv)
        host = f"[{b}]" if ":" in b else b
        print(f"WireView bridge {VERSION} listening on http://{host}:{args.port}/api/wireview", flush=True)
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


if __name__ == "__main__":
    raise SystemExit(main())
