//! Read Thermal Grizzly WireView Pro II values from HWiNFO64 shared memory.
//!
//! HWiNFO (8.41 or newer) reads the WireView Pro II directly over USB and,
//! when "Shared Memory Support" is enabled, publishes every sensor in a
//! memory-mapped block named `Global\HWiNFO_SENS_SM2`. This module maps that
//! block, finds the WireView sensor, and returns its readings.
//!
//! The producer is not trusted: every offset, count and record size in the
//! header is checked against the real size of the mapped view before any
//! native read, the arrays are snapshotted once and parsed from the copy, and
//! counts are capped. A malformed block yields [`HwinfoUnavailable`] rather
//! than a bad dereference.

use std::fmt;

use crate::readings::{Faults, PIN_COUNT, Pin, Readings};
use crate::{round_to, unix_time};

pub const HEADER_SIZE: usize = 44; // <4sIIqIIIIII
const SENSOR_MATCH: &str = "wireview";

// Record layouts we read (offsets within one record). A record may be larger
// in a newer HWiNFO, never smaller.
const SENSOR_MIN_SIZE: u64 = 8 + 128 + 128; // id, instance, name[128], user_name[128]
const READING_MIN_SIZE: u64 = 284 + 4 * 8; // ... value, min, max, avg doubles at 284
const MAX_RECORD_SIZE: u64 = 64 * 1024;
const MAX_SENSORS: u64 = 4096;
const MAX_READINGS: u64 = 65536;
const MAX_ARRAY_BYTES: u64 = 64 * 1024 * 1024;
const TYPE_YES_NO: u32 = 8;

/// HWiNFO is not running, Shared Memory Support is off, or the block is malformed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HwinfoUnavailable(pub String);

impl fmt::Display for HwinfoUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for HwinfoUnavailable {}

fn unavailable<T>(msg: impl Into<String>) -> Result<T, HwinfoUnavailable> {
    Err(HwinfoUnavailable(msg.into()))
}

#[derive(Debug, Clone, PartialEq)]
pub struct Sensor {
    pub id: u32,
    pub instance: u32,
    pub name: String,
    pub user_name: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    pub kind: u32,
    pub sensor: u32,
    pub id: u32,
    pub label: String,
    pub user_label: String,
    pub unit: String,
    pub value: f64,
    pub min: f64,
    pub max: f64,
    pub avg: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SharedMemory {
    pub version: u32,
    pub revision: u32,
    pub poll_time: i64,
    pub sensors: Vec<Sensor>,
    pub readings: Vec<Reading>,
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn f64_at(b: &[u8], o: usize) -> f64 {
    f64::from_le_bytes(b[o..o + 8].try_into().expect("8 bytes"))
}

/// NUL-terminated latin-1 text in a fixed-size field.
fn cstr(b: &[u8], start: usize, len: usize) -> String {
    b[start..start + len].iter().take_while(|c| **c != 0).map(|c| *c as char).collect()
}

/// Byte length of `n` records of `size` at `off`; must fit in `total`.
fn span(off: u64, n: u64, size: u64, total: u64, what: &str) -> Result<usize, HwinfoUnavailable> {
    let len = n * size; // both are below 2^32 and capped by the caller
    if len > MAX_ARRAY_BYTES || off + len > total {
        return unavailable(format!(
            "{what} array ({n} x {size} bytes at {off}) exceeds the {total}-byte mapping"
        ));
    }
    Ok(len as usize)
}

/// Parse an HWiNFO shared-memory block through a bounded `read(off, n)`.
///
/// `total` is the size of the mapped view. Nothing is read before the range
/// has been checked against it, so `read` is never asked for bytes outside
/// the mapping. Pure, so it can be tested with a bytes-backed reader.
pub fn parse_shared_memory(
    mut read: impl FnMut(usize, usize) -> Result<Vec<u8>, HwinfoUnavailable>,
    total: usize,
) -> Result<SharedMemory, HwinfoUnavailable> {
    if total < HEADER_SIZE {
        return unavailable(format!("mapping is {total} bytes, smaller than the {HEADER_SIZE}-byte header"));
    }
    let header = read(0, HEADER_SIZE)?;
    if header.len() != HEADER_SIZE {
        return unavailable("short header read");
    }
    if &header[0..4] != b"HWiS" {
        return unavailable(format!(
            "unexpected shared memory signature {:?}",
            String::from_utf8_lossy(&header[0..4])
        ));
    }
    let (version, revision) = (u32_at(&header, 4), u32_at(&header, 8));
    let poll_time = i64::from_le_bytes(header[12..20].try_into().expect("8 bytes"));
    let field = |i: usize| u64::from(u32_at(&header, 20 + 4 * i));
    let (s_off, s_sz, s_n, r_off, r_sz, r_n) = (field(0), field(1), field(2), field(3), field(4), field(5));
    if !(SENSOR_MIN_SIZE..=MAX_RECORD_SIZE).contains(&s_sz) || !(READING_MIN_SIZE..=MAX_RECORD_SIZE).contains(&r_sz) {
        return unavailable(format!("unsupported record sizes (sensor {s_sz}, reading {r_sz})"));
    }
    if s_n > MAX_SENSORS || r_n > MAX_READINGS {
        return unavailable(format!("implausible record counts (sensors {s_n}, readings {r_n})"));
    }
    let s_len = span(s_off, s_n, s_sz, total as u64, "sensor")?;
    let r_len = span(r_off, r_n, r_sz, total as u64, "reading")?;

    // Snapshot both arrays once; the producer may rewrite the block while we parse.
    let s_blob = if s_len > 0 { read(s_off as usize, s_len)? } else { Vec::new() };
    let r_blob = if r_len > 0 { read(r_off as usize, r_len)? } else { Vec::new() };
    if s_blob.len() != s_len || r_blob.len() != r_len {
        return unavailable("short array read");
    }

    let sensors = s_blob
        .chunks_exact(s_sz as usize)
        .map(|b| Sensor {
            id: u32_at(b, 0),
            instance: u32_at(b, 4),
            name: cstr(b, 8, 128),
            user_name: cstr(b, 136, 128),
        })
        .collect();
    let readings = r_blob
        .chunks_exact(r_sz as usize)
        .map(|b| Reading {
            kind: u32_at(b, 0),
            sensor: u32_at(b, 4),
            id: u32_at(b, 8),
            label: cstr(b, 12, 128),
            user_label: cstr(b, 140, 128),
            unit: cstr(b, 268, 16),
            value: f64_at(b, 284),
            min: f64_at(b, 292),
            max: f64_at(b, 300),
            avg: f64_at(b, 308),
        })
        .collect();
    Ok(SharedMemory {
        version,
        revision,
        poll_time,
        sensors,
        readings,
    })
}

#[cfg(windows)]
mod native {
    use std::ffi::c_void;

    use windows_sys::Win32::Foundation::CloseHandle;
    use windows_sys::Win32::System::Memory::{
        FILE_MAP_READ, MEM_COMMIT, MEMORY_BASIC_INFORMATION, MEMORY_MAPPED_VIEW_ADDRESS, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile,
        VirtualQuery,
    };

    use super::{HwinfoUnavailable, SharedMemory, parse_shared_memory, unavailable};

    const MAPPING_NAME: &str = "Global\\HWiNFO_SENS_SM2";

    struct Handle(*mut c_void);

    impl Drop for Handle {
        fn drop(&mut self) {
            // SAFETY: the handle came from OpenFileMappingW and is closed once.
            unsafe { CloseHandle(self.0) };
        }
    }

    struct View(MEMORY_MAPPED_VIEW_ADDRESS);

    impl Drop for View {
        fn drop(&mut self) {
            // SAFETY: the address came from MapViewOfFile and is unmapped once.
            unsafe { UnmapViewOfFile(self.0) };
        }
    }

    /// Committed size of the mapped view starting at `view`.
    fn view_size(view: *const c_void) -> Result<usize, HwinfoUnavailable> {
        // SAFETY: MEMORY_BASIC_INFORMATION is plain data; VirtualQuery fills it.
        let mut mbi: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: `mbi` is a valid buffer of the size passed.
        let n = unsafe { VirtualQuery(view, &mut mbi, size_of::<MEMORY_BASIC_INFORMATION>()) };
        if n == 0 || mbi.State != MEM_COMMIT {
            return unavailable("VirtualQuery on the mapped view failed");
        }
        let (base, view) = (mbi.BaseAddress as usize, view as usize);
        if base > view {
            return unavailable("VirtualQuery returned an unexpected region");
        }
        mbi.RegionSize
            .checked_sub(view - base)
            .ok_or_else(|| HwinfoUnavailable("VirtualQuery returned an unexpected region".into()))
    }

    pub fn read_shared_memory() -> Result<SharedMemory, HwinfoUnavailable> {
        let name: Vec<u16> = MAPPING_NAME.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `name` is a NUL-terminated UTF-16 string that outlives the call.
        let handle = unsafe { OpenFileMappingW(FILE_MAP_READ, 0, name.as_ptr()) };
        if handle.is_null() {
            return unavailable("HWiNFO shared memory not found (is HWiNFO running with Shared Memory Support on?)");
        }
        let handle = Handle(handle);
        // SAFETY: the handle is a valid file mapping opened for reading.
        let view = unsafe { MapViewOfFile(handle.0, FILE_MAP_READ, 0, 0, 0) };
        if view.Value.is_null() {
            return unavailable("MapViewOfFile failed");
        }
        let view = View(view);
        let base = view.0.Value as *const u8;
        let total = view_size(base.cast())?;
        parse_shared_memory(
            |off, n| {
                // Belt and braces; the parser checks first.
                if off.checked_add(n).is_none_or(|end| end > total) {
                    return unavailable("read outside the mapped view refused");
                }
                let mut out = vec![0u8; n];
                // SAFETY: [off, off + n) lies inside the committed view, which
                // stays mapped until `view` is dropped after the parse.
                unsafe { std::ptr::copy_nonoverlapping(base.add(off), out.as_mut_ptr(), n) };
                Ok(out)
            },
            total,
        )
    }
}

#[cfg(not(windows))]
mod native {
    use super::{HwinfoUnavailable, SharedMemory, unavailable};

    pub fn read_shared_memory() -> Result<SharedMemory, HwinfoUnavailable> {
        unavailable("HWiNFO shared memory exists only on Windows")
    }
}

/// Every HWiNFO sensor and reading.
pub fn read_shared_memory() -> Result<SharedMemory, HwinfoUnavailable> {
    native::read_shared_memory()
}

/// The WireView Pro II readings from HWiNFO.
///
/// Always returns readings; `hwinfo_running` and `device_found` say what went
/// wrong when values are missing.
pub fn read_wireview() -> Readings {
    shape(read_shared_memory(), unix_time())
}

fn shape(sm: Result<SharedMemory, HwinfoUnavailable>, now: f64) -> Readings {
    let mut out = Readings::blank("hwinfo");
    let sm = match sm {
        Ok(sm) => sm,
        Err(e) => {
            out.error = Some(e.0);
            return out;
        }
    };
    out.hwinfo_running = true;
    out.poll_time = Some(sm.poll_time as f64);
    out.age_s = round_to(now - sm.poll_time as f64, 3);

    let Some(idx) = sm.sensors.iter().position(|s| s.name.to_lowercase().contains(SENSOR_MATCH)) else {
        out.error = Some("HWiNFO is running but reports no WireView sensor (close the WireView app and restart HWiNFO)".into());
        return out;
    };
    out.device_found = true;

    let mut pins: Vec<Pin> = (1..=PIN_COUNT as u8)
        .map(|n| Pin {
            n,
            voltage: None,
            current: None,
            power: None,
        })
        .collect();
    let mut flags: Vec<bool> = Vec::new();
    for r in sm.readings.iter().filter(|r| r.sensor as usize == idx) {
        let label = r.label.as_str();
        let b = label.as_bytes();
        if label.starts_with("Pin ") && b.len() > 5 && b[4].is_ascii_digit() {
            let n = usize::from(b[4] - b'0');
            if let (Some(pin), Some(kind)) = (n.checked_sub(1).and_then(|i| pins.get_mut(i)), label.get(6..)) {
                match kind.trim().to_lowercase().as_str() {
                    "voltage" => pin.voltage = round_to(r.value, 3),
                    "current" => pin.current = round_to(r.value, 3),
                    "power" => pin.power = round_to(r.value, 3),
                    _ => {}
                }
            }
            continue;
        }
        match label {
            "Total Current" => out.total_current = round_to(r.value, 3),
            "Total Power" => out.total_power = round_to(r.value, 3),
            "Average Pin Voltage" => out.avg_voltage = round_to(r.value, 3),
            "Temperature In" => out.temp_in = round_to(r.value, 1),
            "Temperature Out" => out.temp_out = round_to(r.value, 1),
            // Yes/No flags: six live, then six logged/latched.
            _ if r.kind == TYPE_YES_NO => flags.push(r.value != 0.0),
            _ => {}
        }
    }
    out.pins = pins;
    if flags.len() >= 6 {
        out.faults = Faults::from_flags(&flags[..6]);
    }
    if flags.len() >= 12 {
        out.faults_logged = Faults::from_flags(&flags[6..12]);
    }
    out.ok = out.total_current.is_some();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const S_SZ: u32 = 264;
    const R_SZ: u32 = 316;

    fn block(geometry: [u32; 6], total: usize, sig: &[u8; 4]) -> Vec<u8> {
        let mut b = vec![0u8; total];
        let mut head = Vec::new();
        head.extend_from_slice(sig);
        head.extend_from_slice(&1u32.to_le_bytes());
        head.extend_from_slice(&1u32.to_le_bytes());
        head.extend_from_slice(&1_790_000_000i64.to_le_bytes());
        for v in geometry {
            head.extend_from_slice(&v.to_le_bytes());
        }
        let n = head.len().min(total);
        b[..n].copy_from_slice(&head[..n]);
        b
    }

    /// Parse `buf`, failing the test if the parser asks for bytes outside it.
    fn parse(buf: &[u8], total: usize) -> (Result<SharedMemory, HwinfoUnavailable>, usize) {
        let mut calls = 0;
        let r = parse_shared_memory(
            |off, n| {
                calls += 1;
                assert!(
                    off + n <= buf.len(),
                    "parser asked for bytes outside the mapping: {off}+{n} > {}",
                    buf.len()
                );
                Ok(buf[off..off + n].to_vec())
            },
            total,
        );
        (r, calls)
    }

    fn assert_unavailable(buf: &[u8]) {
        let (r, _) = parse(buf, buf.len());
        assert!(r.is_err(), "parsed without error");
    }

    #[test]
    fn a1_truncated_header() {
        assert_unavailable(b"HWiS\0\0\0\0\0\0\0\0\0\0");
    }

    #[test]
    fn a2_sensor_array_outside_mapping() {
        assert_unavailable(&block([65536, S_SZ, 1, 0, R_SZ, 0], 4096, b"HWiS"));
    }

    #[test]
    fn a3_zero_record_size() {
        assert_unavailable(&block([44, 0, 5, 44, R_SZ, 0], 4096, b"HWiS"));
    }

    #[test]
    fn a4_excessive_count() {
        assert_unavailable(&block([44, S_SZ, 100_000_000, 44, R_SZ, 0], 4096, b"HWiS"));
    }

    #[test]
    fn a5_offset_near_overflow() {
        assert_unavailable(&block([u32::MAX, S_SZ, 1, 44, R_SZ, 0], 4096, b"HWiS"));
        assert_unavailable(&block([44, S_SZ, 0, u32::MAX, R_SZ, 1], 4096, b"HWiS"));
    }

    #[test]
    fn a6_wrong_signature() {
        assert_unavailable(&block([44, S_SZ, 0, 44, R_SZ, 0], 4096, b"XXXX"));
    }

    fn put_reading(buf: &mut [u8], at: usize, kind: u32, sensor: u32, label: &str, value: f64) {
        buf[at..at + 4].copy_from_slice(&kind.to_le_bytes());
        buf[at + 4..at + 8].copy_from_slice(&sensor.to_le_bytes());
        buf[at + 12..at + 12 + label.len()].copy_from_slice(label.as_bytes());
        buf[at + 284..at + 292].copy_from_slice(&value.to_le_bytes());
    }

    fn wireview_block(readings: &[(u32, u32, &str, f64)]) -> Vec<u8> {
        let (s, r) = (S_SZ as usize, R_SZ as usize);
        let total = 44 + 2 * s + readings.len() * r;
        let mut buf = block([44, S_SZ, 2, 44 + 2 * S_SZ, R_SZ, readings.len() as u32], total, b"HWiS");
        buf[44 + 8..44 + 8 + 3].copy_from_slice(b"CPU");
        buf[44 + s + 8..44 + s + 8 + 23].copy_from_slice(b"Thermal Grizzly WireVie");
        buf[44 + s + 8 + 23..44 + s + 8 + 31].copy_from_slice(b"w Pro II");
        for (i, (kind, sensor, label, value)) in readings.iter().enumerate() {
            put_reading(&mut buf, 44 + 2 * s + i * r, *kind, *sensor, label, *value);
        }
        buf
    }

    #[test]
    fn a7_valid_block_parses_in_three_reads() {
        let buf = wireview_block(&[(1, 1, "Total Current", 12.5)]);
        let (sm, calls) = parse(&buf, buf.len());
        let sm = sm.unwrap();
        assert_eq!(calls, 3);
        assert_eq!(sm.sensors[1].name, "Thermal Grizzly WireView Pro II");
        assert_eq!(sm.readings[0].label, "Total Current");
        assert_eq!(sm.readings[0].value, 12.5);
        assert_eq!(sm.poll_time, 1_790_000_000);
    }

    #[test]
    fn shapes_wireview_readings() {
        let mut rows: Vec<(u32, u32, String, f64)> = vec![
            (2, 0, "Total Current".into(), 99.0), // another sensor's row is ignored
            (5, 1, "Total Current".into(), 12.4996),
            (6, 1, "Total Power".into(), 150.04),
            (2, 1, "Average Pin Voltage".into(), 12.04),
            (1, 1, "Temperature In".into(), 35.46),
            (1, 1, "Temperature Out".into(), 35.84),
            (5, 1, "Pin 7 Current".into(), 1.0), // no such pin
            (5, 1, "Pin 0 Current".into(), 1.0),
        ];
        for n in 1..=6 {
            rows.push((2, 1, format!("Pin {n} Voltage"), 12.0));
            rows.push((5, 1, format!("Pin {n} Current"), n as f64));
            rows.push((6, 1, format!("Pin {n} Power"), 12.0 * n as f64));
        }
        for i in 0..12 {
            rows.push((TYPE_YES_NO, 1, format!("Flag {i}"), if i == 4 || i == 11 { 1.0 } else { 0.0 }));
        }
        let rows: Vec<(u32, u32, &str, f64)> = rows.iter().map(|(a, b, c, d)| (*a, *b, c.as_str(), *d)).collect();
        let buf = wireview_block(&rows);
        let out = shape(parse(&buf, buf.len()).0, 1_790_000_001.5);
        assert!(out.ok && out.hwinfo_running && out.device_found);
        assert_eq!((out.total_current, out.total_power), (Some(12.5), Some(150.04)));
        assert_eq!((out.temp_in, out.temp_out), (Some(35.5), Some(35.8)));
        assert_eq!(out.age_s, Some(1.5));
        assert_eq!(out.pins.len(), 6);
        assert_eq!(
            out.pins[2],
            Pin {
                n: 3,
                voltage: Some(12.0),
                current: Some(3.0),
                power: Some(36.0)
            }
        );
        assert_eq!(out.faults.active().collect::<Vec<_>>(), ["over_power"]);
        assert_eq!(out.faults_logged.active().collect::<Vec<_>>(), ["imbalance"]);
    }

    #[test]
    fn non_finite_total_is_not_ok() {
        let buf = wireview_block(&[(5, 1, "Total Current", f64::NAN)]);
        let out = shape(parse(&buf, buf.len()).0, 1_790_000_000.0);
        assert!(!out.ok && out.device_found && out.total_current.is_none());
    }

    #[test]
    fn missing_sensor_and_missing_hwinfo_are_told_apart() {
        let buf = block([44, S_SZ, 0, 44, R_SZ, 0], 4096, b"HWiS");
        let out = shape(parse(&buf, buf.len()).0, 0.0);
        assert!(!out.ok && out.hwinfo_running && !out.device_found);
        let out = shape(unavailable("nope"), 0.0);
        assert!(!out.ok && !out.hwinfo_running && out.error.as_deref() == Some("nope"));
    }
}
