use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
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
        // a well-behaved guest never reports more, and a malicious/corrupted return value
        // with the low 32 bits all set (~4 GiB) would otherwise trigger a huge allocation
        // here before wasmtime's own bounds check on `memory.read` ever runs.
        let len = ((packed & 0xFFFF_FFFF) as usize).min(SCRATCH_LEN);
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
        let mut buf = vec![0u8; data_len as u32 as usize];
        if memory.read(&caller, data_ptr as u32 as usize, &mut buf).is_err() { return 0; }
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
        let mut buf = vec![0u8; val_len as u32 as usize];
        if memory.read(&caller, val_ptr as u32 as usize, &mut buf).is_err() { return; }
        let _ = std::fs::write(&path, &buf);
    }).expect("register state_set");
}

fn sanitize_key(key: &str) -> String {
    key.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect()
}

// NOTE: guest pointers/lengths cross the ABI as wasm32 `i32`, which is really an
// unsigned 32-bit value reinterpreted as signed. Casting directly `as usize` on a
// 64-bit host sign-extends a "negative" value to near `usize::MAX`, and
// `vec![0u8; that]` panics immediately (capacity overflow) -- before wasmtime's
// own bounds check on `memory.read` ever gets a chance to reject it gracefully.
// Casting through `as u32` first reinterprets the bits as unsigned (matching wasm32
// semantics) so a malformed guest call fails the bounds check instead of crashing
// the host thread. See also the `.min(SCRATCH_LEN)` clamp in `read_guest_string`.
fn read_str(caller: &mut Caller<'_, HostCtx>, ptr: i32, len: i32) -> String {
    let memory = match caller.get_export("memory").and_then(|e| e.into_memory()) {
        Some(m) => m,
        None => return String::new(),
    };
    let mut buf = vec![0u8; len as u32 as usize];
    if memory.read(&*caller, ptr as u32 as usize, &mut buf).is_err() { return String::new(); }
    String::from_utf8_lossy(&buf).into_owned()
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
