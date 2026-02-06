#!/bin/bash
set -e

BIN_PATH="/usr/local/bin/thermal-governor"
SERVICE_NAME="thermal-governor"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}.service"
STATE_DIR="/var/lib/thermal-governor"

RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m'

info()  { echo -e "${GREEN}[+]${NC} $*"; }
error() { echo -e "${RED}[x]${NC} $*"; exit 1; }

[ "$(id -u)" -eq 0 ] || error "This script must be run as root (use sudo)"

echo "============================================"
echo "  thermal-governor uninstaller"
echo "============================================"
echo ""

# Stop and disable service
if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    info "Stopping service..."
    systemctl stop "$SERVICE_NAME"
fi
if systemctl is-enabled --quiet "$SERVICE_NAME" 2>/dev/null; then
    info "Disabling service..."
    systemctl disable "$SERVICE_NAME"
fi

# Remove files
[ -f "$SERVICE_PATH" ] && info "Removing service file" && rm -f "$SERVICE_PATH"
[ -f "$BIN_PATH" ]     && info "Removing binary"       && rm -f "$BIN_PATH"

systemctl daemon-reload

# Reset CPU to defaults
info "Resetting CPU to defaults..."
for d in /sys/devices/system/cpu/cpu*/cpufreq/; do
    echo 400000  > "${d}scaling_min_freq"  2>/dev/null
    echo 4500000 > "${d}scaling_max_freq"  2>/dev/null
    echo balance_power > "${d}energy_performance_preference" 2>/dev/null
done
echo 0 > /sys/devices/system/cpu/intel_pstate/hwp_dynamic_boost 2>/dev/null

echo ""
info "Uninstalled. CPU reset to defaults."
echo ""

if [ -d "$STATE_DIR" ]; then
    echo "  Learned parameters kept at: $STATE_DIR"
    echo "  To remove: sudo rm -rf $STATE_DIR"
fi
