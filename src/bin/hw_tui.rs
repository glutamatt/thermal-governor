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
use std::fs::File;
use std::io::{self, BufWriter, Stdout, Write as IoWrite};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command};
use std::time::{Duration, Instant};
use thermal_governor::clock::{self, LocalTime};
use thermal_governor::fan_curve::{self, FanMode, Status};
use thermal_governor::hw::{self, FanSensor};
use tui_bar_graph::{BarGraph, BarStyle, ColorMode};

// =============================================================================
// Constants
// =============================================================================

const FAN_LEVELS: &[&str] = &["0", "1", "2", "3", "4", "5", "6", "7", "disengaged"];

// Time shown by the charts, in s. PL1 limits a 28 s average: 2 min shows a
// cut and its cause on the same screen.
const WINDOW_S: f64 = 120.0;
// SEN1 moves ~1 °C per minute and reads in whole degrees: on 2 min it
// shows one or two steps, no trend
const SEN1_WINDOW_S: f64 = 300.0;
const SAMPLE_PERIOD: Duration = Duration::from_secs(1);
// Spacing of the dots of a dotted line, in s of x range
const DOTTED_STEP_S: f64 = 3.0;

const STRESS_LEVELS: &[u32] = &[0, 1, 2, 4, 8, 16];
// Cap steps go from FREQ_CAP_MIN up to the highest core frequency (= no cap)
const FREQ_CAP_MIN: u32 = 2000;
const FREQ_CAP_STEP: usize = 200;
const EPP_VALUES: &[&str] = &["power", "balance_power", "balance_performance", "performance", "default"];

// =============================================================================
// TimeSeries
// =============================================================================

/// The points of the last `window_s` seconds
struct TimeSeries {
    data: VecDeque<(f64, f64)>,
    window_s: f64,
}

impl TimeSeries {
    fn new(window_s: f64) -> Self {
        Self {
            data: VecDeque::new(),
            window_s,
        }
    }

    fn push(&mut self, elapsed: f64, value: f64) {
        self.data.push_back((elapsed, value));
        while self.data.front().is_some_and(|&(x, _)| x < elapsed - self.window_s) {
            self.data.pop_front();
        }
    }

    fn as_vec(&self) -> Vec<(f64, f64)> {
        self.data.iter().copied().collect()
    }

    /// Time of the newest point
    fn last_x(&self) -> Option<f64> {
        self.data.back().map(|&(x, _)| x)
    }

    /// The first point of each DOTTED_STEP_S slot of time: a dotted line.
    /// Slots are fixed in time, so the dots scroll with the data.
    fn dotted(&self) -> Vec<(f64, f64)> {
        let mut slot = None;
        self.data
            .iter()
            .copied()
            .filter(|&(x, _)| {
                let s = Some((x / DOTTED_STEP_S).floor() as i64);
                std::mem::replace(&mut slot, s) != s
            })
            .collect()
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

    /// The whole window, ending at the newest point: the time scale does not
    /// change while the window fills up after start
    fn x_bounds(&self) -> [f64; 2] {
        let last = self.last_x().unwrap_or(0.0);
        [last - self.window_s, last]
    }

    /// The window as `n` bars, oldest first. Each bar is the highest point
    /// of its slot of time, so a short peak stays visible. A slot with no
    /// point keeps the bar before it; before the first point, None.
    fn bars(&self, n: usize) -> Vec<Option<f64>> {
        let mut bars = vec![None; n];
        if n == 0 {
            return bars;
        }
        let [x0, x1] = self.x_bounds();
        let slot_s = (x1 - x0) / n as f64;
        for &(x, v) in &self.data {
            let i = (((x - x0) / slot_s) as usize).min(n - 1);
            bars[i] = Some(bars[i].map_or(v, |b: f64| b.max(v)));
        }
        let mut prev = None;
        for bar in &mut bars {
            if bar.is_none() {
                *bar = prev;
            }
            prev = *bar;
        }
        bars
    }
}

// =============================================================================
// Hardware state helpers
// =============================================================================

/// Index in FAN_LEVELS of the level the fan runs at now; None on "auto"
fn current_fan_level_idx() -> Option<usize> {
    hw::read_fan_level().and_then(|lvl| FAN_LEVELS.iter().position(|&l| l == lvl))
}

/// The daemon's last status, if it is recent enough to mean it is running
fn fresh_fan_status() -> Option<Status> {
    Status::read().filter(|s| clock::unix_now().saturating_sub(s.updated) <= 3)
}

/// FREQ_CAP_MIN, +200, +400, … then `top`, the highest core frequency
fn cap_steps(top: u32) -> Vec<u32> {
    let mut steps: Vec<u32> = (FREQ_CAP_MIN..top).step_by(FREQ_CAP_STEP).collect();
    steps.push(top);
    steps
}

/// Next step above the current cap. Works from any cap, even one set outside
/// hw-tui that is not on a step.
fn next_cap(steps: &[u32], cur: Option<u32>) -> Option<u32> {
    match cur {
        Some(cur) => steps.iter().copied().find(|&s| s > cur),
        None => steps.last().copied(),
    }
}

fn prev_cap(steps: &[u32], cur: Option<u32>) -> Option<u32> {
    match cur {
        Some(cur) => steps.iter().copied().rev().find(|&s| s < cur),
        None => steps.first().copied(),
    }
}

// =============================================================================
// App
// =============================================================================

struct App {
    temp: TimeSeries,
    sen1: TimeSeries,
    fan: TimeSeries,
    throttle_rate: TimeSeries,
    cpu_usage: TimeSeries,
    power: TimeSeries,
    power_avg: TimeSeries,
    pl1: TimeSeries,
    sys_power: TimeSeries,
    rest_power: TimeSeries,
    freq_avg: TimeSeries,
    freq_max: TimeSeries,

    stress_idx: usize,
    cap_steps: Vec<u32>,
    epp_idx: usize,
    // Level set by hand, used in manual mode only
    fan_level_idx: usize,
    fan_mode: FanMode,
    fan_status: Option<Status>,
    stress_children: Vec<Child>,

    recording: bool,
    rec_start: Option<Instant>,
    csv_writer: Option<BufWriter<File>>,
    csv_path: Option<String>,

    cpu_meter: hw::CpuUsage,
    throttle_meter: hw::ThrottleMeter,
    rapl_meter: hw::RaplMeter,
    cpufreq_dirs: Vec<PathBuf>,
    fans: FanSensor,
    temp_sensor: PathBuf,
    sen_sensors: Vec<hw::SenSensor>,

    events: VecDeque<(String, String)>, // (timestamp, message)
    start: Instant,
    last_sample: Instant,
    should_quit: bool,

    cur_temp: f64,
    cur_sen1: Option<f64>,
    cur_fan: u32,
    cur_fan1: u32,
    cur_fan2: u32,
    // Read back from sysfs on every sample, never assumed from the last
    // write: the daemon restores settings at boot and other tools can
    // change them while hw-tui runs
    cur_cap: Option<u32>,
    cur_epp: String,
    cur_profile: String,
    cur_throttle_rate: f64,
    cur_cpu: f64,
    cur_power_w: f64,
    // Package power averaged over the PL1 window: PL1 limits this average,
    // not the power itself
    cur_power_avg_w: Option<f64>,
    cur_pl1_w: Option<f64>,
    cur_pl1_window_s: Option<f64>,
    cur_sys_power_w: Option<f64>,
    cur_freq_min: u32,
    cur_freq_avg: u32,
    cur_freq_max: u32,
}

impl App {
    fn new() -> Self {
        let dirs = hw::cpufreq_dirs();
        let temp_sensor =
            hw::find_temp_sensor().unwrap_or_else(|| PathBuf::from(hw::TEMP_SENSOR_FALLBACK));
        let now = Instant::now();
        let epp = hw::read_epp(&dirs).unwrap_or_default();
        let cur_cap = hw::read_freq_cap(&dirs);
        // Fallback on the current cap: never offer a top below what is set now
        let cap_top = hw::max_hw_freq(&dirs).or(cur_cap).unwrap_or(FREQ_CAP_MIN);
        let fan_level_idx = current_fan_level_idx().unwrap_or(0);
        // Manual mode left over with the fan on auto (daemon restarted, or
        // the EC watchdog took the fan back): there is no hand-set level to
        // keep, and re-applying index 0 would turn the fan off
        let mut fan_mode = FanMode::read();
        if fan_mode == FanMode::Manual && current_fan_level_idx().is_none() {
            fan_mode = FanMode::Curve;
            let _ = fan_mode.write();
        }

        // detect current EPP index
        let epp_idx = EPP_VALUES
            .iter()
            .position(|&e| e == epp)
            .unwrap_or(0);

        Self {
            temp: TimeSeries::new(WINDOW_S),
            sen1: TimeSeries::new(SEN1_WINDOW_S),
            fan: TimeSeries::new(WINDOW_S),
            throttle_rate: TimeSeries::new(WINDOW_S),
            cpu_usage: TimeSeries::new(WINDOW_S),
            power: TimeSeries::new(WINDOW_S),
            power_avg: TimeSeries::new(WINDOW_S),
            pl1: TimeSeries::new(WINDOW_S),
            sys_power: TimeSeries::new(WINDOW_S),
            rest_power: TimeSeries::new(WINDOW_S),
            freq_avg: TimeSeries::new(WINDOW_S),
            freq_max: TimeSeries::new(WINDOW_S),

            stress_idx: 0,
            cap_steps: cap_steps(cap_top),
            epp_idx,
            fan_level_idx,
            fan_mode,
            fan_status: fresh_fan_status(),
            stress_children: Vec::new(),

            recording: false,
            rec_start: None,
            csv_writer: None,
            csv_path: None,

            cpu_meter: hw::CpuUsage::new(),
            throttle_meter: hw::ThrottleMeter::new(),
            rapl_meter: hw::RaplMeter::new(),
            cpufreq_dirs: dirs,
            fans: FanSensor::discover(),
            temp_sensor,
            sen_sensors: hw::find_sen_sensors(),

            events: VecDeque::with_capacity(10),
            start: now,
            last_sample: now,
            should_quit: false,

            cur_temp: 0.0,
            cur_sen1: None,
            cur_fan: 0,
            cur_fan1: 0,
            cur_fan2: 0,
            cur_cap,
            cur_epp: epp,
            cur_profile: hw::read_platform_profile().unwrap_or_default(),
            cur_throttle_rate: 0.0,
            cur_cpu: 0.0,
            cur_power_w: 0.0,
            cur_power_avg_w: None,
            cur_pl1_w: None,
            cur_pl1_window_s: None,
            cur_sys_power_w: None,
            cur_freq_min: 0,
            cur_freq_avg: 0,
            cur_freq_max: 0,
        }
    }

    fn log_event(&mut self, msg: String) {
        self.events.push_back((LocalTime::now().time(), msg));
        if self.events.len() > 6 {
            self.events.pop_front();
        }
    }

    /// Cap label for the UI: the top step is the highest core frequency, i.e. no cap
    fn cap_label(&self) -> String {
        match self.cur_cap {
            Some(mhz) if self.cap_steps.last().is_some_and(|&top| mhz >= top) => {
                format!("{mhz} MHz (max)")
            }
            Some(mhz) => format!("{mhz} MHz"),
            None => "? MHz".into(),
        }
    }

    fn sample(&mut self) {
        let elapsed = self.start.elapsed().as_secs_f64();

        // Temperature: on a failed read keep the last value and skip the
        // point, so the chart does not plunge to 0
        if let Some(temp) = hw::cpu_temp(&self.temp_sensor) {
            self.temp.push(elapsed, temp);
            self.cur_temp = temp;
        }
        // SEN1: the board sensor the EC watches for the PL1 cut
        self.cur_sen1 = hw::sen_temp(&self.sen_sensors, "SEN1");
        if let Some(sen1) = self.cur_sen1 {
            self.sen1.push(elapsed, sen1);
        }

        // Fan
        let (f1, f2) = self.fans.rpms();
        let fan = f1.max(f2);
        self.fan.push(elapsed, fan as f64);
        self.cur_fan = fan;
        self.cur_fan1 = f1;
        self.cur_fan2 = f2;

        // Throttle rate
        let rate = self.throttle_meter.sample().unwrap_or(0.0);
        self.throttle_rate.push(elapsed, rate);
        self.cur_throttle_rate = rate;

        // CPU usage
        let usage = 100.0 * self.cpu_meter.sample();
        self.cpu_usage.push(elapsed, usage);
        self.cur_cpu = usage;

        // PL1 and its window: the EC lowers PL1 when the machine is hot
        self.cur_pl1_w = hw::read_pl1_w();
        if let Some(pl1) = self.cur_pl1_w {
            self.pl1.push(elapsed, pl1);
        }
        self.cur_pl1_window_s = hw::read_pl1_window_s();

        // Power (RAPL), and its average over the PL1 window. The firmware
        // does not clamp the moment this average reaches PL1 (tests of
        // 2026-09-25: up to ~35 s later), so this is an early warning.
        if let Some(watts) = self.rapl_meter.sample() {
            let dt = self.power.last_x().map_or(0.0, |t| elapsed - t);
            self.power.push(elapsed, watts);
            self.cur_power_w = watts;
            if let Some(tau) = self.cur_pl1_window_s {
                let avg = fan_curve::smooth(&mut self.cur_power_avg_w, watts, dt, tau);
                self.power_avg.push(elapsed, avg);
            }
        }

        // System power (battery)
        self.cur_sys_power_w = hw::battery_power_w();
        if let Some(sys_w) = self.cur_sys_power_w {
            self.sys_power.push(elapsed, sys_w);
            let rest = (sys_w - self.cur_power_w).max(0.0);
            self.rest_power.push(elapsed, rest);
        }

        // Freq (all cores: min/avg/max; min only goes to the CSV) + cap + EPP
        let freqs = hw::read_freqs(&self.cpufreq_dirs);
        self.cur_freq_min = freqs.as_ref().map_or(0, |f| f.min);
        self.cur_freq_avg = freqs.as_ref().map_or(0, |f| f.avg);
        self.cur_freq_max = freqs.as_ref().map_or(0, |f| f.max);
        self.freq_avg.push(elapsed, self.cur_freq_avg as f64);
        self.freq_max.push(elapsed, self.cur_freq_max as f64);
        self.cur_cap = hw::read_freq_cap(&self.cpufreq_dirs);
        self.cur_epp = hw::read_epp(&self.cpufreq_dirs).unwrap_or_default();
        self.cur_profile = hw::read_platform_profile().unwrap_or_default();

        // Fan mode: the daemon may have changed it (hard limit in manual mode)
        self.fan_mode = FanMode::read();
        self.fan_status = fresh_fan_status();
        if self.fan_mode == FanMode::Manual {
            // Again every second: the daemon may have sent one last curve
            // level right after the switch to manual
            hw::set_fan_level(FAN_LEVELS[self.fan_level_idx]);
        }

        // CSV
        if self.recording {
            if let Some(ref mut w) = self.csv_writer {
                let rec_elapsed = self
                    .rec_start
                    .map(|s| s.elapsed().as_secs_f64())
                    .unwrap_or(0.0);
                let _ = writeln!(
                    w,
                    "{},{:.1},{:.0},{},{},{},{},{:.1},{:.1},{},{},{},{},{:.1},{},{}",
                    LocalTime::now().iso(),
                    rec_elapsed,
                    self.cur_temp,
                    f1,
                    f2,
                    fan,
                    self.throttle_meter.total_ms().unwrap_or(0),
                    rate,
                    usage,
                    self.cur_freq_min,
                    self.cur_freq_avg,
                    self.cur_freq_max,
                    self.cur_cap.unwrap_or(0),
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

    fn step_cap(&mut self, up: bool) {
        let target = if up {
            next_cap(&self.cap_steps, self.cur_cap)
        } else {
            prev_cap(&self.cap_steps, self.cur_cap)
        };
        let Some(mhz) = target else { return };
        hw::set_freq_cap(&self.cpufreq_dirs, mhz);
        self.cur_cap = hw::read_freq_cap(&self.cpufreq_dirs);
        self.log_event(format!("📏 Freq cap → {}", self.cap_label()));
    }

    fn cycle_epp(&mut self) {
        self.epp_idx = (self.epp_idx + 1) % EPP_VALUES.len();
        let epp = EPP_VALUES[self.epp_idx];
        hw::set_epp(&self.cpufreq_dirs, epp);
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
            let ts = LocalTime::now().iso().replace(':', "-");
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
            KeyCode::Up => self.step_fan(true),
            KeyCode::Down => self.step_fan(false),
            KeyCode::Right => self.step_cap(true),
            KeyCode::Left => self.step_cap(false),
            KeyCode::Char('p') => self.cycle_epp(),
            KeyCode::Char('r') => self.toggle_recording(),
            KeyCode::Char('a') => self.set_fan_mode(FanMode::Auto),
            KeyCode::Char('c') => self.set_fan_mode(FanMode::Curve),
            KeyCode::Char('o') => self.cycle_profile(),
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

    /// Curve and auto are the daemon's job; hw-tui writes the mode file
    fn set_fan_mode(&mut self, mode: FanMode) {
        if let Err(e) = mode.write() {
            self.log_event(format!("❌ Cannot write the fan mode: {e}"));
            return;
        }
        self.fan_mode = mode;
        match mode {
            FanMode::Auto => {
                // Also directly: works without the daemon
                hw::set_fan_level("auto");
                self.log_event("🌀 Fan → auto (EC)".into());
            }
            FanMode::Curve if fresh_fan_status().is_none() => {
                // Nobody would drive the fan: give it to the EC until the daemon runs
                hw::set_fan_level("auto");
                self.log_event("⚠️ Fan → curve, but the daemon is not running: EC for now".into());
            }
            _ => self.log_event(format!("🌀 Fan → {}", mode.as_str())),
        }
    }

    /// ↑/↓ take the fan by hand, from the level it runs at now
    fn step_fan(&mut self, up: bool) {
        let from = if self.fan_mode == FanMode::Manual {
            self.fan_level_idx
        } else {
            // On EC auto the level reads "auto": start from the level with
            // the closest speed, not from 0
            current_fan_level_idx().unwrap_or_else(|| {
                let level = fan_curve::nearest_level(self.cur_fan as f64);
                FAN_LEVELS.iter().position(|&l| l == level).unwrap_or(0)
            })
        };
        let to = if up {
            (from + 1).min(FAN_LEVELS.len() - 1)
        } else {
            from.saturating_sub(1)
        };
        if let Err(e) = FanMode::Manual.write() {
            self.log_event(format!("❌ Cannot write the fan mode: {e}"));
            return;
        }
        self.fan_mode = FanMode::Manual;
        self.fan_level_idx = to;
        hw::set_fan_level(FAN_LEVELS[to]);
        self.log_event(format!("🌀 Fan → manual level {}", FAN_LEVELS[to]));
    }

    /// The profile sets PL1: 10 / 15 / 40 W here. The daemon saves it.
    fn cycle_profile(&mut self) {
        let choices = hw::platform_profile_choices();
        if choices.is_empty() {
            self.log_event("❌ No platform profile on this machine".into());
            return;
        }
        let next = choices
            .iter()
            .position(|c| *c == self.cur_profile)
            .map_or(0, |i| (i + 1) % choices.len());
        hw::set_platform_profile(&choices[next]);
        self.cur_profile = hw::read_platform_profile().unwrap_or_default();
        self.log_event(format!("⚙️ Profile → {}", self.cur_profile));
    }

    fn cleanup(&mut self) {
        self.stop_stress();
        // A hand-set level lasts while hw-tui runs: nobody watches it after
        if FanMode::read() == FanMode::Manual {
            let _ = FanMode::Curve.write();
            if fresh_fan_status().is_none() {
                hw::set_fan_level("auto");
            }
        }
    }
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
    // Not "performance" means PL1 at 15 W or less: drops under load
    let profile_color = if app.cur_profile == "performance" {
        Color::Green
    } else {
        Color::Red
    };
    spans.push(Span::raw("   "));
    spans.push(Span::styled(
        format!("⚙️ profile: {}", app.cur_profile),
        Style::default().fg(profile_color),
    ));

    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_charts(frame: &mut Frame, area: Rect, app: &App) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(area);

    // Heat and cooling on top, load below
    let top = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 4); 4])
        .split(rows[0]);

    let bot = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Ratio(1, 3); 3])
        .split(rows[1]);

    draw_temp_chart(frame, top[0], app);
    draw_sen1_chart(frame, top[1], app);

    let fan_emoji = if app.cur_fan >= 4000 {
        "🌪️"
    } else if app.cur_fan > 0 {
        "💨"
    } else {
        "🤫"
    };
    draw_bar_chart(
        frame,
        top[2],
        BarSpec {
            title: title_line(format!(" {fan_emoji} Fan  {} RPM ", app.cur_fan), Color::Cyan),
            border_color: Color::Cyan,
            series: &app.fan,
            y_bounds: [0.0, app.fan.y_bounds(0.0, 7000.0, 200.0)[1]],
            gradient: &[(0.0, "#155e75"), (3000.0, "#06b6d4"), (7000.0, "#a5f3fc")],
            ref_lines: &[],
        },
    );

    let throttling = app.cur_throttle_rate > 0.0;
    let thr_emoji = if throttling { "⚠️" } else { "✅" };
    draw_bar_chart(
        frame,
        top[3],
        BarSpec {
            title: title_line(
                format!(" {thr_emoji} Throttle  {:.1} ms/s ", app.cur_throttle_rate),
                Color::Red,
            ),
            border_color: if throttling { Color::Red } else { Color::DarkGray },
            series: &app.throttle_rate,
            y_bounds: [0.0, app.throttle_rate.y_bounds(0.0, 10.0, 1.0)[1]],
            gradient: &[(0.0, "#f97316"), (10.0, "#dc2626")],
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
    draw_bar_chart(
        frame,
        bot[0],
        BarSpec {
            title: title_line(format!(" {cpu_emoji} CPU Usage  {:.0}% ", app.cur_cpu), Color::Green),
            border_color: Color::Green,
            series: &app.cpu_usage,
            y_bounds: [0.0, app.cpu_usage.y_bounds(0.0, 100.0, 5.0)[1]],
            gradient: &[(0.0, "#15803d"), (30.0, "#22c55e"), (80.0, "#eab308")],
            ref_lines: &[],
        },
    );

    draw_power_chart(frame, bot[1], app);
    draw_freq_chart(frame, bot[2], app);
}

fn title_line(text: String, color: Color) -> Line<'static> {
    Line::from(Span::styled(text, Style::default().fg(color).add_modifier(Modifier::BOLD)))
}

fn panel_block(title: Line, border_color: Color) -> Block {
    Block::default()
        .title(title)
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .border_style(Style::default().fg(border_color))
}

fn fmt_ago(secs: f64) -> String {
    let m = (secs / 60.0) as u32;
    let s = (secs % 60.0) as u32;
    format!("-{m}:{s:02}")
}

fn x_labels(x_bounds: [f64; 2]) -> Vec<Line<'static>> {
    let range = x_bounds[1] - x_bounds[0];
    vec![
        Line::from(fmt_ago(range)),
        Line::from(fmt_ago(range / 2.0)),
        Line::from("now"),
    ]
}

fn y_labels(y_bounds: [f64; 2]) -> Vec<Line<'static>> {
    vec![
        Line::from(format!("{:.0}", y_bounds[0])),
        Line::from(format!("{:.0}", (y_bounds[0] + y_bounds[1]) / 2.0)),
        Line::from(format!("{:.0}", y_bounds[1])),
    ]
}

fn axis<'a>(bounds: [f64; 2], labels: Vec<Line<'a>>) -> Axis<'a> {
    Axis::default()
        .bounds(bounds)
        .labels(labels)
        .style(Style::default().fg(Color::DarkGray))
}

/// Dotted horizontal line: one scatter point every DOTTED_STEP_S of x range
fn ref_line_points(x_bounds: [f64; 2], y: f64) -> Vec<(f64, f64)> {
    let mut pts = Vec::new();
    let mut x = x_bounds[0];
    while x <= x_bounds[1] {
        pts.push((x, y));
        x += DOTTED_STEP_S;
    }
    pts
}

/// The dotted datasets of the reference values inside the y range
fn ref_datasets<'a>(ref_data: &'a [(Vec<(f64, f64)>, Color)]) -> Vec<Dataset<'a>> {
    ref_data
        .iter()
        .map(|(pts, c)| {
            Dataset::default()
                .data(pts)
                .graph_type(GraphType::Scatter)
                .marker(symbols::Marker::Dot)
                .style(Style::default().fg(*c))
        })
        .collect()
}

fn ref_data(ref_lines: &[(f64, Color)], x_bounds: [f64; 2], y_bounds: [f64; 2]) -> Vec<(Vec<(f64, f64)>, Color)> {
    ref_lines
        .iter()
        .filter(|(v, _)| *v >= y_bounds[0] && *v <= y_bounds[1])
        .map(|&(v, c)| (ref_line_points(x_bounds, v), c))
        .collect()
}

/// Where ratatui's Chart draws its data inside `inner` (its block's inner
/// area): right of the y labels and the y axis, above the x axis and the x
/// labels. The first x label is aligned left: all but its last character
/// stand left of the y axis. The labels take at most a third of the width.
fn chart_graph_area(inner: Rect, y_labels: &[Line], x_labels: &[Line]) -> Rect {
    let y_labels_w = y_labels.iter().map(Line::width).max().unwrap_or(0);
    let x_label_w = x_labels.first().map_or(0, |l| l.width().saturating_sub(1));
    let left = (y_labels_w.max(x_label_w) as u16).min(inner.width / 3) + 1;
    Rect {
        x: inner.x + left,
        y: inner.y,
        width: inner.width.saturating_sub(left),
        height: inner.height.saturating_sub(2),
    }
}

struct BarSpec<'a> {
    title: Line<'a>,
    border_color: Color,
    series: &'a TimeSeries,
    y_bounds: [f64; 2],
    // Color stops (value, color): a color means the same value whatever
    // the y range
    gradient: &'a [(f64, &'a str)],
    // Horizontal reference values, drawn dotted over the bars when inside
    // the y range
    ref_lines: &'a [(f64, Color)],
}

fn gradient(stops: &[(f64, &str)]) -> colorgrad::LinearGradient {
    let colors: Vec<&str> = stops.iter().map(|&(_, c)| c).collect();
    let values: Vec<f32> = stops.iter().map(|&(v, _)| v as f32).collect();
    colorgrad::GradientBuilder::new()
        .html_colors(&colors)
        .domain(&values)
        .build()
        .expect("gradient stops: valid colors, values in increasing order")
}

/// One series as bars, under the same block and axes as the line charts
fn draw_bar_chart(frame: &mut Frame, area: Rect, spec: BarSpec) {
    let x_bounds = spec.series.x_bounds();
    let x_labels = x_labels(x_bounds);
    let y_labels = y_labels(spec.y_bounds);
    let block = panel_block(spec.title, spec.border_color);

    // Bars first: the chart then draws its block, axes and dotted lines,
    // and leaves the cells it does not use as they are
    let graph = chart_graph_area(block.inner(area), &y_labels, &x_labels);
    // Braille: two bars per cell
    let bars: Vec<f64> = spec
        .series
        .bars(2 * graph.width as usize)
        .into_iter()
        .map(|b| b.unwrap_or(spec.y_bounds[0]))
        .collect();
    frame.render_widget(
        BarGraph::new(bars)
            .with_min(spec.y_bounds[0])
            .with_max(spec.y_bounds[1])
            .with_gradient(gradient(spec.gradient))
            .with_bar_style(BarStyle::Braille)
            .with_color_mode(ColorMode::VerticalGradient),
        graph,
    );

    let ref_data = ref_data(spec.ref_lines, x_bounds, spec.y_bounds);
    let chart = Chart::new(ref_datasets(&ref_data))
        .block(block)
        .x_axis(axis(x_bounds, x_labels))
        .y_axis(axis(spec.y_bounds, y_labels));
    frame.render_widget(chart, area);
}

/// Package temperature, with the throttle zone dotted
fn draw_temp_chart(frame: &mut Frame, area: Rect, app: &App) {
    let temp_emoji = if app.cur_temp >= 85.0 {
        "🔥"
    } else if app.cur_temp >= 70.0 {
        "🌡️"
    } else {
        "❄️"
    };
    draw_bar_chart(
        frame,
        area,
        BarSpec {
            title: title_line(format!(" {temp_emoji} Temp  {:.0}°C ", app.cur_temp), Color::Yellow),
            border_color: if app.cur_temp >= 85.0 { Color::Red } else { Color::Yellow },
            series: &app.temp,
            y_bounds: app.temp.y_bounds(30.0, 110.0, 5.0),
            gradient: &[(45.0, "#22c55e"), (70.0, "#eab308"), (85.0, "#ef4444"), (95.0, "#d946ef")],
            ref_lines: &[(85.0, Color::LightRed), (95.0, Color::Red)],
        },
    );
}

/// SEN1, the board sensor the EC watches for the PL1 cut, with the cut
/// dotted. Its own panel: it moves by a degree or two where the package
/// moves by tens, and those degrees under the cut are what matters.
fn draw_sen1_chart(frame: &mut Frame, area: Rect, app: &App) {
    // Red one degree under the cut: the fan curve is at full speed there
    let near_cut = app.cur_sen1.is_some_and(|t| t >= fan_curve::SEN1_FULL_C);
    let color = if near_cut { Color::Red } else { Color::Cyan };
    let value = app.cur_sen1.map_or("?".into(), |t| format!("{t:.0}°C"));
    let title = Line::from(vec![
        Span::styled(format!(" 🎯 SEN1  {value} "), Style::default().fg(color).add_modifier(Modifier::BOLD)),
        Span::styled(format!("(cut {:.0}) ", hw::SEN1_PL1_CUT_C), Style::default().fg(Color::DarkGray)),
    ]);
    // Always show the cut: the room left under it is the point of this panel
    let [lo, hi] = app.sen1.y_bounds(30.0, 60.0, 2.0);
    draw_bar_chart(
        frame,
        area,
        BarSpec {
            title,
            border_color: color,
            series: &app.sen1,
            y_bounds: [lo, hi.max(hw::SEN1_PL1_CUT_C + 1.0)],
            // Cyan while the fan curve ignores SEN1, then to red at full speed
            gradient: &[
                (fan_curve::SEN1_ZERO_C, "#22d3ee"),
                ((fan_curve::SEN1_ZERO_C + fan_curve::SEN1_FULL_C) / 2.0, "#eab308"),
                (fan_curve::SEN1_FULL_C, "#ef4444"),
            ],
            ref_lines: &[(hw::SEN1_PL1_CUT_C, Color::Red)],
        },
    );
}

fn draw_power_chart(frame: &mut Frame, area: Rect, app: &App) {
    let data_rapl = app.power.as_vec();
    let data_avg = app.power_avg.as_vec();
    let data_sys = app.sys_power.as_vec();
    let data_rest = app.rest_power.as_vec();
    // PL1 as it was over time, dotted: it moves with the profile and the EC.
    // Above the y range, it is not drawn.
    let data_pl1 = app.pl1.dotted();

    let on_battery = app.cur_sys_power_w.is_some();

    // Y bounds: use sys_power if on battery, otherwise just RAPL
    let y_lo = 0.0;
    let mut y_hi = app
        .power
        .y_bounds(0.0, 80.0, 3.0)[1]
        .max(app.power_avg.y_bounds(0.0, 80.0, 3.0)[1]);
    if on_battery {
        y_hi = y_hi.max(app.sys_power.y_bounds(0.0, 80.0, 3.0)[1]);
    }
    let y_bounds = [y_lo, y_hi.max(1.0)];
    let x_bounds = app.power.x_bounds();

    // Reference line first so the curves draw on top
    let mut datasets = vec![
        Dataset::default()
            .data(&data_pl1)
            .graph_type(GraphType::Scatter)
            .marker(symbols::Marker::Dot)
            .style(Style::default().fg(Color::White)),
    ];

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
    datasets.push(
        Dataset::default()
            .data(&data_avg)
            .graph_type(GraphType::Line)
            .marker(symbols::Marker::Braille)
            .style(Style::default().fg(Color::Yellow)),
    );

    // Title with current values
    let mut title_spans = vec![
        Span::styled(" Power ", Style::default().fg(Color::Magenta).add_modifier(Modifier::BOLD)),
        Span::styled("cpu:", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{:.1}W", app.cur_power_w), Style::default().fg(Color::Magenta)),
    ];
    if let (Some(avg), Some(tau)) = (app.cur_power_avg_w, app.cur_pl1_window_s) {
        // Red once the average reaches PL1: a clamp to ~400 MHz is coming
        let at_limit = app.cur_pl1_w.is_some_and(|pl1| avg >= pl1);
        title_spans.push(Span::styled(format!(" avg{tau:.0}s:"), Style::default().fg(Color::DarkGray)));
        title_spans.push(Span::styled(
            format!("{avg:.1}W"),
            Style::default().fg(if at_limit { Color::Red } else { Color::Yellow }),
        ));
    }
    if let Some(pl1) = app.cur_pl1_w {
        title_spans.push(Span::styled(" PL1:", Style::default().fg(Color::DarkGray)));
        title_spans.push(Span::styled(format!("{pl1:.0}W"), Style::default().fg(Color::White)));
    }
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
        .block(panel_block(Line::from(title_spans), Color::Magenta))
        .x_axis(axis(x_bounds, x_labels(x_bounds)))
        .y_axis(axis(y_bounds, y_labels(y_bounds)));

    frame.render_widget(chart, area);
}

fn draw_freq_chart(frame: &mut Frame, area: Rect, app: &App) {
    let data_avg = app.freq_avg.as_vec();
    let data_max = app.freq_max.as_vec();

    // avg <= max: the y range goes from the lowest avg to the highest max
    let y_lo = app.freq_avg.y_bounds(0.0, 5000.0, 100.0)[0];
    let y_hi = app.freq_max.y_bounds(0.0, 5000.0, 100.0)[1];
    let y_bounds = [y_lo, y_hi.max(y_lo + 100.0)];
    let x_bounds = app.freq_avg.x_bounds();

    // Current freq cap as a dotted reference line — makes soft-throttling
    // (freq_max dropping away from the cap) visible at a glance
    let cap_line: Vec<(f64, Color)> = app.cur_cap.map(|cap| (f64::from(cap), Color::White)).into_iter().collect();
    let ref_data = ref_data(&cap_line, x_bounds, y_bounds);

    let mut datasets = ref_datasets(&ref_data);
    datasets.push(
        Dataset::default()
            .data(&data_max)
            .graph_type(GraphType::Line)
            .marker(symbols::Marker::Braille)
            .style(Style::default().fg(Color::Red)),
    );
    datasets.push(
        Dataset::default()
            .data(&data_avg)
            .graph_type(GraphType::Line)
            .marker(symbols::Marker::Braille)
            .style(Style::default().fg(Color::Yellow)),
    );

    let title = Line::from(vec![
        Span::styled(" Freq MHz ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        Span::styled("avg:", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{}", app.cur_freq_avg), Style::default().fg(Color::Yellow)),
        Span::styled(" max:", Style::default().fg(Color::DarkGray)),
        Span::styled(format!("{}", app.cur_freq_max), Style::default().fg(Color::Red)),
        Span::styled(" ", Style::default()),
    ]);

    let chart = Chart::new(datasets)
        .block(panel_block(title, Color::Yellow))
        .x_axis(axis(x_bounds, x_labels(x_bounds)))
        .y_axis(axis(y_bounds, y_labels(y_bounds)));

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
            app.cap_label(),
            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
        ),
        Span::raw("   📊 Freq: "),
        Span::styled(
            format!("{}", app.cur_freq_avg),
            Style::default().fg(Color::Yellow),
        ),
        Span::styled("/", Style::default().fg(Color::Gray)),
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
        Span::styled(format!("{}", app.cur_fan1), Style::default().fg(Color::Gray)),
        Span::raw("/"),
        Span::styled(format!("{}", app.cur_fan2), Style::default().fg(Color::Gray)),
        Span::raw(")"),
        Span::raw("   🌀 "),
        fan_mode_span(app),
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

fn fan_mode_span(app: &App) -> Span<'static> {
    let (text, color) = match (app.fan_mode, &app.fan_status) {
        (FanMode::Curve, None) => ("curve (daemon off!)".to_string(), Color::Red),
        (FanMode::Curve, Some(st)) => match &st.guard {
            Some(why) => (format!("curve FULL ({why})"), Color::Red),
            None => (
                format!(
                    "curve lvl {} need {:.2}",
                    st.level.as_deref().unwrap_or("-"),
                    st.need
                ),
                Color::Green,
            ),
        },
        (FanMode::Auto, _) => ("auto (EC)".to_string(), Color::Cyan),
        (FanMode::Manual, None) => (
            format!("manual lvl {} (no daemon: no hard limits)", FAN_LEVELS[app.fan_level_idx]),
            Color::Red,
        ),
        (FanMode::Manual, Some(_)) => (
            format!("manual lvl {}", FAN_LEVELS[app.fan_level_idx]),
            Color::Yellow,
        ),
    };
    Span::styled(text, Style::default().fg(color).add_modifier(Modifier::BOLD))
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
        Span::styled("c", Style::default().fg(Color::Cyan)),
        Span::styled("] curve [", Style::default().fg(Color::DarkGray)),
        Span::styled("a", Style::default().fg(Color::Cyan)),
        Span::styled("] auto   📏 [", Style::default().fg(Color::DarkGray)),
        Span::styled("←→", Style::default().fg(Color::Cyan)),
        Span::styled("] freq cap   ⚡ [", Style::default().fg(Color::DarkGray)),
        Span::styled("p", Style::default().fg(Color::Cyan)),
        Span::styled("] epp   ⚙️ [", Style::default().fg(Color::DarkGray)),
        Span::styled("o", Style::default().fg(Color::Cyan)),
        Span::styled("] profile   🏋️ [", Style::default().fg(Color::DarkGray)),
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
    // re-registers the hwmon device under a new number, so discovering it
    // afterwards avoids a first round of failed fan reads
    let mut startup_events: Vec<String> = Vec::new();
    if !hw::fan_control_enabled() {
        startup_events.push("🌀 Enabling fan control (reloading thinkpad_acpi)...".into());
        if hw::enable_fan_control() {
            startup_events.push("🌀 Fan control enabled".into());
        } else {
            startup_events.push("⚠️ Cannot enable fan control — run as root".into());
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

    if !app.fans.is_found() {
        app.log_event("⚠️ thinkpad hwmon not found — fan RPM unavailable".into());
    }
    for msg in startup_events {
        app.log_event(msg);
    }
    app.log_event(format!(
        "🚀 Started!  EPP: {}  Cap: {}",
        app.cur_epp,
        app.cap_label()
    ));

    // Initial sample
    app.sample();

    let res = run(&mut terminal, &mut app);

    // Cleanup + terminal restore must happen even when the loop errored
    app.cleanup();
    let restored = restore_terminal(&mut terminal);
    res?;
    restored?;

    println!("Exited. Profile, cap and EPP left unchanged; a manual fan level goes back to the curve.");
    if let Some(path) = &app.csv_path {
        if app.recording {
            println!("Recording saved: {path}");
        }
    }

    Ok(())
}

fn run(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut App) -> io::Result<()> {
    // Redraw only when something changed: a constant redraw would load
    // the CPU this tool is measuring
    let mut dirty = true;
    loop {
        if dirty {
            terminal.draw(|frame| draw(frame, app))?;
            dirty = false;
        }

        // Wait for input until the next sample is due
        let timeout = SAMPLE_PERIOD.saturating_sub(app.last_sample.elapsed());
        if event::poll(timeout)? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    app.handle_key(key.code);
                    dirty = true;
                }
                Event::Resize(..) => dirty = true,
                _ => {}
            }
        }
        if app.should_quit {
            return Ok(());
        }

        // Sample at 1Hz
        if app.last_sample.elapsed() >= SAMPLE_PERIOD {
            app.sample();
            dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotted_keeps_one_point_per_slot() {
        let mut series = TimeSeries::new(WINDOW_S);
        for (x, y) in [(0.2, 40.0), (1.2, 40.0), (2.9, 40.0), (3.1, 12.0), (5.0, 12.0), (6.4, 12.0)] {
            series.push(x, y);
        }
        assert_eq!(series.dotted(), vec![(0.2, 40.0), (3.1, 12.0), (6.4, 12.0)]);
        // The oldest point scrolls out: the other dots do not move
        series.data.pop_front();
        assert_eq!(series.dotted(), vec![(1.2, 40.0), (3.1, 12.0), (6.4, 12.0)]);
    }

    #[test]
    fn series_keeps_its_window_only() {
        let mut series = TimeSeries::new(10.0);
        for x in 0..=15 {
            series.push(x as f64, 1.0);
        }
        assert_eq!(series.data.front(), Some(&(5.0, 1.0)));
        assert_eq!(series.x_bounds(), [5.0, 15.0]);
        // Just after start: the scale is already the whole window
        let mut series = TimeSeries::new(10.0);
        series.push(2.0, 1.0);
        assert_eq!(series.x_bounds(), [-8.0, 2.0]);
    }

    #[test]
    fn bars_keep_peaks_and_fill_gaps() {
        let mut series = TimeSeries::new(8.0);
        for (x, y) in [(4.0, 1.0), (4.5, 7.0), (5.0, 2.0), (8.0, 3.0)] {
            series.push(x, y);
        }
        // Window [0, 8], 1 s per bar: nothing before 4 s, the peak of
        // [4, 5), the last value held over [6, 8), the newest in the last bar
        assert_eq!(
            series.bars(8),
            vec![None, None, None, None, Some(7.0), Some(2.0), Some(2.0), Some(3.0)]
        );
        assert_eq!(series.bars(0), vec![]);
    }

    #[test]
    fn graph_area_is_where_chart_draws() {
        // The corner of ratatui's axes is right under and left of the graph area
        for (width, y_bounds) in [(40, [0.0, 7000.0]), (60, [46.0, 55.0]), (12, [0.0, 10.0])] {
            let area = Rect::new(0, 0, width, 12);
            let block = Block::default().borders(Borders::ALL);
            let (x_bounds, x_labels, y_labels) = ([-120.0, 0.0], x_labels([-120.0, 0.0]), y_labels(y_bounds));
            let graph = chart_graph_area(block.inner(area), &y_labels, &x_labels);
            let mut buf = ratatui::buffer::Buffer::empty(area);
            ratatui::widgets::Widget::render(
                Chart::new(vec![])
                    .block(block)
                    .x_axis(axis(x_bounds, x_labels))
                    .y_axis(axis(y_bounds, y_labels)),
                area,
                &mut buf,
            );
            let corner = buf
                .content()
                .iter()
                .position(|c| c.symbol() == symbols::line::BOTTOM_LEFT)
                .map(|i| buf.pos_of(i))
                .expect("chart draws its axes");
            assert_eq!((corner.0 + 1, corner.1), (graph.x, graph.bottom()), "width {width}");
            assert_eq!(graph.right(), area.right() - 1);
            assert_eq!(graph.top(), area.top() + 1);
        }
    }

    #[test]
    fn gradient_colors_mean_values_not_heights() {
        use colorgrad::Gradient;
        let g = gradient(&[(50.0, "#00ff00"), (53.0, "#ff0000")]);
        assert_eq!(g.at(40.0).to_rgba8(), [0, 255, 0, 255]);
        assert_eq!(g.at(53.0).to_rgba8(), [255, 0, 0, 255]);
        assert_eq!(g.at(60.0).to_rgba8(), [255, 0, 0, 255]);
    }

    #[test]
    fn cap_steps_end_at_the_highest_core_frequency() {
        let steps = cap_steps(4800);
        assert_eq!(steps.first(), Some(&2000));
        assert_eq!(&steps[steps.len() - 3..], &[4400, 4600, 4800]);
        // top not on the 200 MHz grid
        assert_eq!(&cap_steps(4500)[12..], &[4400, 4500]);
    }

    #[test]
    fn cap_moves_from_any_current_value() {
        let steps = cap_steps(4800);
        assert_eq!(next_cap(&steps, Some(2000)), Some(2200));
        assert_eq!(next_cap(&steps, Some(2100)), Some(2200)); // set outside hw-tui
        assert_eq!(prev_cap(&steps, Some(2100)), Some(2000));
        assert_eq!(next_cap(&steps, Some(4600)), Some(4800));
        assert_eq!(next_cap(&steps, Some(4800)), None);
        assert_eq!(prev_cap(&steps, Some(2000)), None);
        // below the floor (set elsewhere): up goes back to the grid
        assert_eq!(next_cap(&steps, Some(1200)), Some(2000));
        assert_eq!(prev_cap(&steps, Some(1200)), None);
    }
}
