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
  input. **The cut ends when SEN1 reads 52 °C or less** (all 7 cuts of 2026-09-29: on at
  54, off at 52).
- **The fan cools SEN1 about twice as fast at light load** (test of 2026-09-28,
  `sen1-decay-20260928-120805.csv`, ~6 W of real use, after a morning of work): fan 0
  gave 51 → 49 °C in 3 min (~0.7 °C/min), level 2 gave 49 → 43 °C in 5 min
  (~1.2 °C/min, ~40 s of lag), `disengaged` 43 → 36 °C in 5 min. So a short run at a low
  level is enough to get back the margin that a hot history eats. That is why the curve
  starts the fan from SEN1 = 51 °C.
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

## A power limit of our own: the MSR interface (tests of 2026-09-28)

Tests: `rapl-msr-20260928-13*.csv` in `/var/lib/thermal-governor/tests/`, cap 4800,
fan `disengaged`, `stress-ng`, 5 Hz logs. Scripts: `/var/lib/thermal-governor/tests/scripts/` (not in the repo).

- **The MSR limit register is not locked** (`0x610` bit 63 = 0). `intel-rapl:0`
  `constraint_1` (PL2, window 2.4 ms) is writable, and the lower of the MSR and MMIO
  limits applies.
- **PL2 on the MSR holds to the tenth of a watt, and it throttles gracefully**:
  16 workers at PL2 20 W → 20.0 W, 1690 MHz average, max frequency never under 1800,
  72 °C; PL2 30 W → 30.0 W, 2360 MHz average, 88 °C. No drop to 400 MHz. The limit-reason
  bit is 11 (0x800, PL2).
- **Nobody overwrites it**: not the EC over ~4 min, not a profile change
  (performance → balanced → performance moved the MMIO PL1 40 → 15 → 10 → 40 W; the MSR
  values did not move).
- **PL1 on the MSR (28 s average) is not tested yet**: 15 W with PL2 22 W gave 22 W for
  63 s with no clamp, but the budget model of the power-limit section predicts the clamp
  at ~60 s. The daemon does not need it: it can compute the average itself and drive PL2.
- **The package temperature is a per-core hot spot.** One worker at ~4.5 GHz: package
  power ~20 W, but the package sensor (the hottest core) jumps between 77 and 97 °C
  within 0.2 s. Idle at cap 4800, a short burst of normal use reached 99 °C. A power
  limit cannot prevent this: only the frequency cap bounds the heat of one core.
- **A sustained single thread at cap 4800 does not collapse** (`hotspot-1t-20260928-134607.csv`,
  2 × 60 s, fan `disengaged` from the curve): the package regulates at ~100 °C (max
  103–105, never TjMax), the throttle counter runs 75–85 % of the time (limit-reason bit
  1, thermal), and max frequency stays 4.3–4.5 GHz (median 4440, never under 4089): about
  −8 % against 4800. The user finds this loss harmless (2026-09-28), so the cap can stay
  at max. The cost is heat: ~21 W package for one thread, and SEN1 rose 48 → 50 °C in
  2 min even at full fan.
- **Sustained all-core limit, fan at full speed**: ~30 W gives 85–99 °C within 25 s; 22 W
  stays under 80 °C for 60 s; 23 W held 70–73 °C for 8 min (2026-09-25).
- `MSR 0x1A2`: TjMax 110 °C, TCC offset 5 °C (target 105 °C). The throttle counter still
  moves from ~98 °C (13:13 event on 2026-09-28: 7 → 43 W and 61 → 93 °C in one second at
  cap 4800, throttle 152 ms/s, max frequency stayed 4.2–4.6 GHz).

## Throughput per watt, and the SEN1 model (tests of 2026-09-28)

**Throughput under a PL2 limit** (`pl2-sweep-20260928-*.csv`: `stress-ng --cpu-method
matrixprod`, cap 4800, fan on the curve, bogo ops/s):

| Threads | 10 W | 14 W | 18 W | 22 W | 26 W | 30 W | no limit |
|---|---|---|---|---|---|---|---|
| 1 | 1193 | 2390 | 2719 | **2950** | 2953 | 2718 | 2710 (20 W) |
| 2 | 2493 | 4017 | 4813 | 5297 | **5542** | 5422 | 5310 (31 W) |
| 4 | 2250 | 5267 | 7086 | 8352 | 9210 | 9696 | 9800 (41 W) |
| 8 | 2837 | 7079 | 9653 | 12026 | 13103 | 14632 | – |
| 16 | 3214 | 6806 | 10335 | 12689 | 15768 | 18107 | – |

- **When the thermal throttle dominates, throughput falls**: 1 thread gives 9 % more at
  22 W than with no limit, 2 threads 4 % more at 26 W. So a dominant throttle (limit-reason
  bit 1) means the power budget is above its optimum: a threshold-free signal.
- More threads give much more per watt (2 threads at 14 W beat 1 thread at 22 W). 16
  threads stay near-proportional up to 30 W. Below ~10 W almost nothing reaches the cores
  (~6–8 W go to the rest of the chip and normal use); 16 threads at 10 W sit at 400 MHz.

**SEN1 model** (`fit_sen1.py`, fitted on `thermal-steps-20260928-141113.csv` plus the
2026-09-25 tests and `sen1-decay`): first order per fan level,
`dS/dt = (S_amb + G·P − S) / τ`, P = package power.

| Fan | G (°C/W) | τ (s) | Package R (°C/W) | Sustainable P, ambient 26 / 30 °C |
|---|---|---|---|---|
| 0 | 2.59 | 333 | 2.08 | 10.2 / 8.7 W |
| 2 | 1.65 | 256 | 1.73 | 16.0 / 13.6 W |
| 4 | 1.42 | 240 | 1.54 | 18.7 / 15.9 W |
| 7 | 1.20 | 253 | – | 22.1 / 18.8 W |
| `disengaged` | 1.04 | 190 | 1.35 | 25.5 / 21.7 W |

- "Sustainable" = SEN1 at equilibrium ≤ 52.5 °C (the 54 °C cut minus the 1.5 °C largest
  fit error). Level 1 is poorly identified (one 110 s segment): not in the table.
- Fit error: 0.3–0.8 °C RMS per segment, 1.7 °C at most. SEN1 reads whole degrees.
- **S_amb is not constant**: 25.9 °C fitted for 2026-09-25, 30.0 °C for 2026-09-28. No
  sensor gives it directly: a controller has to estimate it online.
- At idle (~7 W) with the fan off, SEN1 settles near 48 °C (ambient 30): only ~6 °C
  under the cut. A 25 W burst at fan 0 then gains ~0.14 °C/s, so the cut comes in ~45 s
  unless the fan starts. That matches the cuts seen after builds.

## The controller: replay of the 7 cuts, and shadow mode (2026-09-30)

Scripts (not in the repo): `/var/lib/thermal-governor/tests/scripts/replay-20260930/`.

- **The 7 cuts of 2026-09-29 have one cause**: SEN1 sat at 51–52 °C under light load
  (~10 W, fan level 1–5), and a burst of ~26 W for 15–30 s (all cores at cap 2000) added
  2–3 °C. Of 34 bursts over 20 W in 3 days, every one that started at SEN1 ≥ 51 in the
  afternoon of 09-29 came close to or reached the cut. Only 5 of the 7 cuts clamped a
  load (15:19 and 15:23 came when the burst was already over).
- **The curve is ~20 s late on a burst**: `POWER_TAU_S` = 10 s delays the full-speed
  command by 10–14 s, then the fan needs ~8 s to spin up (level 1 → 9000 RPM).
- **The SEN1 model needs no fast part**: first order with a fan-dependent gain,
  `G(rpm) = 1 / (0.3956 + 0.0600 · rpm/1000)` °C/W (2.53 at fan off, 1.57 at 4000 RPM,
  1.01 at full speed), τ = 209 s. Fitted on the windows around the 34 bursts with a free
  ambient per window: 0.50 °C rms, 1.7 °C max. Same numbers as the step tests above.
- **The effective ambient is the big unknown**: ~37 °C in the afternoon of 09-29, 30 °C
  on 09-28, 26 °C on 09-25. Fitted over whole days it moves by degrees within minutes,
  which no room does: it also carries what the model misses (heat from other parts,
  the laptop's position). At 37 °C and full fan, SEN1 holds under 54 °C only below
  ~17 W: a sustained all-core load then gets cut whatever the fan does.
- **Replay**: a closed-loop simulator (plant = the model with the ambient fitted every
  5 min, EC cut at reading 54 / release at 52, PL1 on a 28 s average) replays the current
  curve on the 3 days and finds the same 5 clamped cuts, within seconds of the real ones.
- **Observer**: a Kalman filter on (SEN1, ambient), started from the reading with the
  ambient that explains it at equilibrium. On the real readings, with a restart every
  30 min, it predicts SEN1 60 s ahead (with the real power) within 0.52 °C rms (1.4 °C
  p99); a fixed-gain observer gave 1.34 °C, and its ambient took over an hour to converge
  after a start (a start at a wrong ambient then led to a cut in the unit test).
- **The law** (`src/controller.rs`): fan = the rpm that keeps SEN1 under 52.5 °C in 60 s
  at the current power; PL2 = the highest power that keeps SEN1 under 53 °C in 40 s at
  full fan, never under 18 W (~66 % of the throughput of a 26 W burst; lower, the limit
  would be a drop by itself). Cost used to tune it (the user's choice): one drop = 10 min
  of full fan; fan noise counted as (rpm/9800)^1.5, a drop = delivered throughput under
  half of the demand's. Among the settings with zero drops on 4 plants (nominal, G +10 %
  τ −15 %, G −10 % τ +15 %, ambient +2 °C), the one with the least fan.
- **Replay result** (3 days, ~25 h): 0 drops instead of 5; fan −27 % (off 71 % of the
  time instead of 59.5 %); the power limited ~14 min in all, mostly at 18–19 W. With the
  ambient 2 °C higher: 0 drops instead of 12. Hotter plant and +2 °C together: 11 instead
  of 24 (the 18 W floor is above what SEN1 can hold). Open loop on the real 09-29 data,
  it would have limited the power 29–120 s before each of the 7 cuts.
- **Not validated yet**: the plant is the model itself (optimistic); ~1500 MHz at 18 W
  comes from the cap-4800 test; the package temperature and the demand are the logged
  ones (a limited load would last longer); only one day had cuts.

## Working on this

- **The daemon runs one control loop: the fan curve** (`src/fan_curve.rs`), asked for on
  2026-09-25. Goal: zero max-frequency drops with the least fan RPM. Since 2026-09-28
  SEN1 is one of its inputs (full speed at 53 °C, one degree under the cut), and a PL1
  cut keeps its power input from falling (GitHub issue #3). It never touches the
  cap, EPP or profile on its own: those stay manual (`hw-tui`), the daemon only saves and
  restores them. The learned auto-tuner is abandoned; don't bring it back unasked.
- **The fan + power controller runs in shadow mode** since 2026-09-30
  (`src/controller.rs`, asked for by the user): the daemon computes its fan level and PL2
  every second and logs them (`shadow_*` columns of the daily log, `[shadow]` lines when
  it would limit the power, the status line), and writes nothing. Before it writes the
  MSR PL2: a limit left there survives the daemon (unlike the fan, the EC takes nothing
  back), so the daemon must restore the firmware value on exit and at start, and never
  write under the floor.
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
