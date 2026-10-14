//! Reads the laptop battery state from `/sys/class/power_supply`.

use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    Unknown = 0,
    Discharging = 1,
    Charging = 2,
    /// Plugged in, not charging (full, or charge limit reached).
    OnAc = 3,
}

impl PowerState {
    pub fn from_i64(v: i64) -> Self {
        match v {
            1 => Self::Discharging,
            2 => Self::Charging,
            3 => Self::OnAc,
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BatteryReading {
    /// 0–100
    pub percent: f64,
    /// Instantaneous power draw in watts (always positive), if the battery reports it.
    pub power_w: Option<f64>,
    pub state: PowerState,
    /// Estimated seconds until empty (discharging) or full (charging).
    pub time_left_s: Option<f64>,
}

/// Returns the first system battery (`type == Battery`, `scope != Device`), skipping
/// peripheral batteries such as mice or headsets.
pub fn find_battery() -> Option<PathBuf> {
    find_battery_in(Path::new("/sys/class/power_supply"))
}

pub fn find_battery_in(root: &Path) -> Option<PathBuf> {
    let mut entries: Vec<_> = fs::read_dir(root).ok()?.flatten().map(|e| e.path()).collect();
    entries.sort();
    entries.into_iter().find(|p| {
        let kv = read_uevent(p);
        kv.iter().any(|(k, v)| k == "POWER_SUPPLY_TYPE" && v == "Battery")
            && !kv.iter().any(|(k, v)| k == "POWER_SUPPLY_SCOPE" && v == "Device")
    })
}

pub fn read_battery(path: &Path) -> Option<BatteryReading> {
    parse_uevent(&read_uevent(path))
}

fn read_uevent(path: &Path) -> Vec<(String, String)> {
    fs::read_to_string(path.join("uevent"))
        .map(|s| s.lines().filter_map(|l| l.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect())
        .unwrap_or_default()
}

pub fn parse_uevent(kv: &[(String, String)]) -> Option<BatteryReading> {
    let get = |key: &str| kv.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str());
    let num = |key: &str| get(key).and_then(|v| v.trim().parse::<f64>().ok());

    // Values are in µV, µA, µW, µAh and µWh.
    let voltage_v = num("POWER_SUPPLY_VOLTAGE_NOW").map(|v| v / 1e6);
    let power_w = num("POWER_SUPPLY_POWER_NOW")
        .map(|p| p / 1e6)
        .or_else(|| Some(num("POWER_SUPPLY_CURRENT_NOW")? / 1e6 * voltage_v?))
        .map(f64::abs);

    let (now_wh, full_wh) = match (num("POWER_SUPPLY_ENERGY_NOW"), num("POWER_SUPPLY_ENERGY_FULL")) {
        (Some(n), Some(f)) => (Some(n / 1e6), Some(f / 1e6)),
        _ => {
            let v = voltage_v.or(num("POWER_SUPPLY_VOLTAGE_MIN_DESIGN").map(|v| v / 1e6));
            (
                num("POWER_SUPPLY_CHARGE_NOW").zip(v).map(|(c, v)| c / 1e6 * v),
                num("POWER_SUPPLY_CHARGE_FULL").zip(v).map(|(c, v)| c / 1e6 * v),
            )
        }
    };

    let percent = num("POWER_SUPPLY_CAPACITY").or_else(|| Some(now_wh? / full_wh? * 100.0))?;

    let state = match get("POWER_SUPPLY_STATUS") {
        Some("Discharging") => PowerState::Discharging,
        Some("Charging") => PowerState::Charging,
        Some("Full") | Some("Not charging") => PowerState::OnAc,
        _ => PowerState::Unknown,
    };

    let time_left_s = match (state, power_w, now_wh, full_wh) {
        (PowerState::Discharging, Some(p), Some(n), _) if p > 0.1 => Some(n / p * 3600.0),
        (PowerState::Charging, Some(p), Some(n), Some(f)) if p > 0.1 => Some((f - n).max(0.0) / p * 3600.0),
        _ => None,
    };

    Some(BatteryReading { percent: percent.clamp(0.0, 100.0), power_w, state, time_left_s })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kv(s: &str) -> Vec<(String, String)> {
        s.lines().filter_map(|l| l.trim().split_once('=')).map(|(k, v)| (k.into(), v.into())).collect()
    }

    #[test]
    fn charge_based_battery_without_power_now() {
        // Dell XPS 13 9315
        let r = parse_uevent(&kv("
            POWER_SUPPLY_STATUS=Discharging
            POWER_SUPPLY_VOLTAGE_MIN_DESIGN=11550000
            POWER_SUPPLY_VOLTAGE_NOW=11364000
            POWER_SUPPLY_CURRENT_NOW=732000
            POWER_SUPPLY_CHARGE_FULL=3494000
            POWER_SUPPLY_CHARGE_NOW=1757000
            POWER_SUPPLY_CAPACITY=50"))
        .unwrap();
        assert_eq!(r.state, PowerState::Discharging);
        assert_eq!(r.percent, 50.0);
        let p = r.power_w.unwrap();
        assert!((p - 8.318).abs() < 0.01, "{p}");
        // 1.757 Ah * 11.364 V / 8.318 W ≈ 2.4 h
        let h = r.time_left_s.unwrap() / 3600.0;
        assert!((h - 2.4).abs() < 0.05, "{h}");
    }

    #[test]
    fn energy_based_battery() {
        let r = parse_uevent(&kv("
            POWER_SUPPLY_STATUS=Charging
            POWER_SUPPLY_POWER_NOW=20000000
            POWER_SUPPLY_ENERGY_FULL=50000000
            POWER_SUPPLY_ENERGY_NOW=40000000"))
        .unwrap();
        assert_eq!(r.state, PowerState::Charging);
        assert_eq!(r.percent, 80.0);
        assert_eq!(r.power_w, Some(20.0));
        assert_eq!(r.time_left_s, Some(1800.0));
    }

    #[test]
    fn full_battery_is_on_ac() {
        let r = parse_uevent(&kv("POWER_SUPPLY_STATUS=Not charging\nPOWER_SUPPLY_CAPACITY=80")).unwrap();
        assert_eq!(r.state, PowerState::OnAc);
        assert_eq!(r.power_w, None);
    }
}
