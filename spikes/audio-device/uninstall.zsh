#!/bin/zsh
# Removes everything install.zsh put in place. Run with sudo.
set -euo pipefail
[[ $EUID == 0 ]] || { echo "run with sudo"; exit 1 }
label=nl.stormlantern.aetherdesk.audio-spike
launchctl bootout system/$label 2>/dev/null || true
rm -f /Library/LaunchDaemons/$label.plist
rm -rf "/Library/Application Support/AetherDesk/audio-spike"
rm -rf /Library/Audio/Plug-Ins/HAL/AetherDesk.driver
killall coreaudiod
echo "removed"
