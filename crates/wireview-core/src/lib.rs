//! Shared reader for the Thermal Grizzly WireView Pro II.
//!
//! Used by the Xeneon Edge bridge (this repository) and by wireview-nexus.
//!
//! * [`serial`]  talks to the device directly over USB serial.
//! * [`hwinfo`]  reads HWiNFO64 shared memory (Windows only).
//! * [`source`]  picks between them and an already running bridge, and
//!   returns one [`readings::Readings`] shape whatever the source.
//! * [`auth`]    is the per-user secret and the HMAC that lets a client tell
//!   the real bridge from any other process that owns the port.
//! * [`csvlog`]  writes the readings to CSV files in the format the Thermal
//!   Grizzly WireView app exports (off unless asked for).

pub mod auth;
pub mod console;
pub mod csvlog;
mod http_client;
pub mod hwinfo;
pub mod readings;
pub mod serial;
pub mod source;

pub use csvlog::CsvLog;
pub use readings::{Device, Faults, Pin, Readings};
pub use source::{DEFAULT_BRIDGE_URL, Reader, Source};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Seconds since the Unix epoch, as the JSON timestamps use.
pub fn unix_time() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Round to `decimals` places; non-finite values become `None`.
pub(crate) fn round_to(v: f64, decimals: i32) -> Option<f64> {
    if !v.is_finite() {
        return None;
    }
    let k = 10f64.powi(decimals);
    let r = (v * k).round() / k;
    r.is_finite().then_some(r)
}
