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
  **critical at 80 °C makes the kernel power the machine off**. **SEN1 is the sensor the
  EC watches for the PL1 cut** (see "Power limits"). It is slow: it keeps the heat of past
  loads for minutes, and sits at ~51 °C at idle after an afternoon of builds.
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
- **Even in `performance`, the EC cuts PL1 from 40 W to 12 W when SEN1 reaches 54 °C.**
  In the tests that logged the SEN sensors, both cuts came on the exact sample where SEN1
  first read 54, and every run without a cut stayed at SEN1 ≤ 53 (the 400 s run at fan 7
  peaked at 53). The package temperature does not predict it: cuts were seen from 68 °C
  to 88 °C package (68 °C after 5 min at ~10 W with the fan at level 0–2). Low airflow
  over time is what heats SEN1; a hot history leaves it close to the threshold, and then
  ~30 s at 25 W is enough.
- **The threshold comes from the firmware's DTT policy**, in the ACPI data vault (GDDV),
  not in the static ACPI tables (their PSVT only limits the charger, on SEN3).
  `./hw-dptf-policy.py` decodes it (policy `TH1_2nd_EE-SIT_20231226_a`). Its mode limits
  match what the EC applies (low-power 10 W, balanced 15 W, performance 40 W), and the
  `performance` passive tables all watch SEN1: 54 °C and 64 °C in `0xD78_U`, down to
  12 W. The EC does not follow the tables to the letter (the 12 W floor is the `_U`
  variant, the 64 W PL2 the `_H` one), but the 54 °C threshold matches the measurements.
  Nothing applies the DTT policy on Linux (thermald is not running): the EC does the cut.
- **A cut is felt ~30 s late**: PL1 limits a running average (tau 28 s) and PL2 stays at
  64 W, so the CPU keeps full speed until the average falls to 12 W. A build that ends
  within that window never shows a drop.
- **A cut lasts as long as the load**: PL1 came back to 40 W 1–2 min after the load
  stopped, at ~62 °C package, even with the fan at level 0 (4 times on 2026-09-25). Under
  sustained load it did not come back: 9.5 min at 12 W and 400–800 MHz during a series of
  builds, with the fan curve down to level 1–2 because the capped power lowers its power
  input. The SEN1 value at recovery (the hysteresis) is not known yet: the daemon does
  not log SEN1.
- **Fan levels** (idle, laptop flat on the desk, fan1 / fan2 RPM): 0 → 0 / 0,
  1 → 3985 / 3480, 2 → 4839 / 4124, 3 → 5272 / 4765, 4 → 5790 / 5594, 5 → 6438 / 5924,
  6 → 6849 / 6338, 7 → 7537 / 7009, `disengaged` → ~9540 / 8900. Steps are regular
  (~450–700 RPM) up to 7; `disengaged` adds ~2000 RPM.
- **Fan vs load at cap 2000** (`performance` profile):

  | Load | Fan | Result |
  |---|---|---|
  | 2 workers, 9 W | 0 | 66 °C after 400 s, slow rise, no drop |
  | 4 workers, 13–14 W | 0 | 77–80 °C after ~9 min, then PL1 cut |
  | 16 workers, 24–26 W | 0 | +0.5 °C/s, 90 °C in 70 s |
  | 16 workers | 1 (~3970 RPM) | 89 °C at 170 s, then PL1 cut |
  | 16 workers | 2 (~4830 RPM) | 84 °C at 130 s, still rising |
  | 16 workers | 3 (~5270 RPM) | 84 °C, still rising |
  | 16 workers | 7 (~7540 RPM) | stable 77–80 °C; no cut in 400 s from cold (SEN1 peaked at 53), cut after a hot history |
  | 16 workers, ~23 W | `disengaged` (~9700 RPM) | stable 70–73 °C for 8 min, PL1 stayed 40 W, zero drop |

  `disengaged` (full speed, no regulation) is ~7 °C cooler than level 7 at full load: the
  only fixed level that keeps full load at cap 2000 under 75 °C.
- **In `disengaged`, RPM goes up when airflow goes down.** The fan runs at full power with
  no speed regulation, so a blocked intake unloads it and it spins faster: a hand on the
  grille (under the laptop) gave ~11350 RPM, the desk edge ~10400, flat on the desk ~9800.
  Judge placement by temperature, never by RPM. Lifting the laptop lowered the
  temperature a little. Regulated levels (0–7) hold a target speed, so this matters less.
- **For "zero drop with the least fan"**: `platform_profile=performance` is required, and
  **SEN1 must stay under 54 °C**. The package margin under 75 °C used before was a proxy
  for SEN1, and it does not hold after a hot history. The fan can stay off at ~9–10 W
  only for a while: 5 min at ~10 W with the fan at level 0–2, after earlier loads, gave a
  cut at 68 °C package (SEN1 was not logged). Use RAPL package power (it includes the iGPU, which video decode loads)
  as the early signal — at 25 W the package goes from 70 to 77 °C in ~15 s.
- **Measuring a drop**: `max(scaling_cur_freq) < cap` on a single read is noise (core
  migration gives isolated reads down to ~1935 at cap 2000). Require ~300 ms in a row.
  A short "Thermal" limit bit (bit 1) was seen at 57 °C with no frequency change.
- **Test safety**: `echo "watchdog 10" > /proc/acpi/ibm/fan` makes the EC take the fan
  back if the controlling process stops sending `level` commands. Reset it to 0 after.

## Working on this

- **The daemon runs one control loop: the fan curve** (`src/fan_curve.rs`), asked for on
  2026-09-25. Goal: zero max-frequency drops with the least fan RPM. It never touches the
  cap, EPP or profile on its own: those stay manual (`hw-tui`), the daemon only saves and
  restores them. The learned auto-tuner is abandoned; don't bring it back unasked.
- **Fan modes** (`/run/thermal-governor/fan-mode`, so every boot starts on `curve`):
  `curve` (daemon), `auto` (EC), `manual` (`hw-tui`). Hard limits (package ≥ 80 °C, SEN ≥
  70 °C) force full speed in curve and manual mode, then fall slowly (30 s). In
  curve mode the daemon sends the level (and re-arms the EC watchdog, 10 s) every second,
  so a dead daemon gives the fan back to the EC. In manual mode `hw-tui` re-sends its
  level every second with the watchdog still on, and quitting `hw-tui` goes back to curve. Any change here must keep that property: the fan must never stay low on a
  machine that heats up with nobody watching.
- **The fan level is never persisted** in `settings.json`: a manual level restored at
  boot, with nobody watching, could leave the fan off under load.
- **`fan_control=1`**: `install.sh` sets it in modprobe.d for boot. Without it (before the
  first reboot after install), the daemon and `hw-tui` reload `thinkpad_acpi`, which gives
  the thinkpad hwmon node a new number — `FanSensor` in `src/hw.rs` re-discovers the node
  when a read fails.
- All sysfs access lives in `src/hw.rs`, shared by both binaries. Put new hardware reads
  there, not in one binary.
- `thermal-governor.service` is installed and running on the dev machine itself. A
  `cargo build` is harmless, but `install.sh` restarts the live service and discards the
  5-minute rolling buffer. A restart keeps profile, cap and EPP (restored from
  `settings.json`) and the fan mode (in `/run`).
- The event CSVs in `/var/lib/thermal-governor/events/` are the user's log, not build
  output. Don't clear them to "clean up".
