//! Read the Thermal Grizzly WireView Pro II directly over USB serial.
//!
//! The device is an STM32 CDC ACM port (VID 0483, PID 5740) speaking a tiny
//! command/response protocol at 115200 8N1: the host writes one command byte
//! and reads a fixed-size reply. Only the read-only subset needed for
//! monitoring is used:
//!
//! ```text
//! 01  vendor data      -> EF 05 <fw version>
//! 02  unique id        -> 12 bytes
//! 04  sensor values    -> 100-byte SensorStruct (pack 4, little endian)
//! 0D  build info       -> 68 bytes: vendor(3) name[32] build[32] name_len
//! 0C F1 resume display updates (sent once after connecting)
//! ```
//!
//! Protocol reference: the Linux daemon projects by Gustav0ar (wireview-pro-ii,
//! docs/protocol.md) and emaspa (wireview-hwmon / wireview-linux), both MIT.
//! Verified on firmware TG-WV-PRO2-FW_20260430_1838.
//!
//! Only one process can own the port: close the Thermal Grizzly WireView app
//! (and HWiNFO, if it has the WireView sensor enabled) before using this.

use std::fmt;
use std::io;
use std::thread::sleep;
use std::time::{Duration, Instant};

use serialport::{ClearBuffer, SerialPort, SerialPortType};

use crate::auth::hex;
use crate::readings::{CABLE_WATTS, Faults, Pin, Readings};
use crate::round_to;

pub const USB_VID: u16 = 0x0483;
pub const USB_PID: u16 = 0x5740;
pub const BAUD: u32 = 115_200;
pub const TIMEOUT: Duration = Duration::from_secs(1);
const GREETING: &[u8] = b"Thermal Grizzly WireView Pro II";

const CMD_VENDOR: &[u8] = b"\x01";
const CMD_UID: &[u8] = b"\x02";
const CMD_SENSORS: &[u8] = b"\x04";
const CMD_BUILD: &[u8] = b"\x0d";
const CMD_SCREEN_RESUME: &[u8] = b"\x0c\xf1";

pub const SENSOR_SIZE: usize = 100;
const EXT_TEMP_ABSENT: i16 = -1000; // -100.0 C means no external probe

/// The device is missing, busy, or answered with something unexpected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SerialError {
    /// The port itself failed; the connection has been dropped.
    Transport(String),
    /// The port works but the reply was short or implausible.
    Protocol(String),
}

impl fmt::Display for SerialError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SerialError::Transport(s) | SerialError::Protocol(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for SerialError {}

fn protocol(msg: impl Into<String>) -> SerialError {
    SerialError::Protocol(msg.into())
}

/// Serial ports whose USB IDs match the WireView Pro II.
pub fn find_ports() -> Vec<String> {
    let mut ports: Vec<String> = serialport::available_ports()
        .unwrap_or_default()
        .into_iter()
        .filter(|p| matches!(&p.port_type, SerialPortType::UsbPort(u) if u.vid == USB_VID && u.pid == USB_PID))
        .map(|p| p.port_name)
        .collect();
    ports.sort();
    ports
}

fn i16_at(b: &[u8], o: usize) -> i16 {
    i16::from_le_bytes([b[o], b[o + 1]])
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn milli(v: impl Into<f64>) -> Option<f64> {
    round_to(v.into() / 1000.0, 3)
}

fn deci(v: i16) -> Option<f64> {
    round_to(f64::from(v) / 10.0, 1)
}

/// Decode a 100-byte SensorStruct. The result is not yet marked `ok`.
///
/// The protocol has no framing, so a desynced read is caught by checking the
/// padding bytes and the fan duty range, as the reference implementations do.
pub fn parse_sensors(buf: &[u8]) -> Result<Readings, SerialError> {
    if buf.len() != SENSOR_SIZE {
        return Err(protocol(format!("sensor frame is {} bytes, expected {SENSOR_SIZE}", buf.len())));
    }
    if buf[10] > 100 || buf[11] != 0 || buf[95] != 0 {
        return Err(protocol("sensor frame failed plausibility check (desynced?)"));
    }
    // Head: temp in/out/ext1/ext2 (0.1 C), VDD mV, fan duty %, pad.
    let (t_in, t_out, t_ext1, t_ext2) = (i16_at(buf, 0), i16_at(buf, 2), i16_at(buf, 4), i16_at(buf, 6));
    let (vdd, fan) = (u16_at(buf, 8), buf[10]);
    // Six pins of 12 bytes: voltage mV (i16), pad, current mA, power mW.
    let pins = (0..6)
        .map(|n| {
            let o = 12 + 12 * n;
            Pin {
                n: n as u8 + 1,
                voltage: milli(i16_at(buf, o)),
                current: milli(u32_at(buf, o + 4)),
                power: milli(u32_at(buf, o + 8)),
            }
        })
        .collect();
    // Tail: total power mW, total current mA, avg V mV, cable capability, pad,
    // faults active, faults logged.
    let (tot_mw, tot_ma, avg_mv, cap) = (u32_at(buf, 84), u32_at(buf, 88), u16_at(buf, 92), buf[94]);
    let (f_active, f_logged) = (u16_at(buf, 96), u16_at(buf, 98));
    let cable_w = *CABLE_WATTS
        .get(cap as usize)
        .ok_or_else(|| protocol(format!("unknown cable capability {cap}")))?;

    let ext = |t: i16| if t == EXT_TEMP_ABSENT { None } else { deci(t) };
    let mut out = Readings::blank("serial");
    out.pins = pins;
    out.total_current = milli(tot_ma);
    out.total_power = milli(tot_mw);
    out.avg_voltage = milli(avg_mv);
    out.temp_in = deci(t_in);
    out.temp_out = deci(t_out);
    out.temp_ext = [ext(t_ext1), ext(t_ext2)];
    out.vdd = milli(vdd);
    out.fan_duty = Some(fan);
    out.cable_w = Some(cable_w);
    out.faults = Faults::from_mask(f_active);
    out.faults_logged = Faults::from_mask(f_logged);
    out.faults_raw = Some([f_active, f_logged]);
    Ok(out)
}

/// Read up to `n` bytes, giving up when `timeout` has passed.
fn read_upto(port: &mut dyn SerialPort, n: usize, timeout: Duration) -> io::Result<Vec<u8>> {
    let deadline = Instant::now() + timeout;
    let mut buf = vec![0u8; n];
    let mut got = 0;
    while got < n {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        port.set_timeout(left).map_err(io::Error::other)?;
        match port.read(&mut buf[got..]) {
            Ok(0) => sleep(Duration::from_millis(1)),
            Ok(k) => got += k,
            Err(e) if e.kind() == io::ErrorKind::TimedOut => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    buf.truncate(got);
    Ok(buf)
}

fn xfer(port: &mut dyn SerialPort, cmd: &[u8], n: usize) -> Result<Vec<u8>, SerialError> {
    let io = |e: io::Error| SerialError::Transport(e.to_string());
    port.clear(ClearBuffer::Input).map_err(|e| SerialError::Transport(e.to_string()))?;
    port.set_timeout(TIMEOUT).map_err(|e| SerialError::Transport(e.to_string()))?;
    port.write_all(cmd).map_err(io)?;
    let data = read_upto(port, n, TIMEOUT).map_err(io)?;
    if data.len() != n {
        return Err(protocol(format!("short reply to {}: {}/{n} bytes", hex(cmd), data.len())));
    }
    Ok(data)
}

/// A connection to one WireView Pro II, handshake done.
pub struct WireViewSerial {
    port: Box<dyn SerialPort>,
    pub port_name: String,
    pub fw_version: u8,
    pub uid: String,
    pub build: String,
}

impl fmt::Debug for WireViewSerial {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WireViewSerial")
            .field("port_name", &self.port_name)
            .field("fw_version", &self.fw_version)
            .finish()
    }
}

impl WireViewSerial {
    /// Open `port` (or the first port with the WireView's USB IDs) and
    /// perform the handshake. Any failure closes the port again.
    pub fn open(port: Option<&str>) -> Result<Self, SerialError> {
        let name = match port {
            Some(p) => p.to_string(),
            None => find_ports()
                .into_iter()
                .next()
                .ok_or_else(|| protocol("no WireView Pro II USB serial port found (VID 0483 PID 5740)"))?,
        };
        let mut ser = serialport::new(&name, BAUD)
            .timeout(TIMEOUT)
            .dtr_on_open(true)
            .open()
            .map_err(|e| protocol(format!("cannot open {name}: {e} (is the WireView app or HWiNFO using it?)")))?;
        let (fw_version, uid, build) = Self::handshake(ser.as_mut()).map_err(|e| match e {
            SerialError::Transport(s) => SerialError::Transport(format!("{name}: {s}")),
            e => e,
        })?;
        Ok(WireViewSerial {
            port: ser,
            port_name: name,
            fw_version,
            uid,
            build,
        })
    }

    fn handshake(ser: &mut dyn SerialPort) -> Result<(u8, String, String), SerialError> {
        let t = |e: serialport::Error| SerialError::Transport(e.to_string());
        // A freshly plugged device greets when RTS is asserted; an already
        // initialised one stays silent, so only a wrong greeting is fatal.
        ser.clear(ClearBuffer::Input).map_err(t)?;
        ser.write_data_terminal_ready(true).map_err(t)?;
        ser.write_request_to_send(true).map_err(t)?;
        sleep(Duration::from_millis(10));
        let greeting = read_upto(ser, GREETING.len() + 1, TIMEOUT).map_err(|e| SerialError::Transport(e.to_string()))?;
        sleep(Duration::from_millis(10));
        ser.write_request_to_send(false).map_err(t)?;
        let trimmed = match greeting.iter().rposition(|b| *b != 0) {
            Some(last) => &greeting[..=last],
            None => &[],
        };
        if !greeting.is_empty() && trimmed != GREETING {
            return Err(protocol(format!("unexpected greeting {:?}", String::from_utf8_lossy(&greeting))));
        }
        let vendor = xfer(ser, CMD_VENDOR, 3)?;
        if vendor[..2] != [0xEF, 0x05] {
            return Err(protocol(format!("unexpected vendor data {}", hex(&vendor))));
        }
        let uid = hex(&xfer(ser, CMD_UID, 12)?).to_uppercase();
        let build = xfer(ser, CMD_BUILD, 68)?;
        let build = build[35..67].split(|b| *b == 0).next().unwrap_or_default();
        let build = build.iter().map(|b| *b as char).collect(); // latin-1
        ser.write_all(CMD_SCREEN_RESUME)
            .map_err(|e| SerialError::Transport(e.to_string()))?;
        Ok((vendor[2], uid, build))
    }

    /// One decoded sensor frame.
    ///
    /// After a [`SerialError::Transport`] the connection is unusable and must
    /// be dropped. A merely short or implausible frame keeps it, because the
    /// next read flushes the input and realigns.
    pub fn read(&mut self) -> Result<Readings, SerialError> {
        let frame = xfer(self.port.as_mut(), CMD_SENSORS, SENSOR_SIZE).map_err(|e| match e {
            SerialError::Transport(s) => SerialError::Transport(format!("read failed: {s}")),
            e => e,
        })?;
        parse_sensors(&frame)
    }
}

#[cfg(test)]
pub(crate) fn sample_frame() -> [u8; SENSOR_SIZE] {
    let mut b = [0u8; SENSOR_SIZE];
    b[0..2].copy_from_slice(&355i16.to_le_bytes());
    b[2..4].copy_from_slice(&358i16.to_le_bytes());
    b[4..6].copy_from_slice(&EXT_TEMP_ABSENT.to_le_bytes());
    b[6..8].copy_from_slice(&(-55i16).to_le_bytes());
    b[8..10].copy_from_slice(&3417u16.to_le_bytes());
    b[10] = 40;
    for n in 0..6 {
        let o = 12 + 12 * n;
        b[o..o + 2].copy_from_slice(&12040i16.to_le_bytes());
        b[o + 4..o + 8].copy_from_slice(&(2000 + 100 * n as u32).to_le_bytes());
        b[o + 8..o + 12].copy_from_slice(&(24_080 + 1204 * n as u32).to_le_bytes());
    }
    b[84..88].copy_from_slice(&162_540u32.to_le_bytes());
    b[88..92].copy_from_slice(&13_500u32.to_le_bytes());
    b[92..94].copy_from_slice(&12040u16.to_le_bytes());
    b[94] = 1;
    b[96..98].copy_from_slice(&0b010000u16.to_le_bytes());
    b[98..100].copy_from_slice(&0b010010u16.to_le_bytes());
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_frame() {
        let r = parse_sensors(&sample_frame()).unwrap();
        assert!(!r.ok);
        assert_eq!(r.pins.len(), 6);
        assert_eq!(
            r.pins[0],
            Pin {
                n: 1,
                voltage: Some(12.04),
                current: Some(2.0),
                power: Some(24.08)
            }
        );
        assert_eq!(r.pins[5].current, Some(2.5));
        assert_eq!(
            (r.total_current, r.total_power, r.avg_voltage),
            (Some(13.5), Some(162.54), Some(12.04))
        );
        assert_eq!((r.temp_in, r.temp_out), (Some(35.5), Some(35.8)));
        assert_eq!(r.temp_ext, [None, Some(-5.5)]);
        assert_eq!((r.vdd, r.fan_duty, r.cable_w), (Some(3.417), Some(40), Some(450)));
        assert_eq!(r.faults.active().collect::<Vec<_>>(), ["over_power"]);
        assert_eq!(r.faults_logged.active().collect::<Vec<_>>(), ["temp_sensor", "over_power"]);
        assert_eq!(r.faults_raw, Some([16, 18]));
    }

    #[test]
    fn rejects_short_and_desynced_frames() {
        assert!(matches!(parse_sensors(&[0u8; 99]), Err(SerialError::Protocol(_))));
        for (at, value) in [(10, 101u8), (11, 1), (95, 1), (94, 4)] {
            let mut f = sample_frame();
            f[at] = value;
            assert!(matches!(parse_sensors(&f), Err(SerialError::Protocol(_))), "byte {at} = {value}");
        }
    }
}
