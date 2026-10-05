#!/usr/bin/env bash
#
# Installs kennel on this machine:
#   * builds release `kenneld` and `kennel-gui`
#   * assembles them into ~/Applications/Kennel.app
#   * registers two LaunchAgents: the daemon (KeepAlive) and the tray GUI
#   * starts both, then proves the daemon actually answers its control socket
#
# Re-running this is the upgrade path: it stops what's running, replaces the
# bundle, and starts the new one. Extensions, enabled state and per-extension
# data in ~/Library/Application Support/kennel are never touched.
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
APP="${KENNEL_APP_DIR:-$HOME/Applications}/Kennel.app"
AGENTS="$HOME/Library/LaunchAgents"
LOGS="$HOME/Library/Logs/kennel"
SUPPORT="$HOME/Library/Application Support/kennel"
DAEMON_LABEL="com.max.kenneld"
GUI_LABEL="com.max.kennel-gui"
DOMAIN="gui/$(id -u)"

if [[ "${1:-}" == "-h" || "${1:-}" == "--help" ]]; then
    sed -n '3,11p' "${BASH_SOURCE[0]}" | cut -c 3-
    echo
    echo "usage: packaging/install.sh"
    echo "  KENNEL_APP_DIR=<dir>  install the bundle somewhere other than ~/Applications"
    exit 0
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
    echo "kennel is macOS-only (launchd, menu bar tray)." >&2
    exit 1
fi

step() { printf '\n==> %s\n' "$1"; }

step "Building release binaries"
(cd "$REPO" && cargo build --release -p kennel-daemon -p kennel-gui -p kennel-barprobe)

step "Stopping anything already running"
# Both agents go first: the bundle is about to be deleted from under them, and
# replacing a *running* signed binary in place is what makes macOS respawn-loop
# it on OS_REASON_CODESIGNING.
launchctl bootout "$DOMAIN/$GUI_LABEL" 2>/dev/null || true
launchctl bootout "$DOMAIN/$DAEMON_LABEL" 2>/dev/null || true
pkill -f "$APP/Contents/MacOS/kennel-gui" 2>/dev/null || true
pkill -f "$APP/Contents/MacOS/kenneld" 2>/dev/null || true

step "Installing $APP"
VERSION="$(sed -n 's/^version = "\(.*\)"$/\1/p' "$REPO/crates/kennel-gui/Cargo.toml" | head -1)"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$AGENTS" "$LOGS" "$SUPPORT/extensions"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleIdentifier</key>
    <string>com.max.kennel</string>
    <key>CFBundleName</key>
    <string>Kennel</string>
    <key>CFBundleExecutable</key>
    <string>kennel-gui</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>${VERSION}</string>
    <key>CFBundleVersion</key>
    <string>${VERSION}</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <!-- kennel lives in the menu bar: no Dock icon, no window forced open at
         login, and closing the window leaves the tray icon watching. -->
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
PLIST
install -m 0755 "$REPO/target/release/kenneld" "$APP/Contents/MacOS/kenneld"
install -m 0755 "$REPO/target/release/kennel-gui" "$APP/Contents/MacOS/kennel-gui"
# sketchybar-watchdog spawns this to ask the window server whether a bar is
# actually on screen; see extensions-src/sketchybar-watchdog.
install -m 0755 "$REPO/target/release/kennel-barprobe" "$APP/Contents/MacOS/kennel-barprobe"
# kenneld is launched by launchd directly, and kennel-barprobe is exec'd by
# kenneld, so both need a signature of their own rather than just the bundle
# seal they would get as plain resources.
codesign -f -s - "$APP/Contents/MacOS/kenneld"
codesign -f -s - "$APP/Contents/MacOS/kennel-barprobe"
codesign -f -s - "$APP"

step "Registering LaunchAgents"
cat > "$AGENTS/$DAEMON_LABEL.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${DAEMON_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${APP}/Contents/MacOS/kenneld</string>
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>${LOGS}/kenneld.out.log</string>
    <key>StandardErrorPath</key>
    <string>${LOGS}/kenneld.err.log</string>
</dict>
</plist>
PLIST
# The GUI is deliberately not KeepAlive: "Quit Kennel" in the tray menu must
# stay quit until the next login (or the next open of Kennel.app), while the
# daemon keeps watching regardless.
cat > "$AGENTS/$GUI_LABEL.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>${GUI_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>${APP}/Contents/MacOS/kennel-gui</string>
        <string>--tray</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>${LOGS}/kennel-gui.out.log</string>
    <key>StandardErrorPath</key>
    <string>${LOGS}/kennel-gui.err.log</string>
</dict>
</plist>
PLIST
launchctl bootstrap "$DOMAIN" "$AGENTS/$DAEMON_LABEL.plist"
launchctl bootstrap "$DOMAIN" "$AGENTS/$GUI_LABEL.plist"

step "Verifying"
socket="$SUPPORT/control.sock"
for _ in $(seq 1 20); do
    if [[ -S "$socket" ]] && reply="$(printf '"List"\n' | nc -U -w 2 "$socket" 2>/dev/null)" && [[ "$reply" == *'"Extensions"'* ]]; then
        extensions="$(printf '%s' "$reply" | tr ',' '\n' | grep -c '"name"' || true)"
        echo "kenneld is answering $socket ($extensions extension(s) registered)"
        break
    fi
    sleep 0.5
done
if [[ "${reply:-}" != *'"Extensions"'* ]]; then
    echo "kenneld did not answer its control socket; see $LOGS/kenneld.err.log" >&2
    exit 1
fi

if summary="$("$APP/Contents/MacOS/kennel-barprobe" 2>/dev/null | head -1)"; then
    echo "kennel-barprobe sees: $summary"
else
    echo "kennel-barprobe could not read the window server; sketchybar-watchdog will report" >&2
    echo "  'cannot tell whether the bar is up' and will never restart anything." >&2
fi

if pgrep -qf "$APP/Contents/MacOS/kennel-gui"; then
    echo "kennel-gui is running in the menu bar"
else
    echo "kennel-gui did not start; see $LOGS/kennel-gui.err.log" >&2
    exit 1
fi

if [[ -e /usr/local/bin/kenneld ]]; then
    echo
    echo "note: /usr/local/bin/kenneld is left over from the pre-bundle install and is no longer used."
    echo "      remove it with: sudo rm /usr/local/bin/kenneld"
fi

if [[ "$(defaults read NSGlobalDomain _HIHideMenuBar 2>/dev/null || echo 0)" == "1" ]]; then
    echo
    echo "note: this Mac hides the menu bar (_HIHideMenuBar), so the kennel icon is only visible"
    echo "      while the menu bar is revealed. Opening $APP works regardless,"
    echo "      and sketchybar can surface the icon permanently with:"
    echo "          sketchybar --add alias 'Kennel,Item-0' right"
fi

cat <<DONE

kennel is installed.
  window    closing it hides kennel in the menu bar; reopen from the tray menu
            ("Open Kennel") or by opening $APP
  quit      "Quit Kennel" in the tray menu, or ⌘Q -- the daemon keeps watching
  daemon    $DAEMON_LABEL   (KeepAlive, starts at login)
  gui       $GUI_LABEL   (starts at login, straight into the tray)
  logs      $LOGS
  data      $SUPPORT
  uninstall packaging/uninstall.sh
DONE
