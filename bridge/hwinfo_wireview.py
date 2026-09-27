"""Read Thermal Grizzly WireView Pro II values from HWiNFO64 shared memory.

HWiNFO (8.41 or newer) reads the WireView Pro II directly over USB and, when
"Shared Memory Support" is enabled, publishes every sensor in a memory-mapped
block named ``Global\\HWiNFO_SENS_SM2``. This module maps that block, finds the
WireView sensor, and returns its readings as a plain dict.

The producer is not trusted: every offset, count and record size in the header
is checked against the real size of the mapped view before any native read,
the arrays are snapshotted once and parsed from the copy, and counts are capped.
A malformed block yields ``HwinfoUnavailable`` rather than a bad dereference.

No third-party packages are required; only ``ctypes`` on Windows.
"""

from __future__ import annotations

import ctypes
import struct
import sys
import time
from typing import Any, Callable

_MAPPING_NAME = "Global\\HWiNFO_SENS_SM2"
_FILE_MAP_READ = 0x0004
_HEADER_FMT = "<4sIIqIIIIII"
_HEADER_SIZE = struct.calcsize(_HEADER_FMT)
_SENSOR_MATCH = "wireview"

# Record layouts we read (offsets within one record). A record may be larger
# in a newer HWiNFO, never smaller.
_SENSOR_MIN_SIZE = 8 + 128 + 128          # id, instance, name[128], user_name[128]
_READING_MIN_SIZE = 284 + 4 * 8           # ... value, min, max, avg doubles at 284
_MAX_RECORD_SIZE = 64 * 1024
_MAX_SENSORS = 4096
_MAX_READINGS = 65536
_MAX_ARRAY_BYTES = 64 * 1024 * 1024


class HwinfoUnavailable(RuntimeError):
    """HWiNFO is not running, Shared Memory Support is off, or the block is malformed."""


def _cstr(buf: bytes, start: int, length: int) -> str:
    return buf[start:start + length].split(b"\0", 1)[0].decode("latin-1")


def _span(off: int, n: int, size: int, total: int, what: str) -> int:
    """Byte length of ``n`` records of ``size`` at ``off``; must fit in ``total``."""
    if off < 0 or size < 0 or n < 0:
        raise HwinfoUnavailable(f"negative {what} geometry")
    length = n * size
    if length > _MAX_ARRAY_BYTES or off + length > total:
        raise HwinfoUnavailable(f"{what} array ({n} x {size} bytes at {off}) exceeds the {total}-byte mapping")
    return length


def parse_shared_memory(read: Callable[[int, int], bytes], total: int) -> dict[str, Any]:
    """Parse an HWiNFO shared-memory block through a bounded ``read(off, n)``.

    ``total`` is the size of the mapped view. Nothing is read before the
    range has been checked against it, so ``read`` is never asked for bytes
    outside the mapping. Pure, so it can be tested with a bytes-backed reader.
    """
    if total < _HEADER_SIZE:
        raise HwinfoUnavailable(f"mapping is {total} bytes, smaller than the {_HEADER_SIZE}-byte header")
    header = read(0, _HEADER_SIZE)
    if len(header) != _HEADER_SIZE:
        raise HwinfoUnavailable("short header read")
    sig, ver, rev, poll_time, s_off, s_sz, s_n, r_off, r_sz, r_n = struct.unpack(_HEADER_FMT, header)
    if sig != b"HWiS":
        raise HwinfoUnavailable(f"unexpected shared memory signature {sig!r}")
    if not (_SENSOR_MIN_SIZE <= s_sz <= _MAX_RECORD_SIZE) or not (_READING_MIN_SIZE <= r_sz <= _MAX_RECORD_SIZE):
        raise HwinfoUnavailable(f"unsupported record sizes (sensor {s_sz}, reading {r_sz})")
    if s_n > _MAX_SENSORS or r_n > _MAX_READINGS:
        raise HwinfoUnavailable(f"implausible record counts (sensors {s_n}, readings {r_n})")
    s_len = _span(s_off, s_n, s_sz, total, "sensor")
    r_len = _span(r_off, r_n, r_sz, total, "reading")

    # Snapshot both arrays once; the producer may rewrite the block while we parse.
    s_blob = read(s_off, s_len) if s_len else b""
    r_blob = read(r_off, r_len) if r_len else b""
    if len(s_blob) != s_len or len(r_blob) != r_len:
        raise HwinfoUnavailable("short array read")

    sensors = []
    for i in range(s_n):
        b = s_blob[i * s_sz:(i + 1) * s_sz]
        sid, inst = struct.unpack_from("<II", b, 0)
        sensors.append({"id": sid, "instance": inst, "name": _cstr(b, 8, 128), "user_name": _cstr(b, 136, 128)})
    readings = []
    for i in range(r_n):
        b = r_blob[i * r_sz:(i + 1) * r_sz]
        rtype, sidx, rid = struct.unpack_from("<III", b, 0)
        value, vmin, vmax, vavg = struct.unpack_from("<dddd", b, 284)
        readings.append({
            "type": rtype, "sensor": sidx, "id": rid,
            "label": _cstr(b, 12, 128), "user_label": _cstr(b, 140, 128),
            "unit": _cstr(b, 268, 16),
            "value": value, "min": vmin, "max": vmax, "avg": vavg,
        })
    return {"version": ver, "revision": rev, "poll_time": poll_time, "sensors": sensors, "readings": readings}


if sys.platform == "win32":
    _k32 = ctypes.windll.kernel32
    _k32.OpenFileMappingW.restype = ctypes.c_void_p
    _k32.OpenFileMappingW.argtypes = (ctypes.c_uint32, ctypes.c_int, ctypes.c_wchar_p)
    _k32.MapViewOfFile.restype = ctypes.c_void_p
    _k32.MapViewOfFile.argtypes = (ctypes.c_void_p, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_uint32, ctypes.c_size_t)
    _k32.UnmapViewOfFile.argtypes = (ctypes.c_void_p,)
    _k32.CloseHandle.argtypes = (ctypes.c_void_p,)

    class _MBI(ctypes.Structure):
        # MEMORY_BASIC_INFORMATION; PartitionId exists only on 64-bit Windows.
        _fields_ = (
            [("BaseAddress", ctypes.c_void_p), ("AllocationBase", ctypes.c_void_p), ("AllocationProtect", ctypes.c_uint32)]
            + ([("PartitionId", ctypes.c_uint16)] if ctypes.sizeof(ctypes.c_void_p) == 8 else [])
            + [("RegionSize", ctypes.c_size_t), ("State", ctypes.c_uint32), ("Protect", ctypes.c_uint32), ("Type", ctypes.c_uint32)]
        )

    _k32.VirtualQuery.restype = ctypes.c_size_t
    _k32.VirtualQuery.argtypes = (ctypes.c_void_p, ctypes.POINTER(_MBI), ctypes.c_size_t)
    _MEM_COMMIT = 0x1000

    def _view_size(view: int) -> int:
        """Committed size of the mapped view starting at ``view``."""
        mbi = _MBI()
        if _k32.VirtualQuery(view, ctypes.byref(mbi), ctypes.sizeof(mbi)) == 0 or mbi.State != _MEM_COMMIT:
            raise HwinfoUnavailable("VirtualQuery on the mapped view failed")
        base = mbi.BaseAddress or 0
        if base > view:
            raise HwinfoUnavailable("VirtualQuery returned an unexpected region")
        return int(mbi.RegionSize) - (view - base)

    def read_shared_memory() -> dict[str, Any]:
        """Return every HWiNFO sensor and reading as dicts.

        Raises HwinfoUnavailable when the mapping does not exist or is malformed.
        """
        handle = _k32.OpenFileMappingW(_FILE_MAP_READ, False, _MAPPING_NAME)
        if not handle:
            raise HwinfoUnavailable("HWiNFO shared memory not found (is HWiNFO running with Shared Memory Support on?)")
        try:
            view = _k32.MapViewOfFile(handle, _FILE_MAP_READ, 0, 0, 0)
            if not view:
                raise HwinfoUnavailable("MapViewOfFile failed")
            try:
                total = _view_size(view)

                def read(off: int, n: int) -> bytes:
                    if off < 0 or n < 0 or off + n > total:   # belt and braces; the parser checks first
                        raise HwinfoUnavailable("read outside the mapped view refused")
                    return ctypes.string_at(view + off, n)

                return parse_shared_memory(read, total)
            finally:
                _k32.UnmapViewOfFile(view)
        finally:
            _k32.CloseHandle(handle)
else:
    def read_shared_memory() -> dict[str, Any]:
        raise HwinfoUnavailable("HWiNFO shared memory exists only on Windows")


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
