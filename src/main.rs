use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::fmt;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};
use thermal_governor::clock::{self, LocalTime};
use thermal_governor::fan_curve::{self, Curve, FanMode, Status};
use thermal_governor::hw::{self, FanSensor};

// =============================================================================
// Constants
// =============================================================================

const DATA_DIR: &str = "/var/lib/thermal-governor";
const EVENTS_DIR: &str = "/var/lib/thermal-governor/events";
const SETTINGS_FILE: &str = "/var/lib/thermal-governor/settings.json";

const BUFFER_CAPACITY: usize = 300; // 5 min at 1Hz
const POLL_MS: u64 = 1000;

// Event cooldowns (seconds)
const EVENT_COOLDOWN_SECS: u64 = 60;
const RAPID_TEMP_SUSTAINED: usize = 3;
const PROFILE_SETTLE_SECS: u64 = 5;

// Retention: keep only the newest N event CSVs (~30 KB each)
const EVENTS_MAX_FILES: usize = 1000;

// The EC takes the fan back if the curve stops sending levels for this long
const FAN_WATCHDOG_SECS: u32 = 10;

// =============================================================================
// Persisted settings (what hw-tui last set)
// =============================================================================

// The fan level is deliberately not part of it: a manual level restored at
// boot, with nobody watching, could leave the fan off under load. The fan
// curve drives the fan from boot; manual levels (hw-tui) last one boot.
// Old files with a `fan_level` field still load: serde ignores it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Settings {
    freq_cap_mhz: u32,
    epp: String,
    /// None on machines without an ACPI platform profile, and in files saved
    /// before it was persisted
    #[serde(default, skip_serializing_if = "Option::is_none")]
    platform_profile: Option<String>,
}

impl Settings {
    /// None when nothing was saved yet or the file is unreadable
    fn load() -> Option<Self> {
        let data = fs::read_to_string(SETTINGS_FILE).ok()?;
        match serde_json::from_str(&data) {
            Ok(settings) => Some(settings),
            Err(e) => {
                log("settings", &format!("Bad settings file ({e}), ignoring it"));
                None
            }
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
        // Profile first: it sets PL1, the power limit the cap has to live with
        if let Some(profile) = &self.platform_profile {
            hw::set_platform_profile(profile);
        }
        hw::set_freq_cap(dirs, self.freq_cap_mhz);
        hw::set_epp(dirs, &self.epp);
    }

    /// Actual hardware state; None if any read fails
    fn from_hw(dirs: &[PathBuf]) -> Option<Self> {
        Self::from_reads(
            hw::read_freq_cap(dirs),
            hw::read_epp(dirs),
            hw::read_platform_profile(),
        )
    }

    fn from_reads(cap: Option<u32>, epp: Option<String>, profile: Option<String>) -> Option<Self> {
        // A profile file that exists but cannot be read is a failed read, not a
        // machine without profiles: saving None would forget the user's choice
        if profile.is_none() && hw::has_platform_profile() {
            return None;
        }
        Some(Self {
            freq_cap_mhz: cap?,
            epp: epp?,
            platform_profile: profile,
        })
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
    fan1_rpm: u32,
    fan2_rpm: u32,
    fan_level: String,
    freq_min: u32,
    freq_avg: u32,
    freq_max: u32,
    freq_cap_mhz: u32,
    epp: String,
    throttle_rate: f64,
    rapl_power_w: f64,
    platform_profile: String,
    pl1_w: Option<f64>,
    fan_mode: &'static str,
    fan_need: f64,
}

impl Sample {
    fn csv_header() -> &'static str {
        "timestamp,temp_c,temp_rate,cpu_load,fan1_rpm,fan2_rpm,fan_level,freq_min,freq_avg,freq_max,freq_cap_mhz,epp,throttle_rate,rapl_power_w,platform_profile,pl1_w,fan_mode,fan_need"
    }

    fn to_csv_row(&self) -> String {
        let ts = self
            .timestamp
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format!(
            "{},{:.1},{:.2},{:.3},{},{},{},{},{},{},{},{},{:.1},{:.2},{},{},{},{:.2}",
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
            self.platform_profile,
            self.pl1_w.map_or(String::new(), |w| format!("{w:.0}")),
            self.fan_mode,
            self.fan_need,
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
    Pl1Cut,
}

impl fmt::Display for EventType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Throttle => write!(f, "throttle"),
            Self::TempCross85 => write!(f, "temp-cross-85"),
            Self::TempCross90 => write!(f, "temp-cross-90"),
            Self::TempCross95 => write!(f, "temp-cross-95"),
            Self::RapidTempRise => write!(f, "rapid-temp-rise"),
            Self::Pl1Cut => write!(f, "pl1-cut"),
        }
    }
}

// =============================================================================
// Event detector state
// =============================================================================

struct EventDetector {
    // None = never fired (avoids Instant underflow when the daemon starts early at boot)
    cooldowns: [(EventType, Option<Instant>); 6],
    // Temp crossing hysteresis: track if we're "above" each threshold
    above_85: bool,
    above_90: bool,
    above_95: bool,
    // Rapid temp rise: count consecutive samples with rate > 2
    rapid_rise_count: usize,
    // PL1 cut: a drop of PL1 with no recent profile change (the EC lowering
    // it). The firmware may apply a new profile's PL1 a few ticks late.
    prev_pl1: Option<f64>,
    profile: Option<(String, Instant)>,
}

impl EventDetector {
    fn new() -> Self {
        Self {
            cooldowns: [
                (EventType::Throttle, None),
                (EventType::TempCross85, None),
                (EventType::TempCross90, None),
                (EventType::TempCross95, None),
                (EventType::RapidTempRise, None),
                (EventType::Pl1Cut, None),
            ],
            above_85: false,
            above_90: false,
            above_95: false,
            rapid_rise_count: 0,
            prev_pl1: None,
            profile: None,
        }
    }

    fn can_fire(&self, event: EventType) -> bool {
        for &(et, last) in &self.cooldowns {
            if et == event {
                return last.is_none_or(|t| t.elapsed().as_secs() >= EVENT_COOLDOWN_SECS);
            }
        }
        false
    }

    fn mark_fired(&mut self, event: EventType) {
        for (et, last) in &mut self.cooldowns {
            if *et == event {
                *last = Some(Instant::now());
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

        // PL1 cut by the EC: the CPU will be clamped once its power budget is spent
        if self.profile.as_ref().map(|(p, _)| p) != Some(&sample.platform_profile) {
            self.profile = Some((sample.platform_profile.clone(), Instant::now()));
        }
        let profile_settled = self
            .profile
            .as_ref()
            .is_some_and(|(_, since)| since.elapsed() >= Duration::from_secs(PROFILE_SETTLE_SECS));
        if let Some(pl1) = sample.pl1_w {
            if let Some(prev) = self.prev_pl1 {
                if pl1 < prev - 0.5 && profile_settled && self.can_fire(EventType::Pl1Cut) {
                    events.push(EventType::Pl1Cut);
                }
            }
            self.prev_pl1 = Some(pl1);
        }

        // Mark all fired events
        for &e in &events {
            self.mark_fired(e);
        }

        events
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
        self.push_at(Instant::now(), temp);
    }

    fn push_at(&mut self, now: Instant, temp: f64) {
        self.entries.push_back((now, temp));
        // checked_sub: Instant underflows (panics) if uptime < window
        if let Some(cutoff) = now.checked_sub(self.window) {
            while let Some(&(t, _)) = self.entries.front() {
                if t < cutoff {
                    self.entries.pop_front();
                } else {
                    break;
                }
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

/// Keep only the newest EVENTS_MAX_FILES event CSVs (names sort chronologically).
fn prune_events() {
    let Ok(entries) = fs::read_dir(EVENTS_DIR) else {
        return;
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "csv"))
        .collect();
    if files.len() <= EVENTS_MAX_FILES {
        return;
    }
    files.sort();
    let excess = files.len() - EVENTS_MAX_FILES;
    let mut removed = 0;
    for p in files.iter().take(excess) {
        if fs::remove_file(p).is_ok() {
            removed += 1;
        }
    }
    log(
        "events",
        &format!("Pruned {removed} old event files (keeping newest {EVENTS_MAX_FILES})"),
    );
}

// =============================================================================
// Fan driver: applies the mode from hw-tui (curve by default)
// =============================================================================

/// What the daemon currently does with the fan
#[derive(Debug, Clone, Copy, PartialEq)]
enum FanControl {
    /// Sends a level every tick, with the EC watchdog on
    Curve,
    /// Gave the fan back to the EC
    Ec,
    /// Leaves the level to hw-tui, which sends it every second. The watchdog
    /// stays on: if hw-tui dies, the EC takes the fan back.
    HandsOff,
}

struct FanDriver {
    curve: Curve,
    control: Option<FanControl>,
    last_level: Option<&'static str>,
}

impl FanDriver {
    fn new() -> Self {
        Self {
            curve: Curve::new(),
            control: None,
            last_level: None,
        }
    }

    /// One tick. `temp_c` is a fresh read (None when it failed), not the
    /// observer's reused last value.
    fn tick(
        &mut self,
        dt: f64,
        temp_c: Option<f64>,
        power_w: Option<f64>,
        sen_max_c: Option<f64>,
    ) -> Status {
        let mut mode = FanMode::read();
        // The curve runs in every mode, so its state is ready when the mode
        // comes back to curve
        let guard = fan_curve::guard(temp_c, sen_max_c);
        if guard.is_some() {
            self.curve.force_full();
        }
        let decision = temp_c.map(|t| self.curve.update(dt, t, power_w));

        let level = if !hw::fan_control_enabled() {
            // Fan commands would fail: the EC keeps the fan anyway
            self.take(FanControl::Ec);
            None
        } else {
            match (mode, &guard) {
                // The EC protects itself when it has the fan
                (FanMode::Auto, _) => {
                    self.take(FanControl::Ec);
                    None
                }
                (FanMode::Manual, None) => {
                    self.take(FanControl::HandsOff);
                    None
                }
                // A manual level must not outlive a hot machine
                (FanMode::Manual, Some(why)) => {
                    log("fan", &format!("Hard limit ({why}) in manual mode, back to the curve"));
                    mode = FanMode::Curve;
                    let _ = mode.write();
                    Some(self.drive(fan_curve::FULL_SPEED))
                }
                (FanMode::Curve, Some(_)) => Some(self.drive(fan_curve::FULL_SPEED)),
                (FanMode::Curve, None) => match &decision {
                    Some(d) => Some(self.drive(d.level)),
                    // No temperature: the EC reads its own sensors
                    None => {
                        self.take(FanControl::Ec);
                        None
                    }
                },
            }
        };

        if level.is_some() && level != self.last_level {
            log(
                "fan",
                &format!(
                    "Level {} (need {:.2}: power {:.2}, temp {:.2}{})",
                    level.unwrap_or("-"),
                    decision.as_ref().map_or(0.0, |d| d.need),
                    decision.as_ref().map_or(0.0, |d| d.need_power),
                    decision.as_ref().map_or(0.0, |d| d.need_temp),
                    guard.as_ref().map_or(String::new(), |g| format!(", hard limit: {g}")),
                ),
            );
        }
        self.last_level = level;

        Status {
            updated: clock::unix_now(),
            mode,
            level: level.map(String::from),
            need: decision.as_ref().map_or(0.0, |d| d.need),
            need_power: decision.as_ref().map_or(0.0, |d| d.need_power),
            need_temp: decision.as_ref().map_or(0.0, |d| d.need_temp),
            guard,
        }
    }

    fn take(&mut self, control: FanControl) {
        if self.control == Some(control) {
            return;
        }
        match control {
            // Curve: `drive` arms the watchdog on every tick
            FanControl::Curve => {}
            FanControl::Ec => {
                hw::set_fan_watchdog(0);
                hw::set_fan_level("auto");
            }
            FanControl::HandsOff => hw::set_fan_watchdog(FAN_WATCHDOG_SECS),
        }
        log("fan", &format!("Fan control: {:?}", control));
        self.control = Some(control);
    }

    fn drive(&mut self, level: &'static str) -> &'static str {
        self.take(FanControl::Curve);
        // Every tick, not only on change: a failed write or a module reload
        // (which resets the watchdog) must not leave the curve without it.
        // The level command also restarts the watchdog timer.
        hw::set_fan_watchdog(FAN_WATCHDOG_SECS);
        hw::set_fan_level(level);
        level
    }
}

/// Numbers for the per-minute status line: is the curve quiet, does it hold?
#[derive(Default)]
struct MinuteStats {
    samples: u32,
    rpm_sum: u64,
    fan_off: u32,
    /// Seconds with max frequency under the cap while busy (see `record`)
    drop_s: u32,
    pl1_min_w: Option<f64>,
}

impl MinuteStats {
    fn record(&mut self, s: &Sample) {
        self.samples += 1;
        let rpm = s.fan1_rpm.max(s.fan2_rpm);
        self.rpm_sum += rpm as u64;
        if rpm == 0 {
            self.fan_off += 1;
        }
        // Only meaningful at caps every core can reach (LP-E cores top out at
        // 2500), and with enough load that some core runs at the cap
        if s.freq_cap_mhz <= 2500 && s.cpu_load >= 0.25 && s.freq_max + 100 < s.freq_cap_mhz {
            self.drop_s += 1;
        }
        if let Some(w) = s.pl1_w {
            self.pl1_min_w = Some(self.pl1_min_w.map_or(w, |m| m.min(w)));
        }
    }

    fn summary(&self) -> String {
        let n = self.samples.max(1);
        format!(
            "rpm_avg={} fan_off={}% drop_s={} pl1_min={}",
            self.rpm_sum / n as u64,
            self.fan_off * 100 / n,
            self.drop_s,
            self.pl1_min_w.map_or("?".into(), |w| format!("{w:.0}W")),
        )
    }
}

// =============================================================================
// Logging
// =============================================================================

fn log(tag: &str, msg: &str) {
    eprintln!("[{}] [{tag}] {msg}", LocalTime::now().time());
}

// =============================================================================
// Observer loop
// =============================================================================

fn observer(stop: &AtomicBool) {
    let dirs = hw::cpufreq_dirs();
    if dirs.is_empty() {
        log("obs", "No cpufreq dirs found!");
        return;
    }
    // Before discovering the fan sensor: a module reload renumbers its hwmon node
    if !hw::fan_control_enabled() {
        log("fan", "fan_control is off, reloading thinkpad_acpi with fan_control=1");
        if !hw::enable_fan_control() {
            log("fan", "WARNING: cannot enable fan control, the EC keeps the fan");
        }
    }
    let mut fans = FanSensor::discover();
    if !fans.is_found() {
        log("obs", "WARNING: thinkpad hwmon not found, fan RPMs will be 0");
    }
    let sen_sensors = hw::find_sen_sensors();

    // Startup: restore the saved profile, cap and EPP. The fan driver takes
    // the fan on the first tick.
    let saved = Settings::load();
    match &saved {
        Some(settings) => {
            log("obs", &format!("Restoring settings: {:?}", settings));
            settings.apply(&dirs);
        }
        None => log("obs", "No saved settings, leaving profile, cap and EPP as they are"),
    }

    // Baseline change-detection on the actual hardware state, not the intended
    // settings: if a restore write failed, trusting the intent would make the
    // next tick see a "change" and overwrite the saved settings with the
    // failed state.
    let mut last_settings = match (Settings::from_hw(&dirs), saved) {
        // A file saved before the profile was persisted: keep its None in the
        // baseline, so the first tick sees the profile as a change and saves it
        (Some(hw), Some(saved)) if saved.platform_profile.is_none() => Some(Settings {
            platform_profile: None,
            ..hw
        }),
        (hw, saved) => hw.or(saved),
    };

    prune_events();

    let temp_sensor = hw::find_temp_sensor().unwrap_or_else(|| {
        log(
            "obs",
            &format!(
                "WARNING: no package thermal zone, falling back to {}",
                hw::TEMP_SENSOR_FALLBACK
            ),
        );
        PathBuf::from(hw::TEMP_SENSOR_FALLBACK)
    });
    let mut last_temp = hw::cpu_temp(&temp_sensor).unwrap_or(0.0);
    let mut temp_read_failed = false;

    let mut temp_history = TempHistory::new(16);
    let mut cpu_usage = hw::CpuUsage::new();
    let mut throttle = hw::ThrottleMeter::new();
    let mut rapl = hw::RaplMeter::new();
    let mut buffer: VecDeque<Sample> = VecDeque::with_capacity(BUFFER_CAPACITY);
    let mut detector = EventDetector::new();
    let mut fan_driver = FanDriver::new();
    let mut minute = MinuteStats::default();
    let mut prev_tick = Instant::now();
    let mut tick: u64 = 0;

    log("obs", "Observer loop started (1Hz sampling, 300-sample buffer)");

    while !stop.load(Ordering::Relaxed) {
        // --- Sample all sensors ---
        // On read failure reuse the last temp: a 0.0 sample would poison
        // temp_rate and reset the threshold hysteresis
        let temp_fresh = hw::cpu_temp(&temp_sensor);
        let temp = match temp_fresh {
            Some(t) => {
                temp_read_failed = false;
                last_temp = t;
                t
            }
            None => {
                if !temp_read_failed {
                    log("obs", "WARNING: temp read failed, reusing last value");
                    temp_read_failed = true;
                }
                last_temp
            }
        };
        temp_history.push(temp);
        let temp_rate = temp_history.rate();

        let load = cpu_usage.sample();
        let (f1, f2) = fans.rpms();
        let fan_level = hw::read_fan_level().unwrap_or_else(|| "unknown".into());
        let freqs = hw::read_freqs(&dirs);
        let freq_cap_r = hw::read_freq_cap(&dirs);
        let epp_r = hw::read_epp(&dirs);
        let profile_r = hw::read_platform_profile();
        let freq_cap = freq_cap_r.unwrap_or(0);
        let epp = epp_r.clone().unwrap_or_else(|| "unknown".into());
        let profile = profile_r.clone().unwrap_or_else(|| "unknown".into());
        let pl1_w = hw::read_pl1_w();
        let throttle_rate = throttle.sample().unwrap_or(0.0);
        let rapl_r = rapl.sample();
        let rapl_w = rapl_r.unwrap_or(0.0);

        // --- Fan ---
        let now = Instant::now();
        let dt = now.duration_since(prev_tick).as_secs_f64();
        prev_tick = now;
        let fan_status = fan_driver.tick(dt, temp_fresh, rapl_r, hw::max_sen_temp(&sen_sensors));
        if let Err(e) = fan_status.write() {
            if tick == 0 {
                log("fan", &format!("WARNING: cannot write the status for hw-tui: {e}"));
            }
        }

        let sample = Sample {
            timestamp: SystemTime::now(),
            temp_c: temp,
            temp_rate,
            cpu_load: load,
            fan1_rpm: f1,
            fan2_rpm: f2,
            fan_level: fan_level.clone(),
            freq_min: freqs.as_ref().map_or(0, |f| f.min),
            freq_avg: freqs.as_ref().map_or(0, |f| f.avg),
            freq_max: freqs.as_ref().map_or(0, |f| f.max),
            freq_cap_mhz: freq_cap,
            epp: epp.clone(),
            throttle_rate,
            rapl_power_w: rapl_w,
            platform_profile: profile.clone(),
            pl1_w,
            fan_mode: fan_status.mode.as_str(),
            fan_need: fan_status.need,
        };
        minute.record(&sample);

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
        // Only when every read succeeded: a transient sysfs failure must
        // not overwrite the saved settings with "unknown"/fallback values
        if let Some(current) = Settings::from_reads(freq_cap_r, epp_r, profile_r) {
            if last_settings.as_ref() != Some(&current) {
                log(
                    "settings",
                    &format!(
                        "Change detected: profile={} cap={} epp={}",
                        current.platform_profile.as_deref().unwrap_or("-"),
                        current.freq_cap_mhz,
                        current.epp
                    ),
                );
                current.save();
                last_settings = Some(current);
            }
        }

        // --- Periodic status log (every 60s) ---
        tick += 1;
        if tick.is_multiple_of(3600) {
            prune_events();
        }
        if tick.is_multiple_of(60) {
            log(
                "status",
                &format!(
                    "{:.0}°C Δ{:+.1}°C/s load={:.0}% fan={} ({}, need {:.2}) rpm={}/{} freq={}/{}/{} cap={} epp={} profile={} pl1={} rapl={:.1}W | last min: {}",
                    temp, temp_rate, load * 100.0,
                    fan_level, fan_status.mode.as_str(), fan_status.need, f1, f2,
                    sample.freq_min, sample.freq_avg, sample.freq_max,
                    freq_cap, epp, profile,
                    pl1_w.map_or("?".into(), |w| format!("{w:.0}W")), rapl_w,
                    minute.summary(),
                ),
            );
            minute = MinuteStats::default();
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
    eprintln!("  Fan curve + settings keeper for ThinkPad X1");
    eprintln!("================================================");
    eprintln!("  Settings: {SETTINGS_FILE}");
    eprintln!("  Events:   {EVENTS_DIR}/");
    eprintln!("  Poll: {}ms  Buffer: {} samples (5 min)", POLL_MS, BUFFER_CAPACITY);
    eprintln!("================================================\n");

    let initial_temp = hw::find_temp_sensor()
        .and_then(|p| hw::cpu_temp(&p))
        .unwrap_or(0.0);
    log(
        "main",
        &format!(
            "Initial: {:.0}°C, fan={}",
            initial_temp,
            hw::read_fan_level().unwrap_or_else(|| "unknown".into())
        ),
    );

    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, Arc::clone(&stop))
        .expect("Failed to register SIGTERM handler");
    signal_hook::flag::register(signal_hook::consts::SIGINT, Arc::clone(&stop))
        .expect("Failed to register SIGINT handler");

    observer(&stop);

    // Graceful shutdown: hand the fan back to the EC. Profile, cap and EPP stay
    // as the user set them, so a restart does not undo the current tuning.
    log("main", "Shutting down → fan=auto (profile, cap and EPP unchanged)");
    hw::set_fan_watchdog(0);
    hw::set_fan_level("auto");
    log("main", "Done. Goodbye.");
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(temp_c: f64, temp_rate: f64, throttle_rate: f64) -> Sample {
        Sample {
            timestamp: SystemTime::UNIX_EPOCH,
            temp_c,
            temp_rate,
            cpu_load: 0.0,
            fan1_rpm: 0,
            fan2_rpm: 0,
            fan_level: "auto".into(),
            freq_min: 0,
            freq_avg: 0,
            freq_max: 0,
            freq_cap_mhz: 0,
            epp: "performance".into(),
            throttle_rate,
            rapl_power_w: 0.0,
            platform_profile: "performance".into(),
            pl1_w: Some(40.0),
            fan_mode: "curve",
            fan_need: 0.0,
        }
    }

    #[test]
    fn old_settings_file_with_fan_level_still_loads() {
        let json = r#"{"fan_level": "disengaged", "freq_cap_mhz": 2200, "epp": "performance"}"#;
        let s: Settings = serde_json::from_str(json).unwrap();
        assert_eq!(
            s,
            Settings {
                freq_cap_mhz: 2200,
                epp: "performance".into(),
                platform_profile: None,
            }
        );
    }

    #[test]
    fn settings_round_trip_the_platform_profile() {
        let s = Settings {
            freq_cap_mhz: 2000,
            epp: "performance".into(),
            platform_profile: Some("performance".into()),
        };
        let back: Settings = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn pl1_cut_fires_on_a_drop_but_not_on_a_profile_change() {
        let mut d = EventDetector::new();
        let with = |pl1: f64, profile: &str| Sample {
            pl1_w: Some(pl1),
            platform_profile: profile.into(),
            ..sample(60.0, 0.0, 0.0)
        };
        assert!(d.detect(&with(40.0, "performance")).is_empty());
        // the user switches profile: PL1 follows, that is not a cut, even
        // when the firmware applies the new PL1 one tick late
        assert!(d.detect(&with(40.0, "balanced")).is_empty());
        assert!(d.detect(&with(15.0, "balanced")).is_empty());
        assert!(d.detect(&with(40.0, "performance")).is_empty());
        // the profile has been stable for a while, then the EC lowers PL1
        let settled = Instant::now().checked_sub(Duration::from_secs(10)).unwrap();
        d.profile = Some(("performance".into(), settled));
        assert_eq!(d.detect(&with(12.0, "performance")), vec![EventType::Pl1Cut]);
    }

    #[test]
    fn temp_crossing_fires_once_until_hysteresis_resets() {
        let mut d = EventDetector::new();
        assert_eq!(d.detect(&sample(86.0, 0.0, 0.0)), vec![EventType::TempCross85]);
        // still above, and inside the hysteresis band: no new event
        assert!(d.detect(&sample(87.0, 0.0, 0.0)).is_empty());
        assert!(d.detect(&sample(84.0, 0.0, 0.0)).is_empty());
        assert!(d.detect(&sample(86.0, 0.0, 0.0)).is_empty());
        // dropped below 83: re-armed, but the 60 s cooldown still blocks it
        assert!(d.detect(&sample(80.0, 0.0, 0.0)).is_empty());
        assert!(d.detect(&sample(86.0, 0.0, 0.0)).is_empty());
    }

    #[test]
    fn rapid_rise_needs_three_consecutive_samples() {
        let mut d = EventDetector::new();
        assert!(d.detect(&sample(60.0, 3.0, 0.0)).is_empty());
        assert!(d.detect(&sample(62.0, 3.0, 0.0)).is_empty());
        assert!(d.detect(&sample(63.0, 1.0, 0.0)).is_empty()); // streak broken
        assert!(d.detect(&sample(65.0, 3.0, 0.0)).is_empty());
        assert!(d.detect(&sample(67.0, 3.0, 0.0)).is_empty());
        assert_eq!(d.detect(&sample(70.0, 3.0, 0.0)), vec![EventType::RapidTempRise]);
    }

    #[test]
    fn throttle_fires_with_cooldown() {
        let mut d = EventDetector::new();
        assert_eq!(d.detect(&sample(60.0, 0.0, 5.0)), vec![EventType::Throttle]);
        assert!(d.detect(&sample(60.0, 0.0, 5.0)).is_empty());
    }

    #[test]
    fn temp_rate_is_slope_in_degrees_per_second() {
        let mut h = TempHistory::new(16);
        let t0 = Instant::now();
        for i in 0..5 {
            h.push_at(t0 + Duration::from_secs(i), 50.0 + 2.0 * i as f64);
        }
        assert!((h.rate() - 2.0).abs() < 1e-9);
    }

    #[test]
    fn temp_history_drops_entries_outside_window() {
        let mut h = TempHistory::new(16);
        let t0 = Instant::now();
        h.push_at(t0, 90.0);
        for i in 20..25 {
            h.push_at(t0 + Duration::from_secs(i), 50.0);
        }
        assert_eq!(h.entries.len(), 5);
        assert_eq!(h.rate(), 0.0);
    }
}
