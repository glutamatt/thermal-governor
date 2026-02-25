use std::collections::VecDeque;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

// =============================================================================
// Hardware paths (ThinkPad X1, Intel Core Ultra 7 155H)
// =============================================================================

const TEMP_SENSOR: &str = "/sys/class/thermal/thermal_zone8/temp"; // x86_pkg_temp
const FAN1_SENSOR: &str = "/sys/class/hwmon/hwmon7/fan1_input";
const FAN2_SENSOR: &str = "/sys/class/hwmon/hwmon7/fan2_input";
const HWP_BOOST_PATH: &str = "/sys/devices/system/cpu/intel_pstate/hwp_dynamic_boost";
const THROTTLE_TIME_PATH: &str =
    "/sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_total_time_ms";

const STATE_FILE: &str = "/var/lib/thermal-governor/tuned-params.json";
const CONFIG_FILE: &str = "/etc/thermal-governor/config.json";

// =============================================================================
// Config (all knobs — hot-reloaded from CONFIG_FILE)
// =============================================================================

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    // PD gains (MHz units: kp=100 means 100 MHz per °C error)
    kp: f64,
    kd: f64,
    max_ramp: u32, // MHz, max step-up per poll

    // Frequency limits (MHz)
    min_cap: u32,
    max_cap: u32,
    ps_ceiling: u32,
    bal_ceiling: u32,
    perf_ceiling: u32,

    // Fan
    fan_rpm_floor: u32,
    fan_budget: f64, // rotations per adjust window

    // Throttle
    throttle_budget_ms: u64,

    // Timing (seconds)
    adjust_interval_secs: u64,
    rate_window_secs: u64,
    persist_interval_secs: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            kp: 30.0,
            kd: 60.0,
            max_ramp: 400,
            min_cap: 1200,
            max_cap: 4500,
            ps_ceiling: 3500,
            bal_ceiling: 4500,
            perf_ceiling: 4500,
            fan_rpm_floor: 2500,
            fan_budget: 100.0,
            throttle_budget_ms: 0,
            adjust_interval_secs: 60,
            rate_window_secs: 16,
            persist_interval_secs: 300,
        }
    }
}

impl Config {
    fn load() -> Self {
        match fs::read_to_string(CONFIG_FILE) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                log("config", &format!("Bad config ({e}), using defaults"));
                Self::default()
            }),
            Err(_) => Self::default(),
        }
    }

    fn save_default() {
        if let Some(dir) = std::path::Path::new(CONFIG_FILE).parent() {
            let _ = fs::create_dir_all(dir);
        }
        if !std::path::Path::new(CONFIG_FILE).exists() {
            if let Ok(json) = serde_json::to_string_pretty(&Self::default()) {
                let _ = fs::write(CONFIG_FILE, &json);
                log("config", &format!("Wrote defaults to {CONFIG_FILE}"));
            }
        }
    }

    // --- unit conversions (config is MHz, internals are kHz) ---

    fn kp_khz(&self) -> f64 {
        self.kp * 1000.0
    }
    fn kd_khz(&self) -> f64 {
        self.kd * 1000.0
    }
    fn max_ramp_khz(&self) -> i64 {
        self.max_ramp as i64 * 1000
    }
    fn min_cap_khz(&self) -> u64 {
        self.min_cap as u64 * 1000
    }
    fn max_cap_khz(&self) -> u64 {
        self.max_cap as u64 * 1000
    }
    fn ceiling_khz(&self, profile: Profile) -> u64 {
        let mhz = match profile {
            Profile::PowerSaver => self.ps_ceiling,
            Profile::Balanced => self.bal_ceiling,
            Profile::Performance => self.perf_ceiling,
        };
        mhz as u64 * 1000
    }
    fn adjust_interval(&self) -> Duration {
        Duration::from_secs(self.adjust_interval_secs)
    }
    fn rate_window(&self) -> Duration {
        Duration::from_secs(self.rate_window_secs)
    }
    fn persist_interval(&self) -> Duration {
        Duration::from_secs(self.persist_interval_secs)
    }
}

// =============================================================================
// Profile
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Profile {
    PowerSaver,
    Balanced,
    Performance,
}

impl Profile {
    fn parse(s: &str) -> Option<Self> {
        match s {
            "power-saver" => Some(Self::PowerSaver),
            "balanced" => Some(Self::Balanced),
            "performance" => Some(Self::Performance),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::PowerSaver => "power-saver",
            Self::Balanced => "balanced",
            Self::Performance => "performance",
        }
    }

    fn epp(self) -> &'static str {
        // Always "performance": let the CPU use all cycles up to the freq cap.
        // The PD controller is the sole thermal control knob.
        let _ = self;
        "performance"
    }
}

// =============================================================================
// Persisted state
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    power_saver_target: i32,
    performance_target: i32,
}

impl Default for State {
    fn default() -> Self {
        Self {
            power_saver_target: 50,
            performance_target: 85,
        }
    }
}

impl State {
    fn load() -> Self {
        match fs::read_to_string(STATE_FILE) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                log("state", &format!("Bad state file ({e}), using defaults"));
                Self::default()
            }),
            Err(_) => {
                log("state", "No saved state, using defaults");
                Self::default()
            }
        }
    }

    fn save(&self) {
        if let Some(dir) = std::path::Path::new(STATE_FILE).parent() {
            let _ = fs::create_dir_all(dir);
        }
        match serde_json::to_string_pretty(self) {
            Ok(json) => match fs::write(STATE_FILE, &json) {
                Ok(()) => log(
                    "state",
                    &format!(
                        "Saved ps={}°C perf={}°C",
                        self.power_saver_target, self.performance_target
                    ),
                ),
                Err(e) => log("state", &format!("Save failed: {e}")),
            },
            Err(e) => log("state", &format!("Serialize failed: {e}")),
        }
    }

    fn target(&self, profile: Profile) -> i32 {
        match profile {
            Profile::PowerSaver => self.power_saver_target,
            Profile::Performance => self.performance_target,
            Profile::Balanced => (self.power_saver_target + self.performance_target) / 2,
        }
    }
}

// =============================================================================
// Temperature history (ring buffer + linear regression)
// =============================================================================

struct TempHistory {
    entries: VecDeque<(Instant, i32)>,
    window: Duration,
}

impl TempHistory {
    fn new(window: Duration) -> Self {
        Self {
            entries: VecDeque::new(),
            window,
        }
    }

    fn push(&mut self, temp: i32) {
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

    /// Linear regression slope in °C/s
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
            sum_y += temp as f64;
        }
        let mean_x = sum_x / n;
        let mean_y = sum_y / n;

        let mut num = 0.0;
        let mut den = 0.0;
        for &(t, temp) in &self.entries {
            let dx = t.duration_since(base).as_secs_f64() - mean_x;
            let dy = temp as f64 - mean_y;
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
// Window statistics (constraint tracking over adjust interval)
// =============================================================================

struct WindowStats {
    fan_rotations: f64,
    throttle_start_ms: u64,
    started: Instant,
}

impl WindowStats {
    fn new() -> Self {
        Self {
            fan_rotations: 0.0,
            throttle_start_ms: read_throttle_time_ms(),
            started: Instant::now(),
        }
    }

    fn add_fan_sample(&mut self, rpm: u32, poll_secs: f64) {
        self.fan_rotations += rpm as f64 * poll_secs / 60.0;
    }

    fn throttle_delta_ms(&self) -> u64 {
        read_throttle_time_ms().saturating_sub(self.throttle_start_ms)
    }

    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

// =============================================================================
// Hardware I/O
// =============================================================================

fn read_sysfs_i64(path: &str) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn cpu_temp() -> i32 {
    read_sysfs_i64(TEMP_SENSOR)
        .map(|t| (t / 1000) as i32)
        .unwrap_or(0)
}

fn fan_rpm(floor: u32) -> u32 {
    let f1 = read_sysfs_i64(FAN1_SENSOR).unwrap_or(0) as u32;
    let f2 = read_sysfs_i64(FAN2_SENSOR).unwrap_or(0) as u32;
    let max = f1.max(f2);
    if max >= 60_000 || max < floor { 0 } else { max }
}

fn read_throttle_time_ms() -> u64 {
    read_sysfs_i64(THROTTLE_TIME_PATH).unwrap_or(0) as u64
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

fn set_max_freq(dirs: &[PathBuf], freq: u64) {
    let val = freq.to_string();
    for d in dirs {
        let _ = fs::write(d.join("scaling_max_freq"), &val);
    }
}


fn apply_base(dirs: &[PathBuf], min_freq: u64, epp: &str, boost: u8) {
    let min_val = min_freq.to_string();
    for d in dirs {
        let _ = fs::write(d.join("scaling_min_freq"), &min_val);
        let _ = fs::write(d.join("energy_performance_preference"), epp);
    }
    let _ = fs::write(HWP_BOOST_PATH, boost.to_string());
}

fn detect_profile() -> Option<Profile> {
    let out = Command::new("gdbus")
        .args([
            "call",
            "--system",
            "--dest",
            "net.hadess.PowerProfiles",
            "--object-path",
            "/net/hadess/PowerProfiles",
            "--method",
            "org.freedesktop.DBus.Properties.Get",
            "net.hadess.PowerProfiles",
            "ActiveProfile",
        ])
        .output()
        .ok()?;
    let s = String::from_utf8_lossy(&out.stdout);
    ["power-saver", "balanced", "performance"]
        .iter()
        .find(|name| s.contains(*name))
        .and_then(|name| Profile::parse(name))
}

// =============================================================================
// Logging helpers
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

fn freq_ghz(freq: u64) -> String {
    format!("{:.1}", freq as f64 / 1_000_000.0)
}

// =============================================================================
// Dynamic poll interval
// =============================================================================

fn poll_interval(temp: i32, target: i32, rate: f64) -> Duration {
    // Direct PD doesn't need fast polling — one poll sets the right cap.
    // Only speed up when significantly over target or heating fast.
    let over = (temp - target).max(0) as f64;
    let urgency = over + (rate * 2.0).max(0.0);
    let ms = 2000.0 - urgency * 100.0;
    Duration::from_millis(ms.clamp(500.0, 2000.0) as u64)
}

// =============================================================================
// Target adjustment
// =============================================================================

fn adjust_target(
    profile: Profile,
    window: &WindowStats,
    state: &mut State,
    cfg: &Config,
    target: i32,
    temp: i32,
) {
    let near_target = temp >= target - 3;

    match profile {
        Profile::PowerSaver => {
            let rot = window.fan_rotations;
            if rot > cfg.fan_budget {
                state.power_saver_target -= 1;
                log(
                    "target",
                    &format!(
                        "PS: fan_rot={rot:.0} > budget={} → target={}°C",
                        cfg.fan_budget, state.power_saver_target
                    ),
                );
            } else if rot < 0.01 && near_target {
                state.power_saver_target += 1;
                log(
                    "target",
                    &format!(
                        "PS: no fan near target → target={}°C",
                        state.power_saver_target
                    ),
                );
            } else {
                log(
                    "target",
                    &format!(
                        "PS: fan_rot={rot:.0} budget={} temp={temp}°C → hold {}°C",
                        cfg.fan_budget, state.power_saver_target
                    ),
                );
            }
        }
        Profile::Performance => {
            let thr = window.throttle_delta_ms();
            if thr > cfg.throttle_budget_ms {
                state.performance_target -= 1;
                log(
                    "target",
                    &format!(
                        "Perf: throttle={thr}ms > budget={} → target={}°C",
                        cfg.throttle_budget_ms, state.performance_target
                    ),
                );
            } else if thr == 0 && near_target {
                state.performance_target += 1;
                log(
                    "target",
                    &format!(
                        "Perf: no throttle near target → target={}°C",
                        state.performance_target
                    ),
                );
            } else {
                log(
                    "target",
                    &format!(
                        "Perf: throttle={thr}ms budget={} temp={temp}°C → hold {}°C",
                        cfg.throttle_budget_ms, state.performance_target
                    ),
                );
            }
        }
        Profile::Balanced => {
            let bt = state.target(Profile::Balanced);
            log(
                "target",
                &format!(
                    "Bal: midpoint={}°C (ps={}°C perf={}°C)",
                    bt, state.power_saver_target, state.performance_target
                ),
            );
        }
    }
}

// =============================================================================
// Governor loop (PD controller)
// =============================================================================

fn governor(profile: Profile, state: &mut State, stop: &AtomicBool) {
    let dirs = cpufreq_dirs();
    if dirs.is_empty() {
        log("gov", "No cpufreq dirs found!");
        return;
    }

    let mut cfg = Config::load();

    apply_base(&dirs, 400_000, profile.epp(), 1);

    let ceiling = cfg.ceiling_khz(profile);
    let mut cap = ceiling;
    set_max_freq(&dirs, cap);

    let target = state.target(profile);
    log(
        profile.name(),
        &format!(
            "Governor started: EPP={} ceiling={}GHz target={}°C",
            profile.epp(),
            freq_ghz(ceiling),
            target,
        ),
    );

    let mut temp_history = TempHistory::new(cfg.rate_window());
    let mut window = WindowStats::new();
    let mut last_persist = Instant::now();
    let mut last_poll = Instant::now();

    while !stop.load(Ordering::Relaxed) {
        // Hot-reload config
        let new_cfg = Config::load();
        if new_cfg != cfg {
            log(
                "config",
                &format!(
                    "Reloaded: {}",
                    serde_json::to_string(&new_cfg).unwrap_or_default()
                ),
            );
            temp_history.window = new_cfg.rate_window();
            cfg = new_cfg;
        }

        let poll_secs = last_poll.elapsed().as_secs_f64();
        last_poll = Instant::now();

        let temp = cpu_temp();
        let rpm = fan_rpm(cfg.fan_rpm_floor);
        temp_history.push(temp);
        window.add_fan_sample(rpm, poll_secs);

        let ceiling = cfg.ceiling_khz(profile);
        let target = state.target(profile);
        let rate = temp_history.rate();
        let error = target as f64 - temp as f64;

        // Time-normalized incremental PD:
        // - Accumulates adjustments → finds equilibrium where error=0
        // - poll_secs normalization → consistent behavior at any poll rate
        // - KD subtracted: heating (rate>0) → lower cap, cooling (rate<0) → higher cap
        let adj_per_sec = error * cfg.kp_khz() - rate * cfg.kd_khz();
        let adj = (adj_per_sec * poll_secs) as i64;

        // Ramp-up limited, step-down unlimited
        let max_up = (cfg.max_ramp_khz() as f64 * poll_secs) as i64;
        let adj = if adj > 0 { adj.min(max_up) } else { adj };

        let new_cap =
            (cap as i64 + adj).clamp(cfg.min_cap_khz() as i64, ceiling as i64) as u64;

        // Deadband: ignore cap changes < 100 MHz to filter sensor noise
        let cap_delta = (new_cap as i64 - cap as i64).unsigned_abs();
        if cap_delta >= 100_000 {
            set_max_freq(&dirs, new_cap);
            let arrow = if new_cap < cap { "↓" } else { "↑" };
            log(
                profile.name(),
                &format!(
                    "{temp}°C target={target}°C rate={rate:+.1}°C/s fan:{rpm}rpm {arrow} {}→{}GHz",
                    freq_ghz(cap),
                    freq_ghz(new_cap),
                ),
            );
            cap = new_cap;
        }


        if window.elapsed() >= cfg.adjust_interval() {
            adjust_target(profile, &window, state, &cfg, target, temp);
            window = WindowStats::new();
        }

        if last_persist.elapsed() >= cfg.persist_interval() {
            state.save();
            last_persist = Instant::now();
        }

        let poll = poll_interval(temp, target, rate);
        thread::sleep(poll);
    }

    log(profile.name(), "Governor stopped");
}

// =============================================================================
// D-Bus monitor
// =============================================================================

fn watch_dbus(tx: mpsc::Sender<Profile>) {
    let mut child = match Command::new("dbus-monitor")
        .args([
            "--system",
            "type='signal',interface='org.freedesktop.DBus.Properties',\
             member='PropertiesChanged',\
             path='/net/hadess/PowerProfiles'",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log("dbus", &format!("Failed to spawn dbus-monitor: {e}"));
            return;
        }
    };

    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut check_next = false;

    for line in reader.lines().map_while(Result::ok) {
        if line.contains("ActiveProfile") {
            check_next = true;
            continue;
        }
        if check_next {
            check_next = false;
            for name in ["power-saver", "balanced", "performance"] {
                if line.contains(name) {
                    if let Some(p) = Profile::parse(name) {
                        log("dbus", &format!("Profile changed → {name}"));
                        let _ = tx.send(p);
                    }
                    break;
                }
            }
        }
    }

    let _ = child.wait();
}

// =============================================================================
// Main
// =============================================================================

fn main() {
    let cfg = Config::load();
    Config::save_default();
    let state = State::load();

    eprintln!("================================================");
    eprintln!("  thermal-governor v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("  PD thermal controller for ThinkPad X1");
    eprintln!("================================================");
    eprintln!(
        "  Power Saver  │ EPP=power        │ target={}°C  ceil={}MHz",
        state.power_saver_target, cfg.ps_ceiling
    );
    eprintln!(
        "  Balanced     │ EPP=balance_power│ target={}°C  ceil={}MHz",
        state.target(Profile::Balanced),
        cfg.bal_ceiling
    );
    eprintln!(
        "  Performance  │ EPP=performance  │ target={}°C  ceil={}MHz",
        state.performance_target, cfg.perf_ceiling
    );
    eprintln!("────────────────────────────────────────────────");
    eprintln!(
        "  KP={} KD={} MAX_RAMP={}MHz",
        cfg.kp, cfg.kd, cfg.max_ramp
    );
    eprintln!(
        "  Fan floor={}rpm budget={} rot/win",
        cfg.fan_rpm_floor, cfg.fan_budget
    );
    eprintln!(
        "  Throttle budget={}ms/win",
        cfg.throttle_budget_ms
    );
    eprintln!(
        "  Adjust: {}s  Rate window: {}s  Persist: {}s",
        cfg.adjust_interval_secs, cfg.rate_window_secs, cfg.persist_interval_secs
    );
    eprintln!("  Config: {CONFIG_FILE}  (hot-reload)");
    eprintln!("  State:  {STATE_FILE}");
    eprintln!("================================================\n");

    let mut state = state;

    let initial = detect_profile().unwrap_or_else(|| {
        log("main", "Cannot detect profile, defaulting to balanced");
        Profile::Balanced
    });
    log(
        "main",
        &format!(
            "Initial: {} ({}°C, fan {} rpm)",
            initial.name(),
            cpu_temp(),
            fan_rpm(cfg.fan_rpm_floor),
        ),
    );

    // D-Bus profile change channel
    let (tx, rx) = mpsc::channel::<Profile>();
    thread::spawn(move || watch_dbus(tx));

    // SIGTERM handling
    let running = Arc::new(AtomicBool::new(true));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&running))
        .expect("Failed to register SIGTERM handler");
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&running))
        .expect("Failed to register SIGINT handler");

    let dirs = cpufreq_dirs();
    let mut current = initial;
    let stop = Arc::new(AtomicBool::new(false));

    loop {
        stop.store(false, Ordering::Relaxed);
        let stop_c = Arc::clone(&stop);
        let mut state_c = state.clone();
        let profile = current;

        let handle = thread::spawn(move || {
            governor(profile, &mut state_c, &stop_c);
            state_c
        });

        // Wait for profile switch or shutdown
        let new_profile = loop {
            match rx.recv_timeout(Duration::from_secs(1)) {
                Ok(p) if p != current => break Some(p),
                Ok(_) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !running.load(Ordering::Relaxed) {
                        break None;
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            }
        };

        stop.store(true, Ordering::Relaxed);
        if let Ok(s) = handle.join() {
            state = s;
        }

        match new_profile {
            Some(p) => {
                current = p;
            }
            None => {
                let cfg = Config::load();
                log("main", "Shutting down");
                state.save();
                set_max_freq(&dirs, cfg.max_cap_khz());
                apply_base(&dirs, 400_000, "balance_power", 0);
                log("main", "Reset to defaults. Goodbye.");
                return;
            }
        }
    }
}
