#!/usr/bin/env bash
#
# Removes what packaging/install.sh installed: both LaunchAgents and the app
# bundle. Extensions, enabled state and per-extension data in
# ~/Library/Application Support/kennel are kept -- pass --purge to delete those
# and the logs too.
set -euo pipefail

APP="${KENNEL_APP_DIR:-$HOME/Applications}/Kennel.app"
AGENTS="$HOME/Library/LaunchAgents"
LOGS="$HOME/Library/Logs/kennel"
SUPPORT="$HOME/Library/Application Support/kennel"
DOMAIN="gui/$(id -u)"

purge=false
case "${1:-}" in
    --purge) purge=true ;;
    -h | --help)
        echo "usage: packaging/uninstall.sh [--purge]"
        echo "  --purge  also delete $SUPPORT and $LOGS"
        exit 0
        ;;
    "") ;;
    *)
        echo "unknown argument: $1" >&2
        exit 1
        ;;
esac

for label in com.max.kennel-gui com.max.kenneld; do
    launchctl bootout "$DOMAIN/$label" 2>/dev/null || true
    rm -f "$AGENTS/$label.plist"
done
pkill -f "$APP/Contents/MacOS/kennel-gui" 2>/dev/null || true
rm -rf "$APP"
rm -f "$SUPPORT/control.sock" "$SUPPORT/gui.sock"
echo "removed $APP and both LaunchAgents"

if $purge; then
    rm -rf "$SUPPORT" "$LOGS"
    echo "removed $SUPPORT and $LOGS"
else
    echo "kept $SUPPORT (extensions, enabled state, data) and $LOGS -- delete with --purge"
fi
