# kennel — design spec

Date: 2026-08-19
Status: draft, pending review

## Problem

Several independent LaunchAgent scripts exist to watch and restart broken
things (`cpuwatchdog`, `gdrive-watchdog`, `sd-keepalive`, and now a candidate
sketchybar post-wake restart). Each one required hand-writing a new script,
a new plist, and a new log path, and there's no single place to see whether
everything is actually healthy right now. kennel replaces this pattern with
one daemon that runs pluggable monitors, plus a GUI to manage which monitors
are active without restarting anything.

## Scope

In scope: consolidating the restart-on-failure watchdogs — `cpuwatchdog`,
`gdrive-watchdog`, `sd-keepalive`, and a new sketchybar-post-wake monitor —
into kennel extensions.

Out of scope (stay as separate LaunchAgents): `claude-tmp-cleanup` and
`chezmoi-autosync` (scheduled/triggered maintenance, not restart-on-failure
watchdogs), `voice-reader` (a service, not a watchdog).

## Architecture

- **`kenneld`** — Rust daemon, LaunchAgent-managed (`KeepAlive`), hosts a
  `wasmtime` runtime. Owns a Unix domain socket for control. Runs a
  scheduler that ticks each *enabled* monitor on its own declared interval.
  Only the daemon holds durable state (what's installed, what's enabled,
  last check status per monitor) — this lives in a plain JSON state file,
  no database needed at this scale.
- **GUI app** — native Rust, built with `egui` (+ `eframe`), plus a system
  tray icon (`tray-icon` crate). Connects to `kenneld` over the control
  socket; holds no durable state of its own, purely reflects what the
  daemon reports. Tray icon shows daemon up/down + count of unhealthy
  monitors at a glance. Main window has three tabs:
  - **Installed** — toggle enable/disable live, see last check status per
    monitor.
  - **Browse** — extensions available from configured repos, with an
    Install button.
  - **Settings** — manage the list of repo URLs (default + custom).
- **Extension repos** — plain git repos with an `index.toml` at the root
  listing available extensions (name, version, wasm asset URL, sha256,
  required capabilities). Default repo is Max's public GitHub repo; any
  other repo URL of the same shape can be added as a custom source.
- **Installed extensions** live locally under
  `~/Library/Application Support/kennel/extensions/<name>/` as
  `manifest.toml` + `monitor.wasm`.

## Plugin contract

Each monitor is a single `.wasm` module. It exports:

- `manifest() -> Manifest` — name, version, description, interval_secs,
  required_capabilities.
- `check() -> Status` — `Healthy` or `Unhealthy(detail: String)`.
- `fix()` — called only when `check()` reports unhealthy; performs the
  actual recovery action (restart a process, kick a LaunchAgent, etc.).

The host exposes these imports, each gated on the extension having declared
(and the user having approved) the matching capability:

- `spawn(cmd, args) -> (exit_code, stdout, stderr)`
- `launchctl(action, label)`
- `read_file(path) -> bytes`
- `log(level, msg)`
- `notify(title, body)` — macOS user notification

A monitor with no declared capabilities can still run `check()`/`fix()`,
it just can't call any host import — useful for pure in-wasm logic, though
in practice every real watchdog here needs at least `spawn` and/or
`launchctl`.

## Extension manifest & repo index format

`manifest.toml` (bundled with each extension):

```toml
name = "sd-keepalive"
version = "0.1.0"
description = "Keeps the GL9755 SD reader from link-parking on idle"
interval_secs = 30
capabilities = ["spawn", "read_file"]
```

`index.toml` (repo root):

```toml
[[extensions]]
name = "sd-keepalive"
version = "0.1.0"
wasm_url = "https://github.com/<user>/kennel-extensions/releases/download/v0.1.0/sd-keepalive.wasm"
manifest_url = "https://raw.githubusercontent.com/<user>/kennel-extensions/main/sd-keepalive/manifest.toml"
sha256 = "..."
manifest_sha256 = "..."
```
`manifest_sha256` is required, not just `sha256` for the wasm binary — the manifest is what actually grants capabilities, and hashing only the wasm would let a compromised index pin an audited binary while serving a manifest that grants extra permissions. Added during implementation (final review finding I1) after the schema above was first written without it.

## Data flow

1. **Startup**: daemon reads its state file, loads each *enabled* `.wasm`
   into wasmtime, starts the scheduler.
2. **Enable/disable**: GUI sends a socket command; daemon loads/drops that
   one wasm instance live, updates the state file. Nothing else restarts.
3. **Install**: GUI fetches `index.toml` from each configured repo over
   HTTPS, lists results in Browse. On install, downloads the wasm asset,
   verifies sha256, writes `manifest.toml` + `monitor.wasm` locally. If the
   manifest declares capabilities, GUI shows a permission prompt before the
   daemon is allowed to enable it.
4. **Check/fix cycle**: scheduler calls `check()` per enabled monitor on
   its interval; if unhealthy, calls `fix()`, logs the event, updates
   last-status.
5. **Status**: daemon pushes state-changed events over the same long-lived
   socket connection; GUI just renders what it's told.

## Error handling

- **Plugin panic/trap**: wasmtime catches it, monitor marked `Errored`
  (distinct from `Unhealthy`), daemon keeps running — this is the actual
  payoff of wasm sandboxing over a native dlopen plugin, where a plugin
  crash would take the host down with it.
- **Plugin hang**: wasmtime fuel/epoch-based execution limit kills the call
  after a timeout, marked `Errored`, backs off instead of retrying tightly.
- **Daemon crash**: LaunchAgent `KeepAlive` restarts it; state file tells
  it exactly what was enabled, so it resumes without user action.
- **GUI closed**: daemon keeps running headless; GUI reconnects and
  resyncs state on reopen.
- **Repo fetch fails**: Browse tab shows the last cached index plus an
  error banner; Installed tab is unaffected.
- **sha256 mismatch on download**: refuse to install, surface an error.

## Testing

- Daemon core (scheduler, state transitions, socket protocol): standard
  Rust tests against a fake plugin trait — no wasm involved.
- Plugin host: integration tests using a handful of minimal fixture
  `.wasm` files (always-healthy, unhealthy-then-fixed-by-`fix()`, panics,
  hangs) to prove isolation and the timeout actually hold.
- Ported monitors (`cpuwatchdog`, `gdrive-watchdog`, `sd-keepalive`,
  sketchybar): logic ported into wasm, smoke-tested manually against real
  system state since they touch real OS/hardware behavior that's
  impractical to fully simulate in CI.

## Open questions for implementation planning

- Exact socket wire protocol (line-delimited JSON vs. a framed format).
- Whether the default GitHub repo is the same repo as kennel's source or a
  separate `kennel-extensions` repo (index above assumes separate).
- egui theming/visual design of the store UI — not decided yet, deferred
  to implementation.
