//! sysfs / procfs access: sensors, fan, frequency cap, EPP, power.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

// =============================================================================
// Paths
// =============================================================================

// Zone and hwmon numbers are enumeration order: they move across boots and
// module reloads. Both are discovered by name, never hardcoded.
const TEMP_ZONE_TYPE: &str = "x86_pkg_temp";
pub const TEMP_SENSOR_FALLBACK: &str = "/sys/class/thermal/thermal_zone8/temp";
const THINKPAD_HWMON_NAME: &str = "thinkpad";

const FAN_CONTROL: &str = "/proc/acpi/ibm/fan";
const FAN_CONTROL_PARAM: &str = "/sys/module/thinkpad_acpi/parameters/fan_control";
const THROTTLE_TIME_PATH: &str =
    "/sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_total_time_ms";
const RAPL_ENERGY_PATH: &str = "/sys/class/powercap/intel-rapl:0/energy_uj";
const RAPL_MAX_ENERGY_PATH: &str = "/sys/class/powercap/intel-rapl:0/max_energy_range_uj";
const BATTERY_STATUS_PATH: &str = "/sys/class/power_supply/BAT0/status";
const BATTERY_POWER_PATH: &str = "/sys/class/power_supply/BAT0/power_now";

// Above this, a RAPL delta is a counter reset or a suspend/resume gap, not power
const RAPL_MAX_PLAUSIBLE_W: f64 = 500.0;

// =============================================================================
// sysfs helpers
// =============================================================================

pub fn read_i64(path: impl AsRef<Path>) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

pub fn read_string(path: impl AsRef<Path>) -> Option<String> {
    Some(fs::read_to_string(path).ok()?.trim().to_string())
}

// =============================================================================
// Temperature
// =============================================================================

/// Package temperature sensor, found by zone type
pub fn find_temp_sensor() -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/thermal/").ok()?.flatten() {
        if !entry.file_name().to_string_lossy().starts_with("thermal_zone") {
            continue;
        }
        if read_string(entry.path().join("type")).as_deref() == Some(TEMP_ZONE_TYPE) {
            return Some(entry.path().join("temp"));
        }
    }
    None
}

pub fn cpu_temp(sensor: &Path) -> Option<f64> {
    read_i64(sensor).map(|t| t as f64 / 1000.0)
}

// =============================================================================
// Fan
// =============================================================================

fn find_hwmon_by_name(name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/hwmon/").ok()?.flatten() {
        if read_string(entry.path().join("name")).as_deref() == Some(name) {
            return Some(entry.path());
        }
    }
    None
}

/// Fan speeds from the thinkpad hwmon node
pub struct FanSensor {
    hwmon: Option<PathBuf>,
}

impl FanSensor {
    pub fn discover() -> Self {
        Self {
            hwmon: find_hwmon_by_name(THINKPAD_HWMON_NAME),
        }
    }

    pub fn is_found(&self) -> bool {
        self.hwmon.is_some()
    }

    /// (fan1, fan2) in RPM, 0 when unreadable. Reloading thinkpad_acpi (hw-tui
    /// does it to enable fan control) re-registers the hwmon node under a new
    /// number, so a failed read triggers a new discovery.
    pub fn rpms(&mut self) -> (u32, u32) {
        let fan1 = match self.read(1) {
            Some(rpm) => rpm,
            None => {
                self.hwmon = find_hwmon_by_name(THINKPAD_HWMON_NAME);
                self.read(1).unwrap_or(0)
            }
        };
        (fan1, self.read(2).unwrap_or(0))
    }

    fn read(&self, fan: u8) -> Option<u32> {
        let rpm = read_i64(self.hwmon.as_ref()?.join(format!("fan{fan}_input")))?;
        // The EC reports ~65535 when it has no valid reading, not a real speed
        Some(if rpm >= 60_000 { 0 } else { rpm as u32 })
    }
}

/// Whether thinkpad_acpi accepts fan commands (module loaded with fan_control=1)
pub fn fan_control_enabled() -> bool {
    matches!(read_string(FAN_CONTROL_PARAM).as_deref(), Some("Y" | "1"))
}

/// Current fan level: "auto", "disengaged", "full-speed" or "0".."7"
pub fn read_fan_level() -> Option<String> {
    parse_fan_level(&fs::read_to_string(FAN_CONTROL).ok()?)
}

fn parse_fan_level(content: &str) -> Option<String> {
    content
        .lines()
        .find_map(|line| line.strip_prefix("level:"))
        .map(|rest| rest.trim().to_string())
}

/// Same values as `read_fan_level`. Fails silently when fan control is off.
pub fn set_fan_level(level: &str) {
    let _ = fs::write(FAN_CONTROL, format!("level {level}"));
}

// =============================================================================
// Frequency + EPP
// =============================================================================

/// cpufreq policy dirs of all online CPUs, sorted
pub fn cpufreq_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu/") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.starts_with("cpu") && s.len() > 3 && s.as_bytes()[3].is_ascii_digit() {
                let p = entry.path().join("cpufreq");
                if p.is_dir() {
                    dirs.push(p);
                }
            }
        }
    }
    dirs.sort();
    dirs
}

// Rounded: scaling_cur_freq reads like 1999998 kHz, which is 2000 MHz
fn read_mhz(dir: &Path, file: &str) -> Option<u32> {
    read_i64(dir.join(file)).map(|khz| ((khz + 500) / 1000) as u32)
}

pub struct FreqStats {
    pub min: u32,
    pub avg: u32,
    pub max: u32,
}

/// Current frequency across all cores, in MHz
pub fn read_freqs(dirs: &[PathBuf]) -> Option<FreqStats> {
    let freqs: Vec<u32> = dirs
        .iter()
        .filter_map(|d| read_mhz(d, "scaling_cur_freq"))
        .collect();
    let sum: u64 = freqs.iter().map(|&f| f as u64).sum();
    Some(FreqStats {
        min: *freqs.iter().min()?,
        avg: ((sum as f64 / freqs.len() as f64).round()) as u32,
        max: *freqs.iter().max()?,
    })
}

/// Highest frequency any core can reach, in MHz (the "no cap" value).
/// Cores differ: on the 155H two P-cores reach 4800, the other P-cores 4500.
pub fn max_hw_freq(dirs: &[PathBuf]) -> Option<u32> {
    dirs.iter()
        .filter_map(|d| read_mhz(d, "cpuinfo_max_freq"))
        .max()
}

/// Frequency cap in MHz. The kernel clamps each core's scaling_max_freq to
/// what that core can reach, so the cap that was written is the highest value
/// across cores, not the value of cpu0.
pub fn read_freq_cap(dirs: &[PathBuf]) -> Option<u32> {
    dirs.iter()
        .filter_map(|d| read_mhz(d, "scaling_max_freq"))
        .max()
}

pub fn set_freq_cap(dirs: &[PathBuf], mhz: u32) {
    let khz = (mhz as u64 * 1000).to_string();
    for d in dirs {
        let _ = fs::write(d.join("scaling_max_freq"), &khz);
    }
}

pub fn read_epp(dirs: &[PathBuf]) -> Option<String> {
    dirs.iter()
        .find_map(|d| read_string(d.join("energy_performance_preference")))
}

pub fn set_epp(dirs: &[PathBuf], epp: &str) {
    for d in dirs {
        let _ = fs::write(d.join("energy_performance_preference"), epp);
    }
}

// =============================================================================
// Rate meters (each sample is the rate since the previous one)
// =============================================================================

/// CPU busy fraction (0.0–1.0) from /proc/stat
pub struct CpuUsage {
    prev_idle: u64,
    prev_total: u64,
}

impl CpuUsage {
    pub fn new() -> Self {
        let (idle, total) = read_proc_stat().unwrap_or((0, 0));
        Self {
            prev_idle: idle,
            prev_total: total,
        }
    }

    pub fn sample(&mut self) -> f64 {
        let (idle, total) = read_proc_stat().unwrap_or((self.prev_idle, self.prev_total));
        let idle_d = idle.saturating_sub(self.prev_idle);
        let total_d = total.saturating_sub(self.prev_total);
        self.prev_idle = idle;
        self.prev_total = total;
        if total_d == 0 {
            0.0
        } else {
            (1.0 - idle_d as f64 / total_d as f64).clamp(0.0, 1.0)
        }
    }
}

impl Default for CpuUsage {
    fn default() -> Self {
        Self::new()
    }
}

fn read_proc_stat() -> Option<(u64, u64)> {
    parse_proc_stat(&fs::read_to_string("/proc/stat").ok()?)
}

/// (idle, total) jiffies from the aggregate `cpu` line; idle includes iowait
fn parse_proc_stat(content: &str) -> Option<(u64, u64)> {
    let vals: Vec<u64> = content
        .lines()
        .next()?
        .split_whitespace()
        .skip(1)
        .filter_map(|v| v.parse().ok())
        .collect();
    if vals.len() < 5 {
        return None;
    }
    Some((vals[3] + vals[4], vals.iter().sum()))
}

/// Package thermal throttle time, in ms of throttling per second
pub struct ThrottleMeter {
    prev_ms: Option<u64>,
    prev_time: Instant,
}

impl ThrottleMeter {
    pub fn new() -> Self {
        Self {
            prev_ms: read_throttle_total_ms(),
            prev_time: Instant::now(),
        }
    }

    /// None when the counter is unreadable now or was at the previous
    /// sample: a delta against a missing value would be a false spike
    pub fn sample(&mut self) -> Option<f64> {
        let ms = read_throttle_total_ms();
        let dt = self.prev_time.elapsed().as_secs_f64().max(0.001);
        let prev = std::mem::replace(&mut self.prev_ms, ms);
        self.prev_time = Instant::now();
        Some(ms?.saturating_sub(prev?) as f64 / dt)
    }

    /// Cumulative throttle time since boot (ms) at the last sample
    pub fn total_ms(&self) -> Option<u64> {
        self.prev_ms
    }
}

impl Default for ThrottleMeter {
    fn default() -> Self {
        Self::new()
    }
}

fn read_throttle_total_ms() -> Option<u64> {
    read_i64(THROTTLE_TIME_PATH).map(|v| v as u64)
}

/// CPU package power from the RAPL energy counter, in W
pub struct RaplMeter {
    max_range_uj: u64,
    prev_uj: Option<u64>,
    prev_time: Instant,
}

impl RaplMeter {
    pub fn new() -> Self {
        Self {
            max_range_uj: read_i64(RAPL_MAX_ENERGY_PATH).unwrap_or(0) as u64,
            prev_uj: read_rapl_energy_uj(),
            prev_time: Instant::now(),
        }
    }

    /// None when the counter is unreadable or the delta is not plausible
    pub fn sample(&mut self) -> Option<f64> {
        let now = read_rapl_energy_uj();
        let dt = self.prev_time.elapsed().as_secs_f64().max(0.001);
        let prev = std::mem::replace(&mut self.prev_uj, now);
        self.prev_time = Instant::now();
        let delta = energy_delta_uj(prev?, now?, self.max_range_uj)?;
        let watts = delta as f64 / (dt * 1_000_000.0);
        (watts <= RAPL_MAX_PLAUSIBLE_W).then_some(watts)
    }
}

impl Default for RaplMeter {
    fn default() -> Self {
        Self::new()
    }
}

fn read_rapl_energy_uj() -> Option<u64> {
    read_i64(RAPL_ENERGY_PATH).map(|v| v as u64)
}

/// The counter wraps at max_energy_range_uj (~262 kJ: every ~1.2 h at 60 W),
/// not at u64::MAX
fn energy_delta_uj(prev: u64, now: u64, max_range: u64) -> Option<u64> {
    if now >= prev {
        Some(now - prev)
    } else if max_range > prev {
        Some(now + (max_range - prev))
    } else {
        None
    }
}

// =============================================================================
// Battery
// =============================================================================

/// Whole-system power draw in W, only known when running on battery
pub fn battery_power_w() -> Option<f64> {
    if read_string(BATTERY_STATUS_PATH).as_deref() != Some("Discharging") {
        return None;
    }
    read_i64(BATTERY_POWER_PATH).map(|uw| uw as f64 / 1_000_000.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fan_level() {
        let content = "status:\t\tenabled\nspeed:\t\t4811\nlevel:\t\tauto\ncommands:\tlevel <level>\n";
        assert_eq!(parse_fan_level(content).as_deref(), Some("auto"));
        assert_eq!(parse_fan_level("level:\t7\n").as_deref(), Some("7"));
        assert_eq!(parse_fan_level("status: enabled\n"), None);
    }

    #[test]
    fn parses_proc_stat() {
        let content = "cpu  100 5 20 800 50 0 7 0 0 0\ncpu0 1 2 3 4 5\n";
        // idle = idle + iowait, total = sum of all fields
        assert_eq!(parse_proc_stat(content), Some((850, 982)));
        assert_eq!(parse_proc_stat("cpu 1 2 3\n"), None);
        assert_eq!(parse_proc_stat(""), None);
    }

    #[test]
    fn energy_delta_handles_wrap() {
        assert_eq!(energy_delta_uj(1_000, 3_000, 10_000), Some(2_000));
        // wrapped: 9_000 → max (10_000) → 0 → 500
        assert_eq!(energy_delta_uj(9_000, 500, 10_000), Some(1_500));
        // unknown range: no way to tell how far it went
        assert_eq!(energy_delta_uj(9_000, 500, 0), None);
    }
}
