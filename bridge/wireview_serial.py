"""Read the Thermal Grizzly WireView Pro II directly over USB serial.

The device is an STM32 CDC ACM port (VID 0483, PID 5740) speaking a tiny
command/response protocol at 115200 8N1: the host writes one command byte and
reads a fixed-size reply. This module only uses the read-only subset needed
for monitoring:

    01  vendor data      -> EF 05 <fw version>
    02  unique id        -> 12 bytes
    04  sensor values    -> 100-byte SensorStruct (pack 4, little endian)
    0D  build info       -> 68 bytes: vendor(3) name[32] build[32] name_len
    0C F1 resume display updates (sent once after connecting)

Protocol reference: the Linux daemon projects by Gustav0ar (wireview-pro-ii,
docs/protocol.md) and emaspa (wireview-hwmon / wireview-linux), both MIT.
Verified on firmware TG-WV-PRO2-FW_20260430_1838.

Only one process can own the port: close the Thermal Grizzly WireView app (and
HWiNFO, if it has the WireView sensor enabled) before using this.

Requires ``pyserial``.
"""

from __future__ import annotations

import struct
import threading
import time
from typing import Any

import serial
import serial.tools.list_ports

USB_VID = 0x0483
USB_PID = 0x5740
BAUD = 115200
TIMEOUT_S = 1.0
GREETING = b"Thermal Grizzly WireView Pro II"

CMD_VENDOR = b"\x01"
CMD_UID = b"\x02"
CMD_SENSORS = b"\x04"
CMD_BUILD = b"\x0d"
CMD_SCREEN_RESUME = b"\x0c\xf1"

SENSOR_SIZE = 100
_PIN_FMT = "<hxxII"          # voltage mV, pad, current mA, power mW
_HEAD_FMT = "<hhhhHB"        # temp in/out/ext1/ext2 (0.1 C), VDD mV, fan duty %
_TAIL_FMT = "<IIHBxHH"       # total power mW, total current mA, avg V mV, cable cap, pad, faults active, faults logged

CABLE_WATTS = {0: 600, 1: 450, 2: 300, 3: 150}
EXT_TEMP_ABSENT = -1000      # -100.0 C means no external probe

FAULT_KEYS = (
    "temp_chip", "temp_sensor", "over_current_total", "over_current_wire", "over_power", "imbalance",
)


class WireViewSerialError(RuntimeError):
    """The device is missing, busy, or answered with something unexpected."""


def find_ports() -> list[str]:
    """COM ports whose USB IDs match the WireView Pro II."""
    return sorted(p.device for p in serial.tools.list_ports.comports() if p.vid == USB_VID and p.pid == USB_PID)


def _faults(mask: int) -> dict[str, bool]:
    return {k: bool(mask >> i & 1) for i, k in enumerate(FAULT_KEYS)}


def parse_sensors(buf: bytes) -> dict[str, Any]:
    """Decode a 100-byte SensorStruct into the widget dict shape.

    Raises WireViewSerialError on a short or implausible frame. The protocol
    has no framing, so a desynced read is caught by checking the padding
    bytes and the fan duty range, as the reference implementations do.
    """
    if len(buf) != SENSOR_SIZE:
        raise WireViewSerialError(f"sensor frame is {len(buf)} bytes, expected {SENSOR_SIZE}")
    if buf[10] > 100 or buf[11] != 0 or buf[95] != 0:
        raise WireViewSerialError("sensor frame failed plausibility check (desynced?)")
    t_in, t_out, t_ext1, t_ext2, vdd, fan = struct.unpack_from(_HEAD_FMT, buf, 0)
    pins = []
    for n in range(6):
        mv, ma, mw = struct.unpack_from(_PIN_FMT, buf, 12 + 12 * n)
        pins.append({"n": n + 1, "voltage": round(mv / 1000, 3), "current": round(ma / 1000, 3), "power": round(mw / 1000, 3)})
    tot_mw, tot_ma, avg_mv, cap, f_active, f_logged = struct.unpack_from(_TAIL_FMT, buf, 84)
    if cap not in CABLE_WATTS:
        raise WireViewSerialError(f"unknown cable capability {cap}")
    return {
        "pins": pins,
        "total_current": round(tot_ma / 1000, 3),
        "total_power": round(tot_mw / 1000, 3),
        "avg_voltage": round(avg_mv / 1000, 3),
        "temp_in": round(t_in / 10, 1),
        "temp_out": round(t_out / 10, 1),
        "temp_ext": [None if t == EXT_TEMP_ABSENT else round(t / 10, 1) for t in (t_ext1, t_ext2)],
        "vdd": round(vdd / 1000, 3),
        "fan_duty": fan,
        "cable_w": CABLE_WATTS[cap],
        "faults": _faults(f_active),
        "faults_logged": _faults(f_logged),
        "faults_raw": [f_active, f_logged],
    }


class WireViewSerial:
    """A persistent connection to one WireView Pro II.

    ``open()`` performs the handshake; ``read()`` returns one decoded sensor
    frame. Any transport error closes the port so the next call reconnects.
    """

    def __init__(self, port: str | None = None) -> None:
        self.port_name = port
        self.port: str | None = None
        self._ser: serial.Serial | None = None
        self._lock = threading.Lock()
        self.fw_version: int | None = None
        self.uid: str | None = None
        self.build: str | None = None

    @property
    def is_open(self) -> bool:
        return self._ser is not None

    def _xfer(self, cmd: bytes, n: int) -> bytes:
        assert self._ser is not None
        self._ser.reset_input_buffer()
        self._ser.write(cmd)
        data = self._ser.read(n)
        if len(data) != n:
            raise WireViewSerialError(f"short reply to {cmd.hex()}: {len(data)}/{n} bytes")
        return data

    def open(self) -> None:
        with self._lock:
            if self._ser is not None:
                return
            port = self.port_name or next(iter(find_ports()), None)
            if not port:
                raise WireViewSerialError("no WireView Pro II USB serial port found (VID 0483 PID 5740)")
            try:
                ser = serial.Serial(port, BAUD, timeout=TIMEOUT_S, write_timeout=TIMEOUT_S)
            except serial.SerialException as e:
                raise WireViewSerialError(f"cannot open {port}: {e} (is the WireView app or HWiNFO using it?)") from e
            self._ser = ser
            self.port = port
            try:
                # A freshly plugged device greets when RTS is asserted; an
                # already initialised one stays silent, so only a wrong
                # greeting is fatal.
                ser.reset_input_buffer()
                ser.rts = True
                time.sleep(0.01)
                greeting = ser.read(len(GREETING) + 1)
                time.sleep(0.01)
                ser.rts = False
                if greeting and greeting.rstrip(b"\0") != GREETING:
                    raise WireViewSerialError(f"unexpected greeting {greeting!r}")
                vendor = self._xfer(CMD_VENDOR, 3)
                if vendor[:2] != b"\xef\x05":
                    raise WireViewSerialError(f"unexpected vendor data {vendor.hex()}")
                self.fw_version = vendor[2]
                self.uid = self._xfer(CMD_UID, 12).hex().upper()
                build = self._xfer(CMD_BUILD, 68)
                self.build = build[35:67].split(b"\0", 1)[0].decode("latin-1")
                ser.write(CMD_SCREEN_RESUME)
            except (serial.SerialException, OSError) as e:
                self._drop()
                raise WireViewSerialError(f"{port}: {e}") from e
            except WireViewSerialError:
                self._drop()
                raise

    def _drop(self) -> None:
        if self._ser is not None:
            try:
                self._ser.close()
            except Exception:
                pass
            self._ser = None

    def close(self) -> None:
        with self._lock:
            self._drop()

    def read(self) -> dict[str, Any]:
        """One decoded sensor frame; raises WireViewSerialError on failure.

        Transport errors drop the connection; a merely implausible frame keeps
        it, because the next read flushes the input and realigns.
        """
        if self._ser is None:
            self.open()
        with self._lock:
            try:
                frame = self._xfer(CMD_SENSORS, SENSOR_SIZE)
            except (serial.SerialException, OSError) as e:
                self._drop()
                raise WireViewSerialError(f"read failed: {e}") from e
            return parse_sensors(frame)


if __name__ == "__main__":
    import json
    import sys

    dev = WireViewSerial(sys.argv[1] if len(sys.argv) > 1 else None)
    dev.open()
    print(f"port={dev.port} fw=v{dev.fw_version} uid={dev.uid} build={dev.build}")
    print(json.dumps(dev.read(), indent=2))
