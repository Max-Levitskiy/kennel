use std::collections::{HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use kennel_proto::{Capability, ExtensionManifest, MonitorStatus};
use wasmtime::{Caller, Config, Engine, Instance, Linker, Module, Store};

// Matches kennel_guest_sdk::SCRATCH_LEN -- the fixed size of the guest's scratch buffer.
const SCRATCH_LEN: usize = 65536;

// How often the per-engine ticker thread advances wasmtime's epoch counter.
const EPOCH_TICK: Duration = Duration::from_millis(100);

// Wall-clock ceiling for one guest call (`check()`, `fix()`, `manifest()`),
// enforced via epoch interruption.
//
// This is wall-clock, not guest-CPU: the ticker below advances the epoch while
// the guest is parked inside a host import too, so this budget has to cover the
// slowest legitimate host import as well. That's why it is deliberately larger
// than HOST_COMMAND_TIMEOUT -- otherwise an extension whose probe legitimately
// takes the full host-command timeout (gdrive-watchdog's 8s `ls` probe) would
// trap the instant control returned to the guest, and could never report the
// Unhealthy verdict the probe was for.
const GUEST_CALL_DEADLINE: Duration = Duration::from_secs(13); // 8s host command + 5s of guest slack

struct HostCtx {
    extension_name: String,
    capabilities: HashSet<Capability>,
    privileged_commands: Vec<String>,
    data_dir: PathBuf,
    // Directory tree `write_file` must never write into, regardless of which
    // extension is asking -- kennel's own support directory. See
    // `kennel_support_dir` / `is_inside`.
    protected_dir: Option<PathBuf>,
    scratch_ptr: i32,
}

// Stops the epoch ticker thread once the last clone of a WasmPlugin is dropped.
// Without this the ticker would outlive its engine forever (the original C1 leak
// spawned one such immortal thread per scheduler tick).
struct EngineTicker {
    stop: Arc<AtomicBool>,
}

impl Drop for EngineTicker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

// Instrumentation for the C1 regression tests: how many times a wasmtime
// Engine + Module (and its epoch ticker thread) has actually been built, keyed
// by extension name. Cloning a WasmPlugin -- what every scheduler tick now does
// -- must NOT bump this; only `WasmPlugin::load` does. Keyed by name rather than
// a single global counter so tests running in parallel in one process can each
// assert on their own extension without seeing other tests' loads.
static ENGINE_BUILDS: Mutex<Option<HashMap<String, usize>>> = Mutex::new(None);

fn record_engine_build(name: &str) -> usize {
    let mut guard = ENGINE_BUILDS.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    let count = map.entry(name.to_string()).or_insert(0);
    *count += 1;
    *count
}

// How many Engines have been built for `name` since the process started.
#[cfg(test)]
pub fn engine_builds(name: &str) -> usize {
    let guard = ENGINE_BUILDS.lock().unwrap_or_else(|e| e.into_inner());
    guard.as_ref().and_then(|m| m.get(name).copied()).unwrap_or(0)
}

// `Clone` is the whole point of the C1 fix: the Engine, Module and ticker thread
// are built exactly once per extension (in `load`), and every scheduler tick
// takes a cheap clone of this handle instead. `Engine` and `Module` are
// internally Arc-backed -- cloning them shares the already-compiled module
// rather than recompiling or reallocating anything -- and `fresh_instance`
// still builds a brand-new `Store`+`Instance` per call, which is the part that
// genuinely has to be per-tick (guest state must not persist across ticks).
#[derive(Clone)]
pub struct WasmPlugin {
    engine: Engine,
    module: Module,
    manifest: ExtensionManifest,
    capabilities: HashSet<Capability>,
    data_dir: PathBuf,
    protected_dir: Option<PathBuf>,
    // Shared by every clone; the ticker thread stops when the last one drops.
    _ticker: Arc<EngineTicker>,
}

impl WasmPlugin {
    pub fn load(wasm_path: &Path, manifest: ExtensionManifest, capabilities: HashSet<Capability>, data_dir: PathBuf) -> Result<WasmPlugin, String> {
        let mut config = Config::new();
        config.epoch_interruption(true);
        let engine = Engine::new(&config).map_err(|e| e.to_string())?;
        let bytes = std::fs::read(wasm_path).map_err(|e| e.to_string())?;
        let module = Module::new(&engine, &bytes).map_err(|e| e.to_string())?;
        // Logged, not just counted: this line appearing once per extension is
        // correct, and it appearing over and over for the same extension is
        // exactly what the C1 leak looked like -- worth being visible in the
        // daemon's log rather than only in a test assertion.
        let builds = record_engine_build(&manifest.name);
        println!("[{}] compiled {} (wasm engines built for this extension so far: {builds})", manifest.name, wasm_path.display());

        // Background ticker so per-call deadlines (fresh_instance, below) actually expire --
        // wasmtime's epoch only advances when something calls increment_epoch.
        let engine_for_ticker = engine.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_ticker = stop.clone();
        std::thread::Builder::new()
            .name(format!("kennel-epoch-{}", manifest.name))
            .spawn(move || {
                while !stop_for_ticker.load(Ordering::SeqCst) {
                    std::thread::sleep(EPOCH_TICK);
                    engine_for_ticker.increment_epoch();
                }
            })
            .map_err(|e| format!("could not spawn epoch ticker thread: {e}"))?;

        std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
        let plugin = WasmPlugin {
            engine,
            module,
            manifest,
            capabilities,
            data_dir,
            protected_dir: kennel_support_dir(),
            _ticker: Arc::new(EngineTicker { stop }),
        };
        plugin.verify_guest_manifest()?;
        Ok(plugin)
    }

    // Test seam: point the `write_file` deny-list at a stand-in for
    // ~/Library/Application Support/kennel so the guard can be exercised
    // without a test ever writing near the real one.
    #[cfg(test)]
    fn with_protected_dir(mut self, dir: PathBuf) -> Self {
        self.protected_dir = Some(dir);
        self
    }

    // I5: the guest's exported `manifest()` used to have no callers at all, so an
    // extension's Rust-side capability list and its manifest.toml twin could
    // silently diverge (Task 19's sketchybar-watchdog shipped a manifest.toml
    // missing `spawn` exactly this way). manifest.toml stays the sole authority
    // for what is actually granted -- this cross-checks the guest's own
    // declaration against it once, at load, and fails closed on any
    // disagreement rather than registering a monitor whose code expects
    // capabilities it will not get (or, worse, whose manifest.toml quietly
    // grants more than its code admits to using).
    //
    // Instantiated with NO capabilities on purpose: `load` runs at daemon
    // startup for every installed extension, including disabled ones the user
    // has never approved, so the guest code this executes must not be able to
    // reach any gated host import. (It is still bounded by the usual epoch
    // deadline, so a manifest() that spins just fails the load.)
    fn verify_guest_manifest(&self) -> Result<(), String> {
        let (mut store, instance) = self.instantiate(HashSet::new())?;
        let manifest_fn = instance
            .get_typed_func::<(), u64>(&mut store, "manifest")
            .map_err(|e| format!("guest does not export manifest() (built without kennel_guest_sdk's kennel_extension! macro?): {e}"))?;
        let packed = manifest_fn.call(&mut store, ()).map_err(|e| format!("guest manifest() trapped: {e}"))?;
        let json = Self::read_guest_string(&mut store, &instance, packed)?;

        #[derive(serde::Deserialize)]
        struct GuestManifest {
            #[serde(default)]
            capabilities: Vec<String>,
        }
        let guest: GuestManifest = serde_json::from_str(&json).map_err(|e| format!("guest manifest() returned unparseable JSON: {e}"))?;

        let declared: std::collections::BTreeSet<String> = guest.capabilities.into_iter().collect();
        let granted: std::collections::BTreeSet<String> = self.manifest.capabilities.iter().map(capability_name).collect();
        if declared != granted {
            return Err(format!(
                "capability mismatch for {}: manifest.toml grants {:?} but the wasm's own manifest() declares {:?}",
                self.manifest.name,
                granted.into_iter().collect::<Vec<_>>(),
                declared.into_iter().collect::<Vec<_>>(),
            ));
        }
        Ok(())
    }

    fn fresh_instance(&self) -> Result<(Store<HostCtx>, Instance), String> {
        self.instantiate(self.capabilities.clone())
    }

    fn instantiate(&self, capabilities: HashSet<Capability>) -> Result<(Store<HostCtx>, Instance), String> {
        let mut linker: Linker<HostCtx> = Linker::new(&self.engine);
        register_host_imports(&mut linker);

        let ctx = HostCtx {
            extension_name: self.manifest.name.clone(),
            capabilities,
            privileged_commands: self.manifest.privileged_commands.clone(),
            data_dir: self.data_dir.clone(),
            protected_dir: self.protected_dir.clone(),
            scratch_ptr: 0,
        };
        let mut store = Store::new(&self.engine, ctx);
        store.set_epoch_deadline((GUEST_CALL_DEADLINE.as_millis() / EPOCH_TICK.as_millis()) as u64);

        let instance = linker.instantiate(&mut store, &self.module).map_err(|e| e.to_string())?;
        let scratch_fn = instance.get_typed_func::<(), i32>(&mut store, "__kennel_scratch_ptr").map_err(|e| e.to_string())?;
        let ptr = scratch_fn.call(&mut store, ()).map_err(|e| e.to_string())?;
        store.data_mut().scratch_ptr = ptr;

        Ok((store, instance))
    }

    fn read_guest_string(store: &mut Store<HostCtx>, instance: &Instance, packed: u64) -> Result<String, String> {
        let memory = instance.get_memory(&mut *store, "memory").ok_or("guest did not export memory")?;
        let ptr = (packed >> 32) as usize;
        // Clamp to the scratch buffer's documented max size (kennel_guest_sdk::SCRATCH_LEN):
        // a well-behaved guest never reports more, and this caps how much a
        // check()/fix() return value can make the host copy out even when ptr/len
        // are otherwise perfectly in-bounds for a guest with a much larger memory
        // (nothing else limits how large a guest's linear memory can grow).
        let len = ((packed & 0xFFFF_FFFF) as usize).min(SCRATCH_LEN);
        // Slice the guest's actual linear memory first, allocate second: this
        // bounds-checks ptr/len against the real memory extent using an existing
        // slice (no allocation at all) before any `to_vec()` copy happens, so a
        // corrupted/malicious check()/fix() return value (e.g. the low 32 bits all
        // set, ~4 GiB) can never drive a huge host allocation attempt.
        let data = memory.data(&mut *store);
        let bytes = data
            .get(ptr..)
            .and_then(|s| s.get(..len))
            .ok_or_else(|| "check()/fix() returned an out-of-bounds pointer/length".to_string())?;
        Ok(String::from_utf8_lossy(bytes).into_owned())
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

// The manifest.toml spelling of a capability ("write_file", not "WriteFile"),
// via the same serde rename the manifest parser uses, so the two can never
// drift apart.
fn capability_name(cap: &Capability) -> String {
    match serde_json::to_value(cap) {
        Ok(serde_json::Value::String(s)) => s,
        _ => format!("{cap:?}"),
    }
}

// kennel's own support directory: extensions/ (every extension's manifest.toml
// and monitor.wasm, plus each extension's private data/ dir), state.json and
// the control socket. `write_file` is deliberately allowed to write anywhere
// else -- that is its entire purpose (sd-keepalive touches
// /Volumes/Vault/.keepalive) -- but nothing legitimate needs to write in here
// through it: an extension's own storage is state_get/state_set, which is
// already clamped to its own data dir. See `write_file`'s deny check.
fn kennel_support_dir() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    if home.is_empty() {
        return None;
    }
    Some(PathBuf::from(home).join("Library/Application Support/kennel"))
}

// Resolves `..`/`.` textually. Done before canonicalize because the target of a
// write usually does not exist yet, so canonicalize alone can't be relied on to
// flatten a traversal in the not-yet-existing tail of the path.
fn lexically_normalized(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// Canonicalizes as much of `path` as actually exists (resolving symlinks along
// the way) and re-appends the rest verbatim, so a path whose final components
// don't exist yet -- the normal case for a write -- can still be compared
// against a directory tree by prefix.
fn resolve_as_far_as_possible(path: &Path) -> PathBuf {
    let normalized = lexically_normalized(path);
    let mut existing = normalized.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = existing.canonicalize() {
            let mut out = canonical;
            for component in tail.iter().rev() {
                out.push(component);
            }
            return out;
        }
        let Some(file_name) = existing.file_name().map(|n| n.to_os_string()) else {
            return normalized; // hit the root without finding anything that exists
        };
        tail.push(file_name);
        let Some(parent) = existing.parent().map(|p| p.to_path_buf()) else {
            return normalized;
        };
        if parent == existing {
            return normalized;
        }
        existing = parent;
    }
}

fn is_inside(target: &Path, dir: &Path) -> bool {
    resolve_as_far_as_possible(target).starts_with(resolve_as_far_as_possible(dir))
}

// Wall-clock ceiling for host-spawned child processes (`spawn`, `privileged_spawn`,
// `launchctl`, `notify`). Epoch interruption (see `fresh_instance`) only preempts
// *guest* wasm execution -- once control has left the guest and is blocked inside a
// host import on `Command::output()`, the epoch deadline never fires. Without this,
// a guest calling e.g. `spawn("sleep", ["99999"])` would hang the calling thread
// forever.
//
// 8s deliberately matches the PROBE_TIMEOUT of the gdrive-watchdog.sh script this
// host replaced: at the previous 5s every host-spawned probe was more
// trigger-happy about declaring a "stall" than the script it was ported from
// (a plausible cause of a real false-positive Drive restart during Task 18's
// testing). GUEST_CALL_DEADLINE is sized to cover a host call that takes the
// full 8s and still leave the guest room to report its verdict.
const HOST_COMMAND_TIMEOUT: Duration = Duration::from_secs(8);

// Runs `cmd` to completion, capturing stdout/stderr like `Command::output()` would,
// but kills the child and returns a timeout error if it hasn't finished within
// `timeout` instead of blocking indefinitely.
fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::io::Result<std::process::Output> {
    cmd.stdin(Stdio::null());
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());
    let child = cmd.spawn()?;
    let pid = child.id();

    // Collect output on a helper thread so this function can bound the wait with
    // `recv_timeout` instead of blocking on `wait_with_output()` directly.
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(_) => {
            // Hung (or a malicious/misbehaving child): SIGKILL by pid. The helper
            // thread above will unblock once the kill lands and its result is
            // simply dropped (its receiver is gone) -- the caller here gets a
            // timeout error instead of blocking forever. This has the standard,
            // accepted pid-reuse race of any kill-by-pid approach (the process could
            // theoretically exit and its pid get recycled in the tiny window between
            // the recv_timeout firing and the kill landing); that's an acceptable
            // trade-off for a bounded-wait-then-kill guard, not a correctness
            // requirement for this host.
            //
            // TODO: this only kills the direct child pid. A child that forks a
            // grandchild (which inherits the piped stdout/stderr fds) survives the
            // kill, and the helper thread above then leaks forever waiting on
            // wait_with_output() (which won't return until every fd-holder,
            // including the grandchild, exits). Fixing this needs a process-group
            // kill instead of a single-pid kill -- e.g. put the child in its own
            // session/group via `setsid` (or `pre_exec` + `libc::setpgid`) at
            // spawn time and `killpg` the whole group here.
            let _ = Command::new("/bin/kill").arg("-9").arg(pid.to_string()).status();
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("command did not finish within {timeout:?}"),
            ))
        }
    }
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
        let mut command = Command::new("/usr/bin/osascript");
        command.arg("-e").arg(script);
        let _ = run_with_timeout(command, HOST_COMMAND_TIMEOUT);
    }).expect("register notify");

    linker.func_wrap("kennel", "spawn", |mut caller: Caller<'_, HostCtx>, cmd_ptr: i32, cmd_len: i32, args_ptr: i32, args_len: i32| -> u64 {
        if !require_capability(&caller, Capability::Spawn) { return write_scratch(&mut caller, b"{\"error\":\"capability not granted\"}"); }
        let cmd = read_str(&mut caller, cmd_ptr, cmd_len);
        let args_json = read_str(&mut caller, args_ptr, args_len);
        let args: Vec<String> = serde_json::from_str(&args_json).unwrap_or_default();
        let mut command = Command::new(&cmd);
        command.args(&args);
        let output = run_with_timeout(command, HOST_COMMAND_TIMEOUT);
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
        let mut command = Command::new("/usr/bin/sudo");
        command.arg("-n").arg(&cmd).args(&args);
        let output = run_with_timeout(command, HOST_COMMAND_TIMEOUT);
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
        let mut command = Command::new("/bin/launchctl");
        command.arg(action).args(&args);
        let _ = run_with_timeout(command, HOST_COMMAND_TIMEOUT);
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
        // Deny-list, not allow-list: unlike state_get/state_set (clamped to the
        // extension's own data dir), write_file exists precisely to write to
        // arbitrary user-chosen paths such as /Volumes/Vault/.keepalive, so it
        // can't be confined to one directory. What it must never reach is
        // kennel's own support tree: an extension holding write_file could
        // otherwise rewrite ANOTHER extension's manifest.toml (silently granting
        // it privileged_spawn) or replace its monitor.wasm outright. Applies to
        // every extension, including writes aimed at its own directory.
        if let Some(protected) = caller.data().protected_dir.clone() {
            if is_inside(Path::new(&path), &protected) {
                println!(
                    "[{}] write_file denied: {} is inside kennel's own support directory ({})",
                    caller.data().extension_name.clone(),
                    path,
                    protected.display(),
                );
                return 0;
            }
        }
        let Some(buf) = read_bytes(&mut caller, data_ptr, data_len) else { return 0 };
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
        let Some(buf) = read_bytes(&mut caller, val_ptr, val_len) else { return };
        let _ = std::fs::write(&path, &buf);
    }).expect("register state_set");

    // Deliberately NOT capability-gated, unlike every other import above: wall-clock
    // time isn't a meaningful security boundary for a personal single-user tool the
    // way spawning processes or touching files is (there's nothing to protect by
    // withholding it), and wasm32-unknown-unknown has no clock of its own to fall
    // back on (no WASI here -- see the design doc) -- every guest needs this to do
    // anything time-based at all (e.g. sd-keepalive's freshness timestamp).
    linker.func_wrap("kennel", "now", |_caller: Caller<'_, HostCtx>| -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }).expect("register now");
}

fn sanitize_key(key: &str) -> String {
    key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

// NOTE: guest pointers/lengths cross the ABI as wasm32 `i32`, which is really an
// unsigned 32-bit value reinterpreted as signed. Casting directly `as usize` on a
// 64-bit host sign-extends a "negative" value to near `usize::MAX`.
//
// Critically, that cast alone isn't the whole story: `vec![0u8; huge]` panics on
// allocation failure via `handle_alloc_error`, which *aborts the entire process*
// (uncatchable, unlike a normal panic) -- so allocating a guest-length-sized buffer
// and THEN bounds-checking it against guest memory (via `memory.read`) is backwards:
// the attacker-controlled length reaches the allocator before wasmtime's bounds
// check ever runs. `read_bytes` below fixes the ordering: it slices the guest's
// *existing* memory buffer first (a plain bounds-checked slice index, no
// allocation), and only calls `.to_vec()` -- allocating exactly `len` real,
// in-bounds bytes -- once the slice is known to be valid. A malformed ptr/len
// simply yields `None` here, same as any other invalid host-import call.
fn read_bytes(caller: &mut Caller<'_, HostCtx>, ptr: i32, len: i32) -> Option<Vec<u8>> {
    let memory = caller.get_export("memory").and_then(|e| e.into_memory())?;
    let (ptr, len) = (ptr as u32 as usize, len as u32 as usize);
    let data = memory.data(&*caller);
    Some(data.get(ptr..)?.get(..len)?.to_vec())
}

fn read_str(caller: &mut Caller<'_, HostCtx>, ptr: i32, len: i32) -> String {
    match read_bytes(caller, ptr, len) {
        Some(buf) => String::from_utf8_lossy(&buf).into_owned(),
        None => String::new(),
    }
}

fn write_scratch(caller: &mut Caller<'_, HostCtx>, bytes: &[u8]) -> u64 {
    let scratch_ptr = caller.data().scratch_ptr;
    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return 0,
    };
    let len = bytes.len().min(SCRATCH_LEN);
    let _ = memory.write(&mut *caller, scratch_ptr as usize, &bytes[..len]);
    ((scratch_ptr as u64) << 32) | (len as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Plugin;
    use std::collections::HashSet;

    fn fixture_path(name: &str) -> PathBuf {
        // Built by `cargo build --release --target wasm32-unknown-unknown` (Task 7, Step 4).
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm32-unknown-unknown/release").join(format!("{}.wasm", name.replace('-', "_")))
    }

    #[test]
    fn run_with_timeout_does_not_block_on_a_hung_command() {
        // Epoch interruption never sees this -- the guest isn't executing while a
        // host import is blocked in Command::output(). run_with_timeout is what's
        // supposed to bound it instead.
        let mut cmd = Command::new("/bin/sleep");
        cmd.arg("30");
        let start = std::time::Instant::now();
        let result = run_with_timeout(cmd, Duration::from_millis(300));
        let elapsed = start.elapsed();
        assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::TimedOut);
        assert!(elapsed < Duration::from_secs(5), "must return promptly, not wait anywhere near the child's 30s runtime (took {elapsed:?})");
    }

    #[test]
    fn run_with_timeout_returns_captured_output_for_a_fast_command() {
        let mut cmd = Command::new("/bin/echo");
        cmd.arg("hello");
        let output = run_with_timeout(cmd, Duration::from_secs(5)).unwrap();
        assert!(output.status.success());
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
    }

    fn test_manifest(name: &str) -> ExtensionManifest {
        test_manifest_with_caps(name, &[])
    }

    fn test_manifest_with_caps(name: &str, capabilities: &[Capability]) -> ExtensionManifest {
        ExtensionManifest { name: name.into(), version: "0.1.0".into(), description: "".into(), interval_secs: 1, capabilities: capabilities.to_vec(), privileged_commands: vec![] }
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

    // Neither fixture above ever calls a host import, so a deleted
    // `require_capability` check -- or a reintroduced allocate-before-bounds-check
    // bug in read_bytes()/read_str() -- wouldn't fail either of them. The following
    // two tests exercise the `write_file` host import directly, using a
    // hand-written WAT module (not built through kennel-guest-sdk) that imports
    // `kennel.write_file` and calls it from `check()` with a caller-chosen,
    // deliberately out-of-bounds `data_len`.
    //
    // wasmtime's `wat` feature (a default feature -- confirmed via the `wat` crate
    // appearing in Cargo.lock's `wasmtime` dependency list) lets `Module::new`,
    // and therefore `WasmPlugin::load` unchanged, accept WAT text directly, so the
    // fixture below can be written straight to a temp file with no wasm32
    // compilation step.
    //
    // Since I5 the host also *calls* the guest's exported `manifest()` at load
    // time and refuses to load an extension whose declared capabilities don't
    // match the manifest.toml-sourced ones, so every WAT fixture below has to
    // export a matching `manifest()` too -- `manifest_export_wat` builds it.
    fn write_file_gate_fixture_wat(marker_path: &Path, data_len: i32, capabilities: &[&str]) -> String {
        let marker = marker_path.to_str().expect("marker path must be UTF-8");
        assert!(!marker.contains(['"', '\\']), "marker path must not need WAT string escaping: {marker}");
        format!(
            r#"(module
  (import "kennel" "write_file" (func $write_file (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{marker}")
  (data (i32.const 4096) "{{\"kind\":\"Healthy\"}}")
{manifest_export}
  (func (export "__kennel_scratch_ptr") (result i32) i32.const 8192)
  (func (export "check") (result i64)
    (drop (call $write_file (i32.const 0) (i32.const {path_len}) (i32.const 0) (i32.const {data_len})))
    (i64.or
      (i64.shl (i64.extend_i32_u (i32.const 4096)) (i64.const 32))
      (i64.extend_i32_u (i32.const 18))))
  (func (export "fix")))
"#,
            marker = marker,
            path_len = marker.len(),
            data_len = data_len,
            manifest_export = manifest_export_wat("write-file-gate", capabilities, 6000),
        )
    }

    // A `manifest()` export matching kennel_guest_sdk's ABI (JSON packed into the
    // scratch region as ptr<<32|len), for hand-written WAT fixtures that don't go
    // through the guest SDK's `kennel_extension!` macro.
    fn manifest_export_wat(name: &str, capabilities: &[&str], offset: i32) -> String {
        let caps = capabilities.iter().map(|c| format!("\"{c}\"")).collect::<Vec<_>>().join(",");
        let json = format!("{{\"name\":\"{name}\",\"version\":\"0.1.0\",\"description\":\"\",\"interval_secs\":1,\"capabilities\":[{caps}],\"privileged_commands\":[]}}");
        format!(
            r#"  (data (i32.const {offset}) "{escaped}")
  (func (export "manifest") (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (i32.const {offset})) (i64.const 32))
      (i64.extend_i32_u (i32.const {len}))))"#,
            escaped = wat_escape(&json),
            len = json.len(),
        )
    }

    fn write_wat_fixture(dir: &Path, wat: &str) -> PathBuf {
        let path = dir.join("write_file_gate_fixture.wat");
        std::fs::write(&path, wat).unwrap();
        path
    }

    #[test]
    fn write_file_denied_without_capability_never_touches_disk_or_panics() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("should-not-exist");
        // data_len = 4 -- deliberately a small, perfectly IN-BOUNDS length (unlike
        // the out-of-bounds test below). This makes the assertion below meaningful:
        // if `require_capability` were deleted from write_file, control would fall
        // through to read_bytes(0, 4), which would succeed (4 bytes is well within
        // the guest's one-page memory) and the marker file WOULD get written -- so
        // this test actually exercises the capability gate itself, not the bounds
        // check. (An out-of-bounds data_len here would make the marker's absence
        // prove nothing about the gate: read_bytes would reject it regardless of
        // whether require_capability ran at all.)
        let wat = write_file_gate_fixture_wat(&marker, 4, &[]);
        let wasm_path = write_wat_fixture(dir.path(), &wat);

        let mut plugin = WasmPlugin::load(&wasm_path, test_manifest("write-file-gate"), HashSet::new(), dir.path().to_path_buf()).unwrap();
        assert_eq!(plugin.check(), MonitorStatus::Healthy, "host call must complete without trapping or panicking");
        assert!(!marker.exists(), "write_file must not write when Capability::WriteFile isn't granted");
    }

    #[test]
    fn write_file_out_of_bounds_length_is_rejected_without_panicking() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("should-not-exist");
        // Capability IS granted this time, so the call reaches read_bytes()'s
        // bounds check. The guest's memory is exactly one page (65536 bytes);
        // data_len = -1 reinterprets to u32::MAX (~4 GiB), grossly out of bounds.
        // Before the fix this drove `vec![0u8; ~4 GiB]` ahead of any bounds check,
        // which aborts the whole process on allocation failure -- this test would
        // take the entire `cargo test` run down with it, not just fail on its own,
        // if that regressed.
        let wat = write_file_gate_fixture_wat(&marker, -1, &["write_file"]);
        let wasm_path = write_wat_fixture(dir.path(), &wat);

        let mut capabilities = HashSet::new();
        capabilities.insert(Capability::WriteFile);
        let mut plugin = WasmPlugin::load(&wasm_path, test_manifest_with_caps("write-file-gate", &[Capability::WriteFile]), capabilities, dir.path().to_path_buf()).unwrap();
        assert_eq!(plugin.check(), MonitorStatus::Healthy, "out-of-bounds length must be rejected, not panic/abort the process");
        assert!(!marker.exists(), "an out-of-bounds write_file call must not write to disk");
    }

    // I3: `write_file` is intentionally unconstrained about *where* it writes
    // (sd-keepalive's whole job is touching /Volumes/Vault/.keepalive), with one
    // exception -- kennel's own support tree, where another extension's
    // manifest.toml (its capability grant!) and monitor.wasm live. Both tests
    // below grant Capability::WriteFile and use an in-bounds data_len, so the
    // only thing that can stop the write is the deny check itself.
    #[test]
    fn write_file_into_kennels_own_support_dir_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        // Stand-in for ~/Library/Application Support/kennel so this test never
        // writes anywhere near the real one, shaped exactly like the attack:
        // another (already installed) extension's manifest.toml.
        let protected = dir.path().join("kennel");
        let victim_dir = protected.join("extensions/other-extension");
        std::fs::create_dir_all(&victim_dir).unwrap();
        let victim_manifest = victim_dir.join("manifest.toml");
        // The parent directory really exists, so if the deny check were removed
        // the write below would genuinely succeed -- this assertion is not vacuous.
        let wat = write_file_gate_fixture_wat(&victim_manifest, 4, &["write_file"]);
        let wasm_path = write_wat_fixture(dir.path(), &wat);

        let mut capabilities = HashSet::new();
        capabilities.insert(Capability::WriteFile);
        let mut plugin = WasmPlugin::load(&wasm_path, test_manifest_with_caps("write-file-gate", &[Capability::WriteFile]), capabilities, dir.path().to_path_buf())
            .unwrap()
            .with_protected_dir(protected);
        assert_eq!(plugin.check(), MonitorStatus::Healthy, "a denied write must return cleanly, not trap");
        assert!(!victim_manifest.exists(), "write_file must not be able to rewrite another extension's manifest.toml");
    }

    #[test]
    fn write_file_outside_kennels_support_dir_still_succeeds() {
        // The counterpart to the test above: proves the deny check is a
        // deny-*list*, not an accidental allow-list. This target stands in for
        // /Volumes/Vault/.keepalive, which sd-keepalive writes every 2s in
        // production.
        let dir = tempfile::tempdir().unwrap();
        let protected = dir.path().join("kennel");
        std::fs::create_dir_all(&protected).unwrap();
        let target = dir.path().join("keepalive-marker");
        let wat = write_file_gate_fixture_wat(&target, 4, &["write_file"]);
        let wasm_path = write_wat_fixture(dir.path(), &wat);

        let mut capabilities = HashSet::new();
        capabilities.insert(Capability::WriteFile);
        let mut plugin = WasmPlugin::load(&wasm_path, test_manifest_with_caps("write-file-gate", &[Capability::WriteFile]), capabilities, dir.path().to_path_buf())
            .unwrap()
            .with_protected_dir(protected);
        assert_eq!(plugin.check(), MonitorStatus::Healthy);
        assert!(target.exists(), "write_file must still work for ordinary paths outside kennel's own directory");
    }

    #[test]
    fn is_inside_resolves_traversal_and_symlinks_in_not_yet_existing_paths() {
        let dir = tempfile::tempdir().unwrap();
        let protected = dir.path().join("kennel");
        std::fs::create_dir_all(protected.join("extensions")).unwrap();

        // Plain containment, target doesn't exist yet.
        assert!(is_inside(&protected.join("extensions/x/manifest.toml"), &protected));
        // `..` traversal back into the protected tree from outside it.
        assert!(is_inside(&dir.path().join("elsewhere/../kennel/state.json"), &protected));
        // A symlinked parent pointing into the protected tree.
        let link = dir.path().join("shortcut");
        std::os::unix::fs::symlink(protected.join("extensions"), &link).unwrap();
        assert!(is_inside(&link.join("victim/manifest.toml"), &protected));
        // And the negative case: an ordinary path must stay allowed.
        assert!(!is_inside(Path::new("/tmp/kennel-write-file-target"), &protected));
        assert!(!is_inside(&dir.path().join("kennel-sibling/file"), &protected), "a sibling whose name merely starts with the protected dir's name must not be denied");
    }

    #[test]
    fn a_wasm_whose_declared_capabilities_disagree_with_manifest_toml_fails_to_load() {
        // I5: manifest.toml is the capability grant, but until now the guest's
        // own exported manifest() had no callers at all, so the two could
        // silently disagree (Task 19 shipped exactly that bug). The guest here
        // declares `write_file`; manifest.toml grants nothing.
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("unused");
        let wat = write_file_gate_fixture_wat(&marker, 4, &["write_file"]);
        let wasm_path = write_wat_fixture(dir.path(), &wat);

        let err = match WasmPlugin::load(&wasm_path, test_manifest("write-file-gate"), HashSet::new(), dir.path().to_path_buf()) {
            Err(e) => e,
            Ok(_) => panic!("a capability mismatch must fail the load, not be silently registered"),
        };
        assert!(err.contains("capability mismatch"), "unexpected error: {err}");
    }

    #[test]
    fn a_wasm_whose_declared_capabilities_match_manifest_toml_loads() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("unused");
        let wat = write_file_gate_fixture_wat(&marker, 4, &["write_file"]);
        let wasm_path = write_wat_fixture(dir.path(), &wat);

        WasmPlugin::load(&wasm_path, test_manifest_with_caps("write-file-gate", &[Capability::WriteFile]), HashSet::from([Capability::WriteFile]), dir.path().to_path_buf())
            .expect("matching declarations must load");
    }

    // C1: the daemon used to build a fresh Engine + Module + epoch ticker thread
    // on EVERY scheduler tick (Registry::make_plugin -> the factory ->
    // WasmPlugin::load), leaking one unjoinable OS thread and one Engine+Module
    // per tick -- measured at ~37 threads/minute on the machine this runs on,
    // which reaches macOS's ~8192-thread ceiling in hours. This is the
    // unit-level half of the guard (extensions.rs holds the full
    // scan -> registry -> tick version).
    #[test]
    fn cloning_a_plugin_and_ticking_it_never_rebuilds_the_engine() {
        let dir = tempfile::tempdir().unwrap();
        let name = "engine-build-count-clone";
        let before = engine_builds(name);
        let plugin = WasmPlugin::load(&fixture_path("always-healthy"), test_manifest(name), HashSet::new(), dir.path().to_path_buf()).unwrap();
        assert_eq!(engine_builds(name), before + 1, "load() builds exactly one Engine");

        for _ in 0..10 {
            let mut per_tick = plugin.clone(); // exactly what Registry::make_plugin's factory now does
            assert_eq!(per_tick.check(), MonitorStatus::Healthy);
        }
        assert_eq!(engine_builds(name), before + 1, "ticking a cloned plugin must not build another Engine (or spawn another epoch ticker thread)");
    }

    // Escapes a plain string for embedding as a WAT string literal (WAT uses
    // C-style `\"`/`\\` escaping, same idea as the marker-path assert above but
    // applied for real since these JSON payloads do need it).
    fn wat_escape(s: &str) -> String {
        s.replace('\\', "\\\\").replace('"', "\\\"")
    }

    // `now` is deliberately not capability-gated (see register_host_imports), so this
    // fixture is loaded with an empty capability set on purpose -- if a future change
    // accidentally started gating it, this test would still pass (an implausible
    // fallback of 0 also compares less than the threshold, correctly reporting
    // Unhealthy) rather than silently masking the regression as a trap/Errored.
    // check() compares the returned value against a fixed past threshold (well
    // before this fixture could ever run) entirely in WAT -- no string formatting of
    // the u64 is needed, so this only proves *some* plausible wall-clock value came
    // back through the host import, not an exact one.
    fn now_plausible_fixture_wat() -> String {
        let healthy = "{\"kind\":\"Healthy\"}";
        let unhealthy = "{\"kind\":\"Unhealthy\",\"detail\":\"now() returned an implausible timestamp\"}";
        format!(
            r#"(module
  (import "kennel" "now" (func $now (result i64)))
  (memory (export "memory") 1)
  (data (i32.const 4096) "{healthy_wat}")
  (data (i32.const 4200) "{unhealthy_wat}")
{manifest_export}
  (func (export "__kennel_scratch_ptr") (result i32) i32.const 8192)
  (func (export "check") (result i64)
    (if (result i64)
      (i64.gt_u (call $now) (i64.const 1700000000)) ;; 2023-11-14 -- any real host clock is far past this
      (then
        (i64.or
          (i64.shl (i64.extend_i32_u (i32.const 4096)) (i64.const 32))
          (i64.extend_i32_u (i32.const {healthy_len}))))
      (else
        (i64.or
          (i64.shl (i64.extend_i32_u (i32.const 4200)) (i64.const 32))
          (i64.extend_i32_u (i32.const {unhealthy_len}))))))
  (func (export "fix")))
"#,
            healthy_wat = wat_escape(healthy),
            unhealthy_wat = wat_escape(unhealthy),
            healthy_len = healthy.len(),
            unhealthy_len = unhealthy.len(),
            manifest_export = manifest_export_wat("now-plausible", &[], 6000),
        )
    }

    #[test]
    fn now_import_returns_a_plausible_unix_timestamp_without_any_capability() {
        let dir = tempfile::tempdir().unwrap();
        let wat = now_plausible_fixture_wat();
        let wasm_path = write_wat_fixture(dir.path(), &wat);

        // Empty capability set -- proves `now` works ungated, unlike every other
        // host import tested above.
        let mut plugin = WasmPlugin::load(&wasm_path, test_manifest("now-plausible"), HashSet::new(), dir.path().to_path_buf()).unwrap();
        assert_eq!(plugin.check(), MonitorStatus::Healthy, "now() should return a real, recent-ish wall-clock value with no capability granted");
    }

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
        // GUEST_CALL_DEADLINE is 13s (it has to outlast an 8s host command --
        // see its definition); this bound only proves the spin is killed rather
        // than running forever.
        assert!(elapsed < GUEST_CALL_DEADLINE + Duration::from_secs(10), "epoch deadline should kill the call, took {elapsed:?}");
    }
}
