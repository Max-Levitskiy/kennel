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
}

pub fn host_write_file(path: &str, data: &[u8]) -> bool {
    unsafe { write_file(path.as_ptr() as i32, path.len() as i32, data.as_ptr() as i32, data.len() as i32) != 0 }
}

pub fn host_log(level: &str, msg: &str) {
    unsafe { log(level.as_ptr() as i32, level.len() as i32, msg.as_ptr() as i32, msg.len() as i32) }
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
