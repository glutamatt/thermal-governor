use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

// =============================================================================
// Constants + Hardware paths (ThinkPad X1, Intel Core Ultra 7 155H)
// =============================================================================

const TEMP_SENSOR: &str = "/sys/class/thermal/thermal_zone8/temp";
const FAN1_SENSOR: &str = "/sys/class/hwmon/hwmon7/fan1_input";
const FAN2_SENSOR: &str = "/sys/class/hwmon/hwmon7/fan2_input";
const FAN_CONTROL: &str = "/proc/acpi/ibm/fan";
const FAN_CONTROL_PARAM: &str = "/sys/module/thinkpad_acpi/parameters/fan_control";
const THROTTLE_TIME_PATH: &str =
    "/sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_total_time_ms";

const DATA_DIR: &str = "/var/lib/thermal-governor";
const EVENTS_DIR: &str = "/var/lib/thermal-governor/events";
const SETTINGS_FILE: &str = "/var/lib/thermal-governor/settings.json";

const BUFFER_CAPACITY: usize = 300; // 5 min at 1Hz
const POLL_MS: u64 = 1000;

// Event cooldowns (seconds)
const EVENT_COOLDOWN_SECS: u64 = 60;
const LOAD_DEBOUNCE_SECS: u64 = 10;
const RAPID_TEMP_SUSTAINED: usize = 3;
const SOFT_THROTTLE_WINDOW: usize = 10;
const SOFT_THROTTLE_THRESHOLD: usize = 5;

// =============================================================================
// Persisted settings (what hw-tui last set)
// =============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Settings {
    fan_level: String,
    freq_cap_mhz: u32,
    epp: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            fan_level: "auto".into(),
            freq_cap_mhz: 4500,
            epp: "balance_performance".into(),
        }
    }
}

impl Settings {
    fn load() -> Self {
        match fs::read_to_string(SETTINGS_FILE) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                log("settings", &format!("Bad settings file ({e}), using defaults"));
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    fn save(&self) {
        let _ = fs::create_dir_all(DATA_DIR);
        match serde_json::to_string_pretty(self) {
            Ok(json) => match fs::write(SETTINGS_FILE, &json) {
                Ok(()) => log("settings", &format!("Saved: {:?}", self)),
                Err(e) => log("settings", &format!("Write failed: {e}")),
            },
            Err(e) => log("settings", &format!("Serialize failed: {e}")),
        }
    }

    fn apply(&self, dirs: &[PathBuf]) {
        // Fan level
        match self.fan_level.as_str() {
            "auto" => set_fan_level_raw("level auto"),
            "disengaged" => set_fan_level_raw("level disengaged"),
            s => {
                if let Ok(n) = s.parse::<u8>() {
                    set_fan_level_raw(&format!("level {n}"));
                } else {
                    log("settings", &format!("Unknown fan_level '{}', setting auto", s));
                    set_fan_level_raw("level auto");
                }
            }
        }
        // Freq cap
        set_max_freq(dirs, self.freq_cap_mhz);
        // EPP
        set_epp(dirs, &self.epp);
    }
}

// =============================================================================
// Sample (one observation at a point in time)
// =============================================================================

#[derive(Clone)]
struct Sample {
    timestamp: SystemTime,
    temp_c: f64,
    temp_rate: f64,
    cpu_load: f64,
    fan1_rpm: f64,
    fan2_rpm: f64,
    fan_level: String,
    freq_min: f64,
    freq_avg: f64,
    freq_max: f64,
    freq_cap_mhz: u32,
    epp: String,
    throttle_rate: f64,
    rapl_power_w: f64,
}

impl Sample {
    fn csv_header() -> &'static str {
        "timestamp,temp_c,temp_rate,cpu_load,fan1_rpm,fan2_rpm,fan_level,freq_min,freq_avg,freq_max,freq_cap_mhz,epp,throttle_rate,rapl_power_w"
    }

    fn to_csv_row(&self) -> String {
        let ts = self
            .timestamp
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!(
            "{},{:.1},{:.2},{:.3},{:.0},{:.0},{},{:.0},{:.0},{:.0},{},{},{:.1},{:.2}",
            ts,
            self.temp_c,
            self.temp_rate,
            self.cpu_load,
            self.fan1_rpm,
            self.fan2_rpm,
            self.fan_level,
            self.freq_min,
            self.freq_avg,
            self.freq_max,
            self.freq_cap_mhz,
            self.epp,
            self.throttle_rate,
            self.rapl_power_w,
        )
    }
}

// =============================================================================
// Event types
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EventType {
    Throttle,
    TempCross85,
    TempCross90,
    TempCross95,
    RapidTempRise,
    LoadTransitionUp,
    LoadTransitionDown,
    SoftThrottle,
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Throttle => write!(f, "throttle"),
            Self::TempCross85 => write!(f, "temp-cross-85"),
            Self::TempCross90 => write!(f, "temp-cross-90"),
            Self::TempCross95 => write!(f, "temp-cross-95"),
            Self::RapidTempRise => write!(f, "rapid-temp-rise"),
            Self::LoadTransitionUp => write!(f, "load-transition-up"),
            Self::LoadTransitionDown => write!(f, "load-transition-down"),
            Self::SoftThrottle => write!(f, "soft-throttle"),
        }
    }
}

// =============================================================================
// Event detector state
// =============================================================================

struct EventDetector {
    cooldowns: [(EventType, Instant); 8],
    // Temp crossing hysteresis: track if we're "above" each threshold
    above_85: bool,
    above_90: bool,
    above_95: bool,
    // Rapid temp rise: count consecutive samples with rate > 2
    rapid_rise_count: usize,
    // Load state: 0=idle(<0.3), 1=medium(0.3-0.7), 2=heavy(>0.7)
    load_zone: u8,
    load_zone_last_change: Instant,
    // Soft throttle: ring buffer of bools (was soft-throttled in last N samples)
    soft_throttle_ring: VecDeque<bool>,
}

impl EventDetector {
    fn new() -> Self {
        let now = Instant::now() - Duration::from_secs(EVENT_COOLDOWN_SECS + 1);
        Self {
            cooldowns: [
                (EventType::Throttle, now),
                (EventType::TempCross85, now),
                (EventType::TempCross90, now),
                (EventType::TempCross95, now),
                (EventType::RapidTempRise, now),
                (EventType::LoadTransitionUp, now),
                (EventType::LoadTransitionDown, now),
                (EventType::SoftThrottle, now),
            ],
            above_85: false,
            above_90: false,
            above_95: false,
            rapid_rise_count: 0,
            load_zone: 0,
            load_zone_last_change: Instant::now(),
            soft_throttle_ring: VecDeque::with_capacity(SOFT_THROTTLE_WINDOW),
        }
    }

    fn can_fire(&self, event: EventType) -> bool {
        for &(et, last) in &self.cooldowns {
            if et == event {
                return last.elapsed().as_secs() >= EVENT_COOLDOWN_SECS;
            }
        }
        false
    }

    fn mark_fired(&mut self, event: EventType) {
        for (et, last) in &mut self.cooldowns {
            if *et == event {
                *last = Instant::now();
                return;
            }
        }
    }

    fn detect(&mut self, sample: &Sample) -> Vec<EventType> {
        let mut events = Vec::new();

        // Throttle detected
        if sample.throttle_rate > 0.0 && self.can_fire(EventType::Throttle) {
            events.push(EventType::Throttle);
        }

        // Temp threshold crossings (upward with hysteresis)
        if sample.temp_c >= 85.0 && !self.above_85 {
            self.above_85 = true;
            if self.can_fire(EventType::TempCross85) {
                events.push(EventType::TempCross85);
            }
        } else if sample.temp_c < 83.0 {
            self.above_85 = false;
        }

        if sample.temp_c >= 90.0 && !self.above_90 {
            self.above_90 = true;
            if self.can_fire(EventType::TempCross90) {
                events.push(EventType::TempCross90);
            }
        } else if sample.temp_c < 88.0 {
            self.above_90 = false;
        }

        if sample.temp_c >= 95.0 && !self.above_95 {
            self.above_95 = true;
            if self.can_fire(EventType::TempCross95) {
                events.push(EventType::TempCross95);
            }
        } else if sample.temp_c < 93.0 {
            self.above_95 = false;
        }

        // Rapid temp rise: rate > 2°C/s sustained for 3+ samples
        if sample.temp_rate > 2.0 {
            self.rapid_rise_count += 1;
            if self.rapid_rise_count >= RAPID_TEMP_SUSTAINED
                && self.can_fire(EventType::RapidTempRise)
            {
                events.push(EventType::RapidTempRise);
            }
        } else {
            self.rapid_rise_count = 0;
        }

        // Load transitions (debounced)
        let new_zone = if sample.cpu_load < 0.3 {
            0
        } else if sample.cpu_load < 0.7 {
            1
        } else {
            2
        };
        if new_zone != self.load_zone
            && self.load_zone_last_change.elapsed().as_secs() >= LOAD_DEBOUNCE_SECS
        {
            let event = if new_zone > self.load_zone {
                EventType::LoadTransitionUp
            } else {
                EventType::LoadTransitionDown
            };
            if self.can_fire(event) {
                events.push(event);
            }
            self.load_zone = new_zone;
            self.load_zone_last_change = Instant::now();
        }

        // Soft throttle: freq_max < 90% of effective ceiling
        let effective_ceiling = epp_effective_max(&sample.epp).min(sample.freq_cap_mhz as f64);
        let is_soft_throttled = sample.freq_max < 0.9 * effective_ceiling && sample.cpu_load > 0.3;
        if self.soft_throttle_ring.len() >= SOFT_THROTTLE_WINDOW {
            self.soft_throttle_ring.pop_front();
        }
        self.soft_throttle_ring.push_back(is_soft_throttled);
        let soft_count = self.soft_throttle_ring.iter().filter(|&&x| x).count();
        if soft_count >= SOFT_THROTTLE_THRESHOLD && self.can_fire(EventType::SoftThrottle) {
            events.push(EventType::SoftThrottle);
        }

        // Mark all fired events
        for &e in &events {
            self.mark_fired(e);
        }

        events
    }
}

/// Approximate effective max freq for a given EPP value
fn epp_effective_max(epp: &str) -> f64 {
    match epp {
        "performance" => 4500.0,
        "balance_performance" => 4500.0,
        "balance_power" => 2000.0,
        "power" => 2000.0,
        _ => 4500.0,
    }
}

// =============================================================================
// Temperature history (ring buffer + linear regression for rate)
// =============================================================================

struct TempHistory {
    entries: VecDeque<(Instant, f64)>,
    window: Duration,
}

impl TempHistory {
    fn new(window_secs: u64) -> Self {
        Self {
            entries: VecDeque::new(),
            window: Duration::from_secs(window_secs),
        }
    }

    fn push(&mut self, temp: f64) {
        let now = Instant::now();
        self.entries.push_back((now, temp));
        let cutoff = now - self.window;
        while let Some(&(t, _)) = self.entries.front() {
            if t < cutoff {
                self.entries.pop_front();
            } else {
                break;
            }
        }
    }

    fn rate(&self) -> f64 {
        if self.entries.len() < 2 {
            return 0.0;
        }
        let base = self.entries.front().unwrap().0;
        let n = self.entries.len() as f64;

        let mut sum_x = 0.0;
        let mut sum_y = 0.0;
        for &(t, temp) in &self.entries {
            sum_x += t.duration_since(base).as_secs_f64();
            sum_y += temp;
        }
        let mean_x = sum_x / n;
        let mean_y = sum_y / n;

        let mut num = 0.0;
        let mut den = 0.0;
        for &(t, temp) in &self.entries {
            let dx = t.duration_since(base).as_secs_f64() - mean_x;
            let dy = temp - mean_y;
            num += dx * dy;
            den += dx * dx;
        }

        if den.abs() < 1e-9 {
            0.0
        } else {
            num / den
        }
    }
}

// =============================================================================
// CPU usage reader (from /proc/stat)
// =============================================================================

struct CpuUsage {
    prev_idle: u64,
    prev_total: u64,
}

impl CpuUsage {
    fn new() -> Self {
        let (idle, total) = Self::read_stat();
        Self {
            prev_idle: idle,
            prev_total: total,
        }
    }

    fn read_stat() -> (u64, u64) {
        if let Ok(s) = fs::read_to_string("/proc/stat") {
            if let Some(line) = s.lines().next() {
                let vals: Vec<u64> = line
                    .split_whitespace()
                    .skip(1)
                    .filter_map(|v| v.parse().ok())
                    .collect();
                if vals.len() >= 5 {
                    let idle = vals[3] + vals[4];
                    let total: u64 = vals.iter().sum();
                    return (idle, total);
                }
            }
        }
        (0, 0)
    }

    fn sample(&mut self) -> f64 {
        let (idle, total) = Self::read_stat();
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

// =============================================================================
// Hardware I/O
// =============================================================================

fn read_sysfs_i64(path: impl AsRef<Path>) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_sysfs_string(path: impl AsRef<Path>) -> Option<String> {
    Some(fs::read_to_string(path).ok()?.trim().to_string())
}

fn cpu_temp() -> f64 {
    read_sysfs_i64(TEMP_SENSOR)
        .map(|t| t as f64 / 1000.0)
        .unwrap_or(0.0)
}

fn fan_rpms() -> (f64, f64) {
    let f1 = read_sysfs_i64(FAN1_SENSOR).unwrap_or(0) as f64;
    let f2 = read_sysfs_i64(FAN2_SENSOR).unwrap_or(0) as f64;
    let clamp = |v: f64| if v >= 60000.0 { 0.0 } else { v };
    (clamp(f1), clamp(f2))
}

fn read_throttle_time_ms() -> u64 {
    read_sysfs_i64(THROTTLE_TIME_PATH).unwrap_or(0) as u64
}

fn read_fan_level() -> String {
    if let Ok(content) = fs::read_to_string(FAN_CONTROL) {
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("level:") {
                return rest.trim().to_string();
            }
        }
    }
    "unknown".into()
}

fn set_fan_level_raw(cmd: &str) {
    let _ = fs::write(FAN_CONTROL, cmd);
}

fn cpufreq_dirs() -> Vec<PathBuf> {
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

fn read_cpu_freqs(dirs: &[PathBuf]) -> (f64, f64, f64) {
    let mut sum = 0.0;
    let mut min = f64::MAX;
    let mut max = 0.0f64;
    let mut count = 0;
    for d in dirs {
        if let Some(freq) = read_sysfs_i64(d.join("scaling_cur_freq")) {
            let mhz = freq as f64 / 1000.0;
            sum += mhz;
            min = min.min(mhz);
            max = max.max(mhz);
            count += 1;
        }
    }
    if count == 0 {
        (0.0, 0.0, 0.0)
    } else {
        (min, sum / count as f64, max)
    }
}

fn read_freq_cap(dirs: &[PathBuf]) -> u32 {
    // Read from first cpu that has scaling_max_freq
    for d in dirs {
        if let Some(freq) = read_sysfs_i64(d.join("scaling_max_freq")) {
            return (freq / 1000) as u32;
        }
    }
    4500
}

fn read_epp(dirs: &[PathBuf]) -> String {
    for d in dirs {
        if let Some(epp) = read_sysfs_string(d.join("energy_performance_preference")) {
            return epp;
        }
    }
    "unknown".into()
}

fn set_max_freq(dirs: &[PathBuf], mhz: u32) {
    let val = (mhz as u64 * 1000).to_string();
    for d in dirs {
        let _ = fs::write(d.join("scaling_max_freq"), &val);
    }
}

fn set_epp(dirs: &[PathBuf], epp: &str) {
    for d in dirs {
        let _ = fs::write(d.join("energy_performance_preference"), epp);
    }
}

fn check_fan_control() -> bool {
    fs::read_to_string(FAN_CONTROL_PARAM)
        .map(|v| v.trim() == "Y" || v.trim() == "1")
        .unwrap_or(false)
}

fn enable_fan_control() -> bool {
    if check_fan_control() {
        return true;
    }
    log("fan", "Enabling fan_control via modprobe...");
    let status = Command::new("modprobe")
        .args(["thinkpad_acpi", "fan_control=1"])
        .status();
    match status {
        Ok(s) if s.success() => {
            log("fan", "fan_control=1 enabled");
            true
        }
        Ok(s) => {
            log("fan", &format!("modprobe failed with status {s}"));
            false
        }
        Err(e) => {
            log("fan", &format!("modprobe error: {e}"));
            false
        }
    }
}

fn read_rapl_energy_uj() -> u64 {
    read_sysfs_i64("/sys/class/powercap/intel-rapl:0/energy_uj").unwrap_or(0) as u64
}

// =============================================================================
// Event dump (write rolling buffer to CSV)
// =============================================================================

fn dump_event(buffer: &VecDeque<Sample>, event: EventType) {
    let _ = fs::create_dir_all(EVENTS_DIR);

    let ts = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let filename = format!("{}/{}-{}.csv", EVENTS_DIR, ts, event);

    let mut csv = String::with_capacity(buffer.len() * 120);
    csv.push_str(Sample::csv_header());
    csv.push('\n');
    for s in buffer {
        csv.push_str(&s.to_csv_row());
        csv.push('\n');
    }

    match fs::write(&filename, &csv) {
        Ok(()) => log(
            "event",
            &format!("{} → {} ({} samples)", event, filename, buffer.len()),
        ),
        Err(e) => log("event", &format!("Failed to write {}: {}", filename, e)),
    }
}

// =============================================================================
// Logging
// =============================================================================

fn log(tag: &str, msg: &str) {
    let ts = timestamp();
    eprintln!("[{ts}] [{tag}] {msg}");
}

fn timestamp() -> String {
    Command::new("date")
        .arg("+%H:%M:%S")
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_else(|_| "??:??:??".into())
}

// =============================================================================
// Observer loop
// =============================================================================

fn observer(stop: &AtomicBool) {
    let dirs = cpufreq_dirs();
    if dirs.is_empty() {
        log("obs", "No cpufreq dirs found!");
        return;
    }

    // Startup: ensure fan control, restore settings
    if !enable_fan_control() {
        log("obs", "WARNING: fan control not available, fan commands will fail");
    }

    let settings = Settings::load();
    log("obs", &format!("Restoring settings: {:?}", settings));
    settings.apply(&dirs);

    let mut last_settings = settings;

    let mut temp_history = TempHistory::new(16);
    let mut cpu_usage = CpuUsage::new();
    let mut buffer: VecDeque<Sample> = VecDeque::with_capacity(BUFFER_CAPACITY);
    let mut detector = EventDetector::new();

    let mut prev_throttle_ms = read_throttle_time_ms();
    let mut prev_energy_uj = read_rapl_energy_uj();
    let mut prev_time = Instant::now();
    let mut tick: u64 = 0;

    log("obs", "Observer loop started (1Hz sampling, 300-sample buffer)");

    while !stop.load(Ordering::Relaxed) {
        let now = Instant::now();
        let dt = now.duration_since(prev_time).as_secs_f64().max(0.001);
        prev_time = now;

        // --- Sample all sensors ---
        let temp = cpu_temp();
        temp_history.push(temp);
        let temp_rate = temp_history.rate();

        let load = cpu_usage.sample();
        let (f1, f2) = fan_rpms();
        let fan_level = read_fan_level();
        let (freq_min, freq_avg, freq_max) = read_cpu_freqs(&dirs);
        let freq_cap = read_freq_cap(&dirs);
        let epp = read_epp(&dirs);

        let throttle_ms = read_throttle_time_ms();
        let throttle_rate = (throttle_ms.saturating_sub(prev_throttle_ms)) as f64 / dt;
        prev_throttle_ms = throttle_ms;

        let energy_uj = read_rapl_energy_uj();
        let rapl = (energy_uj.wrapping_sub(prev_energy_uj)) as f64 / (dt * 1_000_000.0);
        prev_energy_uj = energy_uj;

        let sample = Sample {
            timestamp: SystemTime::now(),
            temp_c: temp,
            temp_rate,
            cpu_load: load,
            fan1_rpm: f1,
            fan2_rpm: f2,
            fan_level: fan_level.clone(),
            freq_min,
            freq_avg,
            freq_max,
            freq_cap_mhz: freq_cap,
            epp: epp.clone(),
            throttle_rate,
            rapl_power_w: rapl,
        };

        // --- Push to rolling buffer ---
        if buffer.len() >= BUFFER_CAPACITY {
            buffer.pop_front();
        }
        buffer.push_back(sample.clone());

        // --- Detect events and dump ---
        let events = detector.detect(&sample);
        for event in &events {
            dump_event(&buffer, *event);
        }

        // --- Settings persistence: detect changes from hw-tui ---
        let current_settings = Settings {
            fan_level: fan_level.clone(),
            freq_cap_mhz: freq_cap,
            epp: epp.clone(),
        };
        if current_settings != last_settings {
            log(
                "settings",
                &format!(
                    "Change detected: fan={} cap={} epp={}",
                    fan_level, freq_cap, epp
                ),
            );
            current_settings.save();
            last_settings = current_settings;
        }

        // --- Periodic status log (every 60s) ---
        tick += 1;
        if tick % 60 == 0 {
            log(
                "status",
                &format!(
                    "{:.0}°C Δ{:+.1}°C/s load={:.0}% fan={} rpm={:.0}/{:.0} freq={:.0}/{:.0}/{:.0} cap={} epp={} rapl={:.1}W",
                    temp, temp_rate, load * 100.0,
                    fan_level, f1, f2,
                    freq_min, freq_avg, freq_max,
                    freq_cap, epp, rapl,
                ),
            );
        }

        thread::sleep(Duration::from_millis(POLL_MS));
    }
}

// =============================================================================
// Main
// =============================================================================

fn main() {
    eprintln!("================================================");
    eprintln!("  thermal-governor v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("  Observer daemon for ThinkPad X1");
    eprintln!("================================================");
    eprintln!("  Mode: passive observer + settings persistence");
    eprintln!("  Poll: {}ms  Buffer: {} samples (5 min)", POLL_MS, BUFFER_CAPACITY);
    eprintln!("  Settings: {SETTINGS_FILE}");
    eprintln!("  Events: {EVENTS_DIR}/");
    eprintln!("================================================\n");

    log(
        "main",
        &format!("Initial: {:.0}°C, fan={}", cpu_temp(), read_fan_level()),
    );

    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))
        .expect("Failed to register SIGTERM handler");
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))
        .expect("Failed to register SIGINT handler");

    let dirs = cpufreq_dirs();

    observer(&stop);

    // Graceful shutdown: conservative defaults
    log("main", "Shutting down → fan=auto, cap=2000 MHz");
    set_fan_level_raw("level auto");
    set_max_freq(&dirs, 2000);
    log("main", "Done. Goodbye.");
}
