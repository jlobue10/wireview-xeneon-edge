//! Pick where WireView Pro II readings come from and return one shape.
//!
//! Sources:
//!
//! * `serial`  talk to the device directly over USB ([`crate::serial`]). No
//!   other software is needed, and nothing else may hold the COM port.
//! * `hwinfo`  read HWiNFO64 shared memory ([`crate::hwinfo`]). Useful when
//!   HWiNFO already owns the device for other reasons.
//! * `bridge`  ask a running wireview-xeneon-edge bridge (localhost JSON).
//!   Lets a second program share the one device the bridge owns.
//! * `auto`    bridge if one answers, else serial if the COM port opens, else
//!   hwinfo.
//!
//! [`Reader::read`] always returns readings. When they are missing, `ok` is
//! false and `status`/`hint` carry a short human-readable explanation.
//!
//! Trust model for the bridge: whatever answers on the bridge port is just
//! another local process, so its JSON is type-checked, its response is read
//! against an end-to-end deadline, and it must prove it is the bridge (see
//! [`crate::auth`]). A reply without a valid HMAC is treated as "no bridge",
//! so the serial reader stays in charge.
//!
//! Readings older than [`STALE_S`] are reported as not ok, whichever source
//! they came from, so a stalled producer cannot leave reassuring numbers on
//! screen.

use std::fmt;
use std::str::FromStr;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::auth::{AUTH_HEADER, SecretStore, bridge_verify, new_nonce};
use crate::http_client::{self, Limits};
use crate::readings::{CABLE_WATTS, Device, PIN_COUNT, Pin, Readings};
use crate::serial::{SerialError, WireViewSerial, find_ports};
use crate::{hwinfo, round_to, unix_time};

pub const DEFAULT_BRIDGE_URL: &str = "http://localhost:8765/api/wireview";
/// Readings older than this many seconds are not ok.
pub const STALE_S: f64 = 5.0;
const RETRY: Duration = Duration::from_secs(2); // reopening a busy/missing COM port
const READ_RETRY: Duration = Duration::from_millis(500); // after a dropped connection
const BRIDGE_RETRY: Duration = Duration::from_secs(5); // looking for a bridge that was not answering
const BRIDGE_LIMITS: Limits = Limits {
    op_timeout: Duration::from_millis(500),
    deadline: Duration::from_secs(1),
    max_body: 64 * 1024,
};
const TEXT_MAX: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Auto,
    Serial,
    Hwinfo,
    Bridge,
}

impl Source {
    pub const NAMES: [&'static str; 4] = ["auto", "serial", "hwinfo", "bridge"];

    pub fn as_str(self) -> &'static str {
        match self {
            Source::Auto => "auto",
            Source::Serial => "serial",
            Source::Hwinfo => "hwinfo",
            Source::Bridge => "bridge",
        }
    }
}

impl fmt::Display for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Source {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, String> {
        match s {
            "auto" => Ok(Source::Auto),
            "serial" => Ok(Source::Serial),
            "hwinfo" => Ok(Source::Hwinfo),
            "bridge" => Ok(Source::Bridge),
            _ => Err(format!("source must be one of {}", Source::NAMES.join(", "))),
        }
    }
}

#[derive(Default)]
struct SerialState {
    dev: Option<WireViewSerial>,
    next_try: Option<Instant>,
    last_error: Option<String>,
}

#[derive(Default)]
struct BridgeState {
    next_try: Option<Instant>,
    last_error: Option<String>,
}

type HwinfoBackend = Box<dyn Fn() -> Readings + Send + Sync>;

/// Reads the WireView from the configured source, keeping the serial
/// connection and the retry timers between calls.
pub struct Reader {
    source: Source,
    port: Option<String>,
    bridge_url: Option<String>,
    secrets: SecretStore,
    serial: Mutex<SerialState>,
    bridge: Mutex<BridgeState>,
    hwinfo: HwinfoBackend,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Reader {
    /// `port` overrides serial port auto-detection. `bridge_url` is the
    /// bridge to ask first; the bridge itself passes `None`.
    pub fn new(source: Source, port: Option<String>, bridge_url: Option<String>) -> Self {
        Reader {
            source,
            port: port.filter(|p| !p.is_empty()),
            bridge_url: bridge_url.filter(|u| !u.is_empty()),
            secrets: SecretStore::default(),
            serial: Mutex::default(),
            bridge: Mutex::default(),
            hwinfo: Box::new(hwinfo::read_wireview),
        }
    }

    /// Read the bridge secret from `store` instead of the per-user file.
    pub fn with_secrets(mut self, store: SecretStore) -> Self {
        self.secrets = store;
        self
    }

    /// Replace the HWiNFO reader. For tests, which have no HWiNFO.
    #[doc(hidden)]
    pub fn with_hwinfo_backend(mut self, f: impl Fn() -> Readings + Send + Sync + 'static) -> Self {
        self.hwinfo = Box::new(f);
        self
    }

    pub fn source(&self) -> Source {
        self.source
    }

    /// Why the bridge was last judged absent, if it was.
    pub fn bridge_error(&self) -> Option<String> {
        lock(&self.bridge).last_error.clone()
    }

    /// Release the serial port.
    pub fn close(&self) {
        lock(&self.serial).dev = None;
    }

    /// The readings from the chosen source.
    ///
    /// `auto` asks a bridge first, then opens the device over serial, and
    /// falls back to HWiNFO only while both are unavailable. When an
    /// authenticated bridge is answering, any serial connection held here is
    /// released so the bridge can own the device.
    pub fn read(&self) -> Readings {
        if self.source == Source::Hwinfo {
            return enforce_fresh(self.read_hwinfo());
        }
        if matches!(self.source, Source::Bridge | Source::Auto) {
            if let Some(url) = &self.bridge_url {
                if let Some(b) = self.read_bridge(url) {
                    self.close();
                    return enforce_fresh(b);
                }
            }
            if self.source == Source::Bridge {
                let mut out = Readings::problem("bridge", "Bridge offline", self.bridge_url.as_deref().unwrap_or("no bridge URL"));
                out.error = self.bridge_error();
                return out;
            }
        }
        let mut s = self.read_serial();
        if s.ok || self.source == Source::Serial {
            return enforce_fresh(s);
        }
        let h = enforce_fresh(self.read_hwinfo());
        if h.ok {
            return h;
        }
        // Neither works: report the serial problem, since that is the primary path.
        if s.device_found && h.hwinfo_running {
            s.set_problem("COM port busy", "close the WireView app, or let HWiNFO read it");
        }
        s
    }

    fn read_serial(&self) -> Readings {
        let mut out = Readings::blank("serial");
        let mut st = lock(&self.serial);
        if st.dev.is_none() {
            let now = Instant::now();
            if st.next_try.is_some_and(|t| now < t) {
                out.set_problem("WireView not connected", st.last_error.as_deref().unwrap_or("retrying"));
                out.error = st.last_error.clone();
                out.device_found = !find_ports().is_empty();
                return out;
            }
            st.next_try = Some(now + RETRY);
            match WireViewSerial::open(self.port.as_deref()) {
                Ok(dev) => {
                    st.dev = Some(dev);
                    st.last_error = None;
                }
                Err(e) => {
                    st.last_error = Some(e.to_string());
                    out.error = Some(e.to_string());
                    out.device_found = !find_ports().is_empty();
                    if out.device_found {
                        out.set_problem("COM port busy", "close the WireView app (and HWiNFO)");
                    } else {
                        out.set_problem("WireView not found", "plug the WireView Pro II into USB");
                    }
                    return out;
                }
            }
        }
        let Some(dev) = st.dev.as_mut() else { return out };
        match dev.read() {
            Ok(mut data) => {
                data.ok = true;
                data.device_found = true;
                data.poll_time = Some(unix_time());
                data.age_s = Some(0.0);
                data.device = Some(Device {
                    port: Some(dev.port_name.clone()),
                    fw: Some(i64::from(dev.fw_version)),
                    uid: Some(dev.uid.clone()),
                    build: Some(dev.build.clone()),
                });
                data
            }
            Err(e) => {
                st.last_error = Some(e.to_string());
                out.error = Some(e.to_string());
                out.device_found = true;
                out.set_problem("Read failed", "reconnecting");
                if matches!(e, SerialError::Transport(_)) {
                    st.dev = None;
                    st.next_try = Some(Instant::now() + READ_RETRY);
                }
                out
            }
        }
    }

    /// Readings from a running, authenticated bridge; `None` when there is none.
    ///
    /// An unauthenticated or malformed answer counts as "no bridge": the
    /// caller keeps (or takes) the serial port instead of trusting the
    /// responder.
    fn read_bridge(&self, url: &str) -> Option<Readings> {
        let data = {
            let mut st = lock(&self.bridge);
            let now = Instant::now();
            if st.next_try.is_some_and(|t| now < t) {
                return None;
            }
            let fetched = match self.secrets.get(false) {
                Some(secret) => fetch_bridge(url, &secret),
                None => Err(format!("no bridge secret at {}", self.secrets.path().display())),
            };
            match fetched {
                Ok(data) => {
                    st.next_try = None;
                    data
                }
                Err(e) => {
                    st.last_error = Some(e);
                    st.next_try = Some(now + BRIDGE_RETRY);
                    return None;
                }
            }
        };
        let mut out = shape_bridge(&data);
        match out.poll_time {
            Some(t) if out.ok => out.age_s = round_to(unix_time() - t, 3),
            _ if !out.ok && out.status.as_deref().is_none_or(str::is_empty) => {
                let hint = out.error.clone().unwrap_or_default();
                out.set_problem("No data", &hint);
            }
            _ => {}
        }
        Some(out)
    }

    fn read_hwinfo(&self) -> Readings {
        let mut out = (self.hwinfo)();
        out.source = "hwinfo".into();
        if !out.ok {
            if !out.hwinfo_running {
                out.set_problem("HWiNFO not running", "start HWiNFO64 with Shared Memory on");
            } else if !out.device_found {
                out.set_problem("WireView not in HWiNFO", "close the WireView app, restart HWiNFO");
            } else {
                let hint = out.error.clone().unwrap_or_default();
                out.set_problem("No data", &hint);
            }
        }
        out
    }
}

/// GET `url` with a fresh nonce; return the JSON once its HMAC checks out.
fn fetch_bridge(url: &str, secret: &[u8]) -> Result<Value, String> {
    let nonce = new_nonce().map_err(|e| format!("no random nonce: {e}"))?;
    let reply = http_client::get(&http_client::with_query(url, "nonce", &nonce), &BRIDGE_LIMITS)?;
    let tag = reply.header(AUTH_HEADER).unwrap_or_default();
    if !bridge_verify(secret, &nonce, &reply.body, tag) {
        return Err("bridge reply failed authentication".into());
    }
    serde_json::from_slice(&reply.body).map_err(|e| format!("bridge reply is not JSON: {e}"))
}

/// A finite number, or `None`. Booleans and strings are not numbers.
fn num(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64().filter(|f| f.is_finite()),
        _ => None,
    }
}

fn text(v: Option<&Value>) -> Option<String> {
    match v {
        Some(Value::String(s)) => Some(s.chars().take(TEXT_MAX).collect()),
        _ => None,
    }
}

fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}

fn flags(v: Option<&Value>) -> crate::Faults {
    let mut out = crate::Faults::default();
    if let Some(Value::Object(o)) = v {
        for (k, x) in o {
            out.set(k, truthy(Some(x)));
        }
    }
    out
}

/// Coerce a bridge reply into [`Readings`], dropping anything odd.
///
/// Whatever answers on the bridge port is another local process; treat its
/// JSON as untrusted so a wrong type cannot upset the caller's render loop.
/// A reply is only `ok` when it carries a complete, finite set of readings.
pub fn shape_bridge(data: &Value) -> Readings {
    let mut out = Readings::blank("bridge");
    let Value::Object(d) = data else {
        out.set_problem("Bad bridge reply", "not a JSON object");
        return out;
    };
    out.ok = d.get("ok") == Some(&Value::Bool(true));
    out.device_found = truthy(d.get("device_found"));
    out.hwinfo_running = truthy(d.get("hwinfo_running"));
    out.status = text(d.get("status"));
    out.hint = text(d.get("hint"));
    out.error = text(d.get("error"));
    out.poll_time = num(d.get("poll_time"));
    out.total_current = num(d.get("total_current"));
    out.total_power = num(d.get("total_power"));
    out.avg_voltage = num(d.get("avg_voltage"));
    out.temp_in = num(d.get("temp_in"));
    out.temp_out = num(d.get("temp_out"));
    out.vdd = num(d.get("vdd"));
    out.cable_w = num(d.get("cable_w")).and_then(|w| CABLE_WATTS.into_iter().find(|r| f64::from(*r) == w));
    out.fan_duty = num(d.get("fan_duty")).filter(|f| (0.0..=100.0).contains(f)).map(|f| f as u8);
    if let Some(Value::Array(ext)) = d.get("temp_ext") {
        out.temp_ext = [num(ext.first()), num(ext.get(1))];
    }
    if let Some(Value::Array(pins)) = d.get("pins") {
        out.pins = pins
            .iter()
            .take(PIN_COUNT)
            .enumerate()
            .filter_map(|(i, p)| p.as_object().map(|p| (i, p)))
            .map(|(i, p)| Pin {
                n: i as u8 + 1,
                voltage: num(p.get("voltage")),
                current: num(p.get("current")),
                power: num(p.get("power")),
            })
            .collect();
    }
    out.faults = flags(d.get("faults"));
    out.faults_logged = flags(d.get("faults_logged"));
    if let Some(Value::Object(dev)) = d.get("device") {
        out.device = Some(Device {
            port: text(dev.get("port")),
            fw: num(dev.get("fw")).filter(|f| f.abs() < 1e15).map(|f| f as i64),
            uid: text(dev.get("uid")),
            build: text(dev.get("build")),
        });
    }
    let complete = out.total_current.is_some()
        && out.total_power.is_some()
        && out.poll_time.is_some()
        && out.pins.len() == PIN_COUNT
        && out.pins.iter().all(|p| p.current.is_some());
    if out.ok && !complete {
        out.set_problem("Bad bridge reply", "readings incomplete");
    }
    out
}

/// Readings must carry a plausible timestamp no older than [`STALE_S`].
pub fn enforce_fresh(mut out: Readings) -> Readings {
    if !out.ok {
        return out;
    }
    let fresh = out.age_s.is_some_and(|age| age.is_finite() && (-60.0..=STALE_S).contains(&age));
    if !fresh {
        let hint = format!("{} stopped updating", out.source);
        out.set_problem("Stale readings", &hint);
    }
    out
}

#[cfg(test)]
mod tests {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::thread;

    use serde_json::json;

    use super::*;
    use crate::auth::bridge_sign;

    fn good() -> Value {
        json!({"ok": true, "poll_time": unix_time(), "total_current": 1.0, "total_power": 1.0,
               "pins": [{"current": 1.0}, {"current": 1.0}, {"current": 1.0}, {"current": 1.0}, {"current": 1.0}, {"current": 1.0}]})
    }

    fn with(mut v: Value, key: &str, value: Value) -> Value {
        v[key] = value;
        v
    }

    #[test]
    fn b1_missing_or_mistyped_total_rejected() {
        for bad in [Value::Null, json!("1.0"), json!(true), json!([1.0])] {
            let r = shape_bridge(&with(good(), "total_current", bad.clone()));
            assert!(!r.ok && r.status.as_deref() == Some("Bad bridge reply"), "{bad}");
        }
    }

    #[test]
    fn b2_bad_pin_rejected() {
        let mut v = good();
        v["pins"][0]["current"] = Value::Null;
        assert!(!shape_bridge(&v).ok);
        v["pins"][0] = json!(1.0);
        assert!(!shape_bridge(&v).ok);
    }

    #[test]
    fn b3_non_finite_and_huge_numbers_never_reach_the_shape() {
        // Python's json module writes NaN/Infinity and huge ints; none of them is accepted.
        for body in [
            r#"{"total_current": NaN}"#,
            r#"{"total_current": Infinity}"#,
            r#"{"total_current": 1e999}"#,
        ] {
            assert!(serde_json::from_str::<Value>(body).is_err(), "{body}");
        }
        let huge = format!(r#"{{"v": 1{}}}"#, "0".repeat(400));
        let parsed = serde_json::from_str::<Value>(&huge);
        assert!(parsed.map_or(true, |v| num(v.get("v")).is_none()));
        assert_eq!(num(Some(&json!(true))), None);
    }

    #[test]
    fn b4_five_pins_not_ok() {
        let mut v = good();
        v["pins"].as_array_mut().unwrap().pop();
        assert!(!shape_bridge(&v).ok);
    }

    #[test]
    fn b5_good_reply_ok_with_bad_cable_and_fault_filtered() {
        let v = with(
            with(good(), "cable_w", json!(999)),
            "faults",
            json!({"bogus": true, "over_power": 1}),
        );
        let r = shape_bridge(&v);
        assert!(r.ok, "{r:?}");
        assert_eq!(r.cable_w, None);
        assert_eq!(serde_json::to_string(&r.faults).unwrap(), r#"{"over_power":true}"#);
        assert_eq!(shape_bridge(&with(good(), "cable_w", json!(450.0))).cable_w, Some(450));
        assert_eq!(r.pins[5].n, 6);
    }

    #[test]
    fn text_is_capped_and_other_shapes_dropped() {
        let v = with(
            with(good(), "status", json!("x".repeat(500))),
            "device",
            json!({"port": 5, "fw": 5, "uid": "U", "build": "b"}),
        );
        let r = shape_bridge(&v);
        assert_eq!(r.status.map(|s| s.len()), Some(200));
        assert_eq!(
            r.device,
            Some(Device {
                port: None,
                fw: Some(5),
                uid: Some("U".into()),
                build: Some("b".into())
            })
        );
        assert!(!shape_bridge(&json!([1, 2])).ok);
        assert_eq!(shape_bridge(&with(good(), "fan_duty", json!(101))).fan_duty, None);
        assert_eq!(shape_bridge(&with(good(), "fan_duty", json!(40))).fan_duty, Some(40));
    }

    fn aged(age: Option<f64>) -> Readings {
        let mut r = Readings::blank("x");
        r.ok = true;
        r.age_s = age;
        enforce_fresh(r)
    }

    #[test]
    fn b6_b7_freshness() {
        let stale = aged(Some(86400.0));
        assert!(!stale.ok);
        assert_eq!(stale.status.as_deref(), Some("Stale readings"));
        assert_eq!(stale.hint.as_deref(), Some("x stopped updating"));
        assert!(aged(Some(0.2)).ok);
        assert!(aged(Some(-5.0)).ok); // clock skew
        assert!(!aged(Some(-61.0)).ok);
        assert!(!aged(None).ok);
        assert!(!aged(Some(f64::NAN)).ok);
    }

    /// A fake bridge answering every connection with `make(nonce)`.
    fn fake_bridge(make: impl Fn(&str) -> Vec<u8> + Send + 'static) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/wireview", l.local_addr().unwrap());
        thread::spawn(move || {
            for s in l.incoming() {
                let Ok(mut s) = s else { return };
                let mut req = [0u8; 2048];
                let n = s.read(&mut req).unwrap_or(0);
                let req = String::from_utf8_lossy(&req[..n]).into_owned();
                let nonce = req
                    .split("nonce=")
                    .nth(1)
                    .and_then(|r| r.split([' ', '&']).next())
                    .unwrap_or_default();
                let _ = s.write_all(&make(nonce));
            }
        });
        url
    }

    fn reply(body: &[u8], tag: Option<String>) -> Vec<u8> {
        let mut r = format!("HTTP/1.0 200 OK\r\nContent-Length: {}\r\n", body.len());
        if let Some(t) = tag {
            r += &format!("{AUTH_HEADER}: {t}\r\n");
        }
        [r.as_bytes(), b"\r\n", body].concat()
    }

    fn secret_file(tag: &str, secret: &[u8]) -> SecretStore {
        let p = std::env::temp_dir().join(format!("wireview-src-{tag}-{}", new_nonce().unwrap()));
        std::fs::write(&p, secret).unwrap();
        SecretStore::at(p)
    }

    fn reader(source: Source, url: String, secrets: SecretStore) -> Reader {
        Reader::new(source, None, Some(url))
            .with_secrets(secrets)
            .with_hwinfo_backend(|| Readings::blank("hwinfo"))
    }

    #[test]
    fn authenticated_bridge_is_accepted() {
        let secret = [b's'; 64];
        let url = fake_bridge(move |nonce| {
            let body = serde_json::to_vec(&good()).unwrap();
            reply(&body, Some(bridge_sign(&secret, nonce, &body)))
        });
        let store = secret_file("ok", &secret);
        let r = reader(Source::Bridge, url, SecretStore::at(store.path()));
        let out = r.read();
        assert!(out.ok, "{out:?} / {:?}", r.bridge_error());
        assert_eq!(out.source, "bridge");
        assert!(out.age_s.is_some_and(|a| a.abs() < 2.0));
        std::fs::remove_file(store.path()).unwrap();
    }

    #[test]
    fn c9_impersonators_are_rejected() {
        let secret = [b's'; 64];
        let store = secret_file("bad", &secret);
        let body = serde_json::to_vec(&good()).unwrap();
        type Impostor = Box<dyn Fn(&str) -> Vec<u8> + Send>;
        let cases: Vec<(&str, Impostor)> = vec![
            (
                "no tag",
                Box::new({
                    let body = body.clone();
                    move |_| reply(&body, None)
                }),
            ),
            (
                "wrong key",
                Box::new({
                    let body = body.clone();
                    move |n| reply(&body, Some(bridge_sign(&[b'x'; 64], n, &body)))
                }),
            ),
            (
                "replayed nonce",
                Box::new({
                    let body = body.clone();
                    move |_| reply(&body, Some(bridge_sign(&secret, &"ab".repeat(16), &body)))
                }),
            ),
            (
                "body changed after signing",
                Box::new({
                    let body = body.clone();
                    move |n| reply(b"{\"ok\":true}", Some(bridge_sign(&secret, n, &body)))
                }),
            ),
        ];
        for (name, make) in cases {
            let r = reader(Source::Bridge, fake_bridge(make), SecretStore::at(store.path()));
            let out = r.read();
            assert!(!out.ok && out.status.as_deref() == Some("Bridge offline"), "{name}: {out:?}");
            assert!(
                r.bridge_error().is_some_and(|e| e.contains("authentication")),
                "{name}: {:?}",
                r.bridge_error()
            );
        }
        std::fs::remove_file(store.path()).unwrap();
    }

    #[test]
    fn no_secret_means_no_bridge_and_nothing_is_created() {
        let path = std::env::temp_dir().join(format!("wireview-src-none-{}", new_nonce().unwrap()));
        let url = fake_bridge(|_| reply(b"{}", None));
        let r = reader(Source::Bridge, url, SecretStore::at(&path));
        assert!(!r.read().ok);
        assert!(r.bridge_error().is_some_and(|e| e.contains("no bridge secret")));
        assert!(!path.exists());
    }

    #[test]
    fn an_absent_bridge_is_not_asked_again_for_a_while() {
        let secret = [b's'; 64];
        let store = secret_file("retry", &secret);
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/wireview", l.local_addr().unwrap());
        let r = reader(Source::Bridge, url, SecretStore::at(store.path()));
        thread::spawn(move || {
            // First connection: close without an answer. A second one must not come.
            drop(l.accept());
            assert!(l.accept().is_err(), "the bridge was asked again inside the retry interval");
        });
        assert!(!r.read().ok);
        let t0 = Instant::now();
        assert!(!r.read().ok);
        assert!(t0.elapsed() < Duration::from_millis(100));
        std::fs::remove_file(store.path()).unwrap();
    }

    #[test]
    fn stale_hwinfo_is_not_ok() {
        let stub = |age: f64| {
            move || {
                let mut r = Readings::blank("hwinfo");
                r.ok = true;
                r.hwinfo_running = true;
                r.device_found = true;
                r.total_current = Some(12.5);
                r.age_s = Some(age);
                r
            }
        };
        let r = Reader::new(Source::Hwinfo, None, None).with_hwinfo_backend(stub(30.0));
        assert_eq!(r.read().status.as_deref(), Some("Stale readings"));
        let r = Reader::new(Source::Hwinfo, None, None).with_hwinfo_backend(stub(0.5));
        assert!(r.read().ok);
    }

    #[test]
    fn hwinfo_problems_are_explained() {
        let r = Reader::new(Source::Hwinfo, None, None).with_hwinfo_backend(|| Readings::blank("hwinfo"));
        assert_eq!(r.read().status.as_deref(), Some("HWiNFO not running"));
        let r = Reader::new(Source::Hwinfo, None, None).with_hwinfo_backend(|| {
            let mut r = Readings::blank("hwinfo");
            r.hwinfo_running = true;
            r
        });
        assert_eq!(r.read().status.as_deref(), Some("WireView not in HWiNFO"));
    }

    #[test]
    fn source_names_round_trip() {
        for name in Source::NAMES {
            assert_eq!(name.parse::<Source>().unwrap().as_str(), name);
        }
        assert!("usb".parse::<Source>().is_err());
    }
}
