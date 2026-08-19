use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;
use kennel_proto::{Capability, ExtensionManifest, MonitorStatus};
use wasmtime::{Caller, Config, Engine, Instance, Linker, Module, Store};

// Matches kennel_guest_sdk::SCRATCH_LEN -- the fixed size of the guest's scratch buffer.
const SCRATCH_LEN: usize = 65536;

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

        // Background ticker so per-call deadlines (fresh_instance, below) actually expire --
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

// Wall-clock ceiling for host-spawned child processes (`spawn`, `privileged_spawn`,
// `launchctl`, `notify`). Epoch interruption (see `fresh_instance`) only preempts
// *guest* wasm execution -- once control has left the guest and is blocked inside a
// host import on `Command::output()`, the epoch deadline never fires. Without this,
// a guest calling e.g. `spawn("sleep", ["99999"])` would hang the calling thread
// forever. Roughly matches the ~5s epoch deadline given to guest code.
const HOST_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

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
    fn write_file_gate_fixture_wat(marker_path: &Path, data_len: i32) -> String {
        let marker = marker_path.to_str().expect("marker path must be UTF-8");
        assert!(!marker.contains(['"', '\\']), "marker path must not need WAT string escaping: {marker}");
        format!(
            r#"(module
  (import "kennel" "write_file" (func $write_file (param i32 i32 i32 i32) (result i32)))
  (memory (export "memory") 1)
  (data (i32.const 0) "{marker}")
  (data (i32.const 4096) "{{\"kind\":\"Healthy\"}}")
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
        let wat = write_file_gate_fixture_wat(&marker, 4);
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
        let wat = write_file_gate_fixture_wat(&marker, -1);
        let wasm_path = write_wat_fixture(dir.path(), &wat);

        let mut capabilities = HashSet::new();
        capabilities.insert(Capability::WriteFile);
        let mut plugin = WasmPlugin::load(&wasm_path, test_manifest("write-file-gate"), capabilities, dir.path().to_path_buf()).unwrap();
        assert_eq!(plugin.check(), MonitorStatus::Healthy, "out-of-bounds length must be rejected, not panic/abort the process");
        assert!(!marker.exists(), "an out-of-bounds write_file call must not write to disk");
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
        assert!(elapsed < Duration::from_secs(10), "epoch deadline should kill the call well under 10s, took {elapsed:?}");
    }
}
