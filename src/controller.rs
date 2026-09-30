//! Fan + power controller, run in shadow mode: the daemon computes and logs
//! what it would do, and writes nothing (no fan level, no power limit).
//!
//! The goal is to keep SEN1 under the EC's 54 °C PL1 cut with two levers: the
//! fan, which costs noise, and a PL2 package power limit, which costs speed
//! but gracefully (the MSR limit holds to the tenth of a watt, with no drop to
//! 400 MHz). The model, fitted on the 1 Hz daily logs:
//!
//! ```text
//! dS/dt = (amb + G(rpm) · P − S) / TAU_S       G(rpm) = 1 / (H0 + H1 · rpm / 1000)
//! ```
//!
//! S is SEN1, P the package power, rpm the mean of the two fans. `amb`, the
//! effective ambient, is not measured (26 to 38 °C seen, moving within
//! minutes): an observer estimates it from SEN1. Each tick:
//!
//! - **observer**: run the model with the estimated ambient, then correct the
//!   state and the ambient with the SEN1 reading;
//! - **fan**: the rpm that keeps SEN1 under `FAN_TARGET_C` in `FAN_HORIZON_S`
//!   at the current power;
//! - **power**: the highest PL2 that keeps SEN1 under `POWER_LIMIT_C` in
//!   `POWER_HORIZON_S` at full fan, never under `PL2_FLOOR_W`.
//!
//! The tuning comes from a closed-loop replay of 2026-09-28..30 (SKILL.md,
//! "The controller"), with the cost "one drop = 10 min of full fan".

// =============================================================================
// Model (fitted on the windows around the 34 bursts of 2026-09-28..30)
// =============================================================================

const H0: f64 = 0.3956;
const H1: f64 = 0.0600;
const TAU_S: f64 = 209.2;

/// Fan levels with the mean RPM of the two fans (the model's rpm). Level 1–7
/// from the idle table in SKILL.md; `disengaged` is the median of the daily
/// logs, where the curve runs it.
const LEVELS: [(&str, f64); 9] = [
    ("0", 0.0),
    ("1", 3732.0),
    ("2", 4481.0),
    ("3", 5018.0),
    ("4", 5692.0),
    ("5", 6181.0),
    ("6", 6593.0),
    ("7", 7273.0),
    ("disengaged", 9800.0),
];
const FULL_RPM: f64 = LEVELS[LEVELS.len() - 1].1;
/// Margin around the midpoint between two levels, against flapping
const HYSTERESIS_RPM: f64 = 150.0;

// =============================================================================
// Tuning: the "safe" law of the replay, the cheapest with zero drops on every
// plant tried (G ±10 %, TAU ∓15 %, and the ambient 2 °C hotter than on
// 2026-09-29, the day of the 7 cuts)
// =============================================================================

/// Observer (a Kalman filter on SEN1 and the ambient). Noise variances: the
/// model error per second, the ambient drift per second (a random walk), and
/// the reading (whole degrees, errors correlated in time: more than 1/12).
/// Tuned on the real readings of the logs: SEN1 60 s ahead within 0.52 °C rms.
const OBS_MODEL_VAR: f64 = 3e-3;
const OBS_AMBIENT_VAR: f64 = 1e-3;
const OBS_READING_VAR: f64 = 0.25;
/// The ambient at start is a guess (the one that explains the reading at
/// equilibrium): 5 °C standard deviation, so the first minutes trust SEN1
const OBS_AMBIENT_START_VAR: f64 = 25.0;
/// Fan: keep SEN1 under this in this many seconds. The rpm it asks for falls
/// with FAN_FALL_TAU_S.
const FAN_TARGET_C: f64 = 52.5;
const FAN_HORIZON_S: f64 = 60.0;
const FAN_FALL_TAU_S: f64 = 30.0;
/// The fan law reads the package power smoothed over this, against 1 s spikes
const POWER_SMOOTH_S: f64 = 2.0;
/// Power: keep SEN1 under this in this many seconds, at full fan
const POWER_LIMIT_C: f64 = 53.0;
const POWER_HORIZON_S: f64 = 40.0;
/// Our PL2 never goes under this: ~66 % of the throughput of a 26 W burst,
/// ~1500 MHz. Lower, the limit would be a drop by itself.
pub const PL2_FLOOR_W: f64 = 18.0;
/// The firmware PL2: no limit of our own
pub const PL2_MAX_W: f64 = 64.0;
/// After a hard limit (fan_curve::guard), full fan, falling with this
const GUARD_FALL_TAU_S: f64 = 30.0;
/// The logged prediction, to check the observer against the SEN1 read later
pub const PREDICT_S: f64 = 30.0;
/// A longer gap (suspend, a stuck read) restarts the observer
const MAX_DT_S: f64 = 10.0;

/// What the controller reads on one tick
pub struct Inputs {
    /// None when the sensor is missing or unreadable
    pub sen1_c: Option<f64>,
    /// Package power over the last tick; None when RAPL is unreadable
    pub power_w: Option<f64>,
    /// Mean of the two fans
    pub rpm: f64,
    /// A hard limit (package or board sensor) asks for full speed
    pub guard: bool,
}

#[derive(Debug, Clone)]
pub struct Decision {
    /// The observer's SEN1 (the reading is whole degrees)
    pub sen1_c: f64,
    pub ambient_c: f64,
    /// SEN1 in PREDICT_S seconds at the current power and fan
    pub sen1_pred_c: f64,
    /// The fan rpm the controller asks for, and the level it maps to
    pub fan_rpm: f64,
    pub fan_level: &'static str,
    /// The PL2 it would write, PL2_FLOOR_W..=PL2_MAX_W
    pub pl2_w: f64,
}

/// SEN1 and the ambient, with their covariance
struct Observer {
    sen1: f64,
    ambient: f64,
    /// [[var sen1, cov], [cov, var ambient]]
    p: [[f64; 2]; 2],
}

impl Observer {
    fn start(sen1: f64, gp: f64) -> Self {
        Self {
            sen1,
            ambient: sen1 - gp,
            p: [[OBS_READING_VAR, 0.0], [0.0, OBS_AMBIENT_START_VAR]],
        }
    }

    /// The model over `dt` with `gp` = G(rpm) · P, then the reading
    fn step(&mut self, dt: f64, gp: f64, reading: f64) {
        // Predict: sen1' = (1 − a) sen1 + a (ambient + gp); the ambient stays
        let a = decay(dt, TAU_S);
        self.sen1 += a * (self.ambient + gp - self.sen1);
        let [[p00, p01], [_, p11]] = self.p;
        let f = 1.0 - a;
        let p00 = f * f * p00 + 2.0 * f * a * p01 + a * a * p11 + OBS_MODEL_VAR * dt;
        let p01 = f * p01 + a * p11;
        let p11 = p11 + OBS_AMBIENT_VAR * dt;
        // Correct with the reading
        let s = p00 + OBS_READING_VAR;
        let (k0, k1) = (p00 / s, p01 / s);
        let error = reading - self.sen1;
        self.sen1 += k0 * error;
        self.ambient += k1 * error;
        self.p = [
            [(1.0 - k0) * p00, (1.0 - k0) * p01],
            [(1.0 - k0) * p01, p11 - k1 * p01],
        ];
    }
}

struct State {
    obs: Observer,
    power_avg: f64,
    fan_rpm: f64,
    guard_need: f64,
    level: usize,
}

#[derive(Default)]
pub struct Controller {
    state: Option<State>,
}

impl Controller {
    pub fn new() -> Self {
        Self::default()
    }

    /// One step. `dt`: seconds since the last call. None when SEN1 or the
    /// power is unreadable; the state is kept for the next tick.
    pub fn update(&mut self, dt: f64, inputs: &Inputs) -> Option<Decision> {
        let (sen1, power) = (inputs.sen1_c?, inputs.power_w?);
        let gp = gain(inputs.rpm) * power;
        if dt > MAX_DT_S {
            self.state = None;
        }
        let st = match &mut self.state {
            Some(st) => {
                st.obs.step(dt, gp, sen1);
                st.power_avg += decay(dt, POWER_SMOOTH_S) * (power - st.power_avg);
                st
            }
            None => self.state.insert(State {
                obs: Observer::start(sen1, gp),
                power_avg: power,
                fan_rpm: 0.0,
                guard_need: 0.0,
                level: 0,
            }),
        };
        let (s, amb) = (st.obs.sen1, st.obs.ambient);

        // Fan: the gain G that brings SEN1 to FAN_TARGET_C in FAN_HORIZON_S
        // at the current power, then the rpm of that gain
        let sen1_eq = target_equilibrium(s, FAN_TARGET_C, FAN_HORIZON_S);
        let mut need_rpm = if sen1_eq <= amb {
            FULL_RPM
        } else {
            ((st.power_avg / (sen1_eq - amb) - H0) * 1000.0 / H1).clamp(0.0, FULL_RPM)
        };
        if inputs.guard {
            st.guard_need = 1.0;
        } else {
            st.guard_need *= (-dt / GUARD_FALL_TAU_S).exp();
        }
        need_rpm = need_rpm.max(st.guard_need * FULL_RPM);
        st.fan_rpm = need_rpm.max(st.fan_rpm * (-dt / FAN_FALL_TAU_S).exp());
        st.level = pick_level(st.level, st.fan_rpm);

        // Power: the P whose equilibrium at full fan brings SEN1 to
        // POWER_LIMIT_C in POWER_HORIZON_S
        let power_eq = target_equilibrium(s, POWER_LIMIT_C, POWER_HORIZON_S);
        let pl2_w = ((power_eq - amb) / gain(FULL_RPM)).clamp(PL2_FLOOR_W, PL2_MAX_W);

        let e = (-PREDICT_S / TAU_S).exp();
        Some(Decision {
            sen1_c: s,
            ambient_c: amb,
            sen1_pred_c: s * e + (amb + gp) * (1.0 - e),
            fan_rpm: st.fan_rpm,
            fan_level: LEVELS[st.level].0,
            pl2_w,
        })
    }
}

/// °C of SEN1 at equilibrium per W of package power
fn gain(rpm: f64) -> f64 {
    1.0 / (H0 + H1 * rpm / 1000.0)
}

/// Share of the gap to the equilibrium closed in `dt` by a time constant `tau`
fn decay(dt: f64, tau: f64) -> f64 {
    1.0 - (-dt / tau).exp()
}

/// The equilibrium that brings SEN1 from `now` to `target` in `horizon` s
fn target_equilibrium(now: f64, target: f64, horizon: f64) -> f64 {
    let e = (-horizon / TAU_S).exp();
    (target - now * e) / (1.0 - e)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The model as a plant, read in whole degrees like the real sensor
    struct Plant {
        sen1: f64,
        ambient: f64,
    }

    impl Plant {
        fn step(&mut self, power: f64, rpm: f64) {
            self.sen1 += decay(1.0, TAU_S) * (self.ambient + gain(rpm) * power - self.sen1);
        }

        fn reading(&self) -> f64 {
            self.sen1.round()
        }
    }

    fn rpm_of(level: &str) -> f64 {
        LEVELS.iter().find(|l| l.0 == level).unwrap().1
    }

    #[test]
    fn cool_idle_machine_needs_no_fan_and_no_limit() {
        let mut c = Controller::new();
        let mut plant = Plant {
            sen1: 42.0,
            ambient: 28.0,
        };
        let mut rpm = 0.0;
        for _ in 0..900 {
            plant.step(6.0, rpm);
            let d = c
                .update(
                    1.0,
                    &Inputs {
                        sen1_c: Some(plant.reading()),
                        power_w: Some(6.0),
                        rpm,
                        guard: false,
                    },
                )
                .unwrap();
            rpm = rpm_of(d.fan_level);
            assert_eq!(d.pl2_w, PL2_MAX_W);
        }
        assert_eq!(rpm, 0.0);
    }

    #[test]
    fn observer_finds_the_ambient_from_whole_degree_readings() {
        let mut c = Controller::new();
        // Starts away from equilibrium: the first guess of the ambient is wrong
        let mut plant = Plant {
            sen1: 44.0,
            ambient: 37.0,
        };
        let mut d = None;
        for _ in 0..1800 {
            plant.step(10.0, 3732.0);
            d = c.update(
                1.0,
                &Inputs {
                    sen1_c: Some(plant.reading()),
                    power_w: Some(10.0),
                    rpm: 3732.0,
                    guard: false,
                },
            );
        }
        let d = d.unwrap();
        assert!(
            (d.ambient_c - 37.0).abs() < 1.0,
            "ambient {:.2}",
            d.ambient_c
        );
        assert!(
            (d.sen1_c - plant.sen1).abs() < 0.6,
            "sen1 {:.2} vs {:.2}",
            d.sen1_c,
            plant.sen1
        );
    }

    /// The 2026-09-29 afternoon: ambient 37 °C, light load, then 26 W for
    /// `burst_s`. Closed loop: the controller sees the power of the last tick,
    /// the fan spins up in ~10 s like the real one. Returns (the plant's SEN1
    /// max, the lowest PL2, the highest fan rpm, the last PL2).
    fn hot_burst(burst_s: usize) -> (f64, f64, f64, f64) {
        let mut c = Controller::new();
        let mut plant = Plant {
            sen1: 50.0,
            ambient: 37.0,
        };
        let (mut rpm, mut power) = (0.0, 9.0);
        let (mut sen1_max, mut min_pl2, mut max_rpm, mut last_pl2) =
            (0.0_f64, PL2_MAX_W, 0.0_f64, PL2_MAX_W);
        for t in 0..600 + burst_s {
            let demand: f64 = if t < 600 { 9.0 } else { 26.0 };
            let d = c
                .update(
                    1.0,
                    &Inputs {
                        sen1_c: Some(plant.reading()),
                        power_w: Some(power),
                        rpm,
                        guard: false,
                    },
                )
                .unwrap();
            let target = rpm_of(d.fan_level);
            rpm += if target > rpm {
                decay(1.0, 5.0) * (target - rpm)
            } else {
                target - rpm
            };
            power = demand.min(d.pl2_w);
            plant.step(power, rpm);
            assert!((PL2_FLOOR_W..=PL2_MAX_W).contains(&d.pl2_w));
            sen1_max = sen1_max.max(plant.sen1);
            min_pl2 = min_pl2.min(d.pl2_w);
            max_rpm = max_rpm.max(rpm);
            last_pl2 = d.pl2_w;
        }
        (sen1_max, min_pl2, max_rpm, last_pl2)
    }

    #[test]
    fn hot_burst_is_held_under_the_cut_by_fan_then_power() {
        // 90 s: longer than any burst of the logs (83 s)
        let (sen1_max, min_pl2, max_rpm, _) = hot_burst(90);
        assert!(
            sen1_max < 53.5,
            "the reading would reach 54: SEN1 {sen1_max:.2}"
        );
        assert!(max_rpm > 9000.0, "fan only at {max_rpm:.0} rpm");
        assert!(min_pl2 < 26.0, "the power limit never acted");
    }

    #[test]
    fn sustained_load_in_a_hot_room_stops_at_the_floor() {
        // At 37 °C and full fan, SEN1 holds 54 °C only under ~17 W: the limit
        // goes down to the floor and no lower, the EC cut will come
        let (_, min_pl2, _, last_pl2) = hot_burst(900);
        assert_eq!(min_pl2, PL2_FLOOR_W);
        assert_eq!(last_pl2, PL2_FLOOR_W);
    }

    #[test]
    fn missing_input_gives_no_decision_and_keeps_the_state() {
        let mut c = Controller::new();
        let ok = Inputs {
            sen1_c: Some(50.0),
            power_w: Some(10.0),
            rpm: 0.0,
            guard: false,
        };
        let before = c.update(1.0, &ok).unwrap().ambient_c;
        assert!(c.update(1.0, &Inputs { sen1_c: None, ..ok }).is_none());
        assert!(c
            .update(
                1.0,
                &Inputs {
                    power_w: None,
                    sen1_c: Some(50.0),
                    rpm: 0.0,
                    guard: false
                }
            )
            .is_none());
        let after = c
            .update(
                1.0,
                &Inputs {
                    sen1_c: Some(50.0),
                    power_w: Some(10.0),
                    rpm: 0.0,
                    guard: false,
                },
            )
            .unwrap();
        assert!((after.ambient_c - before).abs() < 0.1);
    }

    #[test]
    fn long_gap_restarts_the_observer() {
        let mut c = Controller::new();
        let inputs = |s: f64| Inputs {
            sen1_c: Some(s),
            power_w: Some(6.0),
            rpm: 0.0,
            guard: false,
        };
        c.update(1.0, &inputs(50.0));
        // After a suspend the board is cold: start again from the reading
        let d = c.update(3600.0, &inputs(35.0)).unwrap();
        assert_eq!(d.sen1_c, 35.0);
    }

    #[test]
    fn hard_limit_asks_for_full_speed_then_falls() {
        let mut c = Controller::new();
        let inputs = |guard| Inputs {
            sen1_c: Some(40.0),
            power_w: Some(6.0),
            rpm: 0.0,
            guard,
        };
        assert_eq!(
            c.update(1.0, &inputs(true)).unwrap().fan_level,
            "disengaged"
        );
        let mut level = "";
        for _ in 0..300 {
            level = c.update(1.0, &inputs(false)).unwrap().fan_level;
        }
        assert_eq!(level, "0");
    }
}
