//! Fan curve: the least fan that keeps max frequency from dropping.
//!
//! The drops to avoid are PL1 cuts by the EC, never seen below 75 °C package
//! (see the "Power limits" section of the thermal-governor SKILL.md). Two
//! linear demands, 0.0–1.0, from package power (early: heat is coming) and
//! from package temperature (the correction), each on a smoothed input. The
//! higher one wins and maps to the nearest of the 9 fan levels. The demand
//! follows the inputs both ways: the fan stops soon after the load ends.
//!
//! The daemon runs it; hw-tui switches the mode and shows the state through
//! two small files in /run.

use serde::{Deserialize, Serialize};
use std::fs;
use std::io;

// =============================================================================
// Tuning (from the 2026-09-25 tests at cap 2000)
// =============================================================================

/// Fan off holds up to ~9 W; ~23 W needs full speed
const POWER_ZERO_W: f64 = 9.0;
const POWER_FULL_W: f64 = 23.0;
/// Below 62 °C no demand; full speed at 74 °C, under the 75 °C no-cut limit
const TEMP_ZERO_C: f64 = 62.0;
const TEMP_FULL_C: f64 = 74.0;
/// Power smoothing: long enough to ignore short spikes, short enough to act
/// before the package heats up (70 → 77 °C in ~15 s at 25 W)
const POWER_TAU_S: f64 = 10.0;
/// Temperature smoothing. The package sensor jumps by several °C in 1–2 s
/// with short bursts of load; each jump would start the fan. PL1 cuts come
/// from sustained heat, not from these jumps. Replayed on the 1 Hz event logs:
/// 8 s keeps the fan starts at the rate of the old 30 s fall, and after 10 s
/// above 72 °C the smoothed demand is the raw one (the power demand covers a
/// fast heat-up).
const TEMP_TAU_S: f64 = 8.0;
/// After a hard limit, full demand falls with this time constant
const GUARD_FALL_TAU_S: f64 = 30.0;
/// Margin around the midpoint between two levels, against flapping
const HYSTERESIS_RPM: f64 = 150.0;

/// Hard limits, whatever the mode: full speed at once. The EC cuts PL1 from
/// ~77 °C, and the kernel powers off when a SEN sensor reaches 80 °C.
const GUARD_PKG_C: f64 = 80.0;
const GUARD_SEN_C: f64 = 70.0;

/// Fan levels with their measured fan1 RPM (idle, laptop flat on the desk)
const LEVELS: [(&str, f64); 9] = [
    ("0", 0.0),
    ("1", 3985.0),
    ("2", 4839.0),
    ("3", 5272.0),
    ("4", 5790.0),
    ("5", 6438.0),
    ("6", 6849.0),
    ("7", 7537.0),
    ("disengaged", 9540.0),
];
pub const FULL_SPEED: &str = "disengaged";

// =============================================================================
// Mode and status files
// =============================================================================

pub const RUN_DIR: &str = "/run/thermal-governor";
const MODE_FILE: &str = "/run/thermal-governor/fan-mode";
const STATUS_FILE: &str = "/run/thermal-governor/fan-status.json";

/// Who drives the fan. Lives in /run: every boot starts on the curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FanMode {
    /// The daemon runs the curve
    Curve,
    /// The EC's own control
    Auto,
    /// hw-tui set a level by hand; the daemon only applies the hard limits
    Manual,
}

impl FanMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Curve => "curve",
            Self::Auto => "auto",
            Self::Manual => "manual",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s.trim() {
            "curve" => Some(Self::Curve),
            "auto" => Some(Self::Auto),
            "manual" => Some(Self::Manual),
            _ => None,
        }
    }

    /// Curve when the file is missing or unreadable
    pub fn read() -> Self {
        fs::read_to_string(MODE_FILE)
            .ok()
            .and_then(|s| Self::parse(&s))
            .unwrap_or(Self::Curve)
    }

    pub fn write(self) -> io::Result<()> {
        fs::create_dir_all(RUN_DIR)?;
        fs::write(MODE_FILE, self.as_str())
    }
}

/// What the daemon did on its last tick, for hw-tui
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    /// Unix seconds; hw-tui treats an old status as "daemon not running"
    pub updated: u64,
    pub mode: FanMode,
    /// Level the daemon applied, None when it leaves the fan to the EC or hw-tui
    pub level: Option<String>,
    pub need: f64,
    pub need_power: f64,
    pub need_temp: f64,
    /// Why a hard limit forced full speed
    pub guard: Option<String>,
}

impl Status {
    pub fn read() -> Option<Self> {
        serde_json::from_str(&fs::read_to_string(STATUS_FILE).ok()?).ok()
    }

    pub fn write(&self) -> io::Result<()> {
        fs::create_dir_all(RUN_DIR)?;
        let json = serde_json::to_string(self).map_err(io::Error::other)?;
        // Write then rename: hw-tui never reads a half-written file
        let tmp = format!("{STATUS_FILE}.tmp");
        fs::write(&tmp, json)?;
        fs::rename(tmp, STATUS_FILE)
    }
}

// =============================================================================
// Curve
// =============================================================================

pub struct Decision {
    pub level: &'static str,
    pub need: f64,
    pub need_power: f64,
    pub need_temp: f64,
}

pub struct Curve {
    power_avg: Option<f64>,
    temp_avg: Option<f64>,
    /// Demand left by a tripped hard limit, falling to 0
    guard_need: f64,
    level: usize,
}

impl Default for Curve {
    fn default() -> Self {
        Self::new()
    }
}

impl Curve {
    pub fn new() -> Self {
        Self {
            power_avg: None,
            temp_avg: None,
            guard_need: 0.0,
            level: 0,
        }
    }

    /// One step. `dt`: seconds since the last call. `power_w`: package power,
    /// None when RAPL is unreadable (the temperature demand alone then).
    pub fn update(&mut self, dt: f64, temp_c: f64, power_w: Option<f64>) -> Decision {
        if let Some(p) = power_w {
            smooth(&mut self.power_avg, p, dt, POWER_TAU_S);
        }
        let temp_avg = smooth(&mut self.temp_avg, temp_c, dt, TEMP_TAU_S);
        let need_power = self.power_avg.map_or(0.0, |p| ramp(p, POWER_ZERO_W, POWER_FULL_W));
        let need_temp = ramp(temp_avg, TEMP_ZERO_C, TEMP_FULL_C);
        self.guard_need *= (-dt / GUARD_FALL_TAU_S).exp();
        let need = need_power.max(need_temp).max(self.guard_need);

        self.level = pick_level(self.level, need * LEVELS[LEVELS.len() - 1].1);
        Decision {
            level: LEVELS[self.level].0,
            need,
            need_power,
            need_temp,
        }
    }
}

impl Curve {
    /// A hard limit tripped: full demand, which then falls slowly. Without
    /// it, a sensor hovering at the limit would flip the fan between off and
    /// full speed every second.
    pub fn force_full(&mut self) {
        self.guard_need = 1.0;
    }
}

/// Fan level whose measured RPM is the closest to `rpm`
pub fn nearest_level(rpm: f64) -> &'static str {
    LEVELS
        .iter()
        .min_by(|a, b| (a.1 - rpm).abs().total_cmp(&(b.1 - rpm).abs()))
        .map_or("0", |l| l.0)
}

/// Exponential moving average with time constant `tau`, started on the first
/// value. Returns the new average.
fn smooth(avg: &mut Option<f64>, x: f64, dt: f64, tau: f64) -> f64 {
    let a = avg.get_or_insert(x);
    *a += (1.0 - (-dt / tau).exp()) * (x - *a);
    *a
}

/// 0.0 at `zero`, 1.0 at `full`, linear between
fn ramp(x: f64, zero: f64, full: f64) -> f64 {
    ((x - zero) / (full - zero)).clamp(0.0, 1.0)
}

/// Nearest level to the target RPM, moving only when the target passes the
/// midpoint between two levels by HYSTERESIS_RPM
fn pick_level(current: usize, target_rpm: f64) -> usize {
    let midpoint = |lo: usize| (LEVELS[lo].1 + LEVELS[lo + 1].1) / 2.0;
    let mut level = current.min(LEVELS.len() - 1);
    while level + 1 < LEVELS.len() && target_rpm > midpoint(level) + HYSTERESIS_RPM {
        level += 1;
    }
    while level > 0 && target_rpm < midpoint(level - 1) - HYSTERESIS_RPM {
        level -= 1;
    }
    level
}

/// Why the fan must go to FULL_SPEED now, if it must: the package or a board
/// sensor is near its limit
pub fn guard(temp_c: Option<f64>, sen_max_c: Option<f64>) -> Option<String> {
    if let Some(t) = temp_c.filter(|&t| t >= GUARD_PKG_C) {
        return Some(format!("package {t:.0} °C"));
    }
    if let Some(t) = sen_max_c.filter(|&t| t >= GUARD_SEN_C) {
        return Some(format!("board sensor {t:.0} °C"));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_machine_keeps_the_fan_off() {
        let mut c = Curve::new();
        for _ in 0..120 {
            let d = c.update(1.0, 55.0, Some(6.0));
            assert_eq!(d.level, "0");
        }
    }

    #[test]
    fn full_load_goes_to_full_speed_before_the_heat_arrives() {
        let mut c = Curve::new();
        c.update(1.0, 60.0, Some(6.0));
        // 25 W while the package is still cool: the power demand acts first
        let mut level = "0";
        for s in 1..=30 {
            level = c.update(1.0, 60.0, Some(25.0)).level;
            if level == FULL_SPEED {
                assert!(s <= 20, "full speed only after {s} s");
                break;
            }
        }
        assert_eq!(level, FULL_SPEED);
    }

    #[test]
    fn temperature_alone_reaches_full_speed_at_74() {
        let mut c = Curve::new();
        c.update(1.0, 60.0, None);
        let mut level = "0";
        for s in 1..=30 {
            level = c.update(1.0, 74.0, None).level;
            if level == FULL_SPEED {
                assert!(s <= 25, "full speed only after {s} s");
                break;
            }
        }
        assert_eq!(level, FULL_SPEED);
        assert_eq!(ramp(62.0, TEMP_ZERO_C, TEMP_FULL_C), 0.0);
        assert_eq!(ramp(68.0, TEMP_ZERO_C, TEMP_FULL_C), 0.5);
    }

    #[test]
    fn a_short_temperature_spike_does_not_start_the_fan() {
        let mut c = Curve::new();
        for _ in 0..30 {
            c.update(1.0, 60.0, Some(6.0));
        }
        for _ in 0..3 {
            assert_eq!(c.update(1.0, 70.0, Some(6.0)).level, "0");
        }
        assert_eq!(c.update(1.0, 61.0, Some(6.0)).level, "0");
    }

    #[test]
    fn the_fan_stops_soon_after_the_load_ends() {
        let mut c = Curve::new();
        for _ in 0..120 {
            c.update(1.0, 73.0, Some(24.0));
        }
        assert_eq!(c.update(1.0, 73.0, Some(24.0)).level, FULL_SPEED);
        // The package cools fast once the load is gone
        let mut stopped = None;
        for s in 1..=60 {
            let temp = (73.0 - s as f64).max(58.0);
            if c.update(1.0, temp, Some(7.0)).level == "0" {
                stopped = Some(s);
                break;
            }
        }
        let s = stopped.expect("fan still on 60 s after the load");
        assert!(s <= 20, "fan off only after {s} s");
    }

    #[test]
    fn hysteresis_holds_the_level_near_a_midpoint() {
        let mid = (LEVELS[2].1 + LEVELS[3].1) / 2.0;
        assert_eq!(pick_level(2, mid + 100.0), 2);
        assert_eq!(pick_level(2, mid + 200.0), 3);
        assert_eq!(pick_level(3, mid - 100.0), 3);
        assert_eq!(pick_level(3, mid - 200.0), 2);
    }

    #[test]
    fn a_tripped_guard_falls_slowly() {
        let mut c = Curve::new();
        c.update(1.0, 50.0, Some(6.0));
        c.force_full();
        // guard cleared, machine cool: still high a few seconds later
        let d = c.update(1.0, 50.0, Some(6.0));
        assert_eq!(d.level, FULL_SPEED);
        for _ in 0..10 {
            c.update(1.0, 50.0, Some(6.0));
        }
        assert!(c.update(1.0, 50.0, Some(6.0)).need > 0.6);
    }

    #[test]
    fn nearest_level_by_rpm() {
        assert_eq!(nearest_level(0.0), "0");
        assert_eq!(nearest_level(4800.0), "2");
        assert_eq!(nearest_level(9000.0), FULL_SPEED);
    }

    #[test]
    fn guard_forces_full_speed() {
        assert!(guard(Some(79.0), Some(69.0)).is_none());
        assert_eq!(guard(Some(80.0), None).as_deref(), Some("package 80 °C"));
        assert_eq!(guard(None, Some(70.0)).as_deref(), Some("board sensor 70 °C"));
    }

    #[test]
    fn mode_parses_its_own_output() {
        for m in [FanMode::Curve, FanMode::Auto, FanMode::Manual] {
            assert_eq!(FanMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(FanMode::parse("curve\n"), Some(FanMode::Curve));
        assert_eq!(FanMode::parse("bogus"), None);
    }
}
