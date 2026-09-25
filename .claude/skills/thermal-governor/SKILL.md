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
- **Fan curve (firmware)**: effectively binary — off, or ~4800 RPM. Kicks in around 60 °C.
- **Hardware throttle**: hard limit at 98 °C.
- **SEN trip points**: 65 / 75 / 80 °C.
- **RAPL**: PL1 = PL2 = 64 W. Set high deliberately — power is not the limiting factor
  here, temperature is. Don't reach for RAPL as a thermal knob.

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
