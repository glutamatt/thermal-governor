#!/usr/bin/env bash
# monitor.sh — Real-time TUI dashboard for thermal-governor
set -euo pipefail

# ── Sysfs paths (must match src/main.rs) ─────────────────────────────────────
TEMP_SENSOR="/sys/class/thermal/thermal_zone8/temp"
FAN1="/sys/class/hwmon/hwmon7/fan1_input"
FAN2="/sys/class/hwmon/hwmon7/fan2_input"
THROTTLE="/sys/devices/system/cpu/cpu0/thermal_throttle/package_throttle_total_time_ms"
STATE_FILE="/var/lib/thermal-governor/tuned-params.json"
CPU0_MAX="/sys/devices/system/cpu/cpu0/cpufreq/scaling_max_freq"
CPU0_CUR="/sys/devices/system/cpu/cpu0/cpufreq/scaling_cur_freq"

# ── Colors ────────────────────────────────────────────────────────────────────
R='\033[1;31m' Y='\033[1;33m' G='\033[1;32m' C='\033[1;36m'
B='\033[1m' D='\033[2m' N='\033[0m'

# ── History ring buffers ──────────────────────────────────────────────────────
declare -a TEMPS=() CAPS=() TIMES=()
MAX_HIST=60

# Throttle baseline
THR_BASE=$(cat "$THROTTLE" 2>/dev/null || echo 0)

# ── Helpers ───────────────────────────────────────────────────────────────────
read_int()  { cat "$1" 2>/dev/null || echo 0; }
read_temp() { echo $(( $(read_int "$TEMP_SENSOR") / 1000 )); }

read_fan() {
    local v; v=$(read_int "$1")
    (( v >= 60000 )) && echo 0 || echo "$v"
}

ghz() { awk "BEGIN{printf \"%.1f\", $1/1000000}"; }

temp_color() {
    if   (( $1 >= 85 )); then printf '%b' "$R"
    elif (( $1 >= 70 )); then printf '%b' "$Y"
    else printf '%b' "$G"; fi
}

rate_color() {
    local v=$1
    if   awk "BEGIN{exit !($v > 0.5)}"; then printf '%b' "$R"
    elif awk "BEGIN{exit !($v > 0)}";   then printf '%b' "$Y"
    else printf '%b' "$G"; fi
}

# bar <value> <min> <max> <width>
bar() {
    local val=$1 min=$2 max=$3 w=$4
    local range=$((max - min))
    (( range <= 0 )) && range=1
    local filled=$(( (val - min) * w / range ))
    (( filled < 0 )) && filled=0
    (( filled > w )) && filled=$w
    local empty=$((w - filled))
    local out=""
    for ((i=0; i<filled; i++)); do out+="█"; done
    for ((i=0; i<empty;  i++)); do out+="░"; done
    printf '%s' "$out"
}

# sparkline from array values
sparkline() {
    local -a vals=("$@")
    local blocks=(▁ ▂ ▃ ▄ ▅ ▆ ▇ █)
    local mn=999 mx=0
    for v in "${vals[@]}"; do
        (( v < mn )) && mn=$v
        (( v > mx )) && mx=$v
    done
    local rng=$((mx - mn))
    (( rng == 0 )) && rng=1
    for v in "${vals[@]}"; do
        local idx=$(( (v - mn) * 7 / rng ))
        printf '%s' "${blocks[$idx]}"
    done
}

# linear regression: °C/s from TEMPS[] and TIMES[] (last 16s)
compute_rate() {
    local n=${#TEMPS[@]}
    (( n < 3 )) && { echo "0.00"; return; }

    local now=${TIMES[-1]}
    local cutoff=$((now - 16))

    local pairs=""
    for ((i=0; i<n; i++)); do
        (( TIMES[i] >= cutoff )) && pairs+="${TIMES[i]} ${TEMPS[i]}"$'\n'
    done

    echo "$pairs" | awk '
    NF<2 {next}
    { n++; if(n==1) f=$1; x=($1-f); y=$2; sx+=x; sy+=y; sxx+=x*x; sxy+=x*y }
    END {
        if(n<2){print "0.00"; exit}
        d=sxx - sx*sx/n
        if(d<0.001 && d>-0.001){print "0.00"; exit}
        printf "%.2f", (sxy - sx*sy/n)/d
    }'
}

# ── Cleanup ───────────────────────────────────────────────────────────────────
cleanup() { tput cnorm; tput sgr0; echo; }
trap cleanup EXIT
tput civis
clear

# ── Main loop ─────────────────────────────────────────────────────────────────
while true; do
    temp=$(read_temp)
    fan1=$(read_fan "$FAN1")
    fan2=$(read_fan "$FAN2")
    fan_max=$(( fan1 > fan2 ? fan1 : fan2 ))
    thr_now=$(read_int "$THROTTLE")
    thr_delta=$((thr_now - THR_BASE))
    cap=$(read_int "$CPU0_MAX")
    cur=$(read_int "$CPU0_CUR")
    profile=$(powerprofilesctl get 2>/dev/null || echo "unknown")
    now=$(date +%s)

    # Update history
    TEMPS+=("$temp"); CAPS+=("$cap"); TIMES+=("$now")
    while (( ${#TEMPS[@]} > MAX_HIST )); do
        TEMPS=("${TEMPS[@]:1}"); CAPS=("${CAPS[@]:1}"); TIMES=("${TIMES[@]:1}")
    done

    rate=$(compute_rate)

    # Read state file
    ps_t=$(jq -r '.power_saver_target // 50' "$STATE_FILE" 2>/dev/null || echo 50)
    pf_t=$(jq -r '.performance_target // 85' "$STATE_FILE" 2>/dev/null || echo 85)
    bl_t=$(( (ps_t + pf_t) / 2 ))

    case "$profile" in
        power-saver) target=$ps_t; epp="power";         ceil=3500000 ;;
        performance) target=$pf_t; epp="performance";    ceil=4500000 ;;
        *)           target=$bl_t; epp="balance_power";  ceil=4500000 ;;
    esac

    error=$((target - temp))
    cap_ghz=$(ghz "$cap")
    cur_ghz=$(ghz "$cur")
    ceil_ghz=$(ghz "$ceil")

    # ── Draw ──────────────────────────────────────────────────────────────────
    tput home

    # Header
    printf "${B}${C}"
    printf '═%.0s' {1..62}
    printf '\n  thermal-governor monitor                    %s\n' "$(date +%H:%M:%S)"
    printf '═%.0s' {1..62}
    printf "${N}\n"

    # Profile
    printf "  ${B}Profile${N}  %-14s" "$profile"
    printf "${D}EPP${N} %-16s" "$epp"
    printf "${D}Ceiling${N} %sGHz\n" "$ceil_ghz"

    printf "${D}"; printf '─%.0s' {1..62}; printf "${N}\n"

    # Sensors
    printf "  ${B}SENSORS${N}\n"
    printf "  Temp      "; temp_color "$temp"
    printf "${B}%3d°C${N}  " "$temp"
    bar "$temp" 30 100 24
    printf " ${D}30──────────────100${N}\n"

    if (( fan_max > 0 )); then fc="$Y"; else fc="$G"; fi
    printf "  Fan 1   ${fc}%5d rpm${N}" "$fan1"
    printf "     Fan 2   ${fc}%5d rpm${N}\n" "$fan2"

    if (( thr_delta > 0 )); then tc="$R"; else tc="$G"; fi
    printf "  Throttle  ${tc}%d ms${N} total" "$thr_now"
    printf "  ${D}(Δ%d ms since monitor)${N}\n" "$thr_delta"

    printf "${D}"; printf '─%.0s' {1..62}; printf "${N}\n"

    # Controller
    printf "  ${B}CONTROLLER${N}\n"
    printf "  Target    ${B}%3d°C${N}" "$target"

    printf "       Error   "
    if (( error >= 0 )); then printf "${G}"; else printf "${R}"; fi
    printf "%+d°C${N}" "$error"

    printf "       Rate   "; rate_color "$rate"
    printf "%+6s°C/s${N}\n" "$rate"

    printf "  Cap       ${B}%5sGHz${N}  " "$cap_ghz"
    bar "$cap" 1200000 "$ceil" 24
    printf " ${D}1.2──────────────%s${N}\n" "$ceil_ghz"

    printf "  Actual    ${D}%5sGHz${N}\n" "$cur_ghz"

    printf "${D}"; printf '─%.0s' {1..62}; printf "${N}\n"

    # Targets
    printf "  ${B}TARGETS${N}  ${D}(state file)${N}\n"
    printf "  PS ${B}%d°C${N}" "$ps_t"
    printf "    Perf ${B}%d°C${N}" "$pf_t"
    printf "    Bal ${D}%d°C${N}\n" "$bl_t"

    printf "${D}"; printf '─%.0s' {1..62}; printf "${N}\n"

    # Sparklines
    printf "  ${B}HISTORY${N}  ${D}(%ds)${N}\n" "${#TEMPS[@]}"

    if (( ${#TEMPS[@]} > 2 )); then
        mn=999; mx=0
        for v in "${TEMPS[@]}"; do (( v<mn )) && mn=$v; (( v>mx )) && mx=$v; done
        printf "  Temp  ${C}"
        sparkline "${TEMPS[@]}"
        printf "${N} ${D}%d─%d°C${N}\n" "$mn" "$mx"

        mn=999999999; mx=0
        for v in "${CAPS[@]}"; do (( v<mn )) && mn=$v; (( v>mx )) && mx=$v; done
        # Convert caps to a 0-100 scale for sparkline
        cap_spark=()
        for v in "${CAPS[@]}"; do
            cap_spark+=( $(( (v - 1200000) / 33000 )) )  # ~0-100
        done
        printf "  Cap   ${C}"
        sparkline "${cap_spark[@]}"
        printf "${N} ${D}%s─%sGHz${N}\n" "$(ghz "$mn")" "$(ghz "$mx")"
    else
        printf "  ${D}(collecting...)${N}\n\n"
    fi

    printf "${D}"; printf '─%.0s' {1..62}; printf "${N}\n"

    # Journal
    printf "  ${B}JOURNAL${N}  ${D}(last 6)${N}\n"
    mapfile -t jlines < <(journalctl -u thermal-governor --no-pager -n 6 -o cat 2>/dev/null || true)
    for jl in "${jlines[@]:+${jlines[@]}}"; do
        printf "  ${D}%.60s${N}\n" "$jl"
    done
    # Pad remaining lines
    for ((i=${#jlines[@]}; i<6; i++)); do
        printf "${N}\n"
    done

    printf "${B}${C}"
    printf '═%.0s' {1..62}
    printf "${N}\n"

    # Clear below
    tput ed

    sleep 1
done
