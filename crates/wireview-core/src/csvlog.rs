//! CSV log of the readings, in the format the Thermal Grizzly WireView app
//! exports.
//!
//! One file per run, `log-YYYYMMDD-HHMMSS.csv`, with the same header, column
//! order, number formats and CRLF line endings as the CSV the WireView app
//! exports from its own log, so the same spreadsheets and scripts read both.
//! Off unless a daemon is started with `--csv-log DIR`.
//!
//! Columns: `Timestamp` (local time, 100 ns resolution), `Connected`
//! (`True` when the row holds a live reading), `HW` (hardware revision; the
//! device does not report one over serial, so it is empty), `FW` (firmware
//! version), `SumPowerW`, `SumCurrentA`, `OnboardInC`, `OnboardOutC`,
//! `Ext1C`, `Ext2C`, `V1`..`V6`, `I1`..`I6`. A value the source did not
//! report is written as `0`, as the WireView app does for an absent probe.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::readings::{PIN_COUNT, Readings};

pub const HEADER: &str =
    "Timestamp,Connected,HW,FW,SumPowerW,SumCurrentA,OnboardInC,OnboardOutC,Ext1C,Ext2C,V1,V2,V3,V4,V5,V6,I1,I2,I3,I4,I5,I6";
/// One row a minute, like the WireView app's own log.
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(60);
pub const MIN_INTERVAL: Duration = Duration::from_secs(1);
pub const MAX_INTERVAL: Duration = Duration::from_secs(86_400);

/// Parse a `--csv-interval` value in seconds.
pub fn parse_interval(s: &str) -> Result<Duration, String> {
    let v: f64 = s.trim().parse().map_err(|_| format!("not a number: {s:?}"))?;
    if !v.is_finite() || v < MIN_INTERVAL.as_secs_f64() || v > MAX_INTERVAL.as_secs_f64() {
        return Err(format!(
            "the log interval must be between {} and {} seconds",
            MIN_INTERVAL.as_secs(),
            MAX_INTERVAL.as_secs()
        ));
    }
    Ok(Duration::from_secs_f64(v))
}

/// A wall-clock time in the local zone, to 100 ns, as .NET's round-trip
/// format writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalTime {
    pub year: i64,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
    /// 100 ns ticks within the second (0..10_000_000).
    pub ticks: u32,
}

impl LocalTime {
    pub fn now() -> Self {
        let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
        Self::from_unix(since_epoch, local_offset_minutes())
    }

    /// `since_epoch` shifted by `offset_minutes` (east of UTC positive).
    pub fn from_unix(since_epoch: Duration, offset_minutes: i64) -> Self {
        let secs = since_epoch.as_secs() as i64 + offset_minutes * 60;
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400) as u32;
        let (year, month, day) = civil_from_days(days);
        LocalTime {
            year,
            month,
            day,
            hour: sod / 3600,
            minute: sod / 60 % 60,
            second: sod % 60,
            ticks: since_epoch.subsec_nanos() / 100,
        }
    }

    /// `2026-10-05T08:12:34.5678900`
    pub fn iso8601(&self) -> String {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:07}",
            self.year, self.month, self.day, self.hour, self.minute, self.second, self.ticks
        )
    }

    /// `20261005-081234`, the part of the file name after `log-`.
    pub fn file_stamp(&self) -> String {
        format!(
            "{:04}{:02}{:02}-{:02}{:02}{:02}",
            self.year, self.month, self.day, self.hour, self.minute, self.second
        )
    }
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian
/// (H. Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// (year, month, day) to days since 1970-01-01.
#[cfg(any(windows, test))]
fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = i64::from(if month > 2 { month - 3 } else { month + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Minutes the local zone is ahead of UTC right now (DST included).
#[cfg(windows)]
fn local_offset_minutes() -> i64 {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::SystemInformation::{GetLocalTime, GetSystemTime};

    fn minutes(t: &SYSTEMTIME) -> i64 {
        days_from_civil(i64::from(t.wYear), u32::from(t.wMonth), u32::from(t.wDay)) * 1440 + i64::from(t.wHour) * 60 + i64::from(t.wMinute)
    }
    // SAFETY: both calls only fill the caller's SYSTEMTIME.
    let (local, utc) = unsafe {
        let mut local: SYSTEMTIME = std::mem::zeroed();
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        GetLocalTime(&mut local);
        GetSystemTime(&mut utc);
        (local, utc)
    };
    // The two reads are microseconds apart, so the minute difference is the
    // zone offset unless a minute boundary fell between them; zone offsets
    // are whole quarter hours, so snap to the nearest one.
    let raw = minutes(&local) - minutes(&utc);
    (raw as f64 / 15.0).round() as i64 * 15
}

/// Without a portable way to read the zone, other platforms log in UTC.
#[cfg(not(windows))]
fn local_offset_minutes() -> i64 {
    0
}

fn fixed(v: Option<f64>, decimals: usize) -> String {
    let v = v.filter(|v| v.is_finite()).unwrap_or(0.0);
    format!("{v:.decimals$}")
}

/// One data row for `r` taken at `at`, without the line ending.
pub fn row(r: &Readings, at: LocalTime) -> String {
    let dev = r.device.as_ref();
    let fw = dev.and_then(|d| d.fw).map(|v| v.to_string()).unwrap_or_default();
    let pin = |n: usize| r.pins.iter().find(|p| usize::from(p.n) == n + 1);
    let mut cols = vec![
        at.iso8601(),
        (if r.ok { "True" } else { "False" }).to_string(),
        String::new(), // HW: not reported over serial
        fw,
        fixed(r.total_power, 3),
        fixed(r.total_current, 3),
        fixed(r.temp_in, 2),
        fixed(r.temp_out, 2),
        fixed(r.temp_ext[0], 2),
        fixed(r.temp_ext[1], 2),
    ];
    cols.extend((0..PIN_COUNT).map(|n| fixed(pin(n).and_then(|p| p.voltage), 3)));
    cols.extend((0..PIN_COUNT).map(|n| fixed(pin(n).and_then(|p| p.current), 3)));
    cols.join(",")
}

/// An open log file. Rows are written by [`CsvLog::record`] no more often
/// than the interval, each one flushed, so the file is complete whenever the
/// daemon is stopped or killed.
pub struct CsvLog {
    path: PathBuf,
    file: BufWriter<File>,
    interval: Duration,
    last: Option<Instant>,
    rows: u64,
}

impl std::fmt::Debug for CsvLog {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CsvLog")
            .field("path", &self.path)
            .field("interval", &self.interval)
            .field("rows", &self.rows)
            .finish()
    }
}

impl CsvLog {
    /// Create `dir` if needed and start a new `log-<stamp>.csv` in it with
    /// the header written.
    pub fn create(dir: &Path, interval: Duration) -> io::Result<Self> {
        Self::create_at(dir, interval, LocalTime::now())
    }

    pub fn create_at(dir: &Path, interval: Duration, at: LocalTime) -> io::Result<Self> {
        let interval = interval.clamp(MIN_INTERVAL, MAX_INTERVAL);
        fs::create_dir_all(dir)?;
        let stamp = at.file_stamp();
        let mut n = 0;
        let (path, file) = loop {
            let name = if n == 0 {
                format!("log-{stamp}.csv")
            } else {
                format!("log-{stamp}-{n}.csv")
            };
            let path = dir.join(name);
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(f) => break (path, f),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists && n < 100 => n += 1,
                Err(e) => return Err(e),
            }
        };
        let mut log = CsvLog {
            path,
            file: BufWriter::new(file),
            interval,
            last: None,
            rows: 0,
        };
        log.file.write_all(HEADER.as_bytes())?;
        log.file.write_all(b"\r\n")?;
        log.file.flush()?;
        Ok(log)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Rows written so far.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// Time until the next row is due; zero when one should be written now.
    pub fn due_in(&self) -> Duration {
        match self.last {
            None => Duration::ZERO,
            Some(t) => self.interval.saturating_sub(t.elapsed()),
        }
    }

    /// Write a row for `r` if the interval has passed since the last one.
    /// Returns whether a row was written.
    pub fn record(&mut self, r: &Readings) -> io::Result<bool> {
        if !self.due_in().is_zero() {
            return Ok(false);
        }
        self.write_row(r, LocalTime::now())?;
        Ok(true)
    }

    /// Write a row now, whatever the interval.
    pub fn write_row(&mut self, r: &Readings, at: LocalTime) -> io::Result<()> {
        self.file.write_all(row(r, at).as_bytes())?;
        self.file.write_all(b"\r\n")?;
        self.file.flush()?;
        self.last = Some(Instant::now());
        self.rows += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readings::{Device, Pin};

    fn sample() -> Readings {
        let mut r = Readings::blank("serial");
        r.ok = true;
        r.device_found = true;
        r.device = Some(Device {
            port: Some("COM5".into()),
            fw: Some(5),
            uid: None,
            build: Some("TG-WV-PRO2-FW_20260902_0741".into()),
        });
        r.pins = (1..=6)
            .map(|n| Pin {
                n,
                voltage: Some(12.0 + f64::from(n) / 100.0),
                current: Some(f64::from(n) * 0.5),
                power: Some(6.0 * f64::from(n)),
            })
            .collect();
        r.total_power = Some(133.76);
        r.total_current = Some(11.1);
        r.temp_in = Some(33.0);
        r.temp_out = Some(33.5);
        r.temp_ext = [None, Some(21.25)];
        r
    }

    fn at() -> LocalTime {
        LocalTime {
            year: 2026,
            month: 1,
            day: 1,
            hour: 2,
            minute: 5,
            second: 2,
            ticks: 3_960_000,
        }
    }

    #[test]
    fn header_matches_the_wireview_app_export() {
        assert_eq!(
            HEADER,
            "Timestamp,Connected,HW,FW,SumPowerW,SumCurrentA,OnboardInC,OnboardOutC,Ext1C,Ext2C,V1,V2,V3,V4,V5,V6,I1,I2,I3,I4,I5,I6"
        );
        assert_eq!(HEADER.split(',').count(), 10 + 2 * PIN_COUNT);
    }

    #[test]
    fn row_uses_the_app_formats_and_column_order() {
        let line = row(&sample(), at());
        assert_eq!(
            line,
            "2026-01-01T02:05:02.3960000,True,,5,133.760,11.100,33.00,33.50,0.00,21.25,\
             12.010,12.020,12.030,12.040,12.050,12.060,0.500,1.000,1.500,2.000,2.500,3.000"
        );
        assert_eq!(line.split(',').count(), HEADER.split(',').count());
    }

    #[test]
    fn missing_values_are_zero_and_not_ok_is_false() {
        let r = Readings::problem("serial", "COM port busy", "close the app");
        let line = row(&r, at());
        assert!(
            line.starts_with("2026-01-01T02:05:02.3960000,False,,,0.000,0.000,0.00,0.00,0.00,0.00,"),
            "{line}"
        );
        assert_eq!(line.split(',').count(), HEADER.split(',').count());
        assert_eq!(line.matches("0.000").count(), 2 + 2 * PIN_COUNT);
    }

    #[test]
    fn non_finite_and_out_of_order_pins_are_handled() {
        let mut r = sample();
        r.total_power = Some(f64::NAN);
        r.pins.reverse();
        r.pins.remove(0); // pin 6 gone
        let line = row(&r, at());
        let cols: Vec<&str> = line.split(',').collect();
        assert_eq!(cols[4], "0.000");
        assert_eq!(&cols[10..16], ["12.010", "12.020", "12.030", "12.040", "12.050", "0.000"]);
        assert_eq!(&cols[16..22], ["0.500", "1.000", "1.500", "2.000", "2.500", "0.000"]);
    }

    #[test]
    fn civil_conversion_round_trips_and_matches_known_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(20_731), (2026, 10, 5));
        assert_eq!(days_from_civil(2026, 10, 5), 20_731);
        assert_eq!(civil_from_days(days_from_civil(2000, 2, 29)), (2000, 2, 29));
        for d in (-1_000_000..1_000_000).step_by(997) {
            let (y, m, day) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, day), d);
        }
    }

    #[test]
    fn timestamps_carry_the_zone_offset_and_seven_fraction_digits() {
        // 1_790_000_000 = 2026-09-21T14:13:20Z
        let t = LocalTime::from_unix(Duration::new(1_790_000_000, 123_456_789), -240);
        assert_eq!(t.iso8601(), "2026-09-21T10:13:20.1234567");
        assert_eq!(t.file_stamp(), "20260921-101320");
        let utc = LocalTime::from_unix(Duration::new(1_790_000_000, 0), 0);
        assert_eq!(utc.iso8601(), "2026-09-21T14:13:20.0000000");
        let east = LocalTime::from_unix(Duration::new(1_790_000_000, 0), 720);
        assert_eq!(east.iso8601(), "2026-09-22T02:13:20.0000000");
        let west = LocalTime::from_unix(Duration::new(1_790_000_000, 0), -900);
        assert_eq!(west.iso8601(), "2026-09-20T23:13:20.0000000");
    }

    #[test]
    fn interval_parser_bounds() {
        assert_eq!(parse_interval("60").unwrap(), Duration::from_secs(60));
        assert_eq!(parse_interval(" 2.5 ").unwrap(), Duration::from_secs_f64(2.5));
        for bad in ["0", "0.5", "-1", "nan", "inf", "x", "90000"] {
            assert!(parse_interval(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn file_has_header_crlf_rows_and_is_flushed() {
        let dir = std::env::temp_dir().join(format!("wireview-csvlog-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let mut log = CsvLog::create_at(&dir.join("nested"), Duration::from_secs(60), at()).unwrap();
        assert_eq!(log.path().file_name().unwrap(), "log-20260101-020502.csv");
        assert!(log.due_in().is_zero(), "first row is due at once");
        assert!(log.record(&sample()).unwrap());
        assert!(!log.record(&sample()).unwrap(), "second row waits for the interval");
        assert!(log.due_in() > Duration::from_secs(50));
        assert_eq!(log.rows(), 1);
        let text = fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = text.split("\r\n").collect();
        assert_eq!(lines.len(), 3, "{text:?}");
        assert_eq!(lines[0], HEADER);
        assert!(lines[1].ends_with(",3.000"), "{}", lines[1]);
        assert_eq!(lines[2], "");
        assert!(!text.contains('\n') || text.matches("\r\n").count() == text.matches('\n').count());

        // A second file in the same second gets a suffix instead of failing.
        let second = CsvLog::create_at(&dir.join("nested"), Duration::from_secs(60), at()).unwrap();
        assert_eq!(second.path().file_name().unwrap(), "log-20260101-020502-1.csv");
        drop(second);
        drop(log);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn interval_is_clamped() {
        let dir = std::env::temp_dir().join(format!("wireview-csvlog-clamp-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let log = CsvLog::create_at(&dir, Duration::from_millis(1), at()).unwrap();
        assert_eq!(log.interval(), MIN_INTERVAL);
        drop(log);
        fs::remove_dir_all(&dir).unwrap();
    }
}
