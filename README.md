# thermal-governor

Thermal tooling for the ThinkPad X1 Carbon Gen 12 (Intel Core Ultra 7 155H).

- **`hw-tui`** is the control surface: set the frequency cap, fan level and EPP by hand, and watch the effect live.
- **`thermal-governor`** is a small daemon: it keeps your cap and EPP across reboots, and logs thermal events for post-mortem.

## Problem

The X1 Carbon's firmware fan curve is essentially binary (off or near-max). With unrestricted frequency caps, the CPU boosts to 4.8 GHz, the package temperature shoots to 98 °C in seconds, the firmware hard-throttles to ~400 MHz, then the cycle restarts. The result is a **boost-crash oscillation** that delivers worse sustained throughput than a steady lower frequency — and a laptop that's noisy and uncomfortable to use.

The built-in GNOME power profiles set EPP (Energy Performance Preference) but never cap the maximum frequency, so they can't break the cycle.

## History

The first version was an **active controller**: it adjusted `scaling_max_freq` in a feedback loop, with a self-tuner on top. The tuner had too little signal and its choices felt arbitrary. The next phase turned the daemon into a passive observer, to collect data for a better auto-tuner.

That auto-tuner is **abandoned**. Watching the hardware live in `hw-tui` was enough to build a mental model of the machine, and tuning by hand per situation works well. The daemon stays, for what is still useful: settings that survive a reboot, and an event log.

## `hw-tui`

A terminal UI with six live charts (temperature, fan RPM, power, throttle, CPU usage, frequency min/avg/max with the cap as a dotted line).

| Key   | Action |
|-------|--------|
| `↑` `↓` | Fan level (0–7, then `disengaged` = full speed, no regulation) |
| `a`   | Toggle fan between auto (EC) and manual |
| `←` `→` | Frequency cap, 200 MHz steps from 2000 MHz up to the highest core frequency (4800 MHz = no cap) |
| `p`   | Cycle EPP: `power` → `balance_power` → `balance_performance` → `performance` → `default` |
| `j` `k` | Stress load (`stress-ng`, 0/1/2/4/8/16 workers) |
| `r`   | Record a CSV in the current directory |
| `q`   | Quit |

- Asks for sudo when launched without root.
- Enables fan control at start if needed (reloads `thinkpad_acpi` with `fan_control=1`).
- Shows the real hardware state: cap and EPP are read back from sysfs every second, so changes made elsewhere show up.
- Quitting does not reset anything. Stress workers are stopped.

## `thermal-governor` (daemon)

- **Settings persistence**: when the cap or EPP changes (from `hw-tui` or any tool writing the same sysfs files), it saves them to `/var/lib/thermal-governor/settings.json`. At startup, it restores them. With no saved file, it leaves the hardware as it is.
- **Fan always starts on auto**: the fan level is never saved or restored. A manual level restored at boot, with nobody watching, could leave the fan off under load. Manual levels last one session.
- **Clean shutdown**: on stop, the fan goes back to auto. Cap and EPP are not touched, so a restart does not undo your tuning.
- **Event log**: samples at 1 Hz into a 5-minute rolling buffer. On a notable event, it writes the buffer as a CSV to `/var/lib/thermal-governor/events/`. Only the newest 1000 files are kept.
- No control loop: it never changes the cap or the fan on its own.

### Events captured

| Event              | Trigger |
|--------------------|---------|
| `throttle`         | Hardware throttle counter increased |
| `temp-cross-85`    | Package temperature crossed 85 °C, rising (2 °C hysteresis) |
| `temp-cross-90`    | …crossed 90 °C |
| `temp-cross-95`    | …crossed 95 °C |
| `rapid-temp-rise`  | Temperature rising faster than 2 °C/s for 3 samples |

Each event type has a 60 s cooldown. File name: `<unix-timestamp>-<event>.csv`. Columns:

```
timestamp,temp_c,temp_rate,cpu_load,fan1_rpm,fan2_rpm,fan_level,freq_min,freq_avg,freq_max,freq_cap_mhz,epp,throttle_rate,rapl_power_w
```

## Architecture

```
        ┌──────────────────────────┐
        │     hw-tui (manual)      │
        │   freq cap / fan / EPP   │
        └────────────┬─────────────┘
                     │ writes sysfs
                     ▼
          ┌──────────────────────┐
          │    kernel knobs      │
          │ scaling_max_freq,    │
          │ /proc/acpi/ibm/fan,  │
          │ energy_performance_  │
          │   preference         │
          └──────────┬───────────┘
                     │ reads (1 Hz)
                     ▼
     ┌──────────────────────────────────┐
     │        thermal-governor          │
     │  ├── save cap + EPP on change    │
     │  ├── restore them at startup     │
     │  └── dump 5-min buffer to        │
     │      events/ on throttle / temp  │
     └──────────────────────────────────┘
```

Both binaries share the hardware access code in `src/hw.rs`.

## Install

```bash
cargo build --release
sudo ./install.sh          # daemon: binary, systemd unit, starts the service
./target/release/hw-tui    # control surface (asks for sudo)
```

State directory: `/var/lib/thermal-governor/`
- `settings.json` — saved `freq_cap_mhz` and `epp`
- `events/<timestamp>-<event>.csv` — buffer dump for each event

`sudo ./uninstall.sh` removes the service and resets cap, EPP and fan to their defaults. It keeps the state directory.

## Monitor

```bash
journalctl -u thermal-governor -f
```

Example output:

```
[08:58:09] [main] Initial: 51°C, fan=auto
[08:58:09] [obs] Restoring settings: Settings { freq_cap_mhz: 2000, epp: "performance" }
[08:58:09] [obs] Observer loop started (1Hz sampling, 300-sample buffer)
[08:59:09] [status] 49°C Δ-0.2°C/s load=5% fan=auto rpm=3841/0 freq=400/919/2000 cap=2000 epp=performance rapl=6.3W
[09:00:39] [settings] Change detected: cap=2200 epp=performance
```

## Hardware characterization

`hw-characterize.sh` sweeps power profiles × frequency caps × stress levels and writes a CSV. `hw-plot.py` turns it into charts. It needs `stress-ng` and `power-profiles-daemon`. The findings behind this project's design are in `.claude/skills/thermal-governor/SKILL.md`.

## Requirements

- Linux with the `intel_pstate` driver (active mode)
- `thinkpad_acpi` module; fan control needs `fan_control=1` (`hw-tui` enables it)
- Root (sysfs writes, fan control, RAPL energy counter)
- `stress-ng` for the stress keys in `hw-tui`
- The sysfs paths are specific to recent Intel ThinkPads — see `src/hw.rs` to adapt them

## License

MIT
