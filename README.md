# thermal-governor

Thermal management tooling for the ThinkPad X1 Carbon (Intel Core Ultra 7 155H). **Currently in a data-collection phase** — the daemon runs as a passive observer logging real-world thermal/load events, while manual tuning happens through an interactive companion (`hw-tui`). The goal is to feed that observation data back into a smarter auto-tuner.

## Problem

The X1 Carbon's firmware fan curve is essentially binary (off or near-max). With unrestricted frequency caps, the CPU boosts to 4.5 GHz, the package temperature shoots to 98 °C in seconds, the firmware hard-throttles to ~400 MHz, then the cycle restarts. The result is a **boost-crash oscillation** that delivers worse sustained throughput than a steady lower frequency — and a laptop that's noisy and uncomfortable to use.

The built-in GNOME power profiles set EPP (Energy Performance Preference) but never cap the maximum frequency, so they can't break the cycle.

## Project phases

The first version was an **active controller**: it adjusted `scaling_max_freq` in a feedback loop, with predictive bias on temperature rate, per-profile thermal tables, hysteresis, cooldowns, and a self-tuner that nudged the tables over time based on observed fan activity. It worked, but the auto-tuner was operating on too little signal — it couldn't tell a workload spike apart from background noise, so its adjustments felt arbitrary and non-deterministic.

**Current phase — observation.** The daemon stopped deciding and started recording. It runs as a passive observer at 1 Hz, captures everything that matters around interesting thermal/load events, and persists the user's manual settings (via `hw-tui`) across reboots. The dataset accumulating in `/var/lib/thermal-governor/events/` is the input I want for the **next** iteration of auto-tuning — one that has enough labelled context (real workloads, real thermal responses, real user choices) to converge somewhere useful instead of drifting.

So `hw-tui` is the control surface for now, and the daemon is both the safety net (settings stick across reboots) and the data recorder for what comes next.

## What it does now

Two binaries built from the same crate:

### `thermal-governor` (daemon)

- **Passive observer**: samples CPU temp, fan RPMs, scaling frequencies, EPP, load, throttle time, and RAPL energy at 1 Hz into a rolling 5-minute buffer.
- **Settings persistence**: on startup, reads `/var/lib/thermal-governor/settings.json` and restores the user's chosen `fan_level`, `freq_cap_mhz`, and `epp`. On shutdown, reverts to safe defaults (`fan=auto`, `cap=2000 MHz`).
- **Event log**: when something interesting happens — hardware throttle, temperature crossing 85/90/95 °C, rapid temperature rise, load transition, soft throttle — it dumps the rolling buffer as JSON to `/var/lib/thermal-governor/events/`. Useful post-mortem after a surprising thermal event.
- **No control loop (yet)**. The daemon does not nudge the frequency cap or fan level on its own — those are the user's choice via `hw-tui` (or any tool that writes the same sysfs paths). The recorded events are the dataset for re-introducing automation later.
- **Dynamic hwmon discovery**: locates the `thinkpad` hwmon node by name, so fan RPM reads stay correct across reboots even when the `/sys/class/hwmon/hwmonN` enumeration order changes.

### `hw-tui` (interactive)

A TUI control surface to set the frequency cap, fan level, and EPP by hand while watching their effect live (temperature trend, fan RPM, package power, per-core frequency distribution). Auto-escalates to sudo when launched without root, and preserves the current sysfs state when you quit — closing the TUI does not reset your tuning, only the daemon's clean shutdown does.

## Architecture

```
                ┌──────────────────────────┐
                │       hw-tui (manual)     │
                │   freq cap / fan / EPP    │
                └────────────┬─────────────┘
                             │ writes sysfs
                             ▼
                   ┌─────────────────────┐
   sysfs/procfs ◄──┤   kernel knobs       ├──► sysfs reads
                   │ scaling_max_freq,    │      (samples)
                   │ /proc/acpi/ibm/fan,  │
                   │ energy_performance_  │
                   │   preference         │
                   └─────────┬───────────┘
                             │
                             ▼
              ┌──────────────────────────────────┐
              │       thermal-governor            │
              │                                   │
              │  1 Hz observer loop               │
              │   ├── 5-min rolling sample buffer │
              │   ├── detect threshold events     │
              │   ├── dump buffer to events/ on   │
              │   │   throttle / temp cross / …   │
              │   └── restore settings.json @boot │
              └──────────────────────────────────┘
```

## Install

```bash
cargo build --release
sudo ./install.sh
```

The installer copies `target/release/thermal-governor` to `/usr/local/bin/`, writes the systemd unit, and starts the service.

State directory: `/var/lib/thermal-governor/`
- `settings.json` — user-chosen `fan_level`, `freq_cap_mhz`, `epp` (restored at boot)
- `events/<timestamp>-<event>.json` — buffer dump for each detected event

## Monitor

```bash
journalctl -u thermal-governor -f
```

Example output:

```
[10:14:25] [main] Initial: 56°C, fan=auto
[10:14:25] [fan] fan_control=1 enabled
[10:14:25] [obs] Restoring settings: { fan_level: "auto", freq_cap_mhz: 2000, epp: "power" }
[10:14:25] [obs] Observer loop started (1 Hz sampling, 300-sample buffer)
[10:18:42] [status] 53°C Δ-0.0°C/s load=3% fan=auto rpm=0/0 freq=400/1555/2000 cap=2000 epp=power rapl=5.9W
```

When an event triggers, the buffer is written to disk; nothing else changes.

## Events captured

| Event              | Trigger                                                        |
|--------------------|----------------------------------------------------------------|
| `throttle`         | Hardware throttle counter incremented                          |
| `temp-cross-85`    | CPU package temperature crossed 85 °C (rising)                 |
| `temp-cross-90`    | …crossed 90 °C                                                 |
| `temp-cross-95`    | …crossed 95 °C                                                 |
| `rapid-temp-rise`  | Temperature rate of change exceeded threshold                  |
| `load-transition-up` / `-down` | CPU load crossed activity threshold                |
| `soft-throttle`    | Frequency capped well below the configured ceiling for a while |

Each event file contains the 5 minutes leading up to the trigger — temperature, fan RPM, per-core frequencies, EPP, load, RAPL energy — as a JSON array. Useful raw material when you want to understand *why* the laptop just sounded angry.

## Requirements

- Linux with `intel_pstate` driver (active mode)
- `thinkpad_acpi` module with `fan_control=1` (the daemon enables it via modprobe if needed)
- Root (writes sysfs, enables fan control, reads `/proc/acpi/ibm/fan`)
- The `intel_pstate` and `thinkpad_acpi` sysfs paths used here are specific to recent Intel ThinkPads — adapt the constants in `src/main.rs` for other hardware

## License

MIT
