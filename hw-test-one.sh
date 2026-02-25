#!/usr/bin/env bash
# hw-test-one.sh <profile> <cap_mhz> <step_secs>
# Run one escalation test: ramp stress 0→1→2→4→8→all at given cap.
# Appends to CSV. Designed to be called repeatedly by sudo-watcher.
set -euo pipefail

PROFILE="$1"
CAP="$2"
STEP_SECS="${3:-20}"
NPROC=$(nproc)
STEPS=(0 1 2 4 8 "$NPROC")

TEMP_S="/sys/class/thermal/thermal_zone8/temp"
FAN1="/sys/class/hwmon/hwmon7/fan1_input"
FAN2="/sys/class/hwmon/hwmon7/fan2_input"
THR="/sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_total_time_ms"
CUR="/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq"
CSV="/home/matt/repositories/github.com/glutamatt/thermal-governor/hw-data.csv"

ri()  { cat "$1" 2>/dev/null || echo 0; }

set_all() {
    for d in /sys/devices/system/cpu/cpu[0-9]*/cpufreq; do
        echo "$2" > "$d/$1" 2>/dev/null || true
    done
}

# Init CSV if needed
[[ -f "$CSV" ]] || echo "cap_mhz,profile,stress_cores,elapsed_s,temp_c,fan1_rpm,fan2_rpm,throttle_total_ms,cur_freq_mhz" > "$CSV"

# Stop governor, set profile + cap
systemctl stop thermal-governor 2>/dev/null || true
powerprofilesctl set "$PROFILE"
sleep 1
set_all scaling_max_freq "$((CAP * 1000))"
sleep 1

EPP=$(cat /sys/devices/system/cpu/cpu0/cpufreq/energy_performance_preference 2>/dev/null || echo "?")
PLAT=$(cat /sys/firmware/acpi/platform_profile 2>/dev/null || echo "?")
echo "=== profile=$PROFILE cap=${CAP}MHz EPP=$EPP platform=$PLAT ==="

STRESS_PID=""
trap '[[ -n "$STRESS_PID" ]] && kill "$STRESS_PID" 2>/dev/null; wait "$STRESS_PID" 2>/dev/null || true; set_all scaling_max_freq 1200000' EXIT

ELAPSED=0
for CORES in "${STEPS[@]}"; do
    [[ -n "$STRESS_PID" ]] && { kill "$STRESS_PID" 2>/dev/null; wait "$STRESS_PID" 2>/dev/null || true; STRESS_PID=""; }

    if (( CORES > 0 )); then
        stress-ng --cpu "$CORES" --timeout "$((STEP_SECS + 5))s" --quiet &
        STRESS_PID=$!
    fi

    for (( s=0; s<STEP_SECS; s++ )); do
        t=$(( $(ri "$TEMP_S") / 1000 ))
        f1=$(ri "$FAN1"); (( f1 >= 60000 )) && f1=0
        f2=$(ri "$FAN2"); (( f2 >= 60000 )) && f2=0
        thr=$(ri "$THR")
        freq=$(( $(ri "$CUR") / 1000 ))
        fm=$(( f1>f2 ? f1 : f2 ))

        echo "$CAP,$PROFILE,$CORES,$ELAPSED,$t,$f1,$f2,$thr,$freq" >> "$CSV"
        printf "\r  %3ds %2dc %3d°C fan:%5d freq:%4dMHz thr:%d  " "$ELAPSED" "$CORES" "$t" "$fm" "$freq" "$thr"
        ELAPSED=$((ELAPSED+1))
        sleep 1
    done
done
echo
echo "=== done: profile=$PROFILE cap=${CAP}MHz ==="

# Cooldown: drop cap to min, wait 15s
set_all scaling_max_freq 1200000
echo -n "cooling 15s..."
sleep 15
echo " $(( $(ri "$TEMP_S") / 1000 ))°C"
