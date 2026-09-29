# thermal-governor

Thermal tooling for the ThinkPad X1 Carbon Gen 12 (Intel Core Ultra 7 155H).

- **`thermal-governor`** is a daemon: it drives the fan with a **fan curve** (zero frequency drops with the least fan), keeps your platform profile, cap and EPP across reboots, and logs thermal events for post-mortem.
- **`hw-tui`** is the control surface: set the profile, cap, EPP and fan by hand, and watch the effect live.

## Problem

The X1 Carbon's firmware fan curve is essentially binary (off or near-max). With unrestricted frequency caps, the CPU boosts to 4.8 GHz, the package temperature shoots to 98 °C in seconds, the firmware hard-throttles to ~400 MHz, then the cycle restarts. The result is a **boost-crash oscillation** that delivers worse sustained throughput than a steady lower frequency — and a laptop that's noisy and uncomfortable to use.

A steady cap (2000 MHz) kills that cycle. Two things were still wrong:

- **Max frequency drops without any thermal throttle.** The firmware limits package power (PL1): 15 W in the `balanced` platform profile, 40 W in `performance`. Past PL1 for ~28 s, the CPU falls to 400 MHz. Even in `performance`, the EC cuts PL1 to 12 W when the board sensor SEN1 reaches 54 °C. The package temperature does not predict it (cuts seen from 68 °C to 88 °C package).
- **The EC fan control is on/off** (on at ~55 °C, off at ~52 °C): at cap 2000 it ran 81 % of the time for a CPU at 6 W.

The measurements are in `.claude/skills/thermal-governor/SKILL.md`.

## History

The first version was an **active controller** on the frequency cap, with a self-tuner on top. The tuner had too little signal and its choices felt arbitrary. The next phase turned the daemon into a passive observer, to collect data for a better auto-tuner. That auto-tuner is **abandoned**: tuning by hand with `hw-tui` works well.

What came back is much smaller: a **fan curve** with a few fixed numbers, built on measurements, not on learning.

## Fan curve

Every second, three linear demands between 0 and 1:

```
need_power = (package power, smoothed over 10 s − 9 W) / (23 W − 9 W)
need_temp  = (package temperature, smoothed over 8 s − 62 °C) / (74 °C − 62 °C)
need_sen1  = (SEN1 − 50 °C) / (53 °C − 50 °C)      up at once, down with a 60 s time constant
need       = max(need_power, need_temp, need_sen1)          each clamped to 0..1
```

- **Power acts first**: heat is coming before the temperature shows it (at 25 W the package goes from 70 to 77 °C in ~15 s). RAPL package power includes the iGPU, which video decode loads.
- **Temperature corrects**: full speed at 74 °C. It reacts in seconds, but it does not predict the PL1 cut.
- **SEN1 is the target**: the EC cuts PL1 when this board sensor reaches 54 °C, so the curve is at full speed at 53 °C. SEN1 is slow: it keeps the heat of past loads for minutes, so it raises the fan even when the package is cool. It reads in whole degrees, and its demand falls slowly, so a sensor between two degrees does not move the fan up and down.
- **During a PL1 cut, the power demand does not fall**: PL1 caps the power at 12 W, but that does not mean the machine is cooler. Lowering the fan then would keep SEN1 hot, and the cut going. A cut is a PL1 lower than the highest PL1 seen since the last profile change.
- **Smooth the inputs, not the demand**: the package temperature jumps by several °C in 1–2 s with short bursts of load. The 8 s smoothing keeps these jumps from starting the fan; PL1 cuts come from sustained heat. `need` then follows the inputs both ways: after a full load the fan stops in ~15 s. A load that comes back starts the fan again.
- `need × 9540 RPM` maps to the nearest of the 9 fan levels (0–7, `disengaged`), by their measured RPM, with a 150 RPM hysteresis.
- **Hard limits**, in curve and manual mode: package ≥ 80 °C or a board sensor (SEN) ≥ 70 °C → full speed at once, then the demand falls with a 30 s time constant (a sensor hovering at the limit does not flip the fan every second). The kernel powers the machine off at SEN 80 °C.
- **Safety net**: the daemon sends the level every second with the EC fan watchdog at 10 s. If the daemon dies or hangs, the EC takes the fan back. With no temperature reading, the fan goes back to the EC.

The numbers are constants at the top of `src/fan_curve.rs`.

### Fan modes

The mode lives in `/run/thermal-governor/fan-mode`, so **every boot starts on the curve**.

| Mode | Who drives the fan |
|------|--------------------|
| `curve` | the daemon (default) |
| `auto` | the EC's own control |
| `manual` | `hw-tui`, by hand, re-sent every second. The hard limits still apply (they switch back to the curve), and the EC watchdog stays on: if `hw-tui` dies, the EC takes the fan back in 10 s |

## `hw-tui`

A terminal UI with seven live charts. They show the last 2 minutes: PL1 limits a 28 s average, so a cut and its cause fit on the same screen.

- Bars, one series each: package temperature (85 and 95 °C dotted), SEN1, fan RPM, throttle, CPU usage. The color of a bar means a value, not a height: a color is the same value whatever the y range.
- Lines, several series each: power, and frequency avg/max with the cap as a dotted line. Frequency min is not shown (a core at rest keeps it near 400 MHz), but it stays in the CSV.

SEN1, the board sensor the EC watches for the PL1 cut, has its own chart: it moves by a degree or two where the package moves by tens. The chart always shows the cut (54 °C) as a dotted line, and covers 5 minutes: SEN1 reads in whole degrees and moves ~1 °C per minute, so 2 minutes show no trend. Its bars go from cyan to red where the fan curve goes from SEN1 ignored to full speed (50 to 53 °C), and SEN1 turns red in the title one degree under the cut.

The power chart also shows the package power averaged over the PL1 time window (tau, 28 s here), and PL1 itself as a dotted line. Both come from `intel-rapl-mmio:0`. PL1 limits this average, not the power itself. When the average reaches the line, its value turns red: max frequency will drop to ~400 MHz soon. The firmware clamps later than this average predicts (up to ~35 s later in the tests), so read it as an early warning.

| Key   | Action |
|-------|--------|
| `c`   | Fan: curve (the daemon drives it) |
| `a`   | Fan: auto (the EC drives it) |
| `↑` `↓` | Fan: manual, one level up/down from the current one (0–7, then `disengaged` = full speed, no regulation) |
| `o`   | Cycle platform profile: `low-power` → `balanced` → `performance` (sets PL1: 10 / 15 / 40 W) |
| `←` `→` | Frequency cap, 200 MHz steps from 2000 MHz up to the highest core frequency (4800 MHz = no cap) |
| `p`   | Cycle EPP: `power` → `balance_power` → `balance_performance` → `performance` → `default` |
| `j` `k` | Stress load (`stress-ng`, 0/1/2/4/8/16 workers) |
| `r`   | Record a CSV in the current directory |
| `q`   | Quit |

- The status bar shows the fan mode, the curve's level and `need`, and warns when the daemon is not running. The header shows the profile, in red when it is not `performance`.
- Shows the real hardware state: profile, cap and EPP are read back every second, so changes made elsewhere show up.
- Asks for sudo when launched without root. Enables fan control at start if needed (reloads `thinkpad_acpi` with `fan_control=1`).
- Quitting leaves profile, cap and EPP as they are. A manual fan level goes back to the curve: nobody watches it after. Stress workers are stopped.

## `thermal-governor` (daemon)

- **Fan curve**: see above.
- **Settings persistence**: when the platform profile, cap or EPP changes (from `hw-tui` or any tool writing the same sysfs files), it saves them to `/var/lib/thermal-governor/settings.json`. At startup, it restores them. With no saved file, it leaves the hardware as it is. The fan level is never saved: manual levels last one boot.
- **Clean shutdown**: on stop, the fan goes back to the EC. Profile, cap and EPP are not touched, so a restart does not undo your tuning.
- **Event log**: samples at 1 Hz into a 5-minute rolling buffer. On a notable event, it writes the buffer as a CSV to `/var/lib/thermal-governor/events/`. Only the newest 1000 files are kept.
- **Continuous log**: every sample (1 Hz, same columns as the event CSVs) goes to `/var/lib/thermal-governor/log/<YYYY-MM-DD>.csv`, one file per local day, about 10 MB a day. It is the raw data for fitting and replaying a thermal model. After a restart, the daemon appends to the file of the day. If the columns changed (a daemon update), it starts `<YYYY-MM-DD>T<HH-MM-SS>.csv` instead: one file never mixes two formats. The newest 30 days are kept.
- **Per-minute status** in the journal: SEN1 and SEN2 now; over the last minute, average RPM, % of time with the fan off, seconds of frequency drop, lowest PL1, highest SEN1.

### Events captured

| Event              | Trigger |
|--------------------|---------|
| `throttle`         | Hardware throttle counter increased |
| `temp-cross-85`    | Package temperature crossed 85 °C, rising (2 °C hysteresis) |
| `temp-cross-90`    | …crossed 90 °C |
| `temp-cross-95`    | …crossed 95 °C |
| `rapid-temp-rise`  | Temperature rising faster than 2 °C/s for 3 samples |
| `pl1-cut`          | A PL1 cut starts: PL1 is lower than what the profile set (the EC lowered it). A frequency drop is coming |

Each event type has a 60 s cooldown. File name: `<unix-timestamp>-<event>.csv`. Columns:

```
timestamp,temp_c,temp_rate,cpu_load,fan1_rpm,fan2_rpm,fan_level,freq_min,freq_avg,freq_max,freq_cap_mhz,epp,throttle_rate,rapl_power_w,platform_profile,pl1_w,fan_mode,fan_need,sen1_c,sen2_c
```

## Architecture

```
   ┌────────────────────────────┐        /run/thermal-governor/
   │          hw-tui            │ ─────► fan-mode (curve/auto/manual)
   │ profile / cap / EPP / fan  │ ◄───── fan-status.json
   └─────────────┬──────────────┘              ▲   │
                 │ writes sysfs                │   │ reads
                 ▼                             │   ▼
      ┌──────────────────────┐        ┌──────────────────────────────┐
      │    kernel knobs      │ ◄───── │      thermal-governor        │
      │ platform_profile,    │ writes │  1 Hz loop                   │
      │ scaling_max_freq,    │ ─────► │  ├── fan curve → fan level   │
      │ /proc/acpi/ibm/fan,  │ reads  │  ├── save / restore settings │
      │ energy_performance_  │        │  ├── dump buffer to events/  │
      │   preference         │        │  └── every sample to log/    │
      └──────────────────────┘        └──────────────────────────────┘
```

Both binaries share the hardware access code in `src/hw.rs` and the curve in `src/fan_curve.rs`.

## Install

```bash
cargo build --release
sudo ./install.sh          # daemon: binary, systemd unit, fan_control=1 at boot, starts the service
./target/release/hw-tui    # control surface (asks for sudo)
```

`install.sh` writes `/etc/modprobe.d/thinkpad_acpi-fan-control.conf` and updates the initramfs (the module loads from it), so `thinkpad_acpi` loads with fan control at boot. Until the next reboot, the daemon reloads the module itself if fan control is off.

State directory: `/var/lib/thermal-governor/`
- `settings.json` — saved `platform_profile`, `freq_cap_mhz` and `epp`
- `events/<timestamp>-<event>.csv` — buffer dump for each event
- `log/<YYYY-MM-DD>.csv` — every sample, one file per day (30 days kept)

`sudo ./uninstall.sh` removes the service and the modprobe option, and resets profile, cap, EPP and fan to their defaults. It keeps the state directory.

## Monitor

```bash
journalctl -u thermal-governor -f
```

Example output:

```
[08:58:09] [obs] Restoring settings: Settings { freq_cap_mhz: 2000, epp: "performance", platform_profile: Some("performance") }
[08:58:09] [obs] Observer loop started (1Hz sampling, 300-sample buffer)
[08:58:09] [fan] Fan control: Curve
[08:58:09] [fan] Level 0 (need 0.00: power 0.00, temp 0.00, sen1 0.00)
[09:02:14] [fan] Level 2 (need 0.33: power 0.33, temp 0.08, sen1 0.00)
[09:03:09] [status] 63°C Δ+0.1°C/s load=40% fan=2 (curve, need 0.31) rpm=4838/4120 … pl1=40W rapl=13.2W sen1=49°C sen2=53°C | last min: rpm_avg=3950 fan_off=20% drop_s=0 pl1_min=40W sen1_max=49°C
```

## Hardware characterization

`hw-characterize.sh` sweeps power profiles × frequency caps × stress levels and writes a CSV. `hw-plot.py` turns it into charts. It needs `stress-ng` and `power-profiles-daemon`. `hw-dptf-policy.py` decodes the firmware's thermal policy (DPTF data vault): power limits per mode and the sensor thresholds behind them. The findings behind this project's design are in `.claude/skills/thermal-governor/SKILL.md`.

## Requirements

- Linux with the `intel_pstate` driver (active mode)
- `thinkpad_acpi` module; fan control needs `fan_control=1` (`install.sh` sets it at boot)
- Root (sysfs writes, fan control, RAPL energy counter)
- `stress-ng` for the stress keys in `hw-tui`
- The sysfs paths and the curve numbers are specific to this machine — see `src/hw.rs` and `src/fan_curve.rs` to adapt them

## License

MIT
