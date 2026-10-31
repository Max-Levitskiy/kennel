use serde::{Deserialize, Serialize};

// snake_case: every manifest.toml in the plan (and the design spec) writes
// capabilities as e.g. "write_file"/"read_file", not "WriteFile"/"ReadFile" --
// without this, toml::from_str::<ManifestToml> in kennel-daemon's
// scan_and_register rejects every real manifest with an "unknown variant" error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    Spawn,
    Launchctl,
    ReadFile,
    WriteFile,
    State,
    PrivilegedSpawn,
    Log,
    Notify,
    Emit,
    FsWatch,
    FsDiscover,
    FsCopy,
}

// An event type an extension may emit, declared up front so a subscription can
// be made before the event has ever fired. `example_payload` is JSON text (not
// a parsed value) so manifest.toml, the guest SDK and the Test button all carry
// it verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventType {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default = "empty_json_object")]
    pub example_payload: String,
}

fn empty_json_object() -> String {
    "{}".into()
}


// Paths in these declarations are manifest-owned (never taken from an event).
// The file pattern is either an exact name or one wildcard, e.g. "*.m4a".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FsKind { File, Directory }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsSubscription {
    pub id: String,
    pub path: String,
    pub pattern: String,
    pub kind: FsKind,
    #[serde(default)]
    pub recursive: bool,
}

// A select control. Kennel enumerates only directory names beneath root;
// the extension never gains general read access to those directories.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DirectoryOptions {
    pub root: String,
    pub prefix: String,
    pub child: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigField {
    pub name: String,
    pub label: String,
    pub directories: DirectoryOptions,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChoice {
    pub value: String,
    pub label: String,
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
    #[serde(default)]
    pub events: Vec<EventType>,
    #[serde(default)]
    pub fs_subscriptions: Vec<FsSubscription>,
    #[serde(default)]
    pub config_fields: Vec<ConfigField>,
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

// A user-owned rule: when `event` comes from `extension`, run `command`.
// Stored in kenneld's subscriptions.toml. Only the user creates these; an
// extension never sees or chooses the command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subscription {
    // Assigned by kenneld when PutSubscription sends an empty one.
    #[serde(default)]
    pub id: String,
    pub extension: String,
    pub event: String,
    // Run as `$SHELL -lc <command>`, with the event payload on stdin.
    pub command: String,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

pub const DEFAULT_RUN_TIMEOUT_SECS: u64 = 600;

fn default_timeout_secs() -> u64 {
    DEFAULT_RUN_TIMEOUT_SECS
}

fn default_true() -> bool {
    true
}

// One event kenneld accepted from an extension.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventRecord {
    pub extension: String,
    pub event: String,
    pub id: String,
    pub payload: serde_json::Value,
    pub received_at: u64, // unix seconds
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    Ok,
    Failed,
    Timeout,
}

// One execution of a subscription's command for one event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Run {
    pub id: u64,
    pub subscription: String,
    pub extension: String,
    pub event: String,
    pub event_id: String,
    pub started_at: u64, // unix seconds
    pub finished_at: Option<u64>,
    pub status: RunStatus,
    // None while running, and when the command was killed by a signal.
    pub exit_code: Option<i32>,
    // The last bytes of stdout and stderr, interleaved as they arrived.
    pub output: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Request {
    List,
    Enable { name: String },
    Disable { name: String },
    // Re-scan the on-disk extensions directory and register anything new that
    // has appeared since startup (e.g. the GUI just installed something).
    // Deliberately payload-free: the daemon owns the extensions directory path,
    // a client must not get to point the daemon at an arbitrary directory.
    //
    // Also reloads subscriptions.toml, so hand edits take effect.
    Rescan,
    ListSubscriptions,
    // Creates (empty id) or replaces (existing id) a subscription; answers
    // with the saved one.
    PutSubscription { subscription: Subscription },
    DeleteSubscription { id: String },
    ListEvents,
    // Newest first.
    ListRuns,
    GetConfig { extension: String },
    ListConfigOptions { extension: String, field: String },
    SetConfig { extension: String, field: String, value: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Extensions(Vec<ExtensionInfo>),
    Ok,
    Error { message: String },
    Subscriptions(Vec<Subscription>),
    Subscription(Subscription),
    Events(Vec<EventRecord>),
    Runs(Vec<Run>),
    Config(std::collections::HashMap<String, String>),
    ConfigOptions(Vec<ConfigChoice>),
}

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
            events: vec![], fs_subscriptions: vec![], config_fields: vec![],
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
        // NB: `matches!` on its own is a no-op expression -- this assertion was
        // dead (it "passed" regardless of the result) until it was wrapped here.
        assert!(matches!(back, Request::Enable { name } if name == "gdrive-watchdog"));
    }

    #[test]
    fn rescan_round_trips_as_a_bare_string() {
        // The unit variant serializes as a plain JSON string, so it can be sent
        // over the control socket by hand: echo '"Rescan"' | nc -U ...
        let json = serde_json::to_string(&Request::Rescan).unwrap();
        assert_eq!(json, "\"Rescan\"");
        let back: Request = serde_json::from_str(&json).unwrap();
        assert!(matches!(back, Request::Rescan));
    }
}
