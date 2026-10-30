# Using kennel

kennel runs as two parts:

- `kenneld` is a background daemon that runs your extensions on a schedule.
- `kennel-gui` is a menu bar app that shows their health and installs new ones.

Both are installed together into `~/Applications/Kennel.app` and start at login.

## Install

You need macOS and a Rust toolchain (`rustup`). The installer builds from source:

```bash
git clone https://github.com/Max-Levitskiy/kennel
cd kennel
packaging/install.sh
```

The script does the following:

1. Builds `kenneld` and `kennel-gui`.
2. Assembles `Kennel.app`.
3. Registers two LaunchAgents:
   - `com.max.kenneld` is always kept running.
   - `com.max.kennel-gui` starts straight into the menu bar.
4. Checks that the daemon answers on its control socket.

Set `KENNEL_APP_DIR=<dir>` to put the app somewhere other than `~/Applications`.

**Upgrade:** pull, then run `packaging/install.sh` again. It stops both agents, replaces the app and starts it again. Your extensions, their data, and which ones are enabled are kept.

**Uninstall:** `packaging/uninstall.sh` removes the app and the LaunchAgents. Add `--purge` to also delete `~/Library/Application Support/kennel` and the logs.

## The menu bar app

The menu bar icon is a doghouse:

| Color | Meaning |
|---|---|
| green | every enabled extension is healthy |
| red | at least one is unhealthy or erroring (the tooltip shows how many) |
| gray | `kenneld` isn't running, so nothing is being watched |

Click it and choose **Open** to show the window. Closing the window only hides it; **Quit** in the icon's menu exits. If your menu bar is set to auto-hide, the icon only shows while the bar is revealed. Opening `Kennel.app` always works.

The window has three tabs:

- **Installed** lists every extension the daemon has loaded, with its last status. Tick the checkbox to enable an extension, untick it to disable it.
  - Enabling an extension that asks for host capabilities first shows **Allow …?**, which lists what it will be allowed to do.
  - If it needs root (`privileged_spawn`), the dialog also shows the exact sudoers rule you have to install yourself. kennel never writes sudoers.
- **Browse** installs extensions from a store:
  1. Pick a store URL and click **Fetch**.
  2. Click **Install** on an entry. kennel downloads `monitor.wasm` and `manifest.toml`, checks both against the sha256 hashes in the store index, and refuses the install if either doesn't match.
  3. A newly installed extension starts out **disabled**. Enable it in **Installed**.
- **Settings** manages the list of store URLs. The default store is [kennel-extensions](https://github.com/Max-Levitskiy/kennel-extensions). To use your own, add the URL of its `index.toml` (see [Publishing](writing-extensions.md#publishing)).

## Updating an installed extension

Installing a new version, from **Browse** or by copying files, replaces the files on disk. The daemon keeps running the version it already loaded until it restarts. Restart it with:

```bash
launchctl kickstart -k gui/$(id -u)/com.max.kenneld
```

Enabled extensions stay enabled across the restart.

## Where things live

| Path | What |
|---|---|
| `~/Applications/Kennel.app` | `kenneld`, `kennel-gui` |
| `~/Library/Application Support/kennel/extensions/<name>/` | `manifest.toml`, `monitor.wasm`, and `data/` (the extension's `state` storage) |
| `~/Library/Application Support/kennel/state.json` | which extensions are enabled |
| `~/Library/Application Support/kennel/gui-config.json` | store URLs |
| `~/Library/Application Support/kennel/control.sock` | the daemon's control socket |
| `~/Library/Application Support/kennel/subscriptions.toml` | your [subscriptions](#subscriptions-run-a-command-on-an-event) |
| `~/Library/Application Support/kennel/events.jsonl`, `runs.jsonl` | accepted events and the run log |
| `~/Library/Logs/kennel/kenneld.out.log` | extension `log()` output and daemon events |
| `~/Library/Logs/kennel/kenneld.err.log` | extensions the daemon skipped, and why |
| `~/Library/Logs/kennel/kennel-gui.{out,err}.log` | the menu bar app |

## Without the GUI: the control socket

The daemon speaks newline-delimited JSON on its control socket, so `nc` is enough to drive it:

```bash
SOCK="$HOME/Library/Application Support/kennel/control.sock"
echo '"List"'                               | nc -U -w 2 "$SOCK"  # extensions, enabled flag, last status
echo '"Rescan"'                             | nc -U -w 2 "$SOCK"  # load new extensions, reload subscriptions.toml
echo '{"Enable":{"name":"sd-keepalive"}}'   | nc -U -w 2 "$SOCK"
echo '{"Disable":{"name":"sd-keepalive"}}'  | nc -U -w 2 "$SOCK"
```

To install an extension by hand:

1. Create `~/Library/Application Support/kennel/extensions/<name>/`.
2. Put its `monitor.wasm` and `manifest.toml` in that directory.
3. Send `"Rescan"`, then `Enable` it.

`Enable` over the socket skips the GUI's capability prompt, so read the extension's `manifest.toml` first.

## Subscriptions: run a command on an event

Some extensions emit **events**, e.g. "a meeting's notes are ready". The `List` response shows which ones, under each extension's `manifest.events`. A **subscription** runs a shell command of yours every time an event of that type arrives.

There is no GUI for subscriptions yet; manage them over the control socket:

```bash
SOCK="$HOME/Library/Application Support/kennel/control.sock"

# Create one. kenneld assigns the id and answers with the saved subscription.
echo '{"PutSubscription":{"subscription":{"extension":"fellow-watcher","event":"note_ready","command":"jq -r .title >> ~/meetings.txt"}}}' | nc -U -w 2 "$SOCK"

echo '"ListSubscriptions"' | nc -U -w 2 "$SOCK"
echo '"ListEvents"'        | nc -U -w 2 "$SOCK"   # events kenneld accepted, with their payloads
echo '"ListRuns"'          | nc -U -w 2 "$SOCK"   # newest first: status, exit code, output tail

# Change one: send it back with its id. Delete one by id.
echo '{"PutSubscription":{"subscription":{"id":"sub-…","extension":"fellow-watcher","event":"note_ready","command":"…","timeout_secs":120,"enabled":false}}}' | nc -U -w 2 "$SOCK"
echo '{"DeleteSubscription":{"id":"sub-…"}}' | nc -U -w 2 "$SOCK"
```

A subscription has these fields:

| Field | Default | Meaning |
|---|---|---|
| `id` | assigned | Leave it out to create a subscription. |
| `extension`, `event` | required | Which event type it's on. Both must exist: the extension is installed and declares that event type. |
| `command` | required | A shell command line. |
| `timeout_secs` | 600 | Past this, the command and everything it started are killed. |
| `enabled` | `true` | A disabled subscription starts no runs. |

**How a command runs:**

- It runs as `$SHELL -lc '<command>'`, i.e. your login shell, so your normal `PATH` (Homebrew, `uv`, `gh`, …) works. The working directory is your home directory.
- The event payload arrives as JSON on **stdin**. It never appears in the command line, so odd characters in a payload can't inject anything.
- Three environment variables are set: `KENNEL_EXTENSION`, `KENNEL_EVENT` and `KENNEL_EVENT_ID`.
- Each event runs a command at most once. kenneld drops event ids it has already accepted from that extension, even across restarts.
- One subscription runs one command at a time, in event order. Different subscriptions run in parallel.
- Every run is recorded with its start and finish time, its status (`running`, `ok`, `failed` or `timeout`), its exit code, and the last 64 KiB of its stdout and stderr.
- Failed runs are not retried.

Subscriptions live in `subscriptions.toml`. kenneld owns the file, but you can edit it by hand and send `"Rescan"` to reload it. If the file doesn't parse, the subscriptions already loaded stay in effect and `kenneld.err.log` says what's wrong.

```toml
[[subscription]]
id = "sub-18dcaac139d2dbe8"
extension = "fellow-watcher"
event = "note_ready"
command = "jq -r .title >> ~/meetings.txt"
timeout_secs = 600
enabled = true
```

## Troubleshooting

- **Icon is gray.** The daemon is down. Check `launchctl print gui/$(id -u)/com.max.kenneld` and `kenneld.err.log`.
- **An installed extension doesn't show up.** The daemon refused to load it, and `kenneld.err.log` says why. The usual reasons are:
  - an unreadable `manifest.toml`
  - `interval_secs` below 1
  - a capability list in `manifest.toml` that differs from the one compiled into the wasm
  - a list of event types in `manifest.toml` that differs from the one compiled into the wasm
- **A subscription's command didn't run.** Check `"ListRuns"` for its output tail. Also check `kenneld.err.log`, which says when the extension's emit was rejected. Commands run through your login shell, so a broken `~/.zprofile` breaks every run.
- **Status is `Errored`.** The extension's `check()` crashed (a panic), ran past its 13 s budget, or returned garbage. The detail text says which.
- **An extension stays unhealthy and its fix doesn't seem to run again.** That's on purpose. After a `fix()`, kennel waits at least 60 s (or 5× the extension's interval, whichever is longer) before fixing again, even while `check()` keeps failing.
