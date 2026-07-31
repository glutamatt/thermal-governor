use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Frame, Terminal,
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols,
    text::{Line, Span},
    widgets::{
        Axis, Block, BorderType, Borders, Chart, Dataset, GraphType, Paragraph, Wrap,
    },
};
use std::collections::VecDeque;
use std::fs::{self, File};
use std::io::{self, BufWriter, Stdout, Write as IoWrite};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};

// =============================================================================
// Constants
// =============================================================================

// Zone numbering is not stable across boots/kernels — discover by type at startup
const TEMP_ZONE_TYPE: &str = "x86_pkg_temp";
const TEMP_SENSOR_FALLBACK: &str = "/sys/class/thermal/thermal_zone8/temp";
const THROTTLE_PATH: &str =
    "/sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_total_time_ms";
const EPP_PATH: &str = "/sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference";
const RAPL_PKG_PATH: &str = "/sys/class/powercap/intel-rapl:0/energy_uj";
const FAN_CTRL: &str = "/proc/acpi/ibm/fan";
const FAN_LEVELS: &[&str] = &["0", "1", "2", "3", "4", "5", "6", "7", "disengaged"];

const HISTORY_CAP: usize = 300; // 5 minutes at 1Hz

const STRESS_LEVELS: &[u32] = &[0, 1, 2, 4, 8, 16];
const FREQ_CAPS: &[u32] = &[2000, 2200, 2400, 2600, 2800, 3000, 3200, 3400, 3600, 3800, 4000, 4200, 4400, 4500];
const EPP_VALUES: &[&str] = &["power", "balance_power", "balance_performance", "performance", "default"];

// =============================================================================
// TimeSeries
// =============================================================================

struct TimeSeries {
    data: VecDeque<(f64, f64)>,
}

impl TimeSeries {
    fn new() -> Self {
        Self {
            data: VecDeque::with_capacity(HISTORY_CAP + 1),
        }
    }

    fn push(&mut self, elapsed: f64, value: f64) {
        self.data.push_back((elapsed, value));
        if self.data.len() > HISTORY_CAP {
            self.data.pop_front();
        }
    }

    fn as_vec(&self) -> Vec<(f64, f64)> {
        self.data.iter().copied().collect()
    }

    fn y_bounds(&self, default_min: f64, default_max: f64, padding: f64) -> [f64; 2] {
        if self.data.is_empty() {
            return [default_min, default_max];
        }
        let mut lo = f64::MAX;
        let mut hi = f64::MIN;
        for &(_, v) in &self.data {
            if v < lo {
                lo = v;
            }
            if v > hi {
                hi = v;
            }
        }
        lo = (lo - padding).max(default_min);
        hi = (hi + padding).max(lo + 1.0);
        [lo, hi]
    }

    fn x_bounds(&self) -> [f64; 2] {
        if self.data.is_empty() {
            return [0.0, 300.0];
        }
        let last = self.data.back().unwrap().0;
        let first = (last - 300.0).max(0.0);
        [first, last]
    }
}

// =============================================================================
// Hardware I/O
// =============================================================================

fn find_hwmon_by_name(name: &str) -> Option<PathBuf> {
    for entry in fs::read_dir("/sys/class/hwmon/").ok()? {
        let entry = entry.ok()?;
        let n = fs::read_to_string(entry.path().join("name"))
            .unwrap_or_default()
            .trim()
            .to_string();
        if n == name {
            return Some(entry.path());
        }
    }
    None
}

fn read_sysfs_i64(path: &str) -> Option<i64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn read_sysfs_str(path: &str) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_string()
}

fn find_thermal_zone_by_type(zone_type: &str) -> Option<String> {
    for entry in fs::read_dir("/sys/class/thermal/").ok()? {
        let entry = entry.ok()?;
        if !entry.file_name().to_string_lossy().starts_with("thermal_zone") {
            continue;
        }
        let t = fs::read_to_string(entry.path().join("type")).unwrap_or_default();
        if t.trim() == zone_type {
            return Some(entry.path().join("temp").to_string_lossy().to_string());
        }
    }
    None
}

fn read_temp(sensor: &str) -> f64 {
    read_sysfs_i64(sensor).unwrap_or(0) as f64 / 1000.0
}

fn read_fan_rpms(thinkpad_hwmon: &str) -> (u32, u32) {
    let f1 = read_sysfs_i64(&format!("{thinkpad_hwmon}/fan1_input")).unwrap_or(0) as u32;
    let f2 = read_sysfs_i64(&format!("{thinkpad_hwmon}/fan2_input")).unwrap_or(0) as u32;
    let f1 = if f1 >= 60_000 { 0 } else { f1 };
    let f2 = if f2 >= 60_000 { 0 } else { f2 };
    (f1, f2)
}

fn read_throttle_ms() -> u64 {
    read_sysfs_i64(THROTTLE_PATH).unwrap_or(0) as u64
}

fn read_epp() -> String {
    read_sysfs_str(EPP_PATH)
}

fn read_energy_uj() -> u64 {
    read_sysfs_i64(RAPL_PKG_PATH).unwrap_or(0) as u64
}

fn read_battery_power_w() -> Option<f64> {
    let status = read_sysfs_str("/sys/class/power_supply/BAT0/status");
    if status != "Discharging" {
        return None;
    }
    let uw = read_sysfs_i64("/sys/class/power_supply/BAT0/power_now")?;
    Some(uw as f64 / 1_000_000.0)
}

fn read_cpu_usage() -> (u64, u64) {
    let content = fs::read_to_string("/proc/stat").unwrap_or_default();
    let first = content.lines().next().unwrap_or("");
    let fields: Vec<u64> = first
        .split_whitespace()
        .skip(1)
        .filter_map(|s| s.parse().ok())
        .collect();
    let total: u64 = fields.iter().sum();
    let idle = fields.get(3).copied().unwrap_or(0) + fields.get(4).copied().unwrap_or(0);
    (idle, total)
}

fn set_epp(dirs: &[PathBuf], epp: &str) {
    for d in dirs {
        let _ = fs::write(d.join("energy_performance_preference"), epp);
    }
}

fn set_fan_level(level: &str) {
    let _ = fs::write(FAN_CTRL, format!("level {level}"));
}

fn set_fan_auto() {
    let _ = fs::write(FAN_CTRL, "level auto");
}

/// Current fan state from /proc/acpi/ibm/fan → (auto, level index)
fn read_fan_state() -> (bool, usize) {
    if let Ok(content) = fs::read_to_string(FAN_CTRL) {
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("level:") {
                let lvl = rest.trim();
                if let Some(pos) = FAN_LEVELS.iter().position(|&l| l == lvl) {
                    return (false, pos);
                }
                break; // "auto" or unknown
            }
        }
    }
    (true, 4)
}

fn cpufreq_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(entries) = fs::read_dir("/sys/devices/system/cpu/") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let s = name.to_string_lossy().to_string();
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

fn set_freq_cap(dirs: &[PathBuf], mhz: u32) {
    let khz = (mhz as u64 * 1000).to_string();
    for d in dirs {
        let _ = fs::write(d.join("scaling_max_freq"), &khz);
    }
}


// =============================================================================
// App
// =============================================================================

struct App {
    temp: TimeSeries,
    fan: TimeSeries,
    throttle_rate: TimeSeries,
    cpu_usage: TimeSeries,
    power: TimeSeries,
    sys_power: TimeSeries,
    rest_power: TimeSeries,
    freq_min: TimeSeries,
    freq_avg: TimeSeries,
    freq_max: TimeSeries,

    stress_idx: usize,
    cap_idx: usize,
    epp_idx: usize,
    fan_level_idx: usize,
    fan_auto: bool,
    stress_children: Vec<Child>,

    recording: bool,
    rec_start: Option<Instant>,
    csv_writer: Option<BufWriter<File>>,
    csv_path: Option<String>,

    prev_throttle_ms: u64,
    prev_throttle_time: Instant,
    prev_energy_uj: u64,
    prev_energy_time: Instant,
    prev_cpu_idle: u64,
    prev_cpu_total: u64,
    cpufreq_dirs: Vec<PathBuf>,
    thinkpad_hwmon: String,
    temp_sensor: String,

    events: VecDeque<(String, String)>, // (timestamp, message)
    start: Instant,
    last_sample: Instant,
    should_quit: bool,

    cur_temp: f64,
    cur_fan: u32,
    cur_fan1: u32,
    cur_fan2: u32,
    cur_epp: String,
    cur_throttle_rate: f64,
    cur_cpu: f64,
    cur_power_w: f64,
    cur_sys_power_w: Option<f64>,
    cur_freq_min: u32,
    cur_freq_avg: u32,
    cur_freq_max: u32,
}

impl App {
    fn new() -> Self {
        let dirs = cpufreq_dirs();
        let thinkpad_hwmon = find_hwmon_by_name("thinkpad")
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "/sys/class/hwmon/hwmon_missing".to_string());
        let temp_sensor = find_thermal_zone_by_type(TEMP_ZONE_TYPE)
            .unwrap_or_else(|| TEMP_SENSOR_FALLBACK.to_string());
        let (idle, total) = read_cpu_usage();
        let now = Instant::now();
        let thr = read_throttle_ms();
        let energy = read_energy_uj();
        let epp = read_epp();

        // Sync UI state with the actual hardware state — the tool doesn't
        // reset on exit and the observer daemon restores settings at boot,
        // so assuming defaults would mislabel the status bar and CSVs
        let cap_idx = read_sysfs_i64("/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq")
            .map(|khz| (khz / 1000) as u32)
            .and_then(|mhz| FREQ_CAPS.iter().position(|&c| c >= mhz))
            .unwrap_or(FREQ_CAPS.len() - 1);
        let (fan_auto, fan_level_idx) = read_fan_state();

        // detect current EPP index
        let epp_idx = EPP_VALUES
            .iter()
            .position(|&e| e == epp)
            .unwrap_or(0);

        Self {
            temp: TimeSeries::new(),
            fan: TimeSeries::new(),
            throttle_rate: TimeSeries::new(),
            cpu_usage: TimeSeries::new(),
            power: TimeSeries::new(),
            sys_power: TimeSeries::new(),
            rest_power: TimeSeries::new(),
            freq_min: TimeSeries::new(),
            freq_avg: TimeSeries::new(),
            freq_max: TimeSeries::new(),

            stress_idx: 0,
            cap_idx,
            epp_idx,
            fan_level_idx,
            fan_auto,
            stress_children: Vec::new(),

            recording: false,
            rec_start: None,
            csv_writer: None,
            csv_path: None,

            prev_throttle_ms: thr,
            prev_throttle_time: now,
            prev_energy_uj: energy,
            prev_energy_time: now,
            prev_cpu_idle: idle,
            prev_cpu_total: total,
            cpufreq_dirs: dirs,
            thinkpad_hwmon,
            temp_sensor,

            events: VecDeque::with_capacity(10),
            start: now,
            last_sample: now,
            should_quit: false,

            cur_temp: 0.0,
            cur_fan: 0,
            cur_fan1: 0,
            cur_fan2: 0,
            cur_epp: epp,
            cur_throttle_rate: 0.0,
            cur_cpu: 0.0,
            cur_power_w: 0.0,
            cur_sys_power_w: None,
            cur_freq_min: 0,
            cur_freq_avg: 0,
            cur_freq_max: 0,
        }
    }

    fn log_event(&mut self, msg: String) {
        let ts = chrono_now().split('T').nth(1).unwrap_or("").to_string();
        self.events.push_back((ts, msg));
        if self.events.len() > 6 {
            self.events.pop_front();
        }
    }

    fn sample(&mut self) {
        let elapsed = self.start.elapsed().as_secs_f64();

        // Temperature
        let temp = read_temp(&self.temp_sensor);
        self.temp.push(elapsed, temp);
        self.cur_temp = temp;

        // Fan
        let (f1, f2) = read_fan_rpms(&self.thinkpad_hwmon);
        let fan = f1.max(f2);
        self.fan.push(elapsed, fan as f64);
        self.cur_fan = fan;
        self.cur_fan1 = f1;
        self.cur_fan2 = f2;

        // Throttle rate
        let thr_now = read_throttle_ms();
        let dt = self.prev_throttle_time.elapsed().as_secs_f64();
        let rate = if dt > 0.0 {
            (thr_now.saturating_sub(self.prev_throttle_ms)) as f64 / dt
        } else {
            0.0
        };
        self.throttle_rate.push(elapsed, rate);
        self.cur_throttle_rate = rate;
        self.prev_throttle_ms = thr_now;
        self.prev_throttle_time = Instant::now();

        // CPU usage
        let (idle, total) = read_cpu_usage();
        let d_idle = idle.saturating_sub(self.prev_cpu_idle) as f64;
        let d_total = total.saturating_sub(self.prev_cpu_total) as f64;
        let usage = if d_total > 0.0 {
            100.0 * (1.0 - d_idle / d_total)
        } else {
            0.0
        };
        self.cpu_usage.push(elapsed, usage);
        self.cur_cpu = usage;
        self.prev_cpu_idle = idle;
        self.prev_cpu_total = total;

        // Power (RAPL)
        let energy = read_energy_uj();
        let energy_dt = self.prev_energy_time.elapsed().as_secs_f64();
        if energy_dt > 0.0 && energy > self.prev_energy_uj {
            let watts = (energy - self.prev_energy_uj) as f64 / (energy_dt * 1_000_000.0);
            self.power.push(elapsed, watts);
            self.cur_power_w = watts;
        }
        self.prev_energy_uj = energy;
        self.prev_energy_time = Instant::now();

        // System power (battery)
        self.cur_sys_power_w = read_battery_power_w();
        if let Some(sys_w) = self.cur_sys_power_w {
            self.sys_power.push(elapsed, sys_w);
            let rest = (sys_w - self.cur_power_w).max(0.0);
            self.rest_power.push(elapsed, rest);
        }

        // Freq (all cores: min/avg/max) + EPP
        let mut fmin = u32::MAX;
        let mut fmax = 0u32;
        let mut fsum = 0u64;
        let mut fcount = 0u32;
        for d in &self.cpufreq_dirs {
            let p = d.join("scaling_cur_freq");
            if let Some(khz) = read_sysfs_i64(p.to_str().unwrap_or("")) {
                let mhz = (khz / 1000) as u32;
                if mhz < fmin {
                    fmin = mhz;
                }
                if mhz > fmax {
                    fmax = mhz;
                }
                fsum += mhz as u64;
                fcount += 1;
            }
        }
        let favg = if fcount > 0 {
            (fsum / fcount as u64) as u32
        } else {
            0
        };
        if fmin == u32::MAX {
            fmin = 0;
        }
        self.freq_min.push(elapsed, fmin as f64);
        self.freq_avg.push(elapsed, favg as f64);
        self.freq_max.push(elapsed, fmax as f64);
        self.cur_freq_min = fmin;
        self.cur_freq_avg = favg;
        self.cur_freq_max = fmax;
        self.cur_epp = read_epp();

        // CSV
        if self.recording {
            if let Some(ref mut w) = self.csv_writer {
                let ts = chrono_now();
                let rec_elapsed = self
                    .rec_start
                    .map(|s| s.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
                let _ = writeln!(
                    w,
                    "{},{:.1},{:.0},{},{},{},{},{:.1},{:.1},{},{},{},{},{:.1},{},{}",
                    ts,
                    rec_elapsed,
                    temp,
                    f1,
                    f2,
                    fan,
                    thr_now,
                    rate,
                    usage,
                    self.cur_freq_min,
                    self.cur_freq_avg,
                    self.cur_freq_max,
                    FREQ_CAPS[self.cap_idx],
                    self.cur_power_w,
                    self.cur_epp,
                    STRESS_LEVELS[self.stress_idx],
                );
                let _ = w.flush();
            }
        }

        self.last_sample = Instant::now();
    }

    fn stop_stress(&mut self) {
        for mut c in self.stress_children.drain(..) {
            // SIGTERM (not kill()'s SIGKILL) so the stress-ng supervisor
            // reaps its worker processes instead of orphaning them
            unsafe {
                libc::kill(c.id() as i32, libc::SIGTERM);
            }
            let _ = c.wait();
        }
    }

    fn set_stress(&mut self, idx: usize) {
        self.stop_stress();
        let cores = STRESS_LEVELS[idx];
        if cores == 0 {
            self.stress_idx = 0;
            self.log_event("🏋️ Stress → idle 😴".to_string());
            return;
        }
        let mut cmd = Command::new("stress-ng");
        cmd.args(["--cpu", &cores.to_string(), "--quiet"]);
        // die with the TUI even if it panics or is SIGKILLed
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(child) => {
                self.stress_children.push(child);
                self.stress_idx = idx;
                self.log_event(format!("🏋️ Stress → {cores} cores"));
            }
            Err(e) => {
                self.stress_idx = 0;
                self.log_event(format!("❌ stress-ng error: {e}"));
            }
        }
    }

    fn set_cap(&mut self, idx: usize) {
        self.cap_idx = idx;
        let mhz = FREQ_CAPS[idx];
        set_freq_cap(&self.cpufreq_dirs, mhz);
        self.log_event(format!("📏 Freq cap → {mhz} MHz"));
    }

    fn cycle_epp(&mut self) {
        self.epp_idx = (self.epp_idx + 1) % EPP_VALUES.len();
        let epp = EPP_VALUES[self.epp_idx];
        set_epp(&self.cpufreq_dirs, epp);
        self.cur_epp = epp.to_string();
        let emoji = match epp {
            "power" => "🔋",
            "balance_power" => "⚖️",
            "balance_performance" => "⚡",
            "performance" => "🚀",
            "default" => "🔄",
            _ => "❓",
        };
        self.log_event(format!("{emoji} EPP → {epp}"));
    }

    fn toggle_recording(&mut self) {
        if self.recording {
            self.csv_writer = None;
            self.recording = false;
            self.log_event("⏹️  Recording stopped".into());
        } else {
            let ts = chrono_now().replace(':', "-");
            let path = format!("hw-tui-{ts}.csv");
            match File::create(&path) {
                Ok(f) => {
                    let mut w = BufWriter::new(f);
                    let _ = writeln!(w, "timestamp,elapsed_s,temp_c,fan1_rpm,fan2_rpm,fan_max_rpm,throttle_total_ms,throttle_rate_ms_s,cpu_usage_pct,freq_min_mhz,freq_avg_mhz,freq_max_mhz,cap_mhz,power_w,epp,stress_cores");
                    self.csv_writer = Some(w);
                    self.csv_path = Some(path.clone());
                    self.rec_start = Some(Instant::now());
                    self.recording = true;
                    self.log_event(format!("🔴 Recording → {path}"));
                }
                Err(e) => self.log_event(format!("❌ CSV error: {e}")),
            }
        }
    }

    fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') | KeyCode::Esc => self.should_quit = true,
            KeyCode::Up => self.fan_up(),
            KeyCode::Down => self.fan_down(),
            KeyCode::Right => {
                let next = (self.cap_idx + 1).min(FREQ_CAPS.len() - 1);
                if next != self.cap_idx {
                    self.set_cap(next);
                }
            }
            KeyCode::Left => {
                if self.cap_idx > 0 {
                    let next = self.cap_idx - 1;
                    self.set_cap(next);
                }
            }
            KeyCode::Char('p') => self.cycle_epp(),
            KeyCode::Char('r') => self.toggle_recording(),
            KeyCode::Char('a') => self.toggle_fan_auto(),
            KeyCode::Char('k') => {
                let next = (self.stress_idx + 1).min(STRESS_LEVELS.len() - 1);
                if next != self.stress_idx {
                    self.set_stress(next);
                }
            }
            KeyCode::Char('j') => {
                if self.stress_idx > 0 {
                    let next = self.stress_idx - 1;
                    self.set_stress(next);
                }
            }
            _ => {}
        }
    }

    fn toggle_fan_auto(&mut self) {
        if self.fan_auto {
            let level = FAN_LEVELS[self.fan_level_idx];
            set_fan_level(level);
            self.fan_auto = false;
            self.log_event(format!("🌀 Fan → manual level {level}"));
        } else {
            set_fan_auto();
            self.fan_auto = true;
            self.log_event("🌀 Fan → auto (EC)".into());
        }
    }

    fn fan_up(&mut self) {
        let next = (self.fan_level_idx + 1).min(FAN_LEVELS.len() - 1);
        if next != self.fan_level_idx {
            self.fan_level_idx = next;
            let level = FAN_LEVELS[next];
            set_fan_level(level);
            self.fan_auto = false;
            self.log_event(format!("🌀 Fan → level {level}"));
        }
    }

    fn fan_down(&mut self) {
        if self.fan_level_idx > 0 {
            self.fan_level_idx -= 1;
            let level = FAN_LEVELS[self.fan_level_idx];
            set_fan_level(level);
            self.fan_auto = false;
            self.log_event(format!("🌀 Fan → level {level}"));
        }
    }

    fn cleanup(&mut self) {
        self.stop_stress();
    }
}

fn chrono_now() -> String {
    // Simple timestamp without chrono dependency
    let output = Command::new("date")
        .arg("+%Y-%m-%dT%H:%M:%S")
        .output()
        .ok();
    output
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .unwrap_or_default()
        .trim()
        .to_string()
}

// =============================================================================
// UI
// =============================================================================

fn draw(frame: &mut Frame, app: &App) {
    let area = frame.area();

    let outer = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),  // header
            Constraint::Min(8),    // charts (fills remaining)
            Constraint::Length(1), // status bar
            Constraint::Length(4), // event log
            Constraint::Length(1), // keybindings
        ])
        .split(area);

    draw_header(frame, outer[0], app);
    draw_charts(frame, outer[1], app);
    draw_status(frame, outer[2], app);
    draw_events(frame, outer[3], app);
    draw_help(frame, outer[4], app);
}

fn draw_header(frame: &mut Frame, area: Rect, app: &App) {
    let mut spans = vec![
        Span::styled(" 🌡️  HW THERMAL MONITOR ", Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("  "),
    ];

    if app.recording {
        let elapsed = app.rec_start.map(|s| s.elapsed().as_secs()).unwrap_or(0);
        let mm = elapsed / 60;
        let ss = elapsed % 60;
        spans.push(Span::styled(
            format!(" 🔴 REC {mm}:{ss:02} "),
            Style::default().fg(Color::White).bg(Color::Red).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw("  "));
    }

    let epp_emoji = match app.cur_epp.as_str() {
        "power" => "🔋",
        "balance_power" => "⚖️",
        "balance_performance" => "⚡",
        "performance" => "🚀",
        "default" => "🔄",
        _ => "❓",
    };
    spans.push(Span::styled(
        format!("{epp_emoji} epp: {}", app.cur_epp),
        Style::default().fg(Color::Yellow),
    ));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_charts(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(34),
            Constraint::Percentage(33),
        ])
        .split(rows[0]);

    let bot = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(33),
            Constraint::Percentage(34),
            Constraint::Percentage(33),
        ])
        .split(rows[1]);

    let temp_emoji = if app.cur_temp >= 85.0 {
        "🔥"
    } else if app.cur_temp >= 70.0 {
        "🌡️"
    } else {
        "❄️"
    };
    draw_chart(
        frame,
        top[0],
        ChartSpec {
            title: format!(" {temp_emoji} Temp  {:.0}°C ", app.cur_temp),
            series: &app.temp,
            color: Color::Yellow,
            border_color: if app.cur_temp >= 85.0 {
                Color::Red
            } else {
                Color::Yellow
            },
            y_min: 30.0,
            y_max: 110.0,
            y_pad: 5.0,
            ref_lines: &[(85.0, Color::LightRed), (95.0, Color::Red)],
        },
    );

    let fan_emoji = if app.cur_fan >= 4000 {
        "🌪️"
    } else if app.cur_fan > 0 {
        "💨"
    } else {
        "🤫"
    };
    draw_chart(
        frame,
        top[1],
        ChartSpec {
            title: format!(" {fan_emoji} Fan  {} RPM ", app.cur_fan),
            series: &app.fan,
            color: Color::Cyan,
            border_color: Color::Cyan,
            y_min: 0.0,
            y_max: 7000.0,
            y_pad: 200.0,
            ref_lines: &[],
        },
    );

    draw_power_chart(frame, top[2], app);

    let throttling = app.cur_throttle_rate > 0.0;
    let thr_emoji = if throttling { "⚠️" } else { "✅" };
    draw_chart(
        frame,
        bot[0],
        ChartSpec {
            title: format!(" {thr_emoji} Throttle  {:.1} ms/s ", app.cur_throttle_rate),
            series: &app.throttle_rate,
            color: Color::Red,
            border_color: if throttling { Color::Red } else { Color::DarkGray },
            y_min: 0.0,
            y_max: 10.0,
            y_pad: 1.0,
            ref_lines: &[],
        },
    );

    let cpu_emoji = if app.cur_cpu >= 80.0 {
        "🏋️"
    } else if app.cur_cpu >= 30.0 {
        "⚙️"
    } else {
        "😴"
    };
    draw_chart(
        frame,
        bot[1],
        ChartSpec {
            title: format!(" {cpu_emoji} CPU Usage  {:.0}% ", app.cur_cpu),
            series: &app.cpu_usage,
            color: Color::Green,
            border_color: Color::Green,
            y_min: 0.0,
            y_max: 100.0,
            y_pad: 5.0,
            ref_lines: &[],
        },
    );

    draw_freq_chart(frame, bot[2], app);
}

struct ChartSpec<'a> {
    title: String,
    series: &'a TimeSeries,
    color: Color,
    border_color: Color,
    y_min: f64,
    y_max: f64,
    y_pad: f64,
    // horizontal reference values, drawn dotted when inside the y range
    ref_lines: &'a [(f64, Color)],
}

fn fmt_ago(secs: f64) -> String {
    let m = (secs / 60.0) as u32;
    let s = (secs % 60.0) as u32;
    format!("-{m}:{s:02}")
}

fn x_axis<'a>(x_bounds: [f64; 2]) -> Axis<'a> {
    let range = x_bounds[1] - x_bounds[0];
    let labels = if range > 0.0 {
        vec![
            Line::from(fmt_ago(range)),
            Line::from(fmt_ago(range / 2.0)),
            Line::from("now"),
        ]
    } else {
        vec![Line::from("-0:00"), Line::from("now")]
    };
    Axis::default()
        .bounds(x_bounds)
        .labels(labels)
        .style(Style::default().fg(Color::DarkGray))
}

fn y_axis<'a>(y_bounds: [f64; 2]) -> Axis<'a> {
    Axis::default()
        .bounds(y_bounds)
        .labels(vec![
            Line::from(format!("{:.0}", y_bounds[0])),
            Line::from(format!("{:.0}", (y_bounds[0] + y_bounds[1]) / 2.0)),
            Line::from(format!("{:.0}", y_bounds[1])),
        ])
        .style(Style::default().fg(Color::DarkGray))
}

/// Dotted horizontal line: one scatter point every 3s of x range
fn ref_line_points(x_bounds: [f64; 2], y: f64) -> Vec<(f64, f64)> {
    let mut pts = Vec::new();
    let mut x = x_bounds[0];
    while x <= x_bounds[1] {
        pts.push((x, y));
        x += 3.0;
    }
    pts
}

fn draw_chart(frame: &mut Frame, area: Rect, spec: ChartSpec) {
    let data = spec.series.as_vec();
    let y_bounds = spec.series.y_bounds(spec.y_min, spec.y_max, spec.y_pad);
    let x_bounds = spec.series.x_bounds();

    let ref_data: Vec<(Vec<(f64, f64)>, Color)> = spec
        .ref_lines
        .iter()
        .filter(|(v, _)| *v >= y_bounds[0] && *v <= y_bounds[1])
        .map(|&(v, c)| (ref_line_points(x_bounds, v), c))
        .collect();

    // Reference lines first so the main curve draws on top
    let mut datasets: Vec<Dataset> = ref_data
        .iter()
        .map(|(pts, c)| {
            Dataset::default()
                .data(pts)
                .graph_type(GraphType::Scatter)
                .marker(symbols::Marker::Dot)
                .style(Style::default().fg(*c))
        })
        .collect();
    datasets.push(
        Dataset::default()
            .data(&data)
            .graph_type(GraphType::Line)
            .marker(symbols::Marker::Braille)
            .style(Style::default().fg(spec.color)),
    );

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(Span::styled(
                    spec.title,
                    Style::default().fg(spec.color).add_modifier(Modifier::BOLD),
                ))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(spec.border_color)),
        )
        .x_axis(x_axis(x_bounds))
        .y_axis(y_axis(y_bounds));

    frame.render_widget(chart, area);
}

fn draw_power_chart(frame: &mut Frame, area: Rect, app: &App) {
    let data_rapl = app.power.as_vec();
    let data_sys = app.sys_power.as_vec();
    let data_rest = app.rest_power.as_vec();

    let on_battery = app.cur_sys_power_w.is_some();

    // Y bounds: use sys_power if on battery, otherwise just RAPL
    let y_lo = 0.0;
    let y_hi = if on_battery {
        app.sys_power
            .y_bounds(0.0, 80.0, 3.0)[1]
            .max(app.power.y_bounds(0.0, 80.0, 3.0)[1])
    } else {
        app.power.y_bounds(0.0, 80.0, 3.0)[1]
    };
    let y_bounds = [y_lo, y_hi.max(1.0)];
    let x_bounds = app.power.x_bounds();

    let mut datasets = Vec::new();

    if on_battery {
        datasets.push(
            Dataset::default()
                .data(&data_sys)
                .graph_type(GraphType::Line)
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::Red)),
        );
        datasets.push(
            Dataset::default()
                .data(&data_rest)
                .graph_type(GraphType::Line)
                .marker(symbols::Marker::Braille)
                .style(Style::default().fg(Color::DarkGray)),
        );
    }

    datasets.push(
        Dataset::default()
            .data(&data_rapl)
            .graph_type(GraphType::Line)
            .marker(symbols::Marker::Braille)
            .style(Style::default().fg(Color::Magenta)),
    );

    // Title with current values
    let mut title_spans = vec![
        Span::styled(" Power ", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::styled("cpu:", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{:.1}W", app.cur_power_w), Style::default().fg(Color::Magenta)),
    ];
    if let Some(sys_w) = app.cur_sys_power_w {
        let rest = (sys_w - app.cur_power_w).max(0.0);
        title_spans.push(Span::styled(" rest:", Style::default().fg(Color::DarkGray)));
        title_spans.push(Span::styled(format!("{:.1}W", rest), Style::default().fg(Color::Red)));
        title_spans.push(Span::styled(" total:", Style::default().fg(Color::DarkGray)));
        title_spans.push(Span::styled(format!("{:.1}W", sys_w), Style::default().fg(Color::Red)));
    } else {
        title_spans.push(Span::styled(" (AC)", Style::default().fg(Color::DarkGray)));
    }
    title_spans.push(Span::styled(" ", Style::default()));

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(Line::from(title_spans))
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Magenta)),
        )
        .x_axis(x_axis(x_bounds))
        .y_axis(y_axis(y_bounds));

    frame.render_widget(chart, area);
}

fn draw_freq_chart(frame: &mut Frame, area: Rect, app: &App) {
    let data_min = app.freq_min.as_vec();
    let data_avg = app.freq_avg.as_vec();
    let data_max = app.freq_max.as_vec();

    // Compute y bounds across all three series
    let y_lo = app
        .freq_min
        .y_bounds(0.0, 5000.0, 100.0)[0]
        .min(app.freq_avg.y_bounds(0.0, 5000.0, 100.0)[0]);
    let y_hi = app
        .freq_max
        .y_bounds(0.0, 5000.0, 100.0)[1]
        .max(app.freq_avg.y_bounds(0.0, 5000.0, 100.0)[1]);
    let y_bounds = [y_lo, y_hi.max(y_lo + 100.0)];
    let x_bounds = app.freq_avg.x_bounds();

    // Current freq cap as a dotted reference line — makes soft-throttling
    // (freq_max dropping away from the cap) visible at a glance
    let cap = FREQ_CAPS[app.cap_idx] as f64;
    let cap_pts = if cap >= y_bounds[0] && cap <= y_bounds[1] {
        ref_line_points(x_bounds, cap)
    } else {
        Vec::new()
    };

    let ds_max = Dataset::default()
        .data(&data_max)
        .graph_type(GraphType::Line)
        .marker(symbols::Marker::Braille)
        .style(Style::default().fg(Color::Red));

    let ds_avg = Dataset::default()
        .data(&data_avg)
        .graph_type(GraphType::Line)
        .marker(symbols::Marker::Braille)
        .style(Style::default().fg(Color::Yellow));

    let ds_min = Dataset::default()
        .data(&data_min)
        .graph_type(GraphType::Line)
        .marker(symbols::Marker::Braille)
        .style(Style::default().fg(Color::Cyan));

    let mut datasets = Vec::new();
    if !cap_pts.is_empty() {
        datasets.push(
            Dataset::default()
                .data(&cap_pts)
                .graph_type(GraphType::Scatter)
                .marker(symbols::Marker::Dot)
                .style(Style::default().fg(Color::White)),
        );
    }
    datasets.extend([ds_max, ds_avg, ds_min]);

    let title = Line::from(vec![
        Span::styled(" Freq MHz ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled("min:", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{}", app.cur_freq_min), Style::default().fg(Color::Cyan)),
        Span::styled(" avg:", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{}", app.cur_freq_avg), Style::default().fg(Color::Yellow)),
        Span::styled(" max:", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{}", app.cur_freq_max), Style::default().fg(Color::Red)),
        Span::styled(" ", Style::default()),
    ]);

    let chart = Chart::new(datasets)
        .block(
            Block::default()
                .title(title)
                .borders(Borders::ALL)
                .border_type(BorderType::Rounded)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .x_axis(x_axis(x_bounds))
        .y_axis(y_axis(y_bounds));

    frame.render_widget(chart, area);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let stress_label = if STRESS_LEVELS[app.stress_idx] == 0 {
        "idle".to_string()
    } else {
        format!("{}c", STRESS_LEVELS[app.stress_idx])
    };

    let temp_color = if app.cur_temp >= 85.0 {
        Color::Red
    } else if app.cur_temp >= 70.0 {
        Color::Yellow
    } else {
        Color::Green
    };

    let fan_color = if app.cur_fan >= 4000 {
        Color::Red
    } else if app.cur_fan > 0 {
        Color::Yellow
    } else {
        Color::Green
    };

    let spans = vec![
        Span::raw("  🏋️ Stress: "),
        Span::styled(&stress_label, Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD)),
        Span::raw("   📏 Cap: "),
        Span::styled(
            format!("{} MHz", FREQ_CAPS[app.cap_idx]),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   📊 Freq: "),
        Span::styled(
            format!("{}", app.cur_freq_min),
            Style::default().fg(Color::Cyan),
        ),
        Span::styled("/", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{}", app.cur_freq_avg),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled("/", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!("{} MHz", app.cur_freq_max),
            Style::default().fg(Color::Red),
        ),
        Span::raw("   🌡️ "),
        Span::styled(
            format!("{:.0}°C", app.cur_temp),
            Style::default().fg(temp_color).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   💨 "),
        Span::styled(
            format!("{}", app.cur_fan),
            Style::default().fg(fan_color),
        ),
        Span::raw(" ("),
        Span::styled(format!("{}", app.cur_fan1), Style::default().fg(Color::DarkGray)),
        Span::raw("/"),
        Span::styled(format!("{}", app.cur_fan2), Style::default().fg(Color::DarkGray)),
        Span::raw(")"),
        Span::raw("   🌀 "),
        Span::styled(
            if app.fan_auto {
                "auto".to_string()
            } else {
                format!("lvl {}", FAN_LEVELS[app.fan_level_idx])
            },
            Style::default().fg(if app.fan_auto { Color::Green } else { Color::Yellow }).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   🔌 "),
        Span::styled(
            format!("{:.1}W", app.cur_power_w),
            Style::default().fg(Color::Magenta),
        ),
    ];

    frame.render_widget(
        Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::DarkGray).fg(Color::White)),
        area,
    );
}

fn draw_events(frame: &mut Frame, area: Rect, app: &App) {
    // Newest entry in white, older ones faded
    let n = app.events.len();
    let lines: Vec<Line> = app
        .events
        .iter()
        .enumerate()
        .map(|(i, (ts, e))| {
            let msg_color = if i + 1 == n { Color::White } else { Color::Gray };
            Line::from(vec![
                Span::styled(format!(" {ts} "), Style::default().fg(Color::DarkGray)),
                Span::styled(e.as_str(), Style::default().fg(msg_color)),
            ])
        })
        .collect();

    let block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(Color::DarkGray))
        .border_type(BorderType::Rounded);

    frame.render_widget(Paragraph::new(lines).block(block).wrap(Wrap { trim: true }), area);
}

fn draw_help(frame: &mut Frame, area: Rect, _app: &App) {
    let help = Line::from(vec![
        Span::styled(" 🌀 [", Style::default().fg(Color::DarkGray)),
        Span::styled("↑↓", Style::default().fg(Color::Cyan)),
        Span::styled("] fan [", Style::default().fg(Color::DarkGray)),
        Span::styled("a", Style::default().fg(Color::Cyan)),
        Span::styled("] auto   📏 [", Style::default().fg(Color::DarkGray)),
        Span::styled("←→", Style::default().fg(Color::Cyan)),
        Span::styled("] freq cap   ⚡ [", Style::default().fg(Color::DarkGray)),
        Span::styled("p", Style::default().fg(Color::Cyan)),
        Span::styled("] epp   🏋️ [", Style::default().fg(Color::DarkGray)),
        Span::styled("jk", Style::default().fg(Color::Cyan)),
        Span::styled("] stress   💾 [", Style::default().fg(Color::DarkGray)),
        Span::styled("r", Style::default().fg(Color::Cyan)),
        Span::styled("] record   👋 [", Style::default().fg(Color::DarkGray)),
        Span::styled("q", Style::default().fg(Color::Cyan)),
        Span::styled("] quit", Style::default().fg(Color::DarkGray)),
    ]);

    frame.render_widget(
        Paragraph::new(help).alignment(Alignment::Center),
        area,
    );
}

// =============================================================================
// Terminal
// =============================================================================

fn setup_terminal() -> io::Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend)
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;
    Ok(())
}

// =============================================================================
// Main
// =============================================================================

fn fan_control_enabled() -> bool {
    fs::read_to_string("/sys/module/thinkpad_acpi/parameters/fan_control")
        .unwrap_or_default()
        .trim()
        == "Y"
}

fn main() -> io::Result<()> {
    if unsafe { libc::geteuid() } != 0 {
        let exe = std::env::current_exe().expect("cannot resolve own path");
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut cmd = Command::new("sudo");
        cmd.arg(&exe);
        cmd.args(&args);
        let status = cmd.status().expect("failed to exec sudo");
        std::process::exit(status.code().unwrap_or(1));
    }

    // Enable fan control BEFORE App::new(): reloading thinkpad_acpi
    // re-registers the hwmon device under a new number, which would
    // invalidate an already-discovered path
    let mut startup_events: Vec<String> = Vec::new();
    if !fan_control_enabled() {
        startup_events.push("🌀 Enabling fan control (reloading thinkpad_acpi)...".into());
        let out = Command::new("sh")
            .arg("-c")
            .arg("modprobe -r thinkpad_acpi && modprobe thinkpad_acpi fan_control=1")
            .output();
        match out {
            Ok(o) if o.status.success() && fan_control_enabled() => {
                startup_events.push("🌀 Fan control enabled".into());
            }
            _ => {
                startup_events.push("⚠️ Cannot enable fan control — run as root".into());
            }
        }
    } else {
        startup_events.push("🌀 Fan control enabled".into());
    }

    // Restore the terminal on panic from any exit path; PDEATHSIG on the
    // stress children handles their cleanup
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = execute!(io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));

    let mut terminal = setup_terminal()?;
    let mut app = App::new();

    if app.thinkpad_hwmon.ends_with("hwmon_missing") {
        app.log_event("⚠️ thinkpad hwmon not found — fan RPM unavailable".into());
    }
    for msg in startup_events {
        app.log_event(msg);
    }
    app.log_event(format!(
        "🚀 Started!  EPP: {}  Cap: {} MHz",
        app.cur_epp,
        FREQ_CAPS[app.cap_idx]
    ));

    // Initial sample
    app.sample();

    let res = run(&mut terminal, &mut app);

    // Cleanup + terminal restore must happen even when the loop errored
    app.cleanup();
    let restored = restore_terminal(&mut terminal);
    res?;
    restored?;

    println!("Exited. Settings left unchanged.");
    if let Some(path) = &app.csv_path {
        if app.recording {
            println!("Recording saved: {path}");
        }
    }

    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    loop {
        // Handle input
        if event::poll(Duration::from_millis(50))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    app.handle_key(key.code);
                }
            }
        }

        // Sample at 1Hz
        if app.last_sample.elapsed() >= Duration::from_secs(1) {
            app.sample();
        }

        // Draw
        terminal.draw(|frame| draw(frame, app))?;

        if app.should_quit {
            return Ok(());
        }
    }
}
