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
    // Re-scan the on-disk extensions directory and register anything new that
    // has appeared since startup (e.g. the GUI just installed something).
    // Deliberately payload-free: the daemon owns the extensions directory path,
    // a client must not get to point the daemon at an arbitrary directory.
    Rescan,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Response {
    Extensions(Vec<ExtensionInfo>),
    Ok,
    Error { message: String },
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
