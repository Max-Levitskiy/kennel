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
