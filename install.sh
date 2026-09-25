#!/bin/bash
set -e

BIN_NAME="thermal-governor"
BIN_PATH="/usr/local/bin/$BIN_NAME"
SERVICE_NAME="thermal-governor"
SERVICE_PATH="/etc/systemd/system/${SERVICE_NAME}.service"
STATE_DIR="/var/lib/thermal-governor"
MODPROBE_CONF="/etc/modprobe.d/thinkpad_acpi-fan-control.conf"

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

info()  { echo -e "${GREEN}[+]${NC} $*"; }
warn()  { echo -e "${YELLOW}[!]${NC} $*"; }
error() { echo -e "${RED}[x]${NC} $*"; exit 1; }

# Check root
[ "$(id -u)" -eq 0 ] || error "This script must be run as root (use sudo)"

echo "============================================"
echo "  thermal-governor installer"
echo "============================================"
echo ""

# Determine binary source
if [ -n "$1" ] && [ -f "$1" ]; then
    BIN_SRC="$1"
    info "Using provided binary: $BIN_SRC"
elif [ -f "target/release/$BIN_NAME" ]; then
    BIN_SRC="target/release/$BIN_NAME"
    info "Using local build: $BIN_SRC"
else
    error "No binary found. Either pass it as argument or run 'cargo build --release' first."
fi

# Stop existing service if running
if systemctl is-active --quiet "$SERVICE_NAME" 2>/dev/null; then
    info "Stopping existing service..."
    systemctl stop "$SERVICE_NAME"
    sleep 1
fi

# Install binary
info "Installing binary to $BIN_PATH"
cp "$BIN_SRC" "$BIN_PATH"
chmod 755 "$BIN_PATH"

# The fan curve needs fan_control=1 from boot. Loading the option at boot
# avoids a module reload, which renumbers the thinkpad hwmon node.
MODPROBE_LINE="options thinkpad_acpi fan_control=1"
if [ "$(cat "$MODPROBE_CONF" 2>/dev/null)" != "$MODPROBE_LINE" ]; then
    info "Enabling thinkpad_acpi fan_control at boot: $MODPROBE_CONF"
    echo "$MODPROBE_LINE" > "$MODPROBE_CONF"
    # thinkpad_acpi loads from the initramfs, which has its own copy of modprobe.d
    if command -v update-initramfs >/dev/null; then
        info "Updating the initramfs"
        update-initramfs -u
    else
        warn "update-initramfs not found: rebuild the initramfs, or the option may be ignored at boot"
    fi
fi

# Create state directory
info "Creating state directory: $STATE_DIR"
mkdir -p "$STATE_DIR"

# Install systemd service
info "Installing systemd service"
cat > "$SERVICE_PATH" <<'EOF'
[Unit]
Description=Fan curve, thermal settings keeper and event logger
After=multi-user.target

[Service]
Type=simple
ExecStart=/usr/local/bin/thermal-governor
Restart=always
RestartSec=5
StandardOutput=journal
StandardError=journal

[Install]
WantedBy=multi-user.target
EOF

# Reload and enable
systemctl daemon-reload
systemctl enable "$SERVICE_NAME"
systemctl start "$SERVICE_NAME"

echo ""
info "Installation complete!"
echo ""
echo "  Service: systemctl status $SERVICE_NAME"
echo "  Logs:    journalctl -u $SERVICE_NAME -f"
echo "  State:   $STATE_DIR/settings.json"
echo ""

# Verify
if systemctl is-active --quiet "$SERVICE_NAME"; then
    info "Service is running"
else
    warn "Service failed to start — check: journalctl -u $SERVICE_NAME -e"
fi
