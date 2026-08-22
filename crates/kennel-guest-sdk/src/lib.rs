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

#[link(wasm_import_module = "kennel")]
extern "C" {
    fn write_file(path_ptr: i32, path_len: i32, data_ptr: i32, data_len: i32) -> i32;
    fn log(level_ptr: i32, level_len: i32, msg_ptr: i32, msg_len: i32);
    fn now() -> u64;
}

pub fn host_write_file(path: &str, data: &[u8]) -> bool {
    unsafe { write_file(path.as_ptr() as i32, path.len() as i32, data.as_ptr() as i32, data.len() as i32) != 0 }
}

pub fn host_log(level: &str, msg: &str) {
    unsafe { log(level.as_ptr() as i32, level.len() as i32, msg.as_ptr() as i32, msg.len() as i32) }
}

// Unlike every other host import, `now` is deliberately not capability-gated on the
// host side (see wasm_host.rs) -- wasm32-unknown-unknown has no clock of its own
// (no WASI here), so this is the only way guest code can ever get wall-clock time.
pub fn host_now_unix_secs() -> u64 {
    unsafe { now() }
}

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

// Note: `read_from_scratch`'s pointer-subtraction trick only works because the host
// always writes into the *same* static `SCRATCH` buffer the guest itself owns
// (Task 8's `write_scratch` writes at the address the guest reported via
// `__kennel_scratch_ptr`) -- this is safe specifically because guest and host agree
// on that one buffer, not a general-purpose pointer arithmetic.

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
