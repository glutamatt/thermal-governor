#!/usr/bin/env bash
# hw-characterize.sh — Fast hardware boundary discovery
# Run as: sudo bash hw-characterize.sh
#
# Sweeps PROFILES (via powerprofilesctl) × FREQ CAPS × STRESS LEVELS.
# Each profile brings its own EPP + platform_profile + power limits + fan curve.
set -euo pipefail

CAPS_MHZ=(1200 2000 3000 3500 4500)
PROFILES=(power-saver balanced performance)
NPROC=$(nproc)
STEPS=(0 1 2 4 8 "$NPROC")     # stress workers per step
STEP_SECS=20                    # seconds per load step
COOL_SECS=15                    # fixed cooldown between caps

# ── Hardware paths ────────────────────────────────────────────────────
TEMP="/sys/class/thermal/thermal_zone8/temp"
FAN1="/sys/class/hwmon/hwmon7/fan1_input"
FAN2="/sys/class/hwmon/hwmon7/fan2_input"
THROTTLE="/sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_total_time_ms"
CPU0_CUR="/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq"

# ── Output ────────────────────────────────────────────────────────────
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
TS=$(date +%Y%m%d-%H%M%S)
CSV="${SCRIPT_DIR}/hw-data-${TS}.csv"

# ── Helpers ───────────────────────────────────────────────────────────
ri()  { cat "$1" 2>/dev/null || echo 0; }
read_temp()  { echo $(( $(ri "$TEMP") / 1000 )); }
read_fan()   { local v; v=$(ri "$1"); (( v >= 60000 )) && echo 0 || echo "$v"; }
read_freq()  { echo $(( $(ri "$CPU0_CUR") / 1000 )); }

cpufreq_dirs() {
    find /sys/devices/system/cpu/cpu[0-9]*/cpufreq -maxdepth 0 2>/dev/null | sort
}
set_all() {
    local attr=$1 val=$2
    for d in $(cpufreq_dirs); do echo "$val" > "$d/$attr" 2>/dev/null || true; done
}

# ── Preflight ─────────────────────────────────────────────────────────
[[ $EUID -eq 0 ]] || { echo "Must run as root"; exit 1; }
command -v stress-ng >/dev/null || { echo "stress-ng not found"; exit 1; }
command -v powerprofilesctl >/dev/null || { echo "powerprofilesctl not found"; exit 1; }

TOTAL=$(( ${#CAPS_MHZ[@]} * ${#PROFILES[@]} ))
echo "════════════════════════════════════════════"
echo "  HW Boundary Discovery"
echo "════════════════════════════════════════════"
echo "  Caps:      ${CAPS_MHZ[*]} MHz"
echo "  Profiles:  ${PROFILES[*]}"
echo "  Steps:     ${STEPS[*]} workers × ${STEP_SECS}s each"
echo "  Tests:     ${TOTAL} (cap × profile)"
echo "  Output:    ${CSV}"
echo "════════════════════════════════════════════"
echo

# ── Stop governor ─────────────────────────────────────────────────────
systemctl stop thermal-governor 2>/dev/null || true
sleep 1
echo "Governor stopped."

# ── CSV header ────────────────────────────────────────────────────────
echo "cap_mhz,profile,stress_cores,elapsed_s,temp_c,fan1_rpm,fan2_rpm,throttle_total_ms,cur_freq_mhz" > "$CSV"

# ── Cleanup ───────────────────────────────────────────────────────────
STRESS_PID=""
cleanup() {
    [[ -n "$STRESS_PID" ]] && kill "$STRESS_PID" 2>/dev/null && wait "$STRESS_PID" 2>/dev/null || true
    set_all scaling_max_freq 4500000
    powerprofilesctl set balanced 2>/dev/null || true
    echo
    echo "Restored defaults. Governor stopped."
    echo "  Restart: sudo systemctl start thermal-governor"
    echo "  Plot:    .venv/bin/python hw-plot.py ${CSV}"
}
trap cleanup EXIT

# ── Main loop ─────────────────────────────────────────────────────────
TEST=0
for PROFILE in "${PROFILES[@]}"; do
  # Switch profile via PPD (sets EPP + platform_profile + power limits)
  echo
  echo "════ Switching to profile: $PROFILE ════"
  powerprofilesctl set "$PROFILE"
  sleep 2
  EPP_NOW=$(cat /sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference 2>/dev/null || echo "?")
  PLAT=$(cat /sys/firmware/acpi/platform_profile 2>/dev/null || echo "?")
  echo "  EPP=$EPP_NOW  platform_profile=$PLAT"

  for CAP in "${CAPS_MHZ[@]}"; do
    TEST=$((TEST+1))
    echo
    echo "── [$TEST/$TOTAL] profile=$PROFILE cap=${CAP}MHz ──"

    # Set freq cap only (EPP managed by PPD)
    set_all scaling_max_freq "$((CAP * 1000))"
    sleep 1

    # Ramp through load steps
    ELAPSED=0
    for CORES in "${STEPS[@]}"; do
        # Kill previous stress
        if [[ -n "$STRESS_PID" ]]; then
            kill "$STRESS_PID" 2>/dev/null; wait "$STRESS_PID" 2>/dev/null || true
            STRESS_PID=""
        fi

        # Start new stress level
        if (( CORES > 0 )); then
            stress-ng --cpu "$CORES" --timeout "$((STEP_SECS + 5))s" --quiet &
            STRESS_PID=$!
        fi

        # Sample for STEP_SECS
        for (( s=0; s<STEP_SECS; s++ )); do
            t=$(read_temp)
            f1=$(read_fan "$FAN1")
            f2=$(read_fan "$FAN2")
            thr=$(ri "$THROTTLE")
            freq=$(read_freq)
            fm=$(( f1>f2 ? f1 : f2 ))

            echo "$CAP,$PROFILE,$CORES,$ELAPSED,$t,$f1,$f2,$thr,$freq" >> "$CSV"

            fan_tag=""; (( fm >= 4000 )) && fan_tag=" FAN!"
            printf "\r  %3ds  %2dc  %3d°C  fan:%5d  freq:%4dMHz  thr:%dms%s     " \
                "$ELAPSED" "$CORES" "$t" "$fm" "$freq" "$thr" "$fan_tag"

            ELAPSED=$((ELAPSED+1))
            sleep 1
        done
    done
    echo

    # Kill final stress
    if [[ -n "$STRESS_PID" ]]; then
        kill "$STRESS_PID" 2>/dev/null; wait "$STRESS_PID" 2>/dev/null || true
        STRESS_PID=""
    fi

    # Brief cooldown
    echo -n "  cooling ${COOL_SECS}s..."
    set_all scaling_max_freq 1200000
    sleep "$COOL_SECS"
    echo " $(read_temp)°C"
  done
done

echo
echo "════════════════════════════════════════════"
echo "  Done! $TOTAL tests, CSV: $CSV"
echo "════════════════════════════════════════════"
