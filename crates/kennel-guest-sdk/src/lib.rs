use serde::{Deserialize, Serialize};

// `Default` so a manifest can end in `..Default::default()` and stay
// source-compatible as optional fields are added.
#[derive(Serialize, Default)]
pub struct Manifest {
    pub name: &'static str,
    pub version: &'static str,
    pub description: &'static str,
    pub interval_secs: u64,
    pub capabilities: Vec<&'static str>,
    pub privileged_commands: Vec<&'static str>,
    pub events: Vec<EventType>,
    pub fs_subscriptions: Vec<FsSubscription>,
    pub config_fields: Vec<ConfigField>,
}

// An event type the extension may emit with `host_emit`. Must match a
// `[[events]]` entry in manifest.toml by name, or the extension won't load.
// `example_payload` is JSON text.
#[derive(Serialize, Default)]
pub struct EventType {
    pub name: &'static str,
    pub description: &'static str,
    pub example_payload: &'static str,
}


// Typed declarative subscriptions. The host checks file kinds/patterns and
// paths before dispatch. The marker makes handlers incompatible at compile time.
#[derive(Serialize)]
pub struct FsSubscription {
    pub id: &'static str,
    pub path: &'static str,
    pub pattern: &'static str,
    pub kind: FsKind,
    pub recursive: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FsKind { File, Directory }

#[derive(Serialize)]
pub struct DirectoryOptions {
    pub root: &'static str,
    pub prefix: &'static str,
    pub child: &'static str,
}

#[derive(Serialize)]
pub struct ConfigField {
    pub name: &'static str,
    pub label: &'static str,
    pub directories: DirectoryOptions,
}

pub trait FileMarker {
    const EXTENSION: &'static str;
}

#[derive(Debug, serde::Deserialize)]
struct RawFsEvent {
    pub subscription: String,
    pub path: String,
    pub change: String,
}

pub struct FsEvent<M: FileMarker> {
    pub subscription: String,
    pub change: String,
    file: String,
    _marker: std::marker::PhantomData<M>,
}

impl<M: FileMarker> FsEvent<M> {
    pub fn parse(json: &str) -> Result<Self, String> {
        let raw: RawFsEvent = serde_json::from_str(json).map_err(|e| e.to_string())?;
        if !raw.path.to_ascii_lowercase().ends_with(M::EXTENSION) {
            return Err("event does not match typed file marker".into());
        }
        Ok(Self {
            subscription: raw.subscription,
            change: raw.change,
            file: raw.path,
            _marker: std::marker::PhantomData,
        })
    }

    pub fn file(&self) -> &str { &self.file }
}

// The same trait can be implemented for other event types in future versions.
pub trait Handler<E> {
    fn on_event(&self, event: E) -> Result<(), String>;
    fn on_error(&self, _error: &str) -> Result<(), String> { Ok(()) }
}

pub fn host_event_payload() -> String {
    let packed = unsafe { current_event() };
    let (ptr, len) = unpack(packed);
    read_from_scratch(ptr, len)
}

pub fn host_error_payload() -> String {
    let packed = unsafe { current_error() };
    let (ptr, len) = unpack(packed);
    read_from_scratch(ptr, len)
}

// Destination folder is a relative path inside a user-selected directory.
// The native daemon validates the source against fs_subscriptions, and the
// destination against the selected directory option before queueing the copy.
pub fn host_fs_copy(source: &str, folder: &str) -> bool {
    unsafe { fs_copy(source.as_ptr() as i32, source.len() as i32, folder.as_ptr() as i32, folder.len() as i32) != 0 }
}

pub fn host_config_get(key: &str) -> String {
    let packed = unsafe { config_get(key.as_ptr() as i32, key.len() as i32) };
    let (ptr, len) = unpack(packed);
    read_from_scratch(ptr, len)
}

// A new extension uses this macro with an implementation of Handler<FsEvent<M>>.
// The generated ABI stays C-style and never exposes Rust layouts across WASM.
#[macro_export]
macro_rules! kennel_fs_extension {
    ($manifest_fn:path, $check_fn:path, $fix_fn:path, $handler:ty, $marker:ty) => {
        $crate::kennel_extension!($manifest_fn, $check_fn, $fix_fn);

        #[no_mangle]
        pub extern "C" fn on_event() -> u64 {
            let result: Result<(), String> = (|| {
                let event = $crate::FsEvent::<$marker>::parse(&$crate::host_event_payload())?;
                let handler = <$handler as Default>::default();
                <$handler as $crate::Handler<$crate::FsEvent<$marker>>>::on_event(&handler, event)
            })();
            let status = match result {
                Ok(()) => $crate::Status::Healthy,
                Err(e) => $crate::Status::Unhealthy(e),
            };
            $crate::pack_into_scratch(&serde_json::to_vec(&status).expect("serialize handler status"))
        }

        #[no_mangle]
        pub extern "C" fn on_error() -> u64 {
            let handler = <$handler as Default>::default();
            let status = match <$handler as $crate::Handler<$crate::FsEvent<$marker>>>::on_error(
                &handler, &$crate::host_error_payload()
            ) {
                Ok(()) => $crate::Status::Healthy,
                Err(e) => $crate::Status::Unhealthy(e),
            };
            $crate::pack_into_scratch(&serde_json::to_vec(&status).expect("serialize error status"))
        }
    };
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
    fn emit(type_ptr: i32, type_len: i32, id_ptr: i32, id_len: i32, payload_ptr: i32, payload_len: i32) -> i32;
    fn current_event() -> u64;
    fn current_error() -> u64;
    fn fs_copy(source_ptr: i32, source_len: i32, folder_ptr: i32, folder_len: i32) -> i32;
    fn config_get(key_ptr: i32, key_len: i32) -> u64;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitResult {
    // kenneld recorded the event and queued the subscriptions on it.
    Accepted,
    // kenneld already accepted an event with this id from this extension, so
    // it was dropped. Re-emitting recent items on every poll is expected.
    Duplicate,
    // Not recorded: no `emit` capability, an undeclared event type, an empty
    // id, a payload that isn't JSON, or emitted outside check(). The reason is
    // in kenneld's error log.
    Rejected,
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

// Emits an event from check(): `event_type` must be declared in the manifest,
// `id` identifies the occurrence (kenneld drops ids it has already accepted
// from this extension) and `payload_json` is what subscribed commands get on
// stdin.
pub fn host_emit(event_type: &str, id: &str, payload_json: &str) -> EmitResult {
    let code = unsafe {
        emit(
            event_type.as_ptr() as i32,
            event_type.len() as i32,
            id.as_ptr() as i32,
            id.len() as i32,
            payload_json.as_ptr() as i32,
            payload_json.len() as i32,
        )
    };
    match code {
        1 => EmitResult::Accepted,
        2 => EmitResult::Duplicate,
        _ => EmitResult::Rejected,
    }
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
