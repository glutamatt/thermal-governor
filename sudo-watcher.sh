#!/usr/bin/env bash
# Run as: sudo bash sudo-watcher.sh
# Claude feeds commands by writing to /tmp/thermal-governor-cmd
CMD_FILE="/tmp/thermal-governor-cmd"
rm -f "$CMD_FILE"
echo "Waiting for commands in $CMD_FILE ..."
while true; do
    if [[ -f "$CMD_FILE" ]]; then
        echo "── $(date +%H:%M:%S) running ──"
        cat "$CMD_FILE"
        echo "────────────────────"
        bash "$CMD_FILE" 2>&1
        echo "── done (exit $?) ──"
        rm -f "$CMD_FILE"
    fi
    sleep 0.5
done
