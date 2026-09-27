"""Read Thermal Grizzly WireView Pro II values from HWiNFO64 shared memory.

HWiNFO (8.41 or newer) reads the WireView Pro II directly over USB and, when
"Shared Memory Support" is enabled, publishes every sensor in a memory-mapped
block named ``Global\\HWiNFO_SENS_SM2``. This module maps that block, finds the
WireView sensor, and returns its readings as a plain dict.

No third-party packages are required; only ``ctypes`` on Windows.
"""

from __future__ import annotations

import ctypes
import struct
import time
from typing import Any

_MAPPING_NAME = "Global\\HWiNFO_SENS_SM2"
_FILE_MAP_READ = 0x0004
_HEADER_FMT = "<4sIIqIIIIII"
_HEADER_SIZE = struct.calcsize(_HEADER_FMT)
_SENSOR_MATCH = "wireview"

_k32 = ctypes.windll.kernel32
_k32.OpenFileMappingW.restype = ctypes.c_void_p
_k32.OpenFileMappingW.argtypes = (ctypes.c_uint32, ctypes.c_int, ctypes.c_wchar_p)
_k32.MapViewOfFile.restype = ctypes.c_void_p
_k32.MapViewOfFile.argtypes = (ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_size_t)
_k32.UnmapViewOfFile.argtypes = (ctypes.c_void_p,)
_k32.CloseHandle.argtypes = (ctypes.c_void_p,)


def _cstr(buf: bytes, start: int, length: int) -> str:
    return buf[start:start + length].split(b"\0", 1)[0].decode("latin-1")


class HwinfoUnavailable(RuntimeError):
    """HWiNFO is not running or Shared Memory Support is off."""


def read_shared_memory() -> dict[str, Any]:
    """Return every HWiNFO sensor and reading as dicts.

    Raises HwinfoUnavailable when the mapping does not exist.
    """
    handle = _k32.OpenFileMappingW(_FILE_MAP_READ, False, _MAPPING_NAME)
    if not handle:
        raise HwinfoUnavailable("HWiNFO shared memory not found (is HWiNFO running with Shared Memory Support on?)")
    try:
        view = _k32.MapViewOfFile(handle, _FILE_MAP_READ, 0, 0, 0)
        if not view:
            raise HwinfoUnavailable("MapViewOfFile failed")
        try:
            header = ctypes.string_at(view, _HEADER_SIZE)
            sig, ver, rev, poll_time, s_off, s_sz, s_n, r_off, r_sz, r_n = struct.unpack(_HEADER_FMT, header)
            if sig != b"HWiS":
                raise HwinfoUnavailable(f"unexpected shared memory signature {sig!r}")
            sensors = []
            for i in range(s_n):
                b = ctypes.string_at(view + s_off + i * s_sz, s_sz)
                sid, inst = struct.unpack_from("<II", b, 0)
                sensors.append({"id": sid, "instance": inst, "name": _cstr(b, 8, 128), "user_name": _cstr(b, 136, 128)})
            readings = []
            for i in range(r_n):
                b = ctypes.string_at(view + r_off + i * r_sz, r_sz)
                rtype, sidx, rid = struct.unpack_from("<III", b, 0)
                value, vmin, vmax, vavg = struct.unpack_from("<dddd", b, 284)
                readings.append({
                    "type": rtype, "sensor": sidx, "id": rid,
                    "label": _cstr(b, 12, 128), "user_label": _cstr(b, 140, 128),
                    "unit": _cstr(b, 268, 16),
                    "value": value, "min": vmin, "max": vmax, "avg": vavg,
                })
            return {"version": ver, "revision": rev, "poll_time": poll_time, "sensors": sensors, "readings": readings}
        finally:
            _k32.UnmapViewOfFile(view)
    finally:
        _k32.CloseHandle(handle)


_FAULT_KEYS = (
    "temp_chip", "temp_sensor", "over_current_total", "over_current_wire", "over_power", "imbalance",
)


def read_wireview() -> dict[str, Any]:
    """Return the WireView Pro II readings shaped for the widgets.

    Always returns a dict. ``hwinfo_running`` and ``device_found`` say what
    went wrong when values are missing.
    """
    now = time.time()
    out: dict[str, Any] = {
        "ok": False, "source": "hwinfo", "hwinfo_running": False, "device_found": False,
        "poll_time": None, "age_s": None, "error": None,
        "pins": [], "total_current": None, "total_power": None, "avg_voltage": None,
        "temp_in": None, "temp_out": None, "faults": {}, "faults_logged": {},
    }
    try:
        sm = read_shared_memory()
    except HwinfoUnavailable as e:
        out["error"] = str(e)
        return out
    out["hwinfo_running"] = True
    out["poll_time"] = sm["poll_time"]
    out["age_s"] = round(now - sm["poll_time"], 3)

    idx = next((i for i, s in enumerate(sm["sensors"]) if _SENSOR_MATCH in s["name"].lower()), None)
    if idx is None:
        out["error"] = "HWiNFO is running but reports no WireView sensor (close the WireView app and restart HWiNFO)"
        return out
    out["device_found"] = True

    pins: dict[int, dict[str, float | int | None]] = {n: {"n": n, "voltage": None, "current": None, "power": None} for n in range(1, 7)}
    flag_seen: list[bool] = []
    for r in sm["readings"]:
        if r["sensor"] != idx:
            continue
        label = r["label"]
        v = r["value"]
        if label.startswith("Pin ") and len(label) > 5 and label[4].isdigit():
            n = int(label[4])
            kind = label[6:].strip().lower()
            if n in pins and kind in ("voltage", "current", "power"):
                pins[n][kind] = round(v, 3)
        elif label == "Total Current":
            out["total_current"] = round(v, 3)
        elif label == "Total Power":
            out["total_power"] = round(v, 3)
        elif label == "Average Pin Voltage":
            out["avg_voltage"] = round(v, 3)
        elif label == "Temperature In":
            out["temp_in"] = round(v, 1)
        elif label == "Temperature Out":
            out["temp_out"] = round(v, 1)
        elif r["type"] == 8:  # Yes/No flags: six live, then six logged/latched
            flag_seen.append(bool(v))
    out["pins"] = [pins[n] for n in sorted(pins)]
    if len(flag_seen) >= 6:
        out["faults"] = dict(zip(_FAULT_KEYS, flag_seen[:6]))
    if len(flag_seen) >= 12:
        out["faults_logged"] = dict(zip(_FAULT_KEYS, flag_seen[6:12]))
    out["ok"] = out["total_current"] is not None
    return out


if __name__ == "__main__":
    import json
    print(json.dumps(read_wireview(), indent=2))
