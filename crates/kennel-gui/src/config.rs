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
