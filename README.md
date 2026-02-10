# thermal-governor

Dynamic auto-tuning thermal manager for Linux laptops with aggressive fan curves.

Built for the ThinkPad X1 Carbon (Intel Core Ultra 7 155H) but adaptable to any laptop where the firmware fan curve is binary (off or max) and thermal throttling is harsh.

## Problem

Many thin laptops have an aggressive thermal management strategy:

1. CPU boosts to max frequency (4.5 GHz)
2. Temperature spikes to 98°C in seconds
3. Firmware hard-throttles the CPU to ~400 MHz
4. Temperature drops, CPU boosts again
5. Repeat — creating a **boost-crash cycle** that gives worse sustained performance than a steady lower frequency

The built-in GNOME power profiles only set static EPP (Energy Performance Preference) values with no frequency capping, so they can't prevent this.

## Solution

`thermal-governor` replaces static CPU settings with a **dynamic feedback loop**:

- Reads CPU package temperature every 2 seconds
- Adjusts `scaling_max_freq` based on per-profile thermal tables
- Steps **down immediately** when temperature rises (multi-level jump), with **predictive bias** that uses temperature rate-of-change to trigger step-downs early
- Steps **up gradually** (+200 MHz per poll) with hysteresis and cooldown to prevent oscillation
- **Enforces monotonicity invariants** — frequency caps are always strictly decreasing with minimum 200 MHz spread between levels
- **Auto-tunes** its own parameters based on observed behavior (fan activity, temperature trends)
- **Persists learned parameters** across reboots

## Profiles

| Profile | EPP | Thermal Target | Strategy |
|---|---|---|---|
| **Power Saver** | `power` | Stay below ~58°C | Aggressively cap frequency to keep fans off |
| **Balanced** | `balance_power` | Stay below ~80°C | Moderate caps, accept some fan noise |
| **Performance** | `performance` | Stay below ~95°C | Maximum sustained frequency without hitting thermal throttle |

Profile switching is automatic — the daemon listens to GNOME's power-profiles-daemon via D-Bus and reacts instantly when you switch profiles in Settings.

## Test Results (ThinkPad X1, Intel Core Ultra 7 155H)

All tests run with 100% all-core load (16 threads) for 60 seconds:

| Profile | Sustained Freq | Max Temp | Avg Temp | Throughput | Result |
|---|---|---|---|---|---|
| **Performance** | 2.8–3.2 GHz | 97°C | 86°C | ~150 M/sec | No throttle crash |
| **Balanced** | 3.5 GHz | 75°C | 64°C | ~80 M/sec | Under 80°C |
| **Power Saver** (idle) | 1.9 GHz | 56°C | 55°C | — | 0 RPM fans |
| **Power Saver** (load) | 1.2 GHz | 64°C | 60°C | ~40 M/sec | Capped at floor |

### Before vs After (Performance mode, full load)

| | Before | After |
|---|---|---|
| Behavior | 4.5→0.4→4.5 GHz oscillation | 3.2 GHz sustained |
| Max temp | 101°C (hard throttle) | 97°C (no throttle) |
| Throughput | Unstable, crash cycles | Stable ~150 M/sec |
| Experience | Laptop nearly unusable | Smooth sustained load |

## Auto-Tuning

The governor learns from its own operation:

- **Every 2 minutes**: analyzes a rolling window of temperature/fan samples
- **Power Saver**: if fans stayed off under actual load (avg ≥ 48°C) → raises max_cap by 100 MHz up to a 3.5 GHz ceiling (finds the true fanless ceiling); if fans kicked on too much → lowers all caps
- **Performance**: if temperature never approached danger zone → raises caps; if it got too hot → aggressively lowers them
- **Balanced**: adjusts to stay in the sweet spot
- **Every 5 minutes**: persists learned parameters to `/var/lib/thermal-governor/tuned-params.json`
- Parameters survive reboots and improve over days of use

## Architecture

```
                     ┌────────────────────────┐
                     │     dbus-monitor        │
                     │  (profile change watch) │
                     └──────────┬─────────────┘
                                │ sends Profile via channel
                                ▼
┌──────────────────────────────────────────────────────┐
│                    Main Thread                        │
│  - Detects initial GNOME profile                     │
│  - Spawns/kills governor on profile switch            │
│  - Handles SIGTERM/SIGINT for clean shutdown          │
└──────────────────────┬───────────────────────────────┘
                       │ spawns
                       ▼
┌──────────────────────────────────────────────────────┐
│               Governor Thread                         │
│                                                       │
│  every 2s:                                            │
│    read temp (x86_pkg_temp) + fan RPM (thinkpad)      │
│    compute target_cap from ThermalTable               │
│    apply scaling_max_freq if changed                  │
│    record stats for auto-tuner                        │
│                                                       │
│  every 120s: run auto_tune() → adjust ThermalTable   │
│  every 300s: persist state to JSON                    │
└──────────────────────────────────────────────────────┘
```

## Installation

### Build

```bash
cargo build --release
```

### Install

```bash
sudo cp target/release/thermal-governor /usr/local/bin/
sudo mkdir -p /var/lib/thermal-governor
```

### Systemd Service

Create `/etc/systemd/system/thermal-governor.service`:

```ini
[Unit]
Description=Dynamic Auto-Tuning Thermal Governor
After=multi-user.target

[Service]
Type=simple
ExecStart=/usr/local/bin/thermal-governor
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
```

```bash
sudo systemctl daemon-reload
sudo systemctl enable --now thermal-governor.service
```

### Monitor

```bash
journalctl -u thermal-governor -f
```

Example output:

```
[19:10:02] [main] Initial: performance (58°C, fan 5769 rpm)
[19:10:02] [performance] Governor started: EPP=performance cap=4.5GHz thresh=75/85/92/95°C hyst=5°C
[19:10:24] [performance] 80°C fan:5769rpm ↓ 4.5→3.2 GHz
[19:10:32] [performance] 61°C fan:5777rpm ↑ 3.2→3.4 GHz
[19:10:36] [performance] 62°C fan:5774rpm ↑ 3.4→3.6 GHz
[19:10:40] [performance] 65°C fan:5763rpm ↑ 3.6→3.8 GHz
[19:10:44] [performance] 64°C fan:5771rpm ↑ 3.8→4.0 GHz
[19:10:49] [performance] 65°C fan:5769rpm ↑ 4.0→4.2 GHz
```

## Configuration

The default thermal tables are hardcoded for the ThinkPad X1 (Core Ultra 7 155H). To adapt for different hardware, modify the constants and defaults in `src/main.rs`:

- **Sensor paths**: `TEMP_SENSOR`, `FAN1_SENSOR`, `FAN2_SENSOR` — find yours with `ls /sys/class/hwmon/*/`
- **Thermal tables**: `ThermalTable::power_saver()`, `::balanced()`, `::performance()` — adjust thresholds and caps for your laptop's thermal characteristics
- **Timing**: `POLL_INTERVAL`, `TUNE_INTERVAL`, `PERSIST_INTERVAL`
- **Bounds**: `MIN_CAP`, `MAX_CAP`, `FREQ_STEP`, `MIN_SPREAD`

The auto-tuner will refine the tables from there, but good starting defaults help it converge faster.

### Finding Your Sensor Paths

```bash
# Temperature sensors
for z in /sys/class/thermal/thermal_zone*/; do
    echo "$(basename $z): $(cat ${z}type) = $(($(cat ${z}temp)/1000))°C"
done

# Fan sensors
for h in /sys/class/hwmon/hwmon*/; do
    name=$(cat ${h}name 2>/dev/null)
    fans=$(ls ${h}fan*_input 2>/dev/null)
    [ -n "$fans" ] && echo "$h ($name): $fans"
done
```

### Resetting Learned Parameters

```bash
sudo rm /var/lib/thermal-governor/tuned-params.json
sudo systemctl restart thermal-governor
```

## How It Works

### Step-Down (Immediate)

When temperature exceeds a threshold, the governor immediately jumps to the corresponding frequency cap. Higher thresholds trigger lower caps. This is checked from hottest to coolest, so the most severe cap always wins.

### Step-Up (Gradual)

When temperature drops, the governor ramps up **+200 MHz per poll** toward the next level, gated by hysteresis (default 5°C for Performance/Balanced, 2°C for Power Saver). After any step-down, a **cooldown period** (6 seconds) prevents immediate step-up. After each step-up, a **1-poll pause** (2 seconds) lets the thermal sensor stabilize before the next increase. This produces a smooth ramp that naturally settles at the thermally sustainable frequency.

### Predictive Thermal Bias

The governor tracks the rate of temperature change between polls. When temperature is rising fast, half the delta is added to the effective temperature for threshold checks. For example, if temp jumped +16°C in one poll, thresholds are effectively lowered by 8°C, triggering preemptive step-downs before actually hitting the thermal wall.

### Auto-Tuning

Every 2 minutes, the tuner analyzes collected samples:

- **Fan activity percentage**: how often fans were spinning (>100 RPM)
- **Max/average temperature**: thermal headroom assessment
- **Time at lowest cap**: how often the emergency floor was hit

Based on these metrics, it nudges frequency caps up or down by 100 MHz steps, clamped within safe bounds. After every adjustment, `enforce_invariants()` guarantees caps remain monotonically decreasing with at least 200 MHz spread between adjacent levels, and within per-profile ceilings (3.5 GHz for Power Saver, 4.5 GHz for others).

## Requirements

- Linux with `intel_pstate` driver (active mode)
- GNOME with `power-profiles-daemon` (for profile switching via D-Bus)
- `dbus-monitor` available in PATH
- `gdbus` available in PATH
- Root privileges (writes to sysfs)

## License

MIT
