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

const POLL_INTERVAL: Duration = Duration::from_secs(2);
const TUNE_INTERVAL: Duration = Duration::from_secs(120);
const PERSIST_INTERVAL: Duration = Duration::from_secs(300);

const FREQ_STEP: u64 = 100_000; // 100 MHz
const MIN_CAP: u64 = 1_200_000; // 1.2 GHz absolute floor
const MAX_CAP: u64 = 4_500_000; // 4.5 GHz absolute ceiling
const MIN_SPREAD: u64 = 200_000; // 200 MHz minimum gap between adjacent levels

const STATE_FILE: &str = "/var/lib/thermal-governor/tuned-params.json";

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
        match self {
            Self::PowerSaver => "power",
            Self::Balanced => "balance_power",
            Self::Performance => "performance",
        }
    }

    fn ceiling(self) -> u64 {
        match self {
            Self::PowerSaver => 3_500_000,   // 3.5 GHz — no point going higher for fanless
            Self::Balanced => 4_500_000,
            Self::Performance => 4_500_000,
        }
    }
}

// =============================================================================
// Thermal table: 4 levels per profile
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ThermalTable {
    /// Cap when below all thresholds (full power for this profile)
    max_cap: u64,
    /// Temp thresholds °C ascending: when temp > thresholds[i], apply caps[i]
    thresholds: [i32; 4],
    /// Freq caps kHz descending: caps[i] = cap when temp > thresholds[i]
    caps: [u64; 4],
    /// Hysteresis °C for stepping back up
    hysteresis: i32,
}

impl ThermalTable {
    fn power_saver() -> Self {
        Self {
            max_cap: 3_000_000,        // < 50°C → 3.0 GHz
            thresholds: [50, 55, 58, 62],
            caps: [2_500_000, 2_000_000, 1_500_000, 1_200_000],
            hysteresis: 2,
        }
    }

    fn balanced() -> Self {
        Self {
            max_cap: 4_000_000,        // < 65°C → 4.0 GHz
            thresholds: [65, 72, 78, 83],
            caps: [3_500_000, 3_000_000, 2_500_000, 2_000_000],
            hysteresis: 5,
        }
    }

    fn performance() -> Self {
        Self {
            max_cap: 4_500_000,        // < 75°C → 4.5 GHz
            thresholds: [75, 85, 92, 95],
            caps: [3_800_000, 3_200_000, 2_800_000, 2_200_000],
            hysteresis: 5,
        }
    }

    /// All cap levels ordered from highest to lowest: [max_cap, caps[0], caps[1], caps[2], caps[3]]
    fn all_levels(&self) -> [u64; 5] {
        [self.max_cap, self.caps[0], self.caps[1], self.caps[2], self.caps[3]]
    }

    /// Find which level index (0-4) the current cap is at or closest below.
    fn current_level(&self, current_cap: u64) -> usize {
        let levels = self.all_levels();
        for (i, &cap) in levels.iter().enumerate() {
            if current_cap >= cap {
                return i;
            }
        }
        4 // at or below lowest
    }

    /// Compute target cap given current temp and current cap.
    /// Step-down is immediate (jump to correct level).
    /// Step-up is gradual (one level at a time, with hysteresis).
    fn target_cap(&self, temp: i32, current_cap: u64) -> u64 {
        let levels = self.all_levels();
        // thresholds[i] gates transition from levels[i] down to levels[i+1]
        // i.e., if temp > thresholds[i], you should be at levels[i+1] or lower

        // Step DOWN: find the correct level for this temperature (immediate)
        let mut target_level: usize = 0; // default: max_cap
        for i in 0..4 {
            if temp > self.thresholds[i] {
                target_level = i + 1;
            }
        }
        let down_cap = levels[target_level];

        // If we need to step down, do it immediately
        if down_cap < current_cap {
            return down_cap;
        }

        // Step UP: go only ONE level up, with hysteresis
        let cur_level = self.current_level(current_cap);
        if cur_level > 0 {
            // To step up from cur_level to cur_level-1, temp must be below
            // the threshold that pushed us into cur_level, minus hysteresis
            let thresh_idx = cur_level - 1; // thresholds[thresh_idx] caused us to drop to cur_level
            let up_thresh = self.thresholds[thresh_idx] - self.hysteresis;
            if temp < up_thresh {
                return levels[cur_level - 1]; // go up ONE level only
            }
        }

        current_cap
    }

    fn caps_str(&self) -> String {
        format!(
            "{}/{}/{}/{}/{}",
            freq_ghz(self.max_cap),
            freq_ghz(self.caps[0]),
            freq_ghz(self.caps[1]),
            freq_ghz(self.caps[2]),
            freq_ghz(self.caps[3]),
        )
    }

    fn thresholds_str(&self) -> String {
        format!(
            "{}/{}/{}/{}°C",
            self.thresholds[0], self.thresholds[1], self.thresholds[2], self.thresholds[3],
        )
    }

    fn lowest_cap(&self) -> u64 {
        self.caps[3]
    }

    fn enforce_invariants(&mut self, ceiling: u64) {
        // Clamp max_cap to profile ceiling
        self.max_cap = self.max_cap.clamp(MIN_CAP, ceiling);

        // Enforce monotonically decreasing with minimum spread:
        // max_cap > caps[0] > caps[1] > caps[2] > caps[3]
        let mut prev = self.max_cap;
        for c in &mut self.caps {
            let upper = if prev > MIN_CAP + MIN_SPREAD {
                prev - MIN_SPREAD
            } else {
                MIN_CAP
            };
            if *c > upper {
                *c = upper;
            }
            if *c < MIN_CAP {
                *c = MIN_CAP;
            }
            prev = *c;
        }
    }
}

// =============================================================================
// Persisted state
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct State {
    power_saver: ThermalTable,
    balanced: ThermalTable,
    performance: ThermalTable,
}

impl Default for State {
    fn default() -> Self {
        Self {
            power_saver: ThermalTable::power_saver(),
            balanced: ThermalTable::balanced(),
            performance: ThermalTable::performance(),
        }
    }
}

impl State {
    fn load() -> Self {
        match fs::read_to_string(STATE_FILE) {
            Ok(data) => serde_json::from_str(&data).unwrap_or_else(|e| {
                log("tuner", &format!("Bad state file ({e}), using defaults"));
                Self::default()
            }),
            Err(_) => {
                log("tuner", "No saved state, using defaults");
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
                Ok(()) => log("tuner", "State saved"),
                Err(e) => log("tuner", &format!("Save failed: {e}")),
            },
            Err(e) => log("tuner", &format!("Serialize failed: {e}")),
        }
    }

    fn table(&self, p: Profile) -> &ThermalTable {
        match p {
            Profile::PowerSaver => &self.power_saver,
            Profile::Balanced => &self.balanced,
            Profile::Performance => &self.performance,
        }
    }

    fn table_mut(&mut self, p: Profile) -> &mut ThermalTable {
        match p {
            Profile::PowerSaver => &mut self.power_saver,
            Profile::Balanced => &mut self.balanced,
            Profile::Performance => &mut self.performance,
        }
    }
}

// =============================================================================
// Tune statistics (rolling window)
// =============================================================================

#[derive(Default)]
struct TuneStats {
    samples: u32,
    fan_active: u32,
    max_temp: i32,
    temp_sum: i64,
    at_lowest: u32,
}

impl TuneStats {
    fn record(&mut self, temp: i32, fan_rpm: u32, current_cap: u64, lowest_cap: u64) {
        self.samples += 1;
        self.temp_sum += temp as i64;
        if temp > self.max_temp {
            self.max_temp = temp;
        }
        if fan_rpm > 100 {
            self.fan_active += 1;
        }
        if current_cap == lowest_cap {
            self.at_lowest += 1;
        }
    }

    fn avg_temp(&self) -> i32 {
        if self.samples == 0 { 0 } else { (self.temp_sum / self.samples as i64) as i32 }
    }

    fn fan_pct(&self) -> u32 {
        if self.samples == 0 { 0 } else { self.fan_active * 100 / self.samples }
    }

    fn lowest_pct(&self) -> u32 {
        if self.samples == 0 { 0 } else { self.at_lowest * 100 / self.samples }
    }
}

// =============================================================================
// Hardware I/O
// =============================================================================

fn read_sysfs_i64(path: &str) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn cpu_temp() -> i32 {
    read_sysfs_i64(TEMP_SENSOR).map(|t| (t / 1000) as i32).unwrap_or(0)
}

fn fan_rpm() -> u32 {
    let f1 = read_sysfs_i64(FAN1_SENSOR).unwrap_or(0) as u32;
    let f2 = read_sysfs_i64(FAN2_SENSOR).unwrap_or(0) as u32;
    f1.max(f2)
}

fn cpufreq_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu/") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if s.starts_with("cpu")
                && s.len() > 3
                && s.as_bytes()[3].is_ascii_digit()
            {
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
            "call", "--system",
            "--dest", "net.hadess.PowerProfiles",
            "--object-path", "/net/hadess/PowerProfiles",
            "--method", "org.freedesktop.DBus.Properties.Get",
            "net.hadess.PowerProfiles", "ActiveProfile",
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

fn clamp_freq(freq: u64) -> u64 {
    freq.clamp(MIN_CAP, MAX_CAP)
}

// =============================================================================
// Auto-tuning
// =============================================================================

fn auto_tune(profile: Profile, stats: &TuneStats, state: &mut State) {
    if stats.samples < 10 {
        return;
    }

    let max = stats.max_temp;
    let fan_pct = stats.fan_pct();
    let lowest_pct = stats.lowest_pct();
    let avg = stats.avg_temp();
    let t = state.table_mut(profile);

    match profile {
        Profile::PowerSaver => {
            if fan_pct == 0 && max < t.thresholds[2] && avg >= 48 {
                // Fans OFF under actual load → raise max_cap only (not step-down caps)
                t.max_cap = clamp_freq(t.max_cap + FREQ_STEP);
                log("tuner", &format!("[ps] Fans OFF under load avg={avg}°C → max_cap +100MHz"));
            } else if fan_pct > 20 {
                // Fans active too much → lower all caps
                t.max_cap = clamp_freq(t.max_cap.saturating_sub(FREQ_STEP));
                for c in &mut t.caps { *c = clamp_freq(c.saturating_sub(FREQ_STEP)); }
                log("tuner", &format!("[ps] Fans {fan_pct}% → all caps -100MHz"));
            } else if fan_pct > 0 {
                // Occasional fan → tighten threshold
                t.thresholds[0] = (t.thresholds[0] - 1).clamp(40, 55);
                log("tuner", &format!("[ps] Fan blips ({fan_pct}%) → thresh[0]={}", t.thresholds[0]));
            }
        }
        Profile::Balanced => {
            if max < (t.thresholds[2] - 5) && lowest_pct == 0 {
                t.max_cap = clamp_freq(t.max_cap + FREQ_STEP);
                t.caps[0] = clamp_freq(t.caps[0] + FREQ_STEP);
                log("tuner", &format!("[bal] Headroom max={max}°C → top caps +100MHz"));
            } else if max > t.thresholds[3] {
                t.max_cap = clamp_freq(t.max_cap.saturating_sub(FREQ_STEP));
                t.caps[0] = clamp_freq(t.caps[0].saturating_sub(FREQ_STEP));
                log("tuner", &format!("[bal] Hot max={max}°C → top caps -100MHz"));
            }
        }
        Profile::Performance => {
            if max < (t.thresholds[2] - 3) && lowest_pct == 0 {
                t.max_cap = clamp_freq(t.max_cap + FREQ_STEP);
                t.caps[0] = clamp_freq(t.caps[0] + FREQ_STEP);
                log("tuner", &format!("[perf] Headroom max={max}°C → top caps +100MHz"));
            } else if max > 95 {
                t.max_cap = clamp_freq(t.max_cap.saturating_sub(FREQ_STEP * 2));
                t.caps[0] = clamp_freq(t.caps[0].saturating_sub(FREQ_STEP * 2));
                t.caps[1] = clamp_freq(t.caps[1].saturating_sub(FREQ_STEP));
                log("tuner", &format!("[perf] DANGER max={max}°C → aggressive cap reduction"));
            } else if max > t.thresholds[2] {
                t.max_cap = clamp_freq(t.max_cap.saturating_sub(FREQ_STEP));
                t.caps[0] = clamp_freq(t.caps[0].saturating_sub(FREQ_STEP));
                log("tuner", &format!("[perf] Warm max={max}°C → top caps -100MHz"));
            }
        }
    }

    // Enforce invariants after any adjustment
    state.table_mut(profile).enforce_invariants(profile.ceiling());

    let t = state.table(profile);
    log("tuner", &format!(
        "[{}] samples={} avg={avg}°C max={max}°C fan={fan_pct}% lowest={lowest_pct}% caps={} thresh={}",
        profile.name(), stats.samples, t.caps_str(), t.thresholds_str(),
    ));
}

// =============================================================================
// Governor loop (runs per profile until stopped)
// =============================================================================

fn governor(profile: Profile, state: &mut State, stop: &AtomicBool) {
    let dirs = cpufreq_dirs();
    if dirs.is_empty() {
        log("gov", "No cpufreq dirs found!");
        return;
    }

    apply_base(&dirs, 400_000, profile.epp(), 1);

    let mut current_cap = state.table(profile).max_cap;
    set_max_freq(&dirs, current_cap);

    let t = state.table(profile);
    log(profile.name(), &format!(
        "Governor started: EPP={} cap={}GHz thresh={} hyst={}°C",
        profile.epp(), freq_ghz(current_cap), t.thresholds_str(), t.hysteresis,
    ));

    let mut stats = TuneStats::default();
    let mut last_tune = Instant::now();
    let mut last_persist = Instant::now();
    let mut cooldown: u32 = 0; // polls to wait before allowing step-up

    while !stop.load(Ordering::Relaxed) {
        let temp = cpu_temp();
        let rpm = fan_rpm();

        let table = state.table(profile);
        let raw_target = table.target_cap(temp, current_cap);
        let lowest = table.lowest_cap();

        // Apply cooldown: suppress step-ups for a few polls after a step-down
        let new_cap = if raw_target > current_cap && cooldown > 0 {
            cooldown -= 1;
            current_cap // hold current cap during cooldown
        } else {
            raw_target
        };

        stats.record(temp, rpm, current_cap, lowest);

        if new_cap != current_cap {
            set_max_freq(&dirs, new_cap);
            let arrow = if new_cap < current_cap { "↓" } else { "↑" };
            log(profile.name(), &format!(
                "{temp}°C fan:{rpm}rpm {arrow} {}→{} GHz",
                freq_ghz(current_cap), freq_ghz(new_cap),
            ));
            if new_cap < current_cap {
                cooldown = 3; // after step-down, wait 3 polls (6s) before stepping up
            }
            current_cap = new_cap;
        }

        if last_tune.elapsed() >= TUNE_INTERVAL {
            auto_tune(profile, &stats, state);
            stats = TuneStats::default();
            last_tune = Instant::now();
        }

        if last_persist.elapsed() >= PERSIST_INTERVAL {
            state.save();
            last_persist = Instant::now();
        }

        thread::sleep(POLL_INTERVAL);
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
    eprintln!("================================================");
    eprintln!("  thermal-governor v{}", env!("CARGO_PKG_VERSION"));
    eprintln!("  Auto-tuning thermal manager for ThinkPad X1");
    eprintln!("================================================");
    eprintln!("  Power Saver  │ EPP=power          │ fanless (<58°C)");
    eprintln!("  Balanced     │ EPP=balance_power   │ moderate (<80°C)");
    eprintln!("  Performance  │ EPP=performance     │ max sustained (<95°C)");
    eprintln!("────────────────────────────────────────────────");
    eprintln!("  Tune: every {}s  Persist: every {}s",
        TUNE_INTERVAL.as_secs(), PERSIST_INTERVAL.as_secs());
    eprintln!("  State: {STATE_FILE}");
    eprintln!("================================================\n");

    let mut state = State::load();

    let initial = detect_profile().unwrap_or_else(|| {
        log("main", "Cannot detect profile, defaulting to balanced");
        Profile::Balanced
    });
    log("main", &format!(
        "Initial: {} ({}°C, fan {} rpm)", initial.name(), cpu_temp(), fan_rpm(),
    ));

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
                Ok(_) => {} // same profile, ignore
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if !running.load(Ordering::Relaxed) {
                        break None; // shutdown
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            }
        };

        // Stop governor
        stop.store(true, Ordering::Relaxed);
        if let Ok(s) = handle.join() {
            state = s;
        }

        match new_profile {
            Some(p) => {
                current = p;
                // loop continues → restarts governor with new profile
            }
            None => {
                // Shutdown: save and reset
                log("main", "Shutting down");
                state.save();
                set_max_freq(&dirs, MAX_CAP);
                apply_base(&dirs, 400_000, "balance_power", 0);
                log("main", "Reset to defaults. Goodbye.");
                return;
            }
        }
    }
}
