//! The one shape every source returns, and the bridge serves as JSON.

use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};

pub const PIN_COUNT: usize = 6;

/// Fault bits in device order (bit 0 first).
pub const FAULT_KEYS: [&str; 6] = [
    "temp_chip",
    "temp_sensor",
    "over_current_total",
    "over_current_wire",
    "over_power",
    "imbalance",
];

/// Cable power ratings the device can report, indexed by its capability code.
pub const CABLE_WATTS: [u32; 4] = [600, 450, 300, 150];

/// Fault flags keyed by [`FAULT_KEYS`]. A flag the source did not report is
/// absent, so an empty set serialises as `{}`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Faults([Option<bool>; 6]);

impl Faults {
    pub fn from_mask(mask: u16) -> Self {
        let mut f = [None; 6];
        for (i, slot) in f.iter_mut().enumerate() {
            *slot = Some(mask >> i & 1 == 1);
        }
        Faults(f)
    }

    pub fn from_flags(flags: &[bool]) -> Self {
        let mut f = [None; 6];
        for (slot, v) in f.iter_mut().zip(flags) {
            *slot = Some(*v);
        }
        Faults(f)
    }

    /// Set one flag; an unknown key is ignored and reported as `false`.
    pub fn set(&mut self, key: &str, value: bool) -> bool {
        match FAULT_KEYS.iter().position(|k| *k == key) {
            Some(i) => {
                self.0[i] = Some(value);
                true
            }
            None => false,
        }
    }

    pub fn get(&self, key: &str) -> Option<bool> {
        FAULT_KEYS.iter().position(|k| *k == key).and_then(|i| self.0[i])
    }

    /// Keys of the flags that are raised.
    pub fn active(&self) -> impl Iterator<Item = &'static str> + '_ {
        FAULT_KEYS.iter().zip(self.0).filter(|(_, v)| *v == Some(true)).map(|(k, _)| *k)
    }

    pub fn is_empty(&self) -> bool {
        self.0.iter().all(Option::is_none)
    }
}

impl Serialize for Faults {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut m = s.serialize_map(Some(self.0.iter().flatten().count()))?;
        for (k, v) in FAULT_KEYS.iter().zip(self.0) {
            if let Some(v) = v {
                m.serialize_entry(k, &v)?;
            }
        }
        m.end()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pin {
    pub n: u8,
    pub voltage: Option<f64>,
    pub current: Option<f64>,
    pub power: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Device {
    pub port: Option<String>,
    pub fw: Option<i64>,
    /// Hardware unique id. The bridge clears it before serving.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uid: Option<String>,
    pub build: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Readings {
    pub ok: bool,
    pub source: String,
    pub device_found: bool,
    pub hwinfo_running: bool,
    pub status: Option<String>,
    pub hint: Option<String>,
    pub error: Option<String>,
    pub poll_time: Option<f64>,
    pub age_s: Option<f64>,
    pub device: Option<Device>,
    pub pins: Vec<Pin>,
    pub total_current: Option<f64>,
    pub total_power: Option<f64>,
    pub avg_voltage: Option<f64>,
    pub temp_in: Option<f64>,
    pub temp_out: Option<f64>,
    pub temp_ext: [Option<f64>; 2],
    pub vdd: Option<f64>,
    pub fan_duty: Option<u8>,
    pub cable_w: Option<u32>,
    pub faults: Faults,
    pub faults_logged: Faults,
    /// Raw fault masks (active, logged); serial source only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub faults_raw: Option<[u16; 2]>,
    /// When the bridge produced the reply; set by the bridge only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub served_at: Option<f64>,
}

impl Readings {
    /// No readings yet, from `source`.
    pub fn blank(source: &str) -> Self {
        Readings {
            ok: false,
            source: source.to_string(),
            device_found: false,
            hwinfo_running: false,
            status: None,
            hint: None,
            error: None,
            poll_time: None,
            age_s: None,
            device: None,
            pins: Vec::new(),
            total_current: None,
            total_power: None,
            avg_voltage: None,
            temp_in: None,
            temp_out: None,
            temp_ext: [None, None],
            vdd: None,
            fan_duty: None,
            cable_w: None,
            faults: Faults::default(),
            faults_logged: Faults::default(),
            faults_raw: None,
            served_at: None,
        }
    }

    /// A not-ok result carrying a short explanation.
    pub fn problem(source: &str, status: &str, hint: &str) -> Self {
        let mut r = Self::blank(source);
        r.set_problem(status, hint);
        r
    }

    pub fn set_problem(&mut self, status: &str, hint: &str) {
        self.ok = false;
        self.status = Some(status.to_string());
        self.hint = Some(hint.to_string());
    }

    /// One line for the log saying where readings come from, or why not.
    pub fn describe_source(&self) -> String {
        if !self.ok {
            return format!(
                "none ({}: {})",
                self.status.as_deref().unwrap_or("None"),
                self.hint.as_deref().unwrap_or("None")
            );
        }
        match &self.device {
            Some(d) => format!(
                "{} on {} fw v{}",
                self.source,
                d.port.as_deref().unwrap_or("?"),
                d.fw.map_or("?".to_string(), |v| v.to_string())
            ),
            None => self.source.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn faults_serialise_only_known_present_flags() {
        let mut f = Faults::default();
        assert_eq!(serde_json::to_string(&f).unwrap(), "{}");
        assert!(!f.set("bogus", true));
        assert!(f.set("over_power", true));
        assert_eq!(serde_json::to_string(&f).unwrap(), r#"{"over_power":true}"#);
        assert_eq!(f.active().collect::<Vec<_>>(), ["over_power"]);
    }

    #[test]
    fn mask_maps_bits_in_device_order() {
        let f = Faults::from_mask(0b100001);
        assert_eq!(f.active().collect::<Vec<_>>(), ["temp_chip", "imbalance"]);
        assert_eq!(f.get("over_power"), Some(false));
    }

    #[test]
    fn uid_and_bridge_only_fields_are_omitted_when_unset() {
        let mut r = Readings::blank("serial");
        r.device = Some(Device {
            port: Some("COM5".into()),
            fw: Some(5),
            uid: None,
            build: None,
        });
        let s = serde_json::to_string(&r).unwrap();
        assert!(!s.contains("uid") && !s.contains("served_at") && !s.contains("faults_raw"), "{s}");
        assert!(s.contains(r#""temp_ext":[null,null]"#), "{s}");
    }
}
