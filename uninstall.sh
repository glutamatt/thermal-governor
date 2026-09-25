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

# Reset what the daemon and hw-tui change: cap (each core to its own max),
# EPP (firmware default) and fan (EC-managed)
info "Resetting CPU and fan to defaults..."
for d in /sys/devices/system/cpu/cpu*/cpufreq/; do
    cat "${d}cpuinfo_max_freq" > "${d}scaling_max_freq" 2>/dev/null || true
    echo default > "${d}energy_performance_preference" 2>/dev/null || true
done
echo "level auto" > /proc/acpi/ibm/fan 2>/dev/null || true

echo ""
info "Uninstalled. CPU and fan reset to defaults."
echo ""

if [ -d "$STATE_DIR" ]; then
    echo "  Saved settings and event logs kept at: $STATE_DIR"
    echo "  To remove: sudo rm -rf $STATE_DIR"
fi
