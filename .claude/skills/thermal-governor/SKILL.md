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

- The daemon is **a passive observer** right now: 1 Hz sampling, 5-minute rolling buffer,
  dumps to `/var/lib/thermal-governor/events/` on a threshold event. It restores
  `settings.json` at boot and reverts to safe defaults on clean shutdown. It does **not**
  run a control loop — that was the earlier phase, deliberately removed. `hw-tui` is the
  control surface. Don't reintroduce automatic nudging without saying so explicitly.
- `thermal-governor.service` is installed and running on the dev machine itself. A
  `cargo build` is harmless, but `install.sh` restarts the live service — and the daemon
  owns fan control (`fan_control=1`) and the frequency cap. Restarting it mid-measurement
  discards the rolling buffer and resets tuning to defaults.
- The event JSON in `/var/lib/thermal-governor/events/` is accumulated data, not build
  output. Don't clear it to "clean up" — it is the input for the next auto-tuner.
