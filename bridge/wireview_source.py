"""Pick where WireView Pro II readings come from and return one dict shape.

Sources:

  serial  talk to the device directly over USB (wireview_serial.py). No other
          software is needed, and nothing else may hold the COM port.
  hwinfo  read HWiNFO64 shared memory (hwinfo_wireview.py). Useful when HWiNFO
          already owns the device for other reasons.
  bridge  ask a running wireview-xeneon-edge bridge (localhost JSON). Lets a
          second program share the one device the bridge owns.
  auto    bridge if one answers, else serial if the COM port opens, else hwinfo.

``read_wireview()`` always returns a dict. When readings are missing, ``ok``
is false and ``status``/``hint`` carry a short human-readable explanation.
"""

from __future__ import annotations

import json
import sys
import threading
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any

sys.path.insert(0, str(Path(__file__).resolve().parent))
from hwinfo_wireview import read_wireview as _read_hwinfo  # noqa: E402

try:
    from wireview_serial import WireViewSerial, WireViewSerialError, find_ports
except ImportError:  # pyserial missing: hwinfo only
    WireViewSerial = None  # type: ignore[assignment,misc]
    WireViewSerialError = RuntimeError  # type: ignore[assignment,misc]

    def find_ports() -> list[str]:  # type: ignore[misc]
        return []

SOURCES = ("auto", "serial", "hwinfo", "bridge")
DEFAULT_BRIDGE_URL = "http://localhost:8765/api/wireview"
_RETRY_S = 2.0        # how often to retry opening a busy/missing COM port
_BRIDGE_RETRY_S = 5.0  # how often to look for a bridge that was not answering
_BRIDGE_MAX_BYTES = 64 * 1024
_TEXT_MAX = 200

# Loopback requests must not be sent through an HTTP(S)_PROXY from the
# environment; urllib only skips the proxy when no_proxy says so.
_bridge_opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))


class _SerialState:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.dev: Any = None
        self.port: str | None = None
        self.next_try = 0.0
        self.last_error: str | None = None


_serial = _SerialState()


class _BridgeState:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.next_try = 0.0
        self.last_error: str | None = None


_bridge = _BridgeState()


def _blank(source: str | None) -> dict[str, Any]:
    return {
        "ok": False, "source": source, "device_found": False, "hwinfo_running": False,
        "status": None, "hint": None, "error": None, "poll_time": None, "age_s": None,
        "device": None,
        "pins": [], "total_current": None, "total_power": None, "avg_voltage": None,
        "temp_in": None, "temp_out": None, "temp_ext": [None, None], "vdd": None, "fan_duty": None,
        "cable_w": None, "faults": {}, "faults_logged": {},
    }


def _read_serial(port: str | None) -> dict[str, Any]:
    out = _blank("serial")
    if WireViewSerial is None:
        out.update(status="pyserial missing", hint="pip install pyserial", error="pyserial is not installed")
        return out
    st = _serial
    with st.lock:
        if st.dev is not None and port and st.port != port:
            st.dev.close()
            st.dev = None
        if st.dev is None:
            now = time.monotonic()
            if now < st.next_try:
                out.update(status="WireView not connected", hint=st.last_error or "retrying", error=st.last_error)
                out["device_found"] = bool(find_ports())
                return out
            st.next_try = now + _RETRY_S
            dev = WireViewSerial(port)
            try:
                dev.open()
            except WireViewSerialError as e:
                st.last_error = str(e)
                out["error"] = str(e)
                out["device_found"] = bool(find_ports())
                if not out["device_found"]:
                    out.update(status="WireView not found", hint="plug the WireView Pro II into USB")
                else:
                    out.update(status="COM port busy", hint="close the WireView app (and HWiNFO)")
                return out
            st.dev = dev
            st.port = port
            st.last_error = None
        dev = st.dev
        try:
            data = dev.read()
        except WireViewSerialError as e:
            st.last_error = str(e)
            out["error"] = str(e)
            out["device_found"] = True
            out.update(status="Read failed", hint="reconnecting")
            if not dev.is_open:
                st.dev = None
                st.next_try = time.monotonic() + 0.5
            return out
        now = time.time()
        out.update(data)
        out.update(
            ok=True, device_found=True, poll_time=now, age_s=0.0,
            device={"port": dev.port, "fw": dev.fw_version, "uid": dev.uid, "build": dev.build},
        )
        return out


def _release_serial() -> None:
    with _serial.lock:
        if _serial.dev is not None:
            _serial.dev.close()
            _serial.dev = None


def _num(v: Any) -> float | None:
    return float(v) if isinstance(v, (int, float)) and not isinstance(v, bool) else None


def _text(v: Any) -> str | None:
    return v[:_TEXT_MAX] if isinstance(v, str) else None


def _flags(v: Any) -> dict[str, bool]:
    return {k: bool(x) for k, x in v.items() if isinstance(k, str)} if isinstance(v, dict) else {}


def _shape_bridge(data: Any) -> dict[str, Any]:
    """Coerce a bridge reply into the dict shape, dropping anything odd.

    Whatever answers on the bridge port is another local process; treat its
    JSON as untrusted so a wrong type cannot crash the caller's render loop.
    """
    out = _blank("bridge")
    if not isinstance(data, dict):
        out.update(status="Bad bridge reply", hint="not a JSON object")
        return out
    out["ok"] = data.get("ok") is True
    out["device_found"] = bool(data.get("device_found"))
    out["hwinfo_running"] = bool(data.get("hwinfo_running"))
    for k in ("status", "hint", "error"):
        out[k] = _text(data.get(k))
    for k in ("poll_time", "total_current", "total_power", "avg_voltage", "temp_in", "temp_out", "vdd", "cable_w"):
        out[k] = _num(data.get(k))
    fd = _num(data.get("fan_duty"))
    out["fan_duty"] = int(fd) if fd is not None else None
    ext = data.get("temp_ext")
    out["temp_ext"] = [_num(ext[i]) if isinstance(ext, list) and i < len(ext) else None for i in range(2)]
    pins = data.get("pins")
    out["pins"] = [
        {"n": i + 1, "voltage": _num(pn.get("voltage")), "current": _num(pn.get("current")), "power": _num(pn.get("power"))}
        for i, pn in enumerate(pins[:6] if isinstance(pins, list) else []) if isinstance(pn, dict)
    ]
    out["faults"] = _flags(data.get("faults"))
    out["faults_logged"] = _flags(data.get("faults_logged"))
    dev = data.get("device")
    if isinstance(dev, dict):
        fw = _num(dev.get("fw"))
        out["device"] = {"port": _text(dev.get("port")), "fw": int(fw) if fw is not None else None,
                         "uid": _text(dev.get("uid")), "build": _text(dev.get("build"))}
    if out["ok"] and (out["total_current"] is None or not out["pins"]):
        out.update(ok=False, status="Bad bridge reply", hint="readings missing")
    return out


def _read_bridge(url: str) -> dict[str, Any] | None:
    """Readings from a running bridge, or None when nothing answers there."""
    st = _bridge
    with st.lock:
        now = time.monotonic()
        if now < st.next_try:
            return None
        try:
            with _bridge_opener.open(url, timeout=0.5) as r:
                data = json.loads(r.read(_BRIDGE_MAX_BYTES).decode())
        except (urllib.error.URLError, OSError, ValueError) as e:
            st.last_error = str(e)
            st.next_try = now + _BRIDGE_RETRY_S
            return None
        st.next_try = 0.0
    out = _shape_bridge(data)
    if out["ok"] and out.get("poll_time"):
        out["age_s"] = round(time.time() - out["poll_time"], 3)
    elif not out["ok"] and not out.get("status"):
        out.update(status="No data", hint=out.get("error") or "")
    return out


def _read_hwinfo_shaped() -> dict[str, Any]:
    raw = _read_hwinfo()
    out = _blank("hwinfo")
    out.update(raw)
    out["source"] = "hwinfo"
    if not out["ok"]:
        if not raw.get("hwinfo_running"):
            out.update(status="HWiNFO not running", hint="start HWiNFO64 with Shared Memory on")
        elif not raw.get("device_found"):
            out.update(status="WireView not in HWiNFO", hint="close the WireView app, restart HWiNFO")
        else:
            out.update(status="No data", hint=raw.get("error") or "")
    return out


def read_wireview(source: str = "auto", port: str | None = None,
                  bridge_url: str | None = DEFAULT_BRIDGE_URL) -> dict[str, Any]:
    """Return the readings dict from the chosen source.

    ``auto`` asks a bridge first (pass ``bridge_url=None`` to skip that, as
    the bridge itself must), then opens the device over serial, and falls back
    to HWiNFO only while both are unavailable. When a bridge is answering, any
    serial connection held here is released so the bridge can own the device.
    """
    if source not in SOURCES:
        raise ValueError(f"source must be one of {SOURCES}")
    if source == "hwinfo":
        return _read_hwinfo_shaped()
    if source in ("bridge", "auto") and bridge_url:
        b = _read_bridge(bridge_url)
        if b is not None:
            _release_serial()
            return b
        if source == "bridge":
            out = _blank("bridge")
            out.update(status="Bridge offline", hint=bridge_url, error=_bridge.last_error)
            return out
    s = _read_serial(port)
    if s["ok"] or source == "serial":
        return s
    h = _read_hwinfo_shaped()
    if h["ok"]:
        return h
    # Neither works: report the serial problem, since that is the primary path.
    if s["device_found"] and h["hwinfo_running"]:
        s.update(status="COM port busy", hint="close the WireView app, or let HWiNFO read it")
    return s


def close() -> None:
    _release_serial()


if __name__ == "__main__":
    import json

    src = sys.argv[1] if len(sys.argv) > 1 else "auto"
    print(json.dumps(read_wireview(src), indent=2))
