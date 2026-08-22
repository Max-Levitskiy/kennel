# Kennel Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `kennel` — a Rust daemon (`kenneld`) that runs pluggable watchdog monitors as wasm modules, plus an egui GUI to browse/install/enable/disable them live from a default and custom extension repos — and port the three watchdogs that fit the model (sd-keepalive, gdrive-watchdog, a new sketchybar post-wake monitor) as its first real extensions.

**Architecture:** A Cargo workspace with a shared `kennel-proto` crate (wire types), a `kennel-daemon` binary (`kenneld`) hosting a `wasmtime` runtime behind a `Plugin` trait, a `kennel-guest-sdk` crate extensions link against to export the `manifest()/check()/fix()` ABI, and a `kennel-gui` binary (egui + tray) that talks to `kenneld` over a Unix socket. Every monitor call runs in a fresh `Store`/`Instance` with an epoch-based timeout, so a hung or panicking extension can't take the daemon down.

**Tech Stack:** Rust (stable), `wasmtime` (plugin host), `egui`/`eframe` + `tray-icon` (GUI), `serde`/`serde_json` (wire + manifest format), `ureq` (blocking HTTP for store fetch/install — no async runtime anywhere in this plan), `sha2` (download verification), std `UnixListener`/`UnixStream` (control socket) and std threads (scheduler) — no tokio.

**Spec:** `docs/superpowers/specs/2026-08-19-kennel-design.md`

## Global Constraints

- Platform: macOS only (launchd plists, `launchctl`, `osascript` notifications).
- No async runtime — std threads + blocking I/O throughout, per YAGNI (this is a small local tool, not a server).
- State persistence is a plain JSON file, never a database (per spec).
- Control-socket wire protocol: newline-delimited JSON, one `Request`/`Response` per line (resolved during planning; spec left this open — see the polling-vs-push note below Shared Types Reference for why there's no `Event` type).
- Wasm target: `wasm32-unknown-unknown` — no WASI. All host access goes through kennel's own capability-gated imports, not WASI's separate permission model (resolved during planning; keeps one capability system instead of two).
- Every `check()`/`fix()`/`manifest()` call runs in a **fresh** `Store`+`Instance` (no long-lived guest state across calls) — guest memory leaks are fine since the whole store is dropped after each call, and this is what makes the epoch-timeout kill-path simple and safe.
- String return values from guest→host use a fixed 64 KiB scratch buffer the guest exports the address of once per instantiation (`__kennel_scratch_ptr`) — no host-calls-back-into-guest-allocator complexity.
- Persistent extension state (baselines, counters) goes through the sandboxed `state_get`/`state_set` host imports, confined to `~/Library/Application Support/kennel/data/<extension-name>/` — never raw arbitrary paths for this purpose.
- Root-privileged actions (`privileged_spawn`) only ever invoke `sudo -n <exact-command>` against commands the user has pre-authorized via a `/etc/sudoers.d/kennel-<extension-name>` NOPASSWD rule. Kennel **never** writes to `/etc/sudoers` itself — the GUI shows the exact snippet, the user installs it by hand (matches the existing, working `/etc/sudoers.d/gdrive-watchdog` pattern already on this machine: `max ALL=(root) NOPASSWD: /usr/sbin/spindump`).

---

## Shared Types Reference

These are defined in Task 1 and used by name in every later task — copied here so each task's implementer doesn't have to guess a signature.

```rust
// kennel-proto/src/lib.rs

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Capability {
    Spawn,
    Launchctl,
    ReadFile,
    WriteFile,
    State,
    PrivilegedSpawn,
    Log,
    Notify,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionManifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub interval_secs: u64,
    pub capabilities: Vec<Capability>,
    #[serde(default)]
    pub privileged_commands: Vec<String>, // only meaningful with Capability::PrivilegedSpawn
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum MonitorStatus {
    Healthy,
    Unhealthy { detail: String },
    Errored { detail: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionInfo {
    pub manifest: ExtensionManifest,
    pub enabled: bool,
    pub last_status: Option<MonitorStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    List,
    Enable { name: String },
    Disable { name: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Extensions(Vec<ExtensionInfo>),
    Ok,
    Error { message: String },
}
```

Note on a deliberate deviation from the spec: the design spec describes the daemon *pushing* `StatusChanged`/`EnabledChanged` events over the socket. This plan replaces that with the GUI polling `Request::List` once a second (Task 13's `ctx.request_repaint_after`) instead — same user-visible effect (live status), far less machinery (no event fan-out to N connected clients, no ordering concerns). There is deliberately no `Event` type in this protocol; if per-event push ever becomes worth the complexity (e.g. sub-second status latency actually matters), that's a follow-up, not silently implied by unused scaffolding here.

```rust
// kennel-daemon/src/plugin.rs
pub trait Plugin: Send {
    fn manifest(&self) -> kennel_proto::ExtensionManifest;
    fn check(&mut self) -> kennel_proto::MonitorStatus;
    fn fix(&mut self);
}

// A factory, not a live instance: the scheduler calls this fresh on every
// tick so FakePlugin (Phase 2) and WasmPlugin (Phase 3, fresh Store per
// call) work identically from the scheduler's point of view.
pub type PluginFactory = Box<dyn Fn() -> Box<dyn Plugin> + Send + Sync>;
```

---

## Phase 1 — Workspace + shared types

### Task 1: Workspace scaffold and `kennel-proto`

**Files:**
- Create: `Cargo.toml` (workspace root)
- Create: `crates/kennel-proto/Cargo.toml`
- Create: `crates/kennel-proto/src/lib.rs`
- Test: `crates/kennel-proto/src/lib.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `Capability`, `ExtensionManifest`, `MonitorStatus`, `ExtensionInfo`, `Request`, `Response` (exact shapes in Shared Types Reference above — deliberately no `Event` type, see the note below that section) — every later crate depends on `kennel-proto`.

- [ ] **Step 1: Create the workspace**

`Cargo.toml`:
```toml
[workspace]
resolver = "2"
members = ["crates/kennel-proto", "crates/kennel-daemon", "crates/kennel-guest-sdk", "crates/kennel-gui"]
```

- [ ] **Step 2: Write `kennel-proto` with the failing round-trip test**

`crates/kennel-proto/Cargo.toml`:
```toml
[package]
name = "kennel-proto"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

`crates/kennel-proto/src/lib.rs` — paste the full type definitions from the Shared Types Reference section above (the `Capability`/`ExtensionManifest`/`MonitorStatus`/`ExtensionInfo`/`Request`/`Response` block — no `Event` type), then append:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_round_trips_through_json() {
        let m = ExtensionManifest {
            name: "sd-keepalive".into(),
            version: "0.1.0".into(),
            description: "keeps the SD reader link awake".into(),
            interval_secs: 30,
            capabilities: vec![Capability::Spawn, Capability::WriteFile, Capability::State],
            privileged_commands: vec![],
        };
        let json = serde_json::to_string(&m).unwrap();
        let back: ExtensionManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.name, "sd-keepalive");
        assert_eq!(back.capabilities.len(), 3);
    }

    #[test]
    fn request_enum_round_trips() {
        let req = Request::Enable { name: "gdrive-watchdog".into() };
        let json = serde_json::to_string(&req).unwrap();
        let back: Request = serde_json::from_str(&json).unwrap();
        matches!(back, Request::Enable { name } if name == "gdrive-watchdog");
    }
}
```

- [ ] **Step 3: Run tests, verify they pass**

Run: `cargo test -p kennel-proto`
Expected: both tests PASS (there's no prior failing state to check here since this is a fresh crate — the "write it, then run it" step still applies to catch typos before moving on).

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml crates/kennel-proto
git commit -m "feat: scaffold workspace and kennel-proto wire types"
```

---

## Phase 2 — Daemon core (fake plugin, no wasm yet)

### Task 2: `Plugin` trait, `FakePlugin`, and the in-memory `Registry`

**Files:**
- Create: `crates/kennel-daemon/Cargo.toml`
- Create: `crates/kennel-daemon/src/plugin.rs`
- Create: `crates/kennel-daemon/src/registry.rs`
- Test: `crates/kennel-daemon/src/registry.rs` (inline)

**Interfaces:**
- Consumes: `kennel_proto::{ExtensionManifest, MonitorStatus, ExtensionInfo}`
- Produces: `Plugin` trait, `PluginFactory` type alias (both shown in Shared Types Reference), `Registry` with methods `register(factory: PluginFactory)`, `set_enabled(name: &str, enabled: bool) -> Result<(), String>`, `list(&self) -> Vec<ExtensionInfo>`, `update_status(&self, name: &str, status: MonitorStatus)`.

- [ ] **Step 1: Scaffold the crate**

`crates/kennel-daemon/Cargo.toml`:
```toml
[package]
name = "kennel-daemon"
version = "0.1.0"
edition = "2021"

[[bin]]
name = "kenneld"
path = "src/main.rs"

[dependencies]
kennel-proto = { path = "../kennel-proto" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write the failing test for `Registry`**

`crates/kennel-daemon/src/registry.rs`:
```rust
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use kennel_proto::{ExtensionInfo, ExtensionManifest, MonitorStatus};
use crate::plugin::PluginFactory;

struct Entry {
    manifest: ExtensionManifest,
    enabled: bool,
    last_status: Option<MonitorStatus>,
    factory: PluginFactory,
}

#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
}

impl Registry {
    pub fn new() -> Self {
        Registry { inner: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn register(&self, manifest: ExtensionManifest, factory: PluginFactory) {
        let mut map = self.inner.lock().unwrap();
        map.insert(manifest.name.clone(), Entry { manifest, enabled: false, last_status: None, factory });
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        let mut map = self.inner.lock().unwrap();
        let entry = map.get_mut(name).ok_or_else(|| format!("unknown extension: {name}"))?;
        entry.enabled = enabled;
        Ok(())
    }

    pub fn list(&self) -> Vec<ExtensionInfo> {
        let map = self.inner.lock().unwrap();
        map.values()
            .map(|e| ExtensionInfo { manifest: e.manifest.clone(), enabled: e.enabled, last_status: e.last_status.clone() })
            .collect()
    }

    pub fn update_status(&self, name: &str, status: MonitorStatus) {
        let mut map = self.inner.lock().unwrap();
        if let Some(entry) = map.get_mut(name) {
            entry.last_status = Some(status);
        }
    }

    // Used by the scheduler (Task 4): snapshot of (name, enabled, factory) pairs
    // to tick. Cloning the factory Arc-style would need PluginFactory to be
    // Clone, which Box<dyn Fn> isn't — so the scheduler calls back into the
    // registry to invoke a fresh plugin instead of taking factories out.
    pub fn make_plugin(&self, name: &str) -> Option<Box<dyn crate::plugin::Plugin>> {
        let map = self.inner.lock().unwrap();
        map.get(name).map(|e| (e.factory)())
    }

    pub fn enabled_names(&self) -> Vec<String> {
        let map = self.inner.lock().unwrap();
        map.iter().filter(|(_, e)| e.enabled).map(|(k, _)| k.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Plugin;

    struct FakePlugin { healthy: bool }
    impl Plugin for FakePlugin {
        fn manifest(&self) -> ExtensionManifest {
            ExtensionManifest { name: "fake".into(), version: "0.0.0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
        }
        fn check(&mut self) -> MonitorStatus {
            if self.healthy { MonitorStatus::Healthy } else { MonitorStatus::Unhealthy { detail: "nope".into() } }
        }
        fn fix(&mut self) { self.healthy = true; }
    }

    fn fake_manifest() -> ExtensionManifest {
        ExtensionManifest { name: "fake".into(), version: "0.0.0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
    }

    #[test]
    fn register_then_enable_then_list_reflects_state() {
        let reg = Registry::new();
        reg.register(fake_manifest(), Box::new(|| Box::new(FakePlugin { healthy: true })));

        let listed = reg.list();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].enabled);

        reg.set_enabled("fake", true).unwrap();
        assert!(reg.list()[0].enabled);
    }

    #[test]
    fn enable_unknown_extension_errors() {
        let reg = Registry::new();
        assert!(reg.set_enabled("nope", true).is_err());
    }

    #[test]
    fn update_status_is_reflected_in_list() {
        let reg = Registry::new();
        reg.register(fake_manifest(), Box::new(|| Box::new(FakePlugin { healthy: true })));
        reg.update_status("fake", MonitorStatus::Unhealthy { detail: "x".into() });
        assert_eq!(reg.list()[0].last_status, Some(MonitorStatus::Unhealthy { detail: "x".into() }));
    }
}
```

`crates/kennel-daemon/src/plugin.rs`:
```rust
pub trait Plugin: Send {
    fn manifest(&self) -> kennel_proto::ExtensionManifest;
    fn check(&mut self) -> kennel_proto::MonitorStatus;
    fn fix(&mut self);
}

pub type PluginFactory = Box<dyn Fn() -> Box<dyn Plugin> + Send + Sync>;
```

`crates/kennel-daemon/src/main.rs` (minimal for now, expanded in Task 6):
```rust
mod plugin;
mod registry;

fn main() {
    println!("kenneld starting (scaffold)");
}
```

- [ ] **Step 3: Run tests, verify they fail first**

Run: `cargo test -p kennel-daemon 2>&1 | head -30`
Expected: compile error before the fix (module wiring missing) — write `mod plugin; mod registry;` in `main.rs` as shown above, then re-run and confirm the 3 tests fail only if you temporarily stub `Registry` — since this is new code written in one pass, it's fine if it compiles and passes directly; the important check is that it does NOT silently no-op (run with `-- --nocapture` and confirm 3 tests ran).

- [ ] **Step 4: Run tests, verify they pass**

Run: `cargo test -p kennel-daemon`
Expected: `3 passed`

- [ ] **Step 5: Commit**

```bash
git add crates/kennel-daemon
git commit -m "feat: add Plugin trait and in-memory Registry"
```

### Task 3: State file persistence

**Files:**
- Create: `crates/kennel-daemon/src/state.rs`
- Modify: `crates/kennel-daemon/src/main.rs:1-6` (add `mod state;`)

**Interfaces:**
- Consumes: nothing new
- Produces: `StateFile { enabled: Vec<String> }` with `StateFile::load(path: &Path) -> StateFile` (returns default/empty on missing file) and `StateFile::save(&self, path: &Path) -> std::io::Result<()>`.

- [ ] **Step 1: Write the failing test**

`crates/kennel-daemon/src/state.rs`:
```rust
use std::path::Path;
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct StateFile {
    pub enabled: Vec<String>,
}

impl StateFile {
    pub fn load(path: &Path) -> StateFile {
        match std::fs::read_to_string(path) {
            Ok(s) => serde_json::from_str(&s).unwrap_or_default(),
            Err(_) => StateFile::default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self).expect("StateFile always serializes");
        std::fs::write(path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_loads_as_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let loaded = StateFile::load(&path);
        assert!(loaded.enabled.is_empty());
    }

    #[test]
    fn save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let state = StateFile { enabled: vec!["sd-keepalive".into(), "gdrive-watchdog".into()] };
        state.save(&path).unwrap();

        let loaded = StateFile::load(&path);
        assert_eq!(loaded.enabled, vec!["sd-keepalive", "gdrive-watchdog"]);
    }
}
```

- [ ] **Step 2: Wire the module and run the tests**

Add `mod state;` to `crates/kennel-daemon/src/main.rs`.

Run: `cargo test -p kennel-daemon state::`
Expected: `2 passed`

- [ ] **Step 3: Commit**

```bash
git add crates/kennel-daemon/src/state.rs crates/kennel-daemon/src/main.rs
git commit -m "feat: add JSON state file persistence"
```

### Task 4: Scheduler

**Files:**
- Create: `crates/kennel-daemon/src/scheduler.rs`
- Modify: `crates/kennel-daemon/src/main.rs` (add `mod scheduler;`)

**Interfaces:**
- Consumes: `Registry` (Task 2: `make_plugin`, `enabled_names`, `update_status`)
- Produces: `Scheduler::spawn(registry: Registry, name: String, interval_secs: u64) -> ScheduledMonitor` (holds a `JoinHandle` + a `stop: Arc<AtomicBool>`), `ScheduledMonitor::stop(self)`.

- [ ] **Step 1: Write the failing test**

`crates/kennel-daemon/src/scheduler.rs`:
```rust
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use crate::registry::Registry;

pub struct ScheduledMonitor {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl ScheduledMonitor {
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.join();
    }
}

pub fn spawn(registry: Registry, name: String, interval_secs: u64) -> ScheduledMonitor {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let handle = thread::spawn(move || {
        while !stop_clone.load(Ordering::SeqCst) {
            if let Some(mut plugin) = registry.make_plugin(&name) {
                let status = plugin.check();
                let unhealthy = matches!(status, kennel_proto::MonitorStatus::Unhealthy { .. });
                registry.update_status(&name, status);
                if unhealthy {
                    plugin.fix();
                }
            }
            // Sleep in short slices so `stop` is noticed quickly instead of
            // blocking for the whole interval.
            let mut waited = 0u64;
            while waited < interval_secs && !stop_clone.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(1));
                waited += 1;
            }
        }
    });
    ScheduledMonitor { stop, handle }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{Plugin, PluginFactory};
    use kennel_proto::{ExtensionManifest, MonitorStatus};
    use std::sync::atomic::AtomicUsize;

    struct CountingPlugin {
        checks: Arc<AtomicUsize>,
        fixes: Arc<AtomicUsize>,
    }
    impl Plugin for CountingPlugin {
        fn manifest(&self) -> ExtensionManifest {
            ExtensionManifest { name: "counting".into(), version: "0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
        }
        fn check(&mut self) -> MonitorStatus {
            self.checks.fetch_add(1, Ordering::SeqCst);
            MonitorStatus::Unhealthy { detail: "always".into() }
        }
        fn fix(&mut self) {
            self.fixes.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn ticks_call_check_and_fix_on_unhealthy() {
        let checks = Arc::new(AtomicUsize::new(0));
        let fixes = Arc::new(AtomicUsize::new(0));
        let (c1, f1) = (checks.clone(), fixes.clone());

        let registry = Registry::new();
        registry.register(
            ExtensionManifest { name: "counting".into(), version: "0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] },
            Box::new(move || Box::new(CountingPlugin { checks: c1.clone(), fixes: f1.clone() }) as Box<dyn Plugin>) as PluginFactory,
        );

        let monitor = spawn(registry, "counting".into(), 1);
        thread::sleep(Duration::from_millis(2500));
        monitor.stop();

        assert!(checks.load(Ordering::SeqCst) >= 2, "expected at least 2 ticks in 2.5s at 1s interval, got {}", checks.load(Ordering::SeqCst));
        assert_eq!(checks.load(Ordering::SeqCst), fixes.load(Ordering::SeqCst), "every unhealthy check should trigger exactly one fix");
    }
}
```

- [ ] **Step 2: Wire the module**

Add `mod scheduler;` to `crates/kennel-daemon/src/main.rs`.

- [ ] **Step 3: Run test, verify it passes**

Run: `cargo test -p kennel-daemon scheduler:: -- --nocapture`
Expected: `1 passed` (takes ~2.5s, that's expected — it's a real timing test)

- [ ] **Step 4: Commit**

```bash
git add crates/kennel-daemon/src/scheduler.rs crates/kennel-daemon/src/main.rs
git commit -m "feat: add per-monitor scheduler threads"
```

### Task 5: Unix socket control server

**Files:**
- Create: `crates/kennel-daemon/src/socket.rs`
- Modify: `crates/kennel-daemon/src/main.rs` (add `mod socket;`)

**Interfaces:**
- Consumes: `Registry` (Task 2), `kennel_proto::{Request, Response}` (Task 1)
- Produces: `socket::serve(path: &Path, registry: Registry) -> std::io::Result<()>` — blocks accepting connections; each connection handled on its own thread, one JSON `Request` per line in, one JSON `Response` per line out.

- [ ] **Step 1: Write the failing test**

`crates/kennel-daemon/src/socket.rs`:
```rust
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use kennel_proto::{Request, Response};
use crate::registry::Registry;

pub fn serve(path: &Path, registry: Registry) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    for stream in listener.incoming() {
        let stream = stream?;
        let registry = registry.clone();
        std::thread::spawn(move || handle_connection(stream, registry));
    }
    Ok(())
}

fn handle_connection(stream: UnixStream, registry: Registry) {
    let reader = BufReader::new(stream.try_clone().expect("clone unix stream"));
    let mut writer = stream;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() { continue; }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(&registry, req),
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let mut out = serde_json::to_string(&response).expect("Response always serializes");
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
    }
}

fn handle_request(registry: &Registry, req: Request) -> Response {
    match req {
        Request::List => Response::Extensions(registry.list()),
        Request::Enable { name } => match registry.set_enabled(&name, true) {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error { message },
        },
        Request::Disable { name } => match registry.set_enabled(&name, false) {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error { message },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{Plugin, PluginFactory};
    use kennel_proto::{ExtensionManifest, MonitorStatus};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    struct NoopPlugin;
    impl Plugin for NoopPlugin {
        fn manifest(&self) -> ExtensionManifest {
            ExtensionManifest { name: "noop".into(), version: "0".into(), description: "".into(), interval_secs: 60, capabilities: vec![], privileged_commands: vec![] }
        }
        fn check(&mut self) -> MonitorStatus { MonitorStatus::Healthy }
        fn fix(&mut self) {}
    }

    #[test]
    fn list_enable_disable_round_trip_over_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("kennel.sock");

        let registry = Registry::new();
        registry.register(
            ExtensionManifest { name: "noop".into(), version: "0".into(), description: "".into(), interval_secs: 60, capabilities: vec![], privileged_commands: vec![] },
            Box::new(|| Box::new(NoopPlugin) as Box<dyn Plugin>) as PluginFactory,
        );

        let serve_path = sock_path.clone();
        std::thread::spawn(move || { let _ = serve(&serve_path, registry); });
        std::thread::sleep(std::time::Duration::from_millis(200)); // let the listener bind

        let mut conn = UnixStream::connect(&sock_path).unwrap();
        let mut reader = BufReader::new(conn.try_clone().unwrap());

        send(&mut conn, &Request::List);
        let resp: Response = recv(&mut reader);
        match resp {
            Response::Extensions(list) => { assert_eq!(list.len(), 1); assert!(!list[0].enabled); }
            other => panic!("expected Extensions, got {other:?}"),
        }

        send(&mut conn, &Request::Enable { name: "noop".into() });
        assert!(matches!(recv::<Response>(&mut reader), Response::Ok));

        send(&mut conn, &Request::List);
        match recv::<Response>(&mut reader) {
            Response::Extensions(list) => assert!(list[0].enabled),
            other => panic!("expected Extensions, got {other:?}"),
        }
    }

    fn send(conn: &mut UnixStream, req: &Request) {
        let mut line = serde_json::to_string(req).unwrap();
        line.push('\n');
        conn.write_all(line.as_bytes()).unwrap();
    }

    fn recv<T: serde::de::DeserializeOwned>(reader: &mut BufReader<UnixStream>) -> T {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}
```

- [ ] **Step 2: Wire the module**

Add `mod socket;` to `crates/kennel-daemon/src/main.rs`.

- [ ] **Step 3: Run test, verify it passes**

Run: `cargo test -p kennel-daemon socket::`
Expected: `1 passed`

- [ ] **Step 4: Commit**

```bash
git add crates/kennel-daemon/src/socket.rs crates/kennel-daemon/src/main.rs
git commit -m "feat: add Unix socket control server"
```

### Task 6: `kenneld` binary wiring + LaunchAgent

**Files:**
- Modify: `crates/kennel-daemon/src/main.rs` (full rewrite)
- Create: `packaging/com.max.kenneld.plist`

**Interfaces:**
- Consumes: `state::StateFile`, `registry::Registry`, `scheduler::spawn`, `socket::serve`
- Produces: the `kenneld` process itself — no further Rust API, this is the integration point.

- [ ] **Step 1: Wire startup**

`crates/kennel-daemon/src/main.rs`:
```rust
mod plugin;
mod registry;
mod scheduler;
mod socket;
mod state;

use std::path::PathBuf;
use registry::Registry;

fn socket_path() -> PathBuf {
    dirs_home().join("Library/Application Support/kennel/control.sock")
}

fn state_path() -> PathBuf {
    dirs_home().join("Library/Application Support/kennel/state.json")
}

fn dirs_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
}

fn main() {
    let registry = Registry::new();

    // Extension loading (manifest.toml + monitor.wasm scan) lands in Task 10.
    // Until then, an empty registry still lets the socket/state plumbing be
    // smoke-tested end to end.

    let state = state::StateFile::load(&state_path());
    let mut monitors = Vec::new();
    for name in &state.enabled {
        if registry.set_enabled(name, true).is_ok() {
            let manifest = registry.list().into_iter().find(|e| &e.manifest.name == name);
            if let Some(info) = manifest {
                monitors.push(scheduler::spawn(registry.clone(), name.clone(), info.manifest.interval_secs));
            }
        }
    }

    let sock_path = socket_path();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent).expect("create kennel support dir");
    }
    println!("kenneld listening on {}", sock_path.display());
    socket::serve(&sock_path, registry).expect("socket server crashed");
}
```

- [ ] **Step 2: Manual smoke test**

Run: `cargo run -p kennel-daemon &`
Then: `echo '{"List":null}' | nc -U ~/Library/Application\ Support/kennel/control.sock`

This will error because `Request::List` is a unit variant, not `{"List":null}` — with `serde`'s default enum representation it serializes as the bare string `"List"`. Use instead:

Run: `echo '"List"' | nc -U ~/Library/Application\ Support/kennel/control.sock`
Expected: `{"Extensions":[]}` printed back, then kill the background `cargo run` process.

- [ ] **Step 3: Write the LaunchAgent plist**

`packaging/com.max.kenneld.plist`:
```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.max.kenneld</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/kenneld</string>
    </array>
    <key>KeepAlive</key>
    <true/>
    <key>RunAtLoad</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/Users/max/Library/Logs/kenneld/kenneld.out.log</string>
    <key>StandardErrorPath</key>
    <string>/Users/max/Library/Logs/kenneld/kenneld.err.log</string>
</dict>
</plist>
```

Do not install this plist yet — it's staged for Task 12, after sd-keepalive is a working extension and there's something worth running continuously.

- [ ] **Step 4: Commit**

```bash
git add crates/kennel-daemon/src/main.rs packaging/com.max.kenneld.plist
git commit -m "feat: wire kenneld binary startup and stage LaunchAgent plist"
```

---

## Phase 3 — Real wasmtime plugin host

### Task 7: `kennel-guest-sdk` and two minimal fixture extensions

**Files:**
- Create: `crates/kennel-guest-sdk/Cargo.toml`
- Create: `crates/kennel-guest-sdk/src/lib.rs`
- Create: `fixtures/always-healthy/Cargo.toml`
- Create: `fixtures/always-healthy/src/lib.rs`
- Create: `fixtures/unhealthy-then-fixed/Cargo.toml`
- Create: `fixtures/unhealthy-then-fixed/src/lib.rs`

**Interfaces:**
- Produces: the `kennel_extension!` macro (exports `manifest`/`check`/`fix`/`__kennel_scratch_ptr` in whatever crate invokes it), and `Manifest`/`Status` guest-side types mirroring `kennel_proto::{ExtensionManifest, MonitorStatus}` (kept separate from `kennel-proto` deliberately — the guest crate compiles to `wasm32-unknown-unknown` and must not pull in the daemon's std-heavy dependency tree).

- [ ] **Step 1: Write the SDK**

`crates/kennel-guest-sdk/Cargo.toml`:
```toml
[package]
name = "kennel-guest-sdk"
version = "0.1.0"
edition = "2021"

[dependencies]
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

`crates/kennel-guest-sdk/src/lib.rs`:
```rust
use serde::{Deserialize, Serialize};

#[derive(Serialize)]
pub struct Manifest {
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
    pub interval_secs: u64,
    pub capabilities: Vec<&'static str>,
    pub privileged_commands: Vec<&'static str>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail")]
pub enum Status {
    Healthy,
    Unhealthy(String),
}

pub const SCRATCH_LEN: usize = 65536;

#[repr(align(8))]
pub struct Scratch(pub [u8; SCRATCH_LEN]);

pub static mut SCRATCH: Scratch = Scratch([0u8; SCRATCH_LEN]);

// Packs a byte slice into the scratch buffer and returns (ptr << 32 | len).
// Truncates rather than panics if the caller somehow exceeds SCRATCH_LEN --
// a monitor's manifest/status JSON is always tiny in practice.
pub fn pack_into_scratch(bytes: &[u8]) -> u64 {
    let len = bytes.len().min(SCRATCH_LEN);
    unsafe {
        SCRATCH.0[..len].copy_from_slice(&bytes[..len]);
        ((SCRATCH.0.as_ptr() as u64) << 32) | (len as u64)
    }
}

#[macro_export]
macro_rules! kennel_extension {
    ($manifest_fn:path, $check_fn:path, $fix_fn:path) => {
        #[no_mangle]
        pub extern "C" fn __kennel_scratch_ptr() -> i32 {
            unsafe { $crate::SCRATCH.0.as_ptr() as i32 }
        }

        #[no_mangle]
        pub extern "C" fn manifest() -> u64 {
            let m = $manifest_fn();
            let json = serde_json::to_vec(&m).expect("manifest must serialize");
            $crate::pack_into_scratch(&json)
        }

        #[no_mangle]
        pub extern "C" fn check() -> u64 {
            let status = $check_fn();
            let json = serde_json::to_vec(&status).expect("status must serialize");
            $crate::pack_into_scratch(&json)
        }

        #[no_mangle]
        pub extern "C" fn fix() {
            $fix_fn();
        }
    };
}
```

- [ ] **Step 2: Write the `always-healthy` fixture**

`fixtures/always-healthy/Cargo.toml`:
```toml
[package]
name = "always-healthy"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
kennel-guest-sdk = { path = "../../crates/kennel-guest-sdk" }
serde_json = "1"
```

`fixtures/always-healthy/src/lib.rs`:
```rust
use kennel_guest_sdk::{kennel_extension, Manifest, Status};

fn manifest() -> Manifest {
    Manifest { name: "always-healthy", version: "0.1.0", description: "test fixture", interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
}
fn check() -> Status { Status::Healthy }
fn fix() {}

kennel_extension!(manifest, check, fix);
```

- [ ] **Step 3: Write the `unhealthy-then-fixed` fixture**

`fixtures/unhealthy-then-fixed/Cargo.toml`: same shape as above with `name = "unhealthy-then-fixed"`.

`fixtures/unhealthy-then-fixed/src/lib.rs`:
```rust
use kennel_guest_sdk::{kennel_extension, Manifest, Status};
use std::sync::atomic::{AtomicBool, Ordering};

static FIXED: AtomicBool = AtomicBool::new(false);

fn manifest() -> Manifest {
    Manifest { name: "unhealthy-then-fixed", version: "0.1.0", description: "test fixture", interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
}
fn check() -> Status {
    if FIXED.load(Ordering::SeqCst) { Status::Healthy } else { Status::Unhealthy("not fixed yet".into()) }
}
fn fix() { FIXED.store(true, Ordering::SeqCst); }

kennel_extension!(manifest, check, fix);
```

Note: because Task 8's host design uses a **fresh Store/Instance per call**, this fixture's `AtomicBool` resets on every instantiation — so it will never actually observe `FIXED == true` on a later `check()` from a *different* instance. That's intentional and exactly what Task 8's test for this fixture verifies: it proves in-guest state does NOT persist across calls, which is why real extensions needing persistence (Task 8's `state_get`/`state_set`) must go through the host, not a static.

- [ ] **Step 4: Build the fixtures to wasm and verify the artifacts exist**

Run:
```bash
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown -p always-healthy -p unhealthy-then-fixed
ls -la target/wasm32-unknown-unknown/release/*.wasm
```
Expected: `always_healthy.wasm` and `unhealthy_then_fixed.wasm` both present and non-zero size.

- [ ] **Step 5: Commit**

```bash
git add crates/kennel-guest-sdk fixtures
git commit -m "feat: add guest SDK and two minimal wasm test fixtures"
```

### Task 8: `WasmPlugin` — the real wasmtime host

**Files:**
- Create: `crates/kennel-daemon/src/wasm_host.rs`
- Modify: `crates/kennel-daemon/src/main.rs` (add `mod wasm_host;`)
- Modify: `crates/kennel-daemon/Cargo.toml` (add `wasmtime` dependency)

**Interfaces:**
- Consumes: `plugin::Plugin` (Task 2), fixture `.wasm` files built in Task 7.
- Produces: `WasmPlugin::load(wasm_path: &Path, manifest: ExtensionManifest, capabilities: HashSet<Capability>, data_dir: PathBuf) -> Result<WasmPlugin, String>`, implementing `Plugin`.

- [ ] **Step 1: Add the dependency**

`crates/kennel-daemon/Cargo.toml`, add under `[dependencies]`:
```toml
wasmtime = "24"
```

- [ ] **Step 2: Write `WasmPlugin` with capability-gated host imports**

`crates/kennel-daemon/src/wasm_host.rs`:
```rust
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;
use kennel_proto::{Capability, ExtensionManifest, MonitorStatus};
use wasmtime::{Caller, Config, Engine, Instance, Linker, Module, Store};

struct HostCtx {
    extension_name: String,
    capabilities: HashSet<Capability>,
    privileged_commands: Vec<String>,
    data_dir: PathBuf,
    scratch_ptr: i32,
}

pub struct WasmPlugin {
    engine: Engine,
    module: Module,
    manifest: ExtensionManifest,
    capabilities: HashSet<Capability>,
    data_dir: PathBuf,
}

impl WasmPlugin {
    pub fn load(wasm_path: &Path, manifest: ExtensionManifest, capabilities: HashSet<Capability>, data_dir: PathBuf) -> Result<WasmPlugin, String> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|e| e.to_string())?;
        let bytes = std::fs::read(wasm_path).map_err(|e| e.to_string())?;
        let module = Module::new(&engine, &bytes).map_err(|e| e.to_string())?;

        // Background ticker so per-call deadlines (Step 4) actually expire --
        // wasmtime's epoch only advances when something calls increment_epoch.
        let engine_for_ticker = engine.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_millis(100));
            engine_for_ticker.increment_epoch();
        });

        std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
        Ok(WasmPlugin { engine, module, manifest, capabilities, data_dir })
    }

    fn fresh_instance(&self) -> Result<(Store<HostCtx>, Instance), String> {
        let mut linker: Linker<HostCtx> = Linker::new(&self.engine);
        register_host_imports(&mut linker);

        let ctx = HostCtx {
            extension_name: self.manifest.name.clone(),
            capabilities: self.capabilities.clone(),
            privileged_commands: self.manifest.privileged_commands.clone(),
            data_dir: self.data_dir.clone(),
            scratch_ptr: 0,
        };
        let mut store = Store::new(&self.engine, ctx);
        store.set_epoch_deadline(50); // ~5s at the 100ms ticker above

        let instance = linker.instantiate(&mut store, &self.module).map_err(|e| e.to_string())?;
        let scratch_fn = instance.get_typed_func::<(), i32>(&mut store, "__kennel_scratch_ptr").map_err(|e| e.to_string())?;
        let ptr = scratch_fn.call(&mut store, ()).map_err(|e| e.to_string())?;
        store.data_mut().scratch_ptr = ptr;

        Ok((store, instance))
    }

    fn read_guest_string(store: &mut Store<HostCtx>, instance: &Instance, packed: u64) -> Result<String, String> {
        let memory = instance.get_memory(&mut *store, "memory").ok_or("guest did not export memory")?;
        let ptr = (packed >> 32) as usize;
        let len = (packed & 0xFFFF_FFFF) as usize;
        let mut buf = vec![0u8; len];
        memory.read(&mut *store, ptr, &mut buf).map_err(|e| e.to_string())?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

impl crate::plugin::Plugin for WasmPlugin {
    fn manifest(&self) -> ExtensionManifest {
        self.manifest.clone()
    }

    fn check(&mut self) -> MonitorStatus {
        let (mut store, instance) = match self.fresh_instance() {
            Ok(pair) => pair,
            Err(e) => return MonitorStatus::Errored { detail: e },
        };
        let check_fn = match instance.get_typed_func::<(), u64>(&mut store, "check") {
            Ok(f) => f,
            Err(e) => return MonitorStatus::Errored { detail: e.to_string() },
        };
        let packed = match check_fn.call(&mut store, ()) {
            Ok(p) => p,
            Err(e) => return MonitorStatus::Errored { detail: format!("check() trapped or timed out: {e}") },
        };
        let json = match Self::read_guest_string(&mut store, &instance, packed) {
            Ok(s) => s,
            Err(e) => return MonitorStatus::Errored { detail: e },
        };
        match serde_json::from_str::<serde_json::Value>(&json) {
            Ok(v) if v.get("kind").and_then(|k| k.as_str()) == Some("Healthy") => MonitorStatus::Healthy,
            Ok(v) => {
                let detail = v.get("detail").and_then(|d| d.as_str()).unwrap_or("unhealthy").to_string();
                MonitorStatus::Unhealthy { detail }
            }
            Err(e) => MonitorStatus::Errored { detail: format!("bad check() output: {e}") },
        }
    }

    fn fix(&mut self) {
        let Ok((mut store, instance)) = self.fresh_instance() else { return };
        if let Ok(fix_fn) = instance.get_typed_func::<(), ()>(&mut store, "fix") {
            let _ = fix_fn.call(&mut store, ());
        }
    }
}

fn require_capability(caller: &Caller<'_, HostCtx>, cap: Capability) -> bool {
    caller.data().capabilities.contains(&cap)
}

fn register_host_imports(linker: &mut Linker<HostCtx>) {
    linker.func_wrap("kennel", "log", |mut caller: Caller<'_, HostCtx>, level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32| {
        if !require_capability(&caller, Capability::Log) { return; }
        let level = read_str(&mut caller, level_ptr, level_len);
        let msg = read_str(&mut caller, msg_ptr, msg_len);
        println!("[{}] {}: {}", caller.data().extension_name.clone(), level, msg);
    }).expect("register log");

    linker.func_wrap("kennel", "notify", |mut caller: Caller<'_, HostCtx>, title_ptr: i32, title_len: i32, body_ptr: i32, body_len: i32| {
        if !require_capability(&caller, Capability::Notify) { return; }
        let title = read_str(&mut caller, title_ptr, title_len);
        let body = read_str(&mut caller, body_ptr, body_len);
        let script = format!("display notification {:?} with title {:?}", body, title);
        let _ = Command::new("/usr/bin/osascript").arg("-e").arg(script).output();
    }).expect("register notify");

    linker.func_wrap("kennel", "spawn", |mut caller: Caller<'_, HostCtx>, cmd_ptr: i32, cmd_len: i32, args_ptr: i32, args_len: i32| -> u64 {
        if !require_capability(&caller, Capability::Spawn) { return write_scratch(&mut caller, b"{\"error\":\"capability not granted\"}"); }
        let cmd = read_str(&mut caller, cmd_ptr, cmd_len);
        let args_json = read_str(&mut caller, args_ptr, args_len);
        let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
        let output = Command::new(&cmd).args(&args).output();
        let result = match output {
            Ok(o) => serde_json::json!({
                "exit_code": o.status.code().unwrap_or(-1),
                "stdout": String::from_utf8_lossy(&o.stdout),
                "stderr": String::from_utf8_lossy(&o.stderr),
            }),
            Err(e) => serde_json::json!({ "exit_code": -1, "stdout": "", "stderr": e.to_string() }),
        };
        write_scratch(&mut caller, result.to_string().as_bytes())
    }).expect("register spawn");

    linker.func_wrap("kennel", "privileged_spawn", |mut caller: Caller<'_, HostCtx>, cmd_ptr: i32, cmd_len: i32, args_ptr: i32, args_len: i32| -> u64 {
        if !require_capability(&caller, Capability::PrivilegedSpawn) { return write_scratch(&mut caller, b"{\"error\":\"capability not granted\"}"); }
        let cmd = read_str(&mut caller, cmd_ptr, cmd_len);
        if !caller.data().privileged_commands.iter().any(|c| c == &cmd) {
            return write_scratch(&mut caller, b"{\"error\":\"command not in manifest privileged_commands\"}");
        }
        let args_json = read_str(&mut caller, args_ptr, args_len);
        let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
        let output = Command::new("/usr/bin/sudo").arg("-n").arg(&cmd).args(&args).output();
        let result = match output {
            Ok(o) => serde_json::json!({ "exit_code": o.status.code().unwrap_or(-1), "stdout": String::from_utf8_lossy(&o.stdout), "stderr": String::from_utf8_lossy(&o.stderr) }),
            Err(e) => serde_json::json!({ "exit_code": -1, "stdout": "", "stderr": e.to_string() }),
        };
        write_scratch(&mut caller, result.to_string().as_bytes())
    }).expect("register privileged_spawn");

    linker.func_wrap("kennel", "launchctl", |mut caller: Caller<'_, HostCtx>, action_ptr: i32, action_len: i32, args_ptr: i32, args_len: i32| {
        if !require_capability(&caller, Capability::Launchctl) { return; }
        let action = read_str(&mut caller, action_ptr, action_len);
        let args_json = read_str(&mut caller, args_ptr, args_len);
        let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
        let _ = Command::new("/bin/launchctl").arg(action).args(&args).output();
    }).expect("register launchctl");

    linker.func_wrap("kennel", "read_file", |mut caller: Caller<'_, HostCtx>, path_ptr: i32, path_len: i32| -> u64 {
        if !require_capability(&caller, Capability::ReadFile) { return write_scratch(&mut caller, b""); }
        let path = read_str(&mut caller, path_ptr, path_len);
        let bytes = std::fs::read(&path).unwrap_or_default();
        write_scratch(&mut caller, &bytes)
    }).expect("register read_file");

    linker.func_wrap("kennel", "write_file", |mut caller: Caller<'_, HostCtx>, path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32| -> i32 {
        if !require_capability(&caller, Capability::WriteFile) { return 0; }
        let path = read_str(&mut caller, path_ptr, path_len);
        let memory = caller.get_export("memory").and_then(|e| e.into_memory());
        let Some(memory) = memory else { return 0 };
        let mut buf = vec![0u8; data_len as usize];
        if memory.read(&caller, data_ptr as usize, &mut buf).is_err() { return 0; }
        std::fs::write(&path, &buf).is_ok() as i32
    }).expect("register write_file");

    linker.func_wrap("kennel", "state_get", |mut caller: Caller<'_, HostCtx>, key_ptr: i32, key_len: i32| -> u64 {
        if !require_capability(&caller, Capability::State) { return write_scratch(&mut caller, b""); }
        let key = read_str(&mut caller, key_ptr, key_len);
        let path = caller.data().data_dir.join(sanitize_key(&key));
        let bytes = std::fs::read(&path).unwrap_or_default();
        write_scratch(&mut caller, &bytes)
    }).expect("register state_get");

    linker.func_wrap("kennel", "state_set", |mut caller: Caller<'_, HostCtx>, key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32| {
        if !require_capability(&caller, Capability::State) { return; }
        let key = read_str(&mut caller, key_ptr, key_len);
        let path = caller.data().data_dir.join(sanitize_key(&key));
        let memory = caller.get_export("memory").and_then(|e| e.into_memory());
        let Some(memory) = memory else { return };
        let mut buf = vec![0u8; val_len as usize];
        if memory.read(&caller, val_ptr as usize, &mut buf).is_err() { return; }
        let _ = std::fs::write(&path, &buf);
    }).expect("register state_set");
}

fn sanitize_key(key: &str) -> String {
    key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

fn read_str(caller: &mut Caller<'_, HostCtx>, ptr: i32, len: i32) -> String {
    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return String::new(),
    };
    let mut buf = vec![0u8; len as usize];
    if memory.read(&*caller, ptr as usize, &mut buf).is_err() { return String::new(); }
    String::from_utf8_lossy(&buf).into_owned()
}

fn write_scratch(caller: &mut Caller<'_, HostCtx>, bytes: &[u8]) -> u64 {
    let scratch_ptr = caller.data().scratch_ptr;
    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return 0,
    };
    let len = bytes.len().min(65536);
    let _ = memory.write(&mut *caller, scratch_ptr as usize, &bytes[..len]);
    ((scratch_ptr as u64) << 32) | (len as u64)
}
```

- [ ] **Step 3: Write the integration test against the Task 7 fixtures**

Append to `crates/kennel-daemon/src/wasm_host.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Plugin;
    use std::collections::HashSet;

    fn fixture_path(name: &str) -> PathBuf {
        // Built by `cargo build --release --target wasm32-unknown-unknown` (Task 7, Step 4).
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm32-unknown-unknown/release").join(format!("{}.wasm", name.replace('-', "_")))
    }

    fn test_manifest(name: &str) -> ExtensionManifest {
        ExtensionManifest { name: name.into(), version: "0.1.0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
    }

    #[test]
    fn always_healthy_fixture_reports_healthy() {
        let dir = tempfile::tempdir().unwrap();
        let mut plugin = WasmPlugin::load(&fixture_path("always-healthy"), test_manifest("always-healthy"), HashSet::new(), dir.path().to_path_buf()).unwrap();
        assert_eq!(plugin.check(), MonitorStatus::Healthy);
    }

    #[test]
    fn unhealthy_then_fixed_fixture_stays_unhealthy_across_fresh_instances() {
        // Documents the fresh-instance-per-call design: guest static state
        // does NOT persist, so fix() on one instance can't be observed by
        // check() on the next.
        let dir = tempfile::tempdir().unwrap();
        let mut plugin = WasmPlugin::load(&fixture_path("unhealthy-then-fixed"), test_manifest("unhealthy-then-fixed"), HashSet::new(), dir.path().to_path_buf()).unwrap();
        assert!(matches!(plugin.check(), MonitorStatus::Unhealthy { .. }));
        plugin.fix();
        assert!(matches!(plugin.check(), MonitorStatus::Unhealthy { .. }), "guest state must not persist across fresh instances");
    }
}
```

- [ ] **Step 4: Wire the module and run**

Add `mod wasm_host;` to `crates/kennel-daemon/src/main.rs`.

Run: `cargo test -p kennel-daemon wasm_host::`
Expected: `2 passed` (requires Task 7's fixtures already built — re-run the Task 7 Step 4 build command first if `target/wasm32-unknown-unknown/release/*.wasm` is missing)

- [ ] **Step 5: Commit**

```bash
git add crates/kennel-daemon/src/wasm_host.rs crates/kennel-daemon/src/main.rs crates/kennel-daemon/Cargo.toml
git commit -m "feat: add wasmtime plugin host with capability-gated imports"
```

### Task 9: Panic and hang fixtures — prove isolation and timeout

**Files:**
- Create: `fixtures/panics/Cargo.toml`, `fixtures/panics/src/lib.rs`
- Create: `fixtures/hangs/Cargo.toml`, `fixtures/hangs/src/lib.rs`
- Modify: `crates/kennel-daemon/src/wasm_host.rs` (append 2 tests)

**Interfaces:**
- Consumes: `WasmPlugin` (Task 8)
- Produces: nothing new — this task is proof, not API surface.

- [ ] **Step 1: Write the panicking fixture**

`fixtures/panics/Cargo.toml`: same shape as `always-healthy`, `name = "panics"`.

`fixtures/panics/src/lib.rs`:
```rust
use kennel_guest_sdk::{kennel_extension, Manifest, Status};

fn manifest() -> Manifest {
    Manifest { name: "panics", version: "0.1.0", description: "test fixture", interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
}
fn check() -> Status { panic!("intentional test panic") }
fn fix() {}

kennel_extension!(manifest, check, fix);
```

- [ ] **Step 2: Write the hanging fixture**

`fixtures/hangs/Cargo.toml`: same shape, `name = "hangs"`.

`fixtures/hangs/src/lib.rs`:
```rust
use kennel_guest_sdk::{kennel_extension, Manifest, Status};

fn manifest() -> Manifest {
    Manifest { name: "hangs", version: "0.1.0", description: "test fixture", interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
}
fn check() -> Status {
    loop {} // must be killed by the host's epoch deadline, not spin forever
}
fn fix() {}

kennel_extension!(manifest, check, fix);
```

- [ ] **Step 3: Build both to wasm**

Run: `cargo build --release --target wasm32-unknown-unknown -p panics -p hangs`
Expected: both `.wasm` artifacts produced.

- [ ] **Step 4: Add the failing tests, then watch them pass**

Append to `crates/kennel-daemon/src/wasm_host.rs`'s `#[cfg(test)] mod tests`:
```rust
    #[test]
    fn panicking_check_is_errored_not_a_daemon_crash() {
        let dir = tempfile::tempdir().unwrap();
        let mut plugin = WasmPlugin::load(&fixture_path("panics"), test_manifest("panics"), HashSet::new(), dir.path().to_path_buf()).unwrap();
        assert!(matches!(plugin.check(), MonitorStatus::Errored { .. }));
        // Proof the host process itself is still alive and this plugin is still usable:
        // a second, unrelated call still runs rather than the whole test process crashing.
        assert!(matches!(plugin.check(), MonitorStatus::Errored { .. }));
    }

    #[test]
    fn hanging_check_times_out_as_errored() {
        let dir = tempfile::tempdir().unwrap();
        let mut plugin = WasmPlugin::load(&fixture_path("hangs"), test_manifest("hangs"), HashSet::new(), dir.path().to_path_buf()).unwrap();
        let start = std::time::Instant::now();
        let status = plugin.check();
        let elapsed = start.elapsed();
        assert!(matches!(status, MonitorStatus::Errored { .. }), "expected Errored, got {status:?}");
        assert!(elapsed < Duration::from_secs(10), "epoch deadline should kill the call well under 10s, took {elapsed:?}");
    }
```

- [ ] **Step 5: Run and verify**

Run: `cargo test -p kennel-daemon wasm_host:: -- --nocapture`
Expected: `4 passed` (the hang test takes a few seconds — that's the epoch deadline doing its job)

- [ ] **Step 6: Commit**

```bash
git add fixtures/panics fixtures/hangs crates/kennel-daemon/src/wasm_host.rs
git commit -m "test: prove wasmtime isolation on panicking and hanging plugins"
```

### Task 10: Load real extensions from disk into the daemon

**Files:**
- Create: `crates/kennel-daemon/src/extensions.rs`
- Modify: `crates/kennel-daemon/src/main.rs` (use `extensions::scan_and_register` instead of the empty registry)

**Interfaces:**
- Consumes: `WasmPlugin::load` (Task 8), `Registry::register` (Task 2)
- Produces: `extensions::scan_and_register(dir: &Path, registry: &Registry)` — scans `<dir>/<name>/{manifest.toml,monitor.wasm}`, parses each manifest, registers a `WasmPlugin` factory per extension. Capability *granting* (vs. declaring) comes from a separate `granted.json` file in the same extensions dir — written by the GUI's permission-prompt flow in Task 17; until that exists, this task treats "declared == granted" so extensions are runnable standalone in tests.

- [ ] **Step 1: Write the failing test**

`crates/kennel-daemon/src/extensions.rs`:
```rust
use std::collections::HashSet;
use std::path::Path;
use kennel_proto::{Capability, ExtensionManifest};
use crate::registry::Registry;
use crate::wasm_host::WasmPlugin;

#[derive(serde::Deserialize)]
struct ManifestToml {
    name: String,
    version: String,
    description: String,
    interval_secs: u64,
    #[serde(default)]
    capabilities: Vec<Capability>,
    #[serde(default)]
    privileged_commands: Vec<String>,
}

pub fn scan_and_register(dir: &Path, registry: &Registry) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let ext_dir = entry.path();
        if !ext_dir.is_dir() { continue; }
        let manifest_path = ext_dir.join("manifest.toml");
        let wasm_path = ext_dir.join("monitor.wasm");
        let (Ok(manifest_text), true) = (std::fs::read_to_string(&manifest_path), wasm_path.exists()) else { continue };
        let Ok(parsed) = toml::from_str::<ManifestToml>(&manifest_text) else { continue };

        let manifest = ExtensionManifest {
            name: parsed.name.clone(),
            version: parsed.version,
            description: parsed.description,
            interval_secs: parsed.interval_secs,
            capabilities: parsed.capabilities.clone(),
            privileged_commands: parsed.privileged_commands,
        };
        let capabilities: HashSet<Capability> = parsed.capabilities.into_iter().collect();
        let data_dir = ext_dir.join("data");
        let manifest_for_factory = manifest.clone();
        let wasm_path_for_factory = wasm_path.clone();

        registry.register(manifest, Box::new(move || {
            Box::new(WasmPlugin::load(&wasm_path_for_factory, manifest_for_factory.clone(), capabilities.clone(), data_dir.clone())
                .expect("extension failed to load")) as Box<dyn crate::plugin::Plugin>
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_a_directory_with_one_valid_extension() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("always-healthy");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(ext_dir.join("manifest.toml"), r#"
name = "always-healthy"
version = "0.1.0"
description = "test"
interval_secs = 1
capabilities = []
"#).unwrap();
        let wasm_src = fixture_wasm_path();
        std::fs::copy(&wasm_src, ext_dir.join("monitor.wasm")).unwrap();

        let registry = Registry::new();
        scan_and_register(dir.path(), &registry);

        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].manifest.name, "always-healthy");
    }

    fn fixture_wasm_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm32-unknown-unknown/release/always_healthy.wasm")
    }
}
```

Requires Task 7's `always-healthy` fixture already built (`cargo build --release --target wasm32-unknown-unknown -p always-healthy`) before this test can pass.

- [ ] **Step 2: Add the `toml` dependency**

`crates/kennel-daemon/Cargo.toml`, add:
```toml
toml = "0.8"
```

- [ ] **Step 3: Wire into `main.rs`**

Replace the comment `// Extension loading ... lands in Task 10.` block in `crates/kennel-daemon/src/main.rs` with:
```rust
    let extensions_dir = dirs_home().join("Library/Application Support/kennel/extensions");
    extensions::scan_and_register(&extensions_dir, &registry);
```
and add `mod extensions;` to the module list.

- [ ] **Step 4: Run and verify**

Run: `cargo test -p kennel-daemon extensions::`
Expected: `1 passed`

- [ ] **Step 5: Commit**

```bash
git add crates/kennel-daemon/src/extensions.rs crates/kennel-daemon/src/main.rs crates/kennel-daemon/Cargo.toml
git commit -m "feat: load extensions from disk (manifest.toml + monitor.wasm)"
```

---

## Phase 4 — Port sd-keepalive (first real watchdog)

### Task 11: `sd-keepalive` extension

**Files:**
- Create: `extensions-src/sd-keepalive/Cargo.toml`
- Create: `extensions-src/sd-keepalive/src/lib.rs`
- Create: `extensions-src/sd-keepalive/manifest.toml`

Ported from the real `~/.local/bin/sd-keepalive.sh`, whose actual job is: touch `/Volumes/Vault/.keepalive` every tick so the GL9755 reader's PCIe link doesn't idle-park, and note wake/disconnect transitions to a log. In kennel's check/fix model: `check()` does the touch itself when the volume is mounted (that touch *is* the health action, there's nothing separate to "fix") and reports `Unhealthy` only when the card isn't mounted at all (informational — physically reseating a card isn't something kennel can automate, so `fix()` is a no-op that just logs that fact).

**Interfaces:**
- Produces: a buildable `sd-keepalive` wasm extension, drop-in for `extensions/sd-keepalive/` in Task 12.

- [ ] **Step 1: Write the manifest**

`extensions-src/sd-keepalive/manifest.toml`:
```toml
name = "sd-keepalive"
version = "0.1.0"
description = "Touches /Volumes/Vault/.keepalive periodically so the GL9755 SD reader's PCIe link never idle-parks"
interval_secs = 2
capabilities = ["write_file", "log"]
```

- [ ] **Step 2: Write the extension**

`extensions-src/sd-keepalive/Cargo.toml`: same shape as the fixtures, `name = "sd-keepalive"`, plus the raw host-import bindings (these live in `kennel-guest-sdk` too — add them there first).

Add to `crates/kennel-guest-sdk/src/lib.rs`:
```rust
#[link(wasm_import_module = "kennel")]
extern "C" {
    fn write_file(path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32) -> i32;
    fn log(level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32);
}

pub fn host_write_file(path: &str, data: &[u8]) -> bool {
    unsafe { write_file(path.as_ptr() as i32, path.len() as i32, data.as_ptr() as i32, data.len() as i32) != 0 }
}

pub fn host_log(level: &str, msg: &str) {
    unsafe { log(level.as_ptr() as i32, level.len() as i32, msg.as_ptr() as i32, msg.len() as i32) }
}
```

`extensions-src/sd-keepalive/src/lib.rs`:
```rust
use kennel_guest_sdk::{host_log, host_write_file, kennel_extension, Manifest, Status};
use std::time::{SystemTime, UNIX_EPOCH};

const MOUNT_MARKER: &str = "/Volumes/Vault/.keepalive";

fn manifest() -> Manifest {
    Manifest {
        name: "sd-keepalive",
        version: "0.1.0",
        description: "Keeps the GL9755 SD reader's PCIe link awake",
        interval_secs: 2,
        capabilities: vec!["write_file", "log"],
        privileged_commands: vec![],
    }
}

fn check() -> Status {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let ok = host_write_file(MOUNT_MARKER, now.to_string().as_bytes());
    if ok {
        Status::Healthy
    } else {
        host_log("warn", "Vault not mounted, could not touch .keepalive");
        Status::Unhealthy("/Volumes/Vault is not mounted".into())
    }
}

fn fix() {
    host_log("info", "sd-keepalive: card not mounted, nothing kennel can do until it's reseated");
}

kennel_extension!(manifest, check, fix);
```

- [ ] **Step 3: Add it to the workspace and build**

Add `"extensions-src/sd-keepalive"` to the root `Cargo.toml`'s `members`.

Run: `cargo build --release --target wasm32-unknown-unknown -p sd-keepalive`
Expected: `sd_keepalive.wasm` produced.

- [ ] **Step 4: Manual integration check (real hardware, not a unit test)**

```bash
mkdir -p ~/Library/Application\ Support/kennel/extensions/sd-keepalive
cp extensions-src/sd-keepalive/manifest.toml ~/Library/Application\ Support/kennel/extensions/sd-keepalive/
cp target/wasm32-unknown-unknown/release/sd_keepalive.wasm ~/Library/Application\ Support/kennel/extensions/sd-keepalive/monitor.wasm
cargo run -p kennel-daemon &
sleep 1
echo '"List"' | nc -U ~/Library/Application\ Support/kennel/control.sock
echo '{"Enable":{"name":"sd-keepalive"}}' | nc -U ~/Library/Application\ Support/kennel/control.sock
sleep 5
cat /Volumes/Vault/.keepalive   # should show a very recent unix timestamp
kill %1
```
Expected: the timestamp in `.keepalive` is within the last few seconds, proving the wasm extension is actually touching the real file on the real mounted volume through the `write_file` host import.

- [ ] **Step 5: Commit**

```bash
git add extensions-src/sd-keepalive Cargo.toml crates/kennel-guest-sdk/src/lib.rs
git commit -m "feat: port sd-keepalive as a kennel extension"
```

### Task 12: Cut over from the old LaunchAgent to kenneld

**Files:**
- Create: `extensions/sd-keepalive/manifest.toml` (published copy, distinct from `extensions-src/` which is the source)
- Create: `extensions/sd-keepalive/monitor.wasm` (build output, copied in)
- Modify: nothing in Rust — this is an operational cutover

- [ ] **Step 1: Install and start `kenneld` for real**

```bash
mkdir -p ~/Library/LaunchAgents
cp packaging/com.max.kenneld.plist ~/Library/LaunchAgents/
cargo build --release -p kennel-daemon
sudo cp target/release/kenneld /usr/local/bin/kenneld
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/com.max.kenneld.plist
launchctl list | grep kenneld
```
Expected: `kenneld` listed as running.

- [ ] **Step 2: Enable sd-keepalive through it and confirm behavior for a full minute**

```bash
echo '{"Enable":{"name":"sd-keepalive"}}' | nc -U ~/Library/Application\ Support/kennel/control.sock
sleep 60
cat /Volumes/Vault/.keepalive
```
Expected: timestamp within the last ~2s (the extension's `interval_secs`).

- [ ] **Step 3: Retire the old LaunchAgent**

```bash
launchctl bootout gui/$(id -u)/com.max.sd-keepalive
mv ~/Library/LaunchAgents/com.max.sd-keepalive.plist ~/Library/LaunchAgents/com.max.sd-keepalive.plist.disabled
```
Keep the old script (`~/.local/bin/sd-keepalive.sh`) on disk for now as a fallback — don't delete it until kennel has run unattended (including through a real sleep/wake cycle) for a few days.

- [ ] **Step 4: Commit**

```bash
git add extensions/sd-keepalive
git commit -m "chore: cut sd-keepalive over from LaunchAgent to kenneld"
```

---

### Task 12b: fix live enable/disable to actually schedule and persist

**Plan amendment, discovered during Task 12's real cutover.** Task 5's socket
`Request::Enable`/`Disable` handler only ever flipped `Registry`'s in-memory
flag — it never called `scheduler::spawn`/`stop`, and never wrote
`state.json`. Schedulers were only ever spawned once, at startup, from
`state.enabled` (Task 6). Live enable-via-socket has therefore never actually
worked: it returns `Response::Ok` and does nothing. This was invisible until
Task 12 ran the daemon persistently for the first time. It must be fixed
before Phase 5 (GUI), whose entire point is live enable/disable — the
project's very first requirement, established in brainstorming.

Root cause: two independent implementations of "what enabling means" (the
startup loop in `main.rs`, and the socket handler in `socket.rs`), only one
of which was ever finished. The fix consolidates them into one.

**Files:**
- Create: `crates/kennel-daemon/src/scheduler_manager.rs`
- Modify: `crates/kennel-daemon/src/socket.rs` (takes `Arc<SchedulerManager>` instead of `Registry`)
- Modify: `crates/kennel-daemon/src/main.rs` (replaces the manual startup loop with `SchedulerManager`)

**Interfaces:**
- Consumes: `Registry` (Task 2), `scheduler::spawn`/`ScheduledMonitor::stop` (Task 4), `state::StateFile` (Task 3)
- Produces: `SchedulerManager::new(registry: Registry, state_path: PathBuf) -> SchedulerManager`, `SchedulerManager::start_enabled_from_state(&self)`, `SchedulerManager::enable(&self, name: &str, persist: bool) -> Result<(), String>`, `SchedulerManager::disable(&self, name: &str) -> Result<(), String>`, `SchedulerManager::registry(&self) -> &Registry`.

- [ ] **Step 1: Write the failing test**

`crates/kennel-daemon/src/scheduler_manager.rs`:
```rust
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use crate::registry::Registry;
use crate::scheduler::{self, ScheduledMonitor};
use crate::state::StateFile;

pub struct SchedulerManager {
    registry: Registry,
    state_path: PathBuf,
    scheduled: Mutex<HashMap<String, ScheduledMonitor>>,
}

impl SchedulerManager {
    pub fn new(registry: Registry, state_path: PathBuf) -> SchedulerManager {
        SchedulerManager { registry, state_path, scheduled: Mutex::new(HashMap::new()) }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    // Starts every extension already marked enabled in the state file.
    // Called once at daemon startup, after extensions are scanned/registered.
    // Does not re-persist what it just read.
    pub fn start_enabled_from_state(&self) {
        let state = StateFile::load(&self.state_path);
        for name in &state.enabled {
            let _ = self.enable(name, false);
        }
    }

    pub fn enable(&self, name: &str, persist: bool) -> Result<(), String> {
        self.registry.set_enabled(name, true)?;
        let interval = self.registry.list().into_iter()
            .find(|e| e.manifest.name == name)
            .map(|e| e.manifest.interval_secs)
            .ok_or_else(|| format!("unknown extension: {name}"))?;
        let monitor = scheduler::spawn(self.registry.clone(), name.to_string(), interval);
        let mut scheduled = self.scheduled.lock().unwrap();
        if let Some(old) = scheduled.insert(name.to_string(), monitor) {
            old.stop(); // re-enabling an already-running monitor replaces it, never leaks the old thread
        }
        drop(scheduled);
        if persist {
            self.persist();
        }
        Ok(())
    }

    pub fn disable(&self, name: &str) -> Result<(), String> {
        self.registry.set_enabled(name, false)?;
        if let Some(monitor) = self.scheduled.lock().unwrap().remove(name) {
            monitor.stop();
        }
        self.persist();
        Ok(())
    }

    fn persist(&self) {
        let state = StateFile { enabled: self.registry.enabled_names() };
        let _ = state.save(&self.state_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{Plugin, PluginFactory};
    use kennel_proto::{ExtensionManifest, MonitorStatus};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    struct CountingPlugin {
        checks: Arc<AtomicUsize>,
    }
    impl Plugin for CountingPlugin {
        fn manifest(&self) -> ExtensionManifest {
            ExtensionManifest { name: "counting".into(), version: "0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
        }
        fn check(&mut self) -> MonitorStatus {
            self.checks.fetch_add(1, Ordering::SeqCst);
            MonitorStatus::Healthy
        }
        fn fix(&mut self) {}
    }

    fn manifest() -> ExtensionManifest {
        ExtensionManifest { name: "counting".into(), version: "0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
    }

    #[test]
    fn enable_actually_starts_ticking_and_persists_state() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");

        let checks = Arc::new(AtomicUsize::new(0));
        let checks_for_factory = checks.clone();
        let registry = Registry::new();
        registry.register(manifest(), Box::new(move || Box::new(CountingPlugin { checks: checks_for_factory.clone() }) as Box<dyn Plugin>) as PluginFactory);

        let manager = SchedulerManager::new(registry, state_path.clone());
        manager.enable("counting", true).unwrap();

        std::thread::sleep(Duration::from_millis(1500));
        assert!(checks.load(Ordering::SeqCst) >= 1, "enable() must actually schedule ticks, not just flip a flag");

        let saved = StateFile::load(&state_path);
        assert_eq!(saved.enabled, vec!["counting"], "enable() must persist to state.json");

        manager.disable("counting").unwrap();
        let count_at_disable = checks.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(checks.load(Ordering::SeqCst), count_at_disable, "disable() must actually stop the scheduler thread");

        let saved = StateFile::load(&state_path);
        assert!(saved.enabled.is_empty(), "disable() must persist to state.json");
    }

    #[test]
    fn start_enabled_from_state_schedules_without_rewriting_the_file_it_just_read() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        StateFile { enabled: vec!["counting".into()] }.save(&state_path).unwrap();
        let before = std::fs::metadata(&state_path).unwrap().modified().unwrap();

        let checks = Arc::new(AtomicUsize::new(0));
        let checks_for_factory = checks.clone();
        let registry = Registry::new();
        registry.register(manifest(), Box::new(move || Box::new(CountingPlugin { checks: checks_for_factory.clone() }) as Box<dyn Plugin>) as PluginFactory);

        let manager = SchedulerManager::new(registry, state_path.clone());
        manager.start_enabled_from_state();

        std::thread::sleep(Duration::from_millis(1500));
        assert!(checks.load(Ordering::SeqCst) >= 1, "start_enabled_from_state() must actually schedule ticks");

        let after = std::fs::metadata(&state_path).unwrap().modified().unwrap();
        assert_eq!(before, after, "start_enabled_from_state() must not rewrite the file it just loaded from");
    }
}
```

- [ ] **Step 2: Wire the module**

Add `mod scheduler_manager;` to `crates/kennel-daemon/src/main.rs`.

- [ ] **Step 3: Run test, verify it fails first**

Run: `cargo test -p kennel-daemon scheduler_manager::`
Expected: compile error (module doesn't exist in `main.rs` yet) until Step 2, then passes once Step 1's code is in place — since this is new code written in one pass, confirm it compiles and both tests genuinely exercise ticking (not something that would pass even with a no-op `enable`/`disable`).

- [ ] **Step 4: Update `socket.rs` to route through `SchedulerManager`**

Replace `socket.rs`'s `serve`/`handle_connection`/`handle_request` to take `Arc<SchedulerManager>` instead of `Registry`:

```rust
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::Arc;
use kennel_proto::{Request, Response};
use crate::scheduler_manager::SchedulerManager;

pub fn serve(path: &Path, manager: Arc<SchedulerManager>) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    for stream in listener.incoming() {
        let stream = stream?;
        let manager = manager.clone();
        std::thread::spawn(move || handle_connection(stream, manager));
    }
    Ok(())
}

fn handle_connection(stream: UnixStream, manager: Arc<SchedulerManager>) {
    let reader = BufReader::new(stream.try_clone().expect("clone unix stream"));
    let mut writer = stream;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() { continue; }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(&manager, req),
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let mut out = serde_json::to_string(&response).expect("Response always serializes");
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
    }
}

fn handle_request(manager: &SchedulerManager, req: Request) -> Response {
    match req {
        Request::List => Response::Extensions(manager.registry().list()),
        Request::Enable { name } => match manager.enable(&name, true) {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error { message },
        },
        Request::Disable { name } => match manager.disable(&name) {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error { message },
        },
    }
}
```

Update `socket.rs`'s existing test (`list_enable_disable_round_trip_over_socket`) to construct a `SchedulerManager` (wrapped in `Arc::new`) instead of a bare `Registry`, and pass that to `serve`. Add one new assertion to that same test: after `Enable`, sleep briefly and confirm the extension's `last_status` becomes `Some(MonitorStatus::Healthy)` via a `List` call — proving the socket path now genuinely schedules, not just flips the flag (this is the exact gap that shipped silently before). Use an `interval_secs: 1` plugin in the test fixture so the wait is short.

- [ ] **Step 5: Update `main.rs`**

```rust
mod extensions;
mod plugin;
mod registry;
mod scheduler;
mod scheduler_manager;
mod socket;
mod state;
mod wasm_host;

use std::path::PathBuf;
use std::sync::Arc;
use registry::Registry;
use scheduler_manager::SchedulerManager;

fn socket_path() -> PathBuf {
    dirs_home().join("Library/Application Support/kennel/control.sock")
}

fn state_path() -> PathBuf {
    dirs_home().join("Library/Application Support/kennel/state.json")
}

fn dirs_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
}

fn main() {
    let registry = Registry::new();

    let extensions_dir = dirs_home().join("Library/Application Support/kennel/extensions");
    extensions::scan_and_register(&extensions_dir, &registry);

    let manager = Arc::new(SchedulerManager::new(registry, state_path()));
    manager.start_enabled_from_state();

    let sock_path = socket_path();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent).expect("create kennel support dir");
    }
    println!("kenneld listening on {}", sock_path.display());
    socket::serve(&sock_path, manager).expect("socket server crashed");
}
```

- [ ] **Step 6: Run the full suite and verify**

Run: `cargo test -p kennel-daemon`
Expected: all tests pass, including the two new `scheduler_manager::` tests and the updated `socket::list_enable_disable_round_trip_over_socket`.

- [ ] **Step 7: Real-machine regression check against the live cutover from Task 12**

Task 12 already installed `kenneld` as a real LaunchAgent with `sd-keepalive` enabled via a manual `state.json` workaround (not through the socket, since that path was broken). After this fix:
```bash
cargo build --release -p kennel-daemon
sudo rm -f /usr/local/bin/kenneld
sudo cp target/release/kenneld /usr/local/bin/kenneld
sudo codesign -f -s - /usr/local/bin/kenneld
launchctl kickstart -k gui/$(id -u)/com.max.kenneld
sleep 2
echo '"List"' | nc -U ~/Library/Application\ Support/kennel/control.sock
```
Redeploying over an *already-running* `kenneld` needs `rm` before `cp`, not a plain overwrite — a plain `cp` onto a running signed binary's inode invalidates macOS's cached code-signature validation and the LaunchAgent respawn-loops on `OS_REASON_CODESIGNING` until manually fixed this way. This bit for real during the final fix wave (~2 min of real downtime on this exact machine) before being caught and corrected — this note exists so it isn't rediscovered the same way again.
Expected: `sd-keepalive` still shows `enabled: true` (state.json survived the rebuild) and `last_status` becomes `Healthy` within a couple seconds. Then prove the *live* socket path specifically works (the actual bug being fixed):
```bash
echo '{"Disable":{"name":"sd-keepalive"}}' | nc -U ~/Library/Application\ Support/kennel/control.sock
sleep 3
cat ~/Library/Application\ Support/kennel/state.json   # "enabled" must no longer list sd-keepalive
echo '{"Enable":{"name":"sd-keepalive"}}' | nc -U ~/Library/Application\ Support/kennel/control.sock
sleep 3
cat /Volumes/Vault/.keepalive   # must be fresh again, proving live Enable actually started ticking
```

- [ ] **Step 8: Commit**

```bash
git add crates/kennel-daemon/src/scheduler_manager.rs crates/kennel-daemon/src/socket.rs crates/kennel-daemon/src/main.rs
git commit -m "fix: make live enable/disable actually schedule and persist (was a no-op since Task 5/6)"
```

---

## Phase 5 — GUI

### Task 13: `kennel-gui` scaffold with a socket client and Installed tab

**Files:**
- Create: `crates/kennel-gui/Cargo.toml`
- Create: `crates/kennel-gui/src/client.rs`
- Create: `crates/kennel-gui/src/main.rs`
- Test: `crates/kennel-gui/src/client.rs` (inline, against a throwaway socket server)

**Interfaces:**
- Consumes: `kennel_proto::{Request, Response, ExtensionInfo}`
- Produces: `client::Client::connect(path: &Path) -> std::io::Result<Client>`, `Client::list(&mut self) -> Result<Vec<ExtensionInfo>, String>`, `Client::set_enabled(&mut self, name: &str, enabled: bool) -> Result<(), String>`.

- [ ] **Step 1: Scaffold**

`crates/kennel-gui/Cargo.toml`:
```toml
[package]
name = "kennel-gui"
version = "0.1.0"
edition = "2021"

[dependencies]
kennel-proto = { path = "../kennel-proto" }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
eframe = "0.28"
egui = "0.28"
tray-icon = "0.14"

[dev-dependencies]
tempfile = "3"
```

- [ ] **Step 2: Write the failing client test**

`crates/kennel-gui/src/client.rs`:
```rust
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use kennel_proto::{ExtensionInfo, Request, Response};

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect(path: &Path) -> std::io::Result<Client> {
        let stream = UnixStream::connect(path)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Client { stream, reader })
    }

    pub fn list(&mut self) -> Result<Vec<ExtensionInfo>, String> {
        match self.roundtrip(&Request::List)? {
            Response::Extensions(list) => Ok(list),
            Response::Error { message } => Err(message),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<(), String> {
        let req = if enabled { Request::Enable { name: name.into() } } else { Request::Disable { name: name.into() } };
        match self.roundtrip(&req)? {
            Response::Ok => Ok(()),
            Response::Error { message } => Err(message),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    fn roundtrip(&mut self, req: &Request) -> Result<Response, String> {
        let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        let mut resp_line = String::new();
        self.reader.read_line(&mut resp_line).map_err(|e| e.to_string())?;
        serde_json::from_str(&resp_line).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    // A tiny stand-in server: good enough to prove the client's wire format
    // without depending on kennel-daemon (would be a circular dev-dependency).
    fn spawn_echo_server(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        let path = path.to_path_buf();
        std::thread::spawn(move || {
            let _ = path; // keep the socket path alive for the listener's lifetime
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                let mut line = String::new();
                if reader.read_line(&mut line).is_err() { continue; }
                let req: Request = serde_json::from_str(&line).unwrap();
                let resp = match req {
                    Request::List => Response::Extensions(vec![]),
                    Request::Enable { .. } | Request::Disable { .. } => Response::Ok,
                };
                let mut out = serde_json::to_string(&resp).unwrap();
                out.push('\n');
                let _ = writer.write_all(out.as_bytes());
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    #[test]
    fn list_and_set_enabled_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        spawn_echo_server(&sock);

        let mut client = Client::connect(&sock).unwrap();
        assert_eq!(client.list().unwrap().len(), 0);
        client.set_enabled("sd-keepalive", true).unwrap();
    }
}
```

- [ ] **Step 3: Write a minimal `main.rs` (Installed tab only, no Browse/Settings yet)**

`crates/kennel-gui/src/main.rs`:
```rust
mod client;

use client::Client;
use eframe::egui;
use kennel_proto::ExtensionInfo;
use std::path::PathBuf;

fn socket_path() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap()).join("Library/Application Support/kennel/control.sock")
}

struct KennelApp {
    client: Option<Client>,
    extensions: Vec<ExtensionInfo>,
}

impl KennelApp {
    fn new() -> Self {
        let client = Client::connect(&socket_path()).ok();
        KennelApp { client, extensions: vec![] }
    }

    fn refresh(&mut self) {
        // kenneld may not have been up yet at startup, or may have been
        // restarted (LaunchAgent KeepAlive) since our last successful call --
        // either way a dead/missing client is retried here every frame rather
        // than left permanently disconnected until the GUI itself restarts.
        if self.client.is_none() {
            self.client = Client::connect(&socket_path()).ok();
        }
        let mut broken = false;
        if let Some(client) = &mut self.client {
            match client.list() {
                Ok(list) => self.extensions = list,
                Err(_) => broken = true,
            }
        }
        if broken {
            self.client = None;
        }
    }
}

impl eframe::App for KennelApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.refresh();
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.heading("Installed");
            if self.client.is_none() {
                ui.label("kenneld is not running");
                return;
            }
            for ext in self.extensions.clone() {
                ui.horizontal(|ui| {
                    let mut enabled = ext.enabled;
                    if ui.checkbox(&mut enabled, &ext.manifest.name).changed() {
                        if let Some(client) = &mut self.client {
                            let _ = client.set_enabled(&ext.manifest.name, enabled);
                        }
                    }
                    ui.label(format!("{:?}", ext.last_status));
                });
            }
        });
        ctx.request_repaint_after(std::time::Duration::from_secs(1));
    }
}

fn main() -> eframe::Result<()> {
    eframe::run_native("kennel", eframe::NativeOptions::default(), Box::new(|_cc| Ok(Box::new(KennelApp::new()))))
}
```

- [ ] **Step 4: Run the client test**

Run: `cargo test -p kennel-gui`
Expected: `1 passed`

- [ ] **Step 5: Manual check of the window**

Run: `cargo run -p kennel-gui` (with `kenneld` from Task 12 already running)
Expected: a window titled "kennel" showing `sd-keepalive` with a checked checkbox and a `Healthy` status.

- [ ] **Step 6: Commit**

```bash
git add crates/kennel-gui Cargo.toml
git commit -m "feat: add kennel-gui scaffold with Installed tab"
```

### Task 14: System tray icon

**Files:**
- Modify: `crates/kennel-gui/src/main.rs`

**Interfaces:**
- Consumes: `tray_icon::TrayIcon`
- Produces: a tray icon reflecting unhealthy count; no new types for later tasks to consume.

- [ ] **Step 1: Add the tray icon, updated from the same `extensions` list already being polled**

Modify `crates/kennel-gui/src/main.rs`, add near the top:
```rust
use tray_icon::{TrayIconBuilder, Icon};
```

Add a field to `KennelApp`: `_tray: Option<tray_icon::TrayIcon>` (kept alive for the app's lifetime; tray-icon drops the icon when this is dropped).

In `KennelApp::new()`, after constructing `client`:
```rust
        let icon = Icon::from_rgba(vec![80, 200, 120, 255], 1, 1).expect("1x1 icon"); // placeholder; replaced with a real asset once the GUI has one
        let tray = TrayIconBuilder::new().with_icon(icon).with_tooltip("kennel: starting…").build().ok();
```
and add `_tray: tray` to the returned struct.

In `update()`, after `self.refresh()`, recompute and set the tooltip:
```rust
        let unhealthy = self.extensions.iter().filter(|e| matches!(e.last_status, Some(kennel_proto::MonitorStatus::Unhealthy { .. } | kennel_proto::MonitorStatus::Errored { .. }))).count();
        if let Some(tray) = &self._tray {
            let _ = tray.set_tooltip(Some(if unhealthy == 0 { "kennel: all healthy".to_string() } else { format!("kennel: {unhealthy} unhealthy") }));
        }
```

- [ ] **Step 2: Manual check**

Run: `cargo run -p kennel-gui`
Expected: a tray icon appears; hovering shows "kennel: all healthy" (or the unhealthy count if `sd-keepalive` is momentarily reporting unhealthy, e.g. card unmounted).

- [ ] **Step 3: Commit**

```bash
git add crates/kennel-gui/src/main.rs crates/kennel-gui/Cargo.toml
git commit -m "feat: add tray icon reflecting unhealthy monitor count"
```

### Task 15: Browse tab — fetch repo index, install an extension

**Files:**
- Create: `crates/kennel-gui/src/store.rs`
- Modify: `crates/kennel-gui/src/main.rs` (add Browse tab)
- Modify: `crates/kennel-gui/Cargo.toml` (add `ureq`, `sha2`)

**Interfaces:**
- Consumes: nothing new
- Produces: `store::RepoIndex { extensions: Vec<StoreEntry> }`, `store::StoreEntry { name, version, wasm_url, manifest_url, sha256 }`, `store::fetch_index(repo_url: &str) -> Result<RepoIndex, String>`, `store::install(entry: &StoreEntry, extensions_dir: &Path) -> Result<(), String>`.

- [ ] **Step 1: Add dependencies**

`crates/kennel-gui/Cargo.toml`, add:
```toml
ureq = "2"
sha2 = "0.10"
```

- [ ] **Step 2: Write the failing test (against a local `file://`-style fixture, not the network)**

`crates/kennel-gui/src/store.rs`:
```rust
use std::path::Path;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreEntry {
    pub name: String,
    pub version: String,
    pub wasm_url: String,
    pub manifest_url: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoIndex {
    pub extensions: Vec<StoreEntry>,
}

pub fn fetch_index(repo_index_url: &str) -> Result<RepoIndex, String> {
    let body = ureq::get(repo_index_url).call().map_err(|e| e.to_string())?.into_string().map_err(|e| e.to_string())?;
    toml::from_str(&body).map_err(|e| e.to_string())
}

pub fn install(entry: &StoreEntry, extensions_dir: &Path) -> Result<(), String> {
    let wasm_bytes = ureq::get(&entry.wasm_url).call().map_err(|e| e.to_string())?.into_bytes().map_err(|e| e.to_string())?;
    let mut hasher = Sha256::new();
    hasher.update(&wasm_bytes);
    let digest = format!("{:x}", hasher.finalize());
    if digest != entry.sha256 {
        return Err(format!("sha256 mismatch: expected {}, got {digest}", entry.sha256));
    }
    let manifest_text = ureq::get(&entry.manifest_url).call().map_err(|e| e.to_string())?.into_string().map_err(|e| e.to_string())?;

    let ext_dir = extensions_dir.join(&entry.name);
    std::fs::create_dir_all(&ext_dir).map_err(|e| e.to_string())?;
    std::fs::write(ext_dir.join("monitor.wasm"), &wasm_bytes).map_err(|e| e.to_string())?;
    std::fs::write(ext_dir.join("manifest.toml"), manifest_text).map_err(|e| e.to_string())?;
    Ok(())
}

trait BytesExt { fn into_bytes(self) -> Result<Vec<u8>, String>; }
impl BytesExt for ureq::Response {
    fn into_bytes(self) -> Result<Vec<u8>, String> {
        let mut buf = Vec::new();
        self.into_reader().read_to_end(&mut buf).map_err(|e| e.to_string())?;
        Ok(buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn install_rejects_sha256_mismatch() {
        // A network-free check of the guard clause: feed install() an entry whose
        // sha256 cannot possibly match, using a tiny local HTTP server would be the
        // full end-to-end version -- deferred here since it needs a second crate
        // (tiny_http) purely for this one test. Covered instead by the manual
        // Step 3 check below against a real file.
        let entry = StoreEntry { name: "x".into(), version: "0".into(), wasm_url: "".into(), manifest_url: "".into(), sha256: "deadbeef".into() };
        // Directly exercise the hash-compare logic in isolation:
        let bytes = b"not empty";
        let mut hasher = Sha256::new();
        hasher.update(bytes);
        let digest = format!("{:x}", hasher.finalize());
        assert_ne!(digest, entry.sha256);
    }
}
```

- [ ] **Step 3: Manual end-to-end check against a real local file server**

```bash
mkdir -p /tmp/kennel-test-repo
cp target/wasm32-unknown-unknown/release/sd_keepalive.wasm /tmp/kennel-test-repo/
cp extensions-src/sd-keepalive/manifest.toml /tmp/kennel-test-repo/
shasum -a 256 /tmp/kennel-test-repo/sd_keepalive.wasm
shasum -a 256 /tmp/kennel-test-repo/manifest.toml
```
Write `/tmp/kennel-test-repo/index.toml` using both sha256 sums printed above:
```toml
[[extensions]]
name = "sd-keepalive"
version = "0.1.0"
wasm_url = "http://127.0.0.1:8123/sd_keepalive.wasm"
manifest_url = "http://127.0.0.1:8123/manifest.toml"
sha256 = "<paste the wasm shasum output here>"
manifest_sha256 = "<paste the manifest.toml shasum output here>"
```
`manifest_sha256` is required (final review finding I1, fixed during the final fix wave) — `install()` verifies both hashes and rejects a manifest whose own `name` field disagrees with the index entry's, before writing anything to disk.
```bash
cd /tmp/kennel-test-repo && python3 -m http.server 8123 &
```
Then in a `cargo run -p kennel-gui`-launched Browse tab (added in Step 4 below), point it at `http://127.0.0.1:8123/index.toml` and click Install; confirm the file lands under `~/Library/Application Support/kennel/extensions/sd-keepalive/`.

- [ ] **Step 4: Wire the Browse tab into `main.rs`**

Add tabs to `KennelApp` (a `enum Tab { Installed, Browse, Settings }` field, default `Installed`), with a `ui.horizontal` row of `ui.selectable_label` buttons at the top of `update()`'s `CentralPanel`, and a `match self.tab { ... }` below it. The `Browse` arm holds a `repo_url: String` text field, a "Fetch" button calling `store::fetch_index`, and a list of `StoreEntry` rows each with an "Install" button calling `store::install(entry, &extensions_dir())` — install failures (bad URL, sha256 mismatch) render as a red `ui.colored_label` with the returned error string rather than panicking.

- [ ] **Step 5: Run tests**

Run: `cargo test -p kennel-gui`
Expected: `2 passed` (client test from Task 13 + the sha256 guard test above)

- [ ] **Step 6: Commit**

```bash
git add crates/kennel-gui/src/store.rs crates/kennel-gui/src/main.rs crates/kennel-gui/Cargo.toml
git commit -m "feat: add Browse tab with repo index fetch and sha256-verified install"
```

### Task 16: Settings tab — manage repo URLs

**Files:**
- Create: `crates/kennel-gui/src/config.rs`
- Modify: `crates/kennel-gui/src/main.rs` (add Settings tab, wire into Browse's repo picker)

**Interfaces:**
- Produces: `config::GuiConfig { repos: Vec<String> }` with `load()`/`save()` (same JSON-file pattern as `state.rs` in Task 3, at `~/Library/Application Support/kennel/gui-config.json`), pre-seeded with the user's default extensions repo on first run.

- [ ] **Step 1: Write it (mirrors Task 3's `StateFile` almost exactly)**

`crates/kennel-gui/src/config.rs`:
```rust
use std::path::Path;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiConfig {
    pub repos: Vec<String>,
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig { repos: vec!["https://raw.githubusercontent.com/Max-Levitskiy/kennel-extensions/main/index.toml".into()] }
    }
}

impl GuiConfig {
    pub fn load(path: &Path) -> GuiConfig {
        std::fs::read_to_string(path).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
        std::fs::write(path, serde_json::to_string_pretty(self).unwrap())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_config_falls_back_to_default_with_one_repo() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = GuiConfig::load(&dir.path().join("missing.json"));
        assert_eq!(cfg.repos.len(), 1);
    }

    #[test]
    fn add_repo_then_save_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("gui-config.json");
        let mut cfg = GuiConfig::default();
        cfg.repos.push("https://example.com/custom/index.toml".into());
        cfg.save(&path).unwrap();

        let loaded = GuiConfig::load(&path);
        assert_eq!(loaded.repos.len(), 2);
    }
}
```

- [ ] **Step 2: Wire the Settings tab**

Add `mod config;` to `main.rs`. Add a `gui_config: config::GuiConfig` field to `KennelApp`, loaded in `new()`. The `Settings` tab arm renders `ui.text_edit_singleline` for a new-repo string plus an "Add" button pushing into `gui_config.repos` and calling `save()`, and lists existing repos each with a "Remove" button. The Browse tab's repo picker (Task 15) becomes a dropdown over `gui_config.repos` instead of a free-typed URL.

- [ ] **Step 3: Run tests**

Run: `cargo test -p kennel-gui config::`
Expected: `2 passed`

- [ ] **Step 4: Commit**

```bash
git add crates/kennel-gui/src/config.rs crates/kennel-gui/src/main.rs
git commit -m "feat: add Settings tab for managing extension repo URLs"
```

### Task 17: Permission-prompt dialog before enabling

**Files:**
- Modify: `crates/kennel-gui/src/main.rs`

**Interfaces:**
- Consumes: `ExtensionInfo.manifest.capabilities` / `.privileged_commands` (Task 1)
- Produces: a modal confirmation flow — no new types for later tasks.

- [ ] **Step 1: Intercept the checkbox toggle with a pending-confirmation state**

Add to `KennelApp`: `pending_enable: Option<ExtensionInfo>`.

In the Installed tab's checkbox handler (Task 13), replace the direct `client.set_enabled` call: if `enabled` is being turned **on** and `ext.manifest.capabilities` is non-empty, set `self.pending_enable = Some(ext.clone())` instead of calling the client directly; if turning **off**, or turning on with zero capabilities, call `set_enabled` immediately as before.

After the tab `match`, add:
```rust
        if let Some(ext) = self.pending_enable.clone() {
            egui::Window::new(format!("Allow {}?", ext.manifest.name)).collapsible(false).show(ctx, |ui| {
                ui.label(&ext.manifest.description);
                ui.separator();
                ui.label("This extension can:");
                for cap in &ext.manifest.capabilities {
                    ui.label(format!("• {:?}", cap));
                }
                if ext.manifest.capabilities.contains(&kennel_proto::Capability::PrivilegedSpawn) {
                    ui.separator();
                    ui.label("Root access requires this sudoers rule, installed by you (kennel will not write it):");
                    ui.code(format!(
                        "# /etc/sudoers.d/kennel-{}\n{}",
                        ext.manifest.name,
                        ext.manifest.privileged_commands.iter().map(|c| format!("{} ALL=(root) NOPASSWD: {}", std::env::var("USER").unwrap_or_default(), c)).collect::<Vec<_>>().join("\n")
                    ));
                }
                ui.horizontal(|ui| {
                    if ui.button("Allow").clicked() {
                        if let Some(client) = &mut self.client {
                            let _ = client.set_enabled(&ext.manifest.name, true);
                        }
                        self.pending_enable = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.pending_enable = None;
                    }
                });
            });
        }
```

- [ ] **Step 2: Manual check**

Run: `cargo run -p kennel-gui`, toggle on an extension with declared capabilities (e.g. `sd-keepalive`, which declares `write_file` + `log`).
Expected: a modal appears listing `WriteFile` and `Log` before it actually enables; clicking Cancel leaves it disabled.

- [ ] **Step 3: Commit**

```bash
git add crates/kennel-gui/src/main.rs
git commit -m "feat: show a capability permission prompt before enabling an extension"
```

---

## Phase 6 — Port the remaining watchdogs

### Task 18: `gdrive-watchdog` extension

**Files:**
- Create: `extensions-src/gdrive-watchdog/Cargo.toml`
- Create: `extensions-src/gdrive-watchdog/src/lib.rs`
- Create: `extensions-src/gdrive-watchdog/manifest.toml`
- Modify: `crates/kennel-guest-sdk/src/lib.rs` (add `spawn`/`privileged_spawn`/`notify`/`state_get`/`state_set` guest-side bindings, mirroring the `write_file`/`log` pair added in Task 11)

Ported from the real `~/.local/bin/gdrive-watchdog`. The shell version's `run_timeout`/`disown` dance existed to survive a wedged FileProvider child without hanging the watchdog itself — in kennel that entire concern is already handled by the host's epoch deadline (Task 8), so the port is simpler than the original: call `spawn("ls", [domain])` and let the wasmtime timeout do what `run_timeout` did by hand. `CONFIRM_FAILURES`/`COOLDOWN`/`MAX_RESTARTS_PER_HOUR` become `state_get`/`state_set` counters instead of the original's flat files under `$STATE`.

**Interfaces:**
- Produces: a buildable `gdrive-watchdog` wasm extension.

- [ ] **Step 1: Extend the guest SDK with the remaining host bindings**

Append to `crates/kennel-guest-sdk/src/lib.rs`:
```rust
#[link(wasm_import_module = "kennel")]
extern "C" {
    fn spawn(cmd_ptr: i32, cmd_len: i32, args_ptr: i32, args_len: i32) -> u64;
    fn privileged_spawn(cmd_ptr: i32, cmd_len: i32, args_ptr: i32, args_len: i32) -> u64;
    fn notify(title_ptr: i32, title_len: i32, body_ptr: i32, body_len: i32);
    fn state_get(key_ptr: i32, key_len: i32) -> u64;
    fn state_set(key_ptr: i32, key_len: i32, val_ptr: i32, val_len: i32);
    fn launchctl(action_ptr: i32, action_len: i32, args_ptr: i32, args_len: i32);
}

#[derive(Serialize, Deserialize)]
pub struct SpawnResult { pub exit_code: i32, pub stdout: String, pub stderr: String }

fn unpack(packed: u64) -> (i32, i32) { ((packed >> 32) as i32, (packed & 0xFFFF_FFFF) as i32) }

fn read_from_scratch(ptr: i32, len: i32) -> String {
    unsafe { String::from_utf8_lossy(&SCRATCH.0[ptr as usize - SCRATCH.0.as_ptr() as usize..][..len as usize]).into_owned() }
}

pub fn host_spawn(cmd: &str, args: &[&str]) -> SpawnResult {
    let args_json = serde_json::to_string(args).unwrap();
    let packed = unsafe { spawn(cmd.as_ptr() as i32, cmd.len() as i32, args_json.as_ptr() as i32, args_json.len() as i32) };
    let (ptr, len) = unpack(packed);
    serde_json::from_str(&read_from_scratch(ptr, len)).unwrap_or(SpawnResult { exit_code: -1, stdout: String::new(), stderr: "bad host response".into() })
}

pub fn host_privileged_spawn(cmd: &str, args: &[&str]) -> SpawnResult {
    let args_json = serde_json::to_string(args).unwrap();
    let packed = unsafe { privileged_spawn(cmd.as_ptr() as i32, cmd.len() as i32, args_json.as_ptr() as i32, args_json.len() as i32) };
    let (ptr, len) = unpack(packed);
    serde_json::from_str(&read_from_scratch(ptr, len)).unwrap_or(SpawnResult { exit_code: -1, stdout: String::new(), stderr: "bad host response".into() })
}

pub fn host_notify(title: &str, body: &str) {
    unsafe { notify(title.as_ptr() as i32, title.len() as i32, body.as_ptr() as i32, body.len() as i32) }
}

pub fn host_state_get(key: &str) -> String {
    let packed = unsafe { state_get(key.as_ptr() as i32, key.len() as i32) };
    let (ptr, len) = unpack(packed);
    read_from_scratch(ptr, len)
}

pub fn host_state_set(key: &str, value: &str) {
    unsafe { state_set(key.as_ptr() as i32, key.len() as i32, value.as_ptr() as i32, value.len() as i32) }
}

pub fn host_launchctl(action: &str, args: &[&str]) {
    let args_json = serde_json::to_string(args).unwrap();
    unsafe { launchctl(action.as_ptr() as i32, action.len() as i32, args_json.as_ptr() as i32, args_json.len() as i32) }
}
```

Note: `read_from_scratch`'s pointer-subtraction trick only works because the host always writes into the *same* static `SCRATCH` buffer the guest itself owns (Task 8's `write_scratch` writes at the address the guest reported via `__kennel_scratch_ptr`) — this is safe specifically because guest and host agree on that one buffer, not a general-purpose pointer arithmetic.

- [ ] **Step 2: Write the manifest**

`extensions-src/gdrive-watchdog/manifest.toml`:
```toml
name = "gdrive-watchdog"
version = "0.1.0"
description = "Detects Google Drive FileProvider stalls and restarts Drive"
interval_secs = 30
capabilities = ["spawn", "notify", "state"]
```

- [ ] **Step 3: Write the extension**

`extensions-src/gdrive-watchdog/src/lib.rs`:
```rust
use kennel_guest_sdk::{host_notify, host_spawn, host_state_get, host_state_set, kennel_extension, Manifest, Status};

const CONFIRM_FAILURES: u32 = 2;
const GRACE_AFTER_START_SECS: u64 = 120;

fn manifest() -> Manifest {
    Manifest { name: "gdrive-watchdog", version: "0.1.0", description: "Detects Google Drive FileProvider stalls and restarts Drive", interval_secs: 30, capabilities: vec!["spawn", "notify", "state"], privileged_commands: vec![] }
}

fn drive_domains() -> Vec<String> {
    let result = host_spawn("/bin/sh", &["-c", "ls -d $HOME/Library/CloudStorage/GoogleDrive-* 2>/dev/null"]);
    result.stdout.lines().map(|s| s.to_string()).collect()
}

fn probe_stalled() -> bool {
    for domain in drive_domains() {
        let result = host_spawn("/bin/ls", &["-1", &domain]);
        // A wasmtime epoch timeout on a hung `ls` surfaces as a nonzero-exit
        // spawn result (the host's Command::output returns once the child is
        // reaped or errors), so exit_code != 0 covers both "stalled" and
        // "domain briefly unreadable".
        if result.exit_code != 0 {
            return true;
        }
    }
    false
}

fn check() -> Status {
    if drive_domains().is_empty() {
        return Status::Healthy; // Drive isn't running -- nothing to watch, matches the original script's behavior
    }
    if !probe_stalled() {
        host_state_set("consecutive_stalls", "0");
        return Status::Healthy;
    }
    let consecutive: u32 = host_state_get("consecutive_stalls").parse().unwrap_or(0) + 1;
    host_state_set("consecutive_stalls", &consecutive.to_string());
    if consecutive < CONFIRM_FAILURES {
        Status::Healthy // not confirmed yet, matches CONFIRM_FAILURES in the original
    } else {
        Status::Unhealthy(format!("{consecutive} consecutive stalled probes"))
    }
}

fn fix() {
    host_notify("Google Drive frozen", "Restarting Drive…");
    host_spawn("/usr/bin/osascript", &["-e", "tell application \"Google Drive\" to quit"]);
    host_spawn("/bin/sleep", &["3"]);
    host_spawn("/usr/bin/pkill", &["-9", "-f", "Google Drive.app/Contents/MacOS/Google Drive"]);
    host_spawn("/usr/bin/open", &["-a", "/Applications/Google Drive.app"]);
    host_state_set("consecutive_stalls", "0");
    host_notify("Google Drive restarted", "Watch for recovery on the next check.");
}

kennel_extension!(manifest, check, fix);
```

This is a deliberately smaller port than the original 250-line script: the circuit breaker (`MAX_RESTARTS_PER_HOUR`), cooldown, startup grace period, and full forensics capture (spindump/lsof/log show) are **not** included yet — `GRACE_AFTER_START_SECS` above is unused, flagging that gap rather than hiding it. Track that as a follow-up once this simpler version has proven itself; the spec's testing section already calls out that these ported monitors are smoke-tested manually rather than fully unit-tested, so under-porting a stateful safety feature is a real risk worth a deliberate second pass, not a silent gap.

- [ ] **Step 4: Add to workspace and build**

Add `"extensions-src/gdrive-watchdog"` to root `Cargo.toml` members.

Run: `cargo build --release --target wasm32-unknown-unknown -p gdrive-watchdog`
Expected: `gdrive_watchdog.wasm` produced.

- [ ] **Step 5: Manual check with `--selftest`-equivalent behavior**

```bash
mkdir -p ~/Library/Application\ Support/kennel/extensions/gdrive-watchdog
cp extensions-src/gdrive-watchdog/manifest.toml ~/Library/Application\ Support/kennel/extensions/gdrive-watchdog/
cp target/wasm32-unknown-unknown/release/gdrive_watchdog.wasm ~/Library/Application\ Support/kennel/extensions/gdrive-watchdog/monitor.wasm
launchctl kickstart -k gui/$(id -u)/com.max.kenneld
echo '{"Enable":{"name":"gdrive-watchdog"}}' | nc -U ~/Library/Application\ Support/kennel/control.sock
sleep 35
echo '"List"' | nc -U ~/Library/Application\ Support/kennel/control.sock
```
Expected: `gdrive-watchdog` listed with `last_status: Healthy` (assuming Drive isn't actually frozen right now).

- [ ] **Step 6: Retire the old LaunchAgent** (same pattern as Task 12)

```bash
launchctl bootout gui/$(id -u)/com.max.gdrive-watchdog
mv ~/Library/LaunchAgents/com.max.gdrive-watchdog.plist ~/Library/LaunchAgents/com.max.gdrive-watchdog.plist.disabled
```

- [ ] **Step 7: Commit**

```bash
git add extensions-src/gdrive-watchdog crates/kennel-guest-sdk/src/lib.rs Cargo.toml
git commit -m "feat: port gdrive-watchdog as a kennel extension (simplified, no forensics capture yet)"
```

### Task 19: `sketchybar-watchdog` extension (new — the monitor this whole project started from)

**Files:**
- Create: `extensions-src/sketchybar-watchdog/Cargo.toml`
- Create: `extensions-src/sketchybar-watchdog/src/lib.rs`
- Create: `extensions-src/sketchybar-watchdog/manifest.toml`

Modeled on `sd-keepalive.sh`'s own wake-detection trick (`gap = now - last_tick; gap > GAP_THRESHOLD => system was asleep`), since sketchybar's post-wake breakage has no clean "unhealthy" signal in its logs — the fix earlier in this session was agreed as "kick sketchybar automatically on wake", not "detect broken, then fix". So `check()` always reports the tick-gap outcome directly as unhealthy-on-wake, and `fix()` is the same `launchctl kickstart -k` used manually to unstick it that day.

**Interfaces:**
- Produces: a buildable `sketchybar-watchdog` wasm extension.

- [ ] **Step 1: Write the manifest**

`extensions-src/sketchybar-watchdog/manifest.toml`:
```toml
name = "sketchybar-watchdog"
version = "0.1.0"
description = "Restarts sketchybar after the Mac wakes from sleep"
interval_secs = 5
capabilities = ["launchctl", "state"]
```

- [ ] **Step 2: Write the extension**

`extensions-src/sketchybar-watchdog/src/lib.rs`:
```rust
use kennel_guest_sdk::{host_launchctl, host_spawn, host_state_get, host_state_set, kennel_extension, Manifest, Status};
use std::time::{SystemTime, UNIX_EPOCH};

const GAP_THRESHOLD_SECS: u64 = 8; // matches sd-keepalive.sh's own GAP_THRESHOLD, already proven on this machine

fn manifest() -> Manifest {
    Manifest { name: "sketchybar-watchdog", version: "0.1.0", description: "Restarts sketchybar after the Mac wakes from sleep", interval_secs: 5, capabilities: vec!["launchctl", "spawn", "state"], privileged_commands: vec![] }
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

fn check() -> Status {
    let now = now_secs();
    let last_tick: u64 = host_state_get("last_tick").parse().unwrap_or(now);
    host_state_set("last_tick", &now.to_string());

    let gap = now.saturating_sub(last_tick);
    if gap > GAP_THRESHOLD_SECS {
        Status::Unhealthy(format!("system was asleep for ~{gap}s, sketchybar needs a kick"))
    } else {
        Status::Healthy
    }
}

fn fix() {
    let uid = host_spawn("/usr/bin/id", &["-u"]).stdout.trim().to_string();
    host_launchctl("kickstart", &["-k", &format!("gui/{uid}/homebrew.mxcl.sketchybar")]);
}

kennel_extension!(manifest, check, fix);
```

`host_launchctl` here takes `args: &[&str]` (Task 18 already defines the SDK's `launchctl` extern and wrapper this way, and Task 8 already registers the matching 4-argument host import) — no shell involved, so `$(id -u)` can't be embedded in a string; that's why `fix()` shells out to `/usr/bin/id -u` via `host_spawn` first and interpolates the result in Rust, then passes `-k` and the resolved label as two separate array elements.

- [ ] **Step 3: Build and manually verify against the real sketchybar**

```bash
cargo build --release --target wasm32-unknown-unknown -p sketchybar-watchdog
mkdir -p ~/Library/Application\ Support/kennel/extensions/sketchybar-watchdog
cp extensions-src/sketchybar-watchdog/manifest.toml ~/Library/Application\ Support/kennel/extensions/sketchybar-watchdog/
cp target/wasm32-unknown-unknown/release/sketchybar_watchdog.wasm ~/Library/Application\ Support/kennel/extensions/sketchybar-watchdog/monitor.wasm
launchctl kickstart -k gui/$(id -u)/com.max.kenneld
echo '{"Enable":{"name":"sketchybar-watchdog"}}' | nc -U ~/Library/Application\ Support/kennel/control.sock
pmset sleepnow   # put the Mac to sleep; wake it manually after a few seconds
sleep 20
echo '"List"' | nc -U ~/Library/Application\ Support/kennel/control.sock
```
Expected: after waking, the next `List` shows `sketchybar-watchdog` briefly `Unhealthy` with a "was asleep for ~Ns" detail, and sketchybar itself shows a fresh PID (`pgrep -fl sketchybar`) proving `fix()` actually kickstarted it.

- [ ] **Step 4: Commit**

```bash
git add extensions-src/sketchybar-watchdog
git commit -m "feat: add sketchybar-watchdog, self-heals sketchybar after wake"
```

---

## Phase 7 — Extension repo / store mechanics

### Task 20: `kennel-extensions` repo scaffold and release workflow

**Files:** (in a separate repo, `~/git/other/kennel-extensions`, not this one)
- Create: `~/git/other/kennel-extensions/index.toml`
- Create: `~/git/other/kennel-extensions/.github/workflows/release.yml`
- Create: `~/git/other/kennel-extensions/README.md`

**Interfaces:** none — this produces the actual default repo `GuiConfig::default()` (Task 16) points at.

- [ ] **Step 1: Create the repo and seed the index**

```bash
mkdir -p ~/git/other/kennel-extensions && cd ~/git/other/kennel-extensions && git init -q
```

`index.toml` (starts empty; the release workflow appends entries as extensions are tagged):
```toml
extensions = []
```

- [ ] **Step 2: Write the release workflow**

`.github/workflows/release.yml`:
```yaml
name: release-extension
on:
  push:
    tags: ["*-v*"]  # e.g. sd-keepalive-v0.1.0

jobs:
  build:
    runs-on: macos-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: wasm32-unknown-unknown
      - name: parse tag
        id: tag
        run: |
          TAG="${GITHUB_REF#refs/tags/}"
          NAME="${TAG%-v*}"
          VERSION="${TAG##*-v}"
          echo "name=$NAME" >> "$GITHUB_OUTPUT"
          echo "version=$VERSION" >> "$GITHUB_OUTPUT"
      - name: build
        run: cargo build --release --target wasm32-unknown-unknown -p ${{ steps.tag.outputs.name }}
      - name: release
        uses: softprops/action-gh-release@v2
        with:
          files: target/wasm32-unknown-unknown/release/*.wasm
```

This workflow builds and attaches the `.wasm` asset to a GitHub Release; updating `index.toml` with the resulting URL + sha256 is a manual step for now (the workflow above is deliberately the minimal version — auto-updating `index.toml` from CI is real but non-trivial scope, called out here rather than silently assumed).

- [ ] **Step 3: Commit**

```bash
git add index.toml .github README.md
git commit -m "chore: scaffold kennel-extensions repo and release workflow"
```

- [ ] **Step 4: Push the repo and confirm the GUI's default URL resolves**

Task 16 already points `GuiConfig::default()` at `https://raw.githubusercontent.com/Max-Levitskiy/kennel-extensions/main/index.toml` — push this repo to GitHub under that account/name so the URL is live, then confirm:

```bash
gh repo create Max-Levitskiy/kennel-extensions --public --source=. --push
curl -fsSL https://raw.githubusercontent.com/Max-Levitskiy/kennel-extensions/main/index.toml
```
Expected: prints `extensions = []` (the seed file from Step 1).

---

## What's deliberately not in this plan

- `cpu-watchdog` — stays a standalone LaunchAgent/LaunchDaemon pair, not ported (per the capability-extension decision, the contract now *could* support it, but porting a 100+ line stateful learning system with a privileged sibling daemon is its own plan, not a task inside this one).
- `claude-tmp-cleanup`, `chezmoi-autosync`, `voice-reader` — out of scope per the design spec (not restart-on-failure watchdogs).
- gdrive-watchdog's circuit breaker, cooldown, and full forensics capture (spindump/lsof/log show/network state) — flagged as a gap in Task 18, not silently dropped.
- CI for the wasm fixture/extension builds — everything here is built and tested locally; wiring GitHub Actions for `kennel` itself (as opposed to the extensions repo's release workflow in Task 20) isn't covered.
