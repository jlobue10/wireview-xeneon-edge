"""Regression checks for the bridge and shared reader hardening.

Runs on Linux or macOS with the standard library (Pillow optional, for the
Nexus argument checks). Windows-only pieces (HWiNFO, HID, serial) are stubbed;
no hardware is touched. Usage:  python tests/test_hardening.py

Exit status is the number of failed checks. The cases mirror the findings of
the 2026-09-26 audits (WV-02..WV-06 and the freshness / non-finite notes).
"""
import json, math, os, shutil, socket, struct, subprocess, sys, tempfile, threading, time, types, urllib.request
from http.server import BaseHTTPRequestHandler, HTTPServer
from pathlib import Path

EDGE = Path(__file__).resolve().parents[1]
NEXUS = EDGE.parent / "wireview-nexus"          # sibling clone, optional
results = {}

def check(name, cond, detail=""):
    results[name] = ("PASS" if cond else "FAIL") + (f"  ({detail})" if detail else "")

def free_port():
    s = socket.socket(); s.bind(("127.0.0.1", 0)); p = s.getsockname()[1]; s.close(); return p

# ---------------- A. HWiNFO parser bounds (pure function) ----------------
sys.path.insert(0, str(EDGE / "bridge"))
import hwinfo_wireview as hw

def block(s_off, s_sz, s_n, r_off, r_sz, r_n, total, sig=b"HWiS"):
    buf = bytearray(total)
    struct.pack_into(hw._HEADER_FMT, buf, 0, sig, 1, 1, int(time.time()), s_off, s_sz, s_n, r_off, r_sz, r_n)
    return bytes(buf)

def reader_for(buf):
    calls = []
    def read(off, n):
        calls.append((off, n))
        assert 0 <= off and off + n <= len(buf), f"parser asked for bytes outside the mapping: {off}+{n} > {len(buf)}"
        return buf[off:off + n]
    return read, calls

def expect_unavailable(name, buf, total=None):
    read, calls = reader_for(buf)
    try:
        hw.parse_shared_memory(read, len(buf) if total is None else total)
        check(name, False, "parsed without error")
    except hw.HwinfoUnavailable as e:
        check(name, True, str(e)[:70])
    except AssertionError as e:
        check(name, False, str(e))

expect_unavailable("A1 truncated header", b"HWiS" + b"\0" * 10)
expect_unavailable("A2 sensor array outside mapping (Astra WV-02 case)", block(65536, 264, 1, 0, 316, 0, 4096))
expect_unavailable("A3 zero record size", block(44, 0, 5, 44, 316, 0, 4096))
expect_unavailable("A4 excessive count", block(44, 264, 10**8, 44, 316, 0, 4096))
expect_unavailable("A5 overflow-ish offset", block(2**32 - 1, 264, 1, 44, 316, 0, 4096))
expect_unavailable("A6 wrong signature", block(44, 264, 0, 44, 316, 0, 4096, sig=b"XXXX"))
# valid: one WireView sensor, one Total Current reading
tot = 44 + 264 + 316
buf = bytearray(block(44, 264, 1, 44 + 264, 316, 1, tot))
buf[44 + 8:44 + 8 + 8] = b"WireView"
r0 = 44 + 264
struct.pack_into("<III", buf, r0, 1, 0, 7); buf[r0 + 12:r0 + 12 + 13] = b"Total Current"; struct.pack_into("<dddd", buf, r0 + 284, 12.5, 0, 0, 0)
read, calls = reader_for(bytes(buf))
sm = hw.parse_shared_memory(read, tot)
check("A7 valid block parses", sm["sensors"][0]["name"] == "WireView" and sm["readings"][0]["value"] == 12.5 and len(calls) == 3, f"reads={calls}")

# ---------------- B. shaping / freshness (pure) ----------------
sys.modules.pop("hwinfo_wireview", None)
import wireview_source as ws
nan_reply = {"ok": True, "poll_time": time.time(), "total_current": float("nan"), "total_power": 1.0,
             "pins": [{"current": 1.0}] * 6}
check("B1 NaN total rejected", ws._shape_bridge(nan_reply)["ok"] is False)
inf_pin = dict(nan_reply, total_current=1.0, pins=[{"current": float("inf")}] + [{"current": 1.0}] * 5)
check("B2 inf pin rejected", ws._shape_bridge(inf_pin)["ok"] is False)
check("B3 huge int no exception", ws._num(10**400) is None and ws._num(True) is None)
five = dict(nan_reply, total_current=1.0, pins=[{"current": 1.0}] * 5)
check("B4 five pins not ok", ws._shape_bridge(five)["ok"] is False)
good = dict(nan_reply, total_current=1.0, cable_w=999, faults={"bogus": True, "over_power": 1})
g = ws._shape_bridge(good)
check("B5 good reply ok, bad cable/fault filtered", g["ok"] and g["cable_w"] is None and g["faults"] == {"over_power": True})
stale = ws._enforce_fresh({"ok": True, "source": "x", "age_s": 86400.0})
check("B6 one-day-old sample not ok", stale["ok"] is False and stale["status"] == "Stale readings")
check("B7 fresh sample ok", ws._enforce_fresh({"ok": True, "source": "x", "age_s": 0.2})["ok"] is True)

# ---------------- C. live bridge ----------------
work = Path(tempfile.mkdtemp())
shutil.copytree(EDGE / "bridge", work / "bridge"); shutil.copytree(EDGE / "docs", work / "docs")
(work / "bridge" / "hwinfo_wireview.py").write_text('''
import time, os
class HwinfoUnavailable(RuntimeError): pass
def read_wireview():
    age = float(os.environ.get("STUB_AGE", "0"))
    return {"ok": True, "hwinfo_running": True, "device_found": True, "poll_time": time.time() - age, "age_s": age,
            "total_current": 12.5, "total_power": 150.0, "avg_voltage": 12.0, "cable_w": 600,
            "pins": [{"n": n, "voltage": 12.0, "current": 2.0, "power": 24.0} for n in range(1, 7)], "faults": {}, "faults_logged": {},
            "device": {"port": "COM5", "fw": 5, "uid": "SECRETUID", "build": "x"}}
''')
secret_file = work / "bridge.secret"
env = dict(os.environ, WIREVIEW_BRIDGE_SECRET=str(secret_file), PYTHONUNBUFFERED="1")
(work / "docs" / "nested").mkdir()
outside = work / "OUTSIDE_MARKER.txt"; outside.write_text("outside")
try:
    os.symlink(outside, work / "docs" / "nested" / "index.html")
    have_symlink = True
except (OSError, NotImplementedError):
    have_symlink = False

port = free_port()
proc = subprocess.Popen([sys.executable, str(work / "bridge" / "wireview_bridge.py"), "--port", str(port), "--source", "hwinfo"],
                        env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
time.sleep(1.5)
base = f"http://127.0.0.1:{port}"
def get(path, headers=None):
    req = urllib.request.Request(base + path, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=3) as r:
            return r.status, dict(r.headers), r.read()
    except urllib.error.HTTPError as e:
        return e.code, dict(e.headers), e.read()

check("C1 secret file created by bridge", secret_file.exists() and len(secret_file.read_bytes()) >= 32)
st, hd, body = get("/api/wireview")
check("C2 plain request has no auth header", st == 200 and ws.AUTH_HEADER not in hd and b"SECRETUID" not in body)
st, hd, body = get("/api/wireview?nonce=" + "ab" * 16)
tag = hd.get(ws.AUTH_HEADER)
check("C3 nonce request signed correctly", st == 200 and tag == ws.bridge_sign(secret_file.read_bytes().strip(), "ab" * 16, body))
st, hd, body = get("/api/wireview?nonce=ZZ")
check("C4 invalid nonce not signed", st == 200 and ws.AUTH_HEADER not in hd)
if have_symlink:
    st, _, body = get("/nested/")
    check("C5 index.html symlink escape blocked (Astra WV-06)", st == 403, f"status {st}")
else:
    results["C5 index.html symlink escape"] = "SKIP (no symlink support)"
st, _, _ = get("/per-wire/")
check("C6 normal directory index still served", st == 200)
st, _, _ = get("/api/wireview", {"Origin": "https://evil.example.com"})
check("C7 CORS allowlist still enforced", st == 403)

# nexus client against the real bridge (same secret) and against a fake
os.environ["WIREVIEW_BRIDGE_SECRET"] = str(secret_file)
ws._bridge.next_try = 0.0
r = ws._read_bridge(f"http://localhost:{port}/api/wireview")
check("C8 nexus client accepts authenticated bridge", r is not None and r["ok"] and r["source"] == "bridge", str(ws._bridge.last_error))

class Fake(BaseHTTPRequestHandler):
    mode = "unauth"
    def log_message(self, *a): pass
    def do_GET(self):
        body = json.dumps({"ok": True, "poll_time": time.time(), "total_current": 0.0, "total_power": 0.0,
                           "pins": [{"current": 0.0}] * 6}).encode()
        if self.mode == "slow":
            self.send_response(200); self.send_header("Content-Length", str(len(body) + 40)); self.end_headers()
            for _ in range(40):
                self.wfile.write(b" "); self.wfile.flush(); time.sleep(0.1)
            self.wfile.write(body); return
        self.send_response(200); self.send_header("Content-Length", str(len(body))); self.end_headers(); self.wfile.write(body)

fport = free_port()
fake = HTTPServer(("127.0.0.1", fport), Fake); threading.Thread(target=fake.serve_forever, daemon=True).start()
ws._bridge.next_try = 0.0
r = ws._read_bridge(f"http://127.0.0.1:{fport}/api/wireview")
check("C9 unauthenticated impersonator rejected (Astra WV-03)", r is None and "authentication" in (ws._bridge.last_error or ""), str(ws._bridge.last_error))
Fake.mode = "slow"; ws._bridge.next_try = 0.0
t0 = time.monotonic(); r = ws._read_bridge(f"http://127.0.0.1:{fport}/api/wireview"); dt = time.monotonic() - t0
check("C10 trickling server cut off by deadline (Astra WV-04)", r is None and dt < 2.0, f"{dt:.2f}s, err={ws._bridge.last_error}")
fake.shutdown()

# worker cap: 40 idle connections must not create 40 threads
idle = []
for _ in range(40):
    s = socket.create_connection(("127.0.0.1", port)); s.sendall(b"GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:" + str(port).encode() + b"\r\n"); idle.append(s)
time.sleep(0.5)
if Path(f"/proc/{proc.pid}/task").is_dir():
    tcount = len(list(Path(f"/proc/{proc.pid}/task").iterdir()))
    check("C11 worker cap holds (Astra WV-05)", tcount <= 32 + 6, f"{tcount} threads for 40 half-open connections")
else:
    results["C11 worker cap"] = "SKIP (no /proc)"
for s in idle: s.close()

proc.terminate(); out = proc.communicate(timeout=5)[0]
check("C12 bridge started on both loopback families", "127.0.0.1" in out and "[::1]" in out, out.strip().splitlines()[0][:80])

# stale via bridge
env2 = dict(env, STUB_AGE="30")
p2 = subprocess.Popen([sys.executable, str(work / "bridge" / "wireview_bridge.py"), "--port", str(port), "--source", "hwinfo", "--no-static"],
                      env=env2, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
time.sleep(1.2)
st, _, body = get("/api/wireview"); d = json.loads(body)
check("C13 stale source reported not ok", st == 200 and d["ok"] is False and d["status"] == "Stale readings", d.get("status"))
p2.terminate(); p2.wait(timeout=5)

# IPv6 collision: occupy [::1]:port first
port6 = free_port()
occ = socket.socket(socket.AF_INET6); occ.bind(("::1", port6)); occ.listen(1)
p3 = subprocess.run([sys.executable, str(work / "bridge" / "wireview_bridge.py"), "--port", str(port6), "--source", "hwinfo", "--no-static"],
                    env=env, capture_output=True, text=True, timeout=10)
check("C14 IPv6 collision refuses to start (Astra WV-03b)", p3.returncode != 0 and "cannot bind ::1" in (p3.stdout + p3.stderr), (p3.stdout + p3.stderr).strip()[-120:])
occ.close()

# ---------------- D. nexus daemon argument validation ----------------
(work / "stubs").mkdir(); (work / "stubs" / "hid.py").write_text("class device: pass\ndef enumerate(*a): return []\n")
try:
    import PIL  # noqa
    have_pil = True
except ImportError:
    have_pil = False
if have_pil and NEXUS.is_dir():
    for bad in (["--wire-limit", "-1"], ["--total-limit", "nan"], ["--fps", "0"], ["--cable-w", "inf"]):
        p = subprocess.run([sys.executable, str(NEXUS / "nexus_wireview.py"), *bad, "--preview", "/dev/null", "--demo"],
                           capture_output=True, text=True, env={**os.environ, "PYTHONPATH": str(work / "stubs")})
        check(f"D1 rejects {' '.join(bad)}", p.returncode == 2 and "positive finite" in p.stderr)
    p = subprocess.run([sys.executable, str(NEXUS / "nexus_wireview.py"), "--version"], capture_output=True, text=True, env={**os.environ, "PYTHONPATH": str(work / "stubs")})
    check("D2 --version", "1.0.1" in p.stdout)
else:
    results["D  nexus arg checks"] = "SKIP (needs Pillow and a sibling wireview-nexus clone)"

shutil.rmtree(work)
w = max(len(k) for k in results)
for k, v in results.items():
    print(f"{k:<{w}}  {v}")
failures = sum(1 for v in results.values() if v.startswith("FAIL"))
print("\nFAILURES:", failures)
sys.exit(failures)
