# PD Controller Test Plan

## Prerequisites

```bash
cargo build --release
# deploy binary (stop service, copy, start service)
# open monitor in a separate terminal:
./monitor.sh
```

---

## 1. Startup & State Loading

**Action:** Start service fresh (delete state file first to test defaults).

```bash
sudo rm -f /var/lib/thermal-governor/tuned-params.json
sudo systemctl restart thermal-governor
```

**Expect in monitor:**
- PS target = 50°C, Perf target = 85°C
- Cap starts at profile ceiling
- Rate near 0 at idle

**Expect in journal:**
- `No saved state, using defaults`
- `Governor started: EPP=... ceiling=...GHz target=...°C`

---

## 2. Power Saver — Fan Boundary Discovery

**Action:** Switch to power-saver, generate light load.

```bash
powerprofilesctl set power-saver
# light load: compile something, browse, normal work
```

**Watch for (60s windows):**
- [ ] If fans stay off AND temp near target → target nudges UP (+1°C per window)
- [ ] If fans spin (rotations > budget) → target nudges DOWN (-1°C per window)
- [ ] Target converges to the fan boundary (fans barely spin)
- [ ] Cap tracks via PD: drops when temp approaches target, rises when below

**Healthy signals:**
- Cap oscillates smoothly, no hard jumps
- Fan rotations in journal stay near budget boundary
- Rate goes positive under load, cap responds by dropping

**Red flags:**
- Cap stuck at MIN_CAP (1.2GHz) — KP too aggressive or target too low
- Cap stuck at ceiling — target too high, never reached
- Rapid oscillation in cap (±500MHz every poll) — KD too low or KP too high

---

## 3. Performance — Throttle Boundary Discovery

**Action:** Switch to performance, generate heavy load.

```bash
powerprofilesctl set performance
# heavy load:
stress-ng --cpu $(nproc) --timeout 120s
# or: cargo build of a large project
```

**Watch for:**
- [ ] Under load, temp rises toward target (85°C default)
- [ ] Cap decreases smoothly as temp approaches target
- [ ] If throttle occurs → target nudges DOWN
- [ ] If no throttle near target → target nudges UP
- [ ] Target converges below the throttle point

**Key metric:** `Throttle Δms` in monitor. Any increase means the system throttled.

---

## 4. Balanced — Midpoint Behavior

**Action:** Switch to balanced, mixed workload.

```bash
powerprofilesctl set balanced
```

**Expect:**
- Target = (PS target + Perf target) / 2
- Behavior is a compromise between the two
- Target updates automatically as PS/Perf targets move

---

## 5. Profile Switching

**Action:** Rapidly switch profiles.

```bash
for p in power-saver performance balanced power-saver; do
  powerprofilesctl set $p; sleep 5
done
```

**Expect:**
- [ ] Each switch logs `Governor stopped` / `Governor started`
- [ ] Cap resets to new profile's ceiling
- [ ] EPP changes in journal
- [ ] No crash, no stale state

---

## 6. PD Controller Dynamics

### 6a. Step-down speed (uncapped)

**Action:** In power-saver at idle, spike load suddenly.

```bash
stress-ng --cpu $(nproc) --timeout 30s
```

**Expect:** Cap drops rapidly (no MAX_RAMP limit on step-down). Watch rate go positive, cap responds within 1-2 polls.

### 6b. Step-up speed (capped at MAX_RAMP)

**Action:** After load ends, watch recovery.

**Expect:** Cap rises gradually, max +400MHz per poll. Should take several polls to reach ceiling.

### 6c. Rate damping (KD term)

**Action:** Watch during temperature transitions.

**Expect:** When rate is positive (heating), KD adds negative pressure (helps cap drop sooner). When rate is negative (cooling), KD adds positive pressure (helps cap rise sooner). This reduces overshoot.

---

## 7. Dynamic Polling

**Watch the poll interval in journal timestamps:**
- Idle, well below target → ~2s between log entries
- Heating or near target → faster polling (down to 200ms)
- Cooling and below target → back to 2s

---

## 8. Target Convergence (Long Run)

**Action:** Leave in power-saver for 30+ minutes with normal workload.

**Watch:**
- [ ] Target stabilizes (stops moving ±1°C)
- [ ] Fan rotations per window hover near budget
- [ ] State file shows converged values

**Action:** Same for performance with sustained load.

---

## 9. Persistence

```bash
# Check current targets in monitor
sudo systemctl restart thermal-governor
# Verify targets restored from state file
```

---

## 10. Budget Tuning (Hot-Configurable)

Test env var overrides:

```bash
# Tighter fan budget (fans should be nearly silent)
sudo systemctl set-environment FAN_BUDGET=50
sudo systemctl restart thermal-governor

# Looser throttle budget (allow some throttling for more perf)
sudo systemctl set-environment THROTTLE_BUDGET_MS=100
sudo systemctl restart thermal-governor

# Reset
sudo systemctl unset-environment FAN_BUDGET THROTTLE_BUDGET_MS
```

---

## Tuning Reference

| Symptom | Likely Cause | Fix |
|---|---|---|
| Cap oscillates wildly | KP too high or KD too low | Lower KP or raise KD |
| Cap never reaches ceiling at idle | Target too low | Increase default or wait for convergence |
| Cap stuck at floor under load | Target too high or unreachable | Lower target or check sensor |
| Fans always on in PS | Target too high | Lower FAN_BUDGET or wait for convergence |
| Throttling in perf | Target too high | Lower THROTTLE_BUDGET_MS or wait |
| Sluggish response | KP too low | Raise KP |
| Overshoot on load spike | KD too low | Raise KD |
| Cap drops but never recovers | MAX_RAMP too low | Raise MAX_RAMP |
