---
name: thermal-governor
description: Hardware context for working on this repo — the ThinkPad X1 Carbon Gen 12 thermal behaviour the daemon observes, and the characterization measurements the design rests on.
user-invocable: false
---

# thermal-governor — hardware context

The README covers what the code does and how to install it. This file holds what it
does *not*: the measured behaviour of the machine, which is why the design is what it
is. Read it before changing thresholds, sampling, or anything touching sysfs.

## The machine

- **CPU**: Intel Core Ultra 7 155H — 16 cores (6P + 8E + 2LP)
- **Max frequency differs per core** (`cpuinfo_max_freq`): cpu1/cpu2 reach 4800 MHz, the
  other P-cores 4500, E-cores 3800, LP-E cores 2500. The kernel clamps each core's
  `scaling_max_freq` to its own max, so cpu0 (a 4500 core) cannot tell you the cap: read
  the highest value across cores. "No cap" means 4800.
- **Package temp**: `/sys/class/thermal/thermal_zone*/` where `type` reads `x86_pkg_temp`.
  Resolve it by `type`, never by a hardcoded index — the numbering is enumeration order
  and moves across boots, exactly like the hwmon node the daemon already discovers by name.
- **Fan curve (EC, auto)**: effectively on/off. On around 55 °C, off around 52 °C, so it
  cycles. At cap 2000 it ran 81 % of the time at ~3600 RPM for a CPU at 52–57 °C and 6 W.
- **Hardware throttle**: hard limit at 98 °C.
- **SEN1/2/3/5** (thermal zones, board sensors): trips 65 passive / 75 hot / 80 critical.
  The zones are enabled in Linux with no cooling device: passive does nothing, but
  **critical at 80 °C makes the kernel power the machine off**.
- **RAPL**: the MSR interface (`intel-rapl:0`) shows PL1 = PL2 = 64 W, but it is **not**
  the limit that applies. The MMIO interface (`intel-rapl-mmio:0`) holds a much lower PL1
  set by the firmware, and the lower of the two wins. See "Power limits" below.

## What the characterization runs found

These came out of `hw-characterize.sh`, and they are the reason the daemon caps
frequency rather than playing with EPP:

- **EPP changes throughput, not the ceiling.** `EPP=performance` is ~30 % faster than
  `EPP=power` (174 vs 121 M/sec on the benchmark).
- **Every EPP level reaches the same 96 °C ceiling under sustained full load.** So EPP
  is a performance preference, not a thermal control. **`scaling_max_freq` is the knob
  that actually bounds temperature.**
- The failure mode this project exists to kill is the boost-crash oscillation described
  in the README: unrestricted boost → 98 °C in seconds → firmware throttle to ~400 MHz →
  repeat. Sustained throughput at a steady cap beats it.

## Power limits: why max frequency drops without a throttle (tests of 2026-09-25)

Tests: cap 2000, EPP `performance`, `stress-ng --cpu N`, fixed fan levels, 1 Hz logs plus
`MSR_CORE_PERF_LIMIT_REASONS` (0x64F) read at 10 Hz. Raw CSVs and an ACPI dump are in
`/var/lib/thermal-governor/tests/`.

- **`platform_profile` sets PL1** (MMIO, tau 28 s): `low-power` 10 W, `balanced` 15 W,
  `performance` 40 W. PL2 stays 64 W. In `balanced`, any load above 15 W is cut back to
  15 W (then 10 W) after ~28 s of average: frequency falls to 400 MHz, the limit-reason
  bit is 10 (PL1), and the thermal throttle counters do not move. This was the user's
  "max freq drops without throttle" in video meetings. power-profiles-daemon is not
  running, so the profile stays `balanced` unless something sets it.
- **Even in `performance`, the EC cuts PL1 from 40 W to 12 W when the machine is hot.**
  Seen at 77–87 °C package. The exact trigger is inside the EC firmware (not in the ACPI
  tables: their PSVT only limits the charger, on SEN3) and depends on thermal history.
  Neither the package temperature nor the visible SEN sensors separate cut from no-cut
  runs. **No cut was ever seen below 75 °C package.**
- **A cut costs a lot**: PL1 stays at 12 W until the package is back to ~58–60 °C
  (~50 s with the fan at full speed), then returns to 40 W on its own.
- **Fan vs load at cap 2000** (`performance` profile):

  | Load | Fan | Result |
  |---|---|---|
  | 2 workers, 9 W | 0 | 66 °C after 400 s, slow rise, no drop |
  | 4 workers, 13–14 W | 0 | 77–80 °C after ~9 min, then PL1 cut |
  | 16 workers, 24–26 W | 0 | +0.5 °C/s, 90 °C in 70 s |
  | 16 workers | 1 (~3970 RPM) | 89 °C at 170 s, then PL1 cut |
  | 16 workers | 2 (~4830 RPM) | 84 °C at 130 s, still rising |
  | 16 workers | 3 (~5270 RPM) | 84 °C, still rising |
  | 16 workers | 7 (~7540 RPM) | stable 77–80 °C; no cut in 400 s from cold, cut after a hot history |
  | 16 workers, ~23 W | `disengaged` (~9700 RPM) | stable 70–73 °C for 8 min, PL1 stayed 40 W, zero drop |

  `disengaged` (full speed, no regulation) is ~7 °C cooler than level 7 at full load: the
  only fixed level that keeps full load at cap 2000 under 75 °C.
- **In `disengaged`, RPM goes up when airflow goes down.** The fan runs at full power with
  no speed regulation, so a blocked intake unloads it and it spins faster: a hand on the
  grille (under the laptop) gave ~11350 RPM, the desk edge ~10400, flat on the desk ~9800.
  Judge placement by temperature, never by RPM. Lifting the laptop lowered the
  temperature a little. Regulated levels (0–7) hold a target speed, so this matters less.
- **For "zero drop with the least fan"**: `platform_profile=performance` is required; the
  fan can stay off up to ~9–10 W; keep the package around 70–72 °C (margin under 75);
  use RAPL package power (it includes the iGPU, which video decode loads) as the early
  signal — at 25 W the package goes from 70 to 77 °C in ~15 s.
- **Measuring a drop**: `max(scaling_cur_freq) < cap` on a single read is noise (core
  migration gives isolated reads down to ~1935 at cap 2000). Require ~300 ms in a row.
  A short "Thermal" limit bit (bit 1) was seen at 57 °C with no frequency change.
- **Test safety**: `echo "watchdog 10" > /proc/acpi/ibm/fan` makes the EC take the fan
  back if the controlling process stops sending `level` commands. Reset it to 0 after.

## Working on this

- **Manual tuning is the product.** The auto-tuner is abandoned, and so is the idea of
  collecting data for one. `hw-tui` is the control surface. The daemon only keeps cap +
  EPP across reboots and writes an event log (CSV) for post-mortem. It runs no control
  loop. Don't reintroduce automatic nudging without saying so explicitly.
- **The fan level is never persisted.** The daemon sets the fan to auto at start and on
  stop; manual levels last one `hw-tui` session. This is deliberate: a manual level
  restored at boot, with nobody watching, could leave the fan off under load.
- **`fan_control=1`**: at boot `thinkpad_acpi` is loaded without it. `hw-tui` reloads the
  module to enable it, which gives the thinkpad hwmon node a new number — `FanSensor` in
  `src/hw.rs` re-discovers the node when a read fails.
- All sysfs access lives in `src/hw.rs`, shared by both binaries. Put new hardware reads
  there, not in one binary.
- `thermal-governor.service` is installed and running on the dev machine itself. A
  `cargo build` is harmless, but `install.sh` restarts the live service and discards the
  5-minute rolling buffer. A restart keeps cap and EPP (restored from `settings.json`)
  but resets the fan to auto.
- The event CSVs in `/var/lib/thermal-governor/events/` are the user's log, not build
  output. Don't clear them to "clean up".
