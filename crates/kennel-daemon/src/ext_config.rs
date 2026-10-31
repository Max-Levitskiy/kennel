use kennel_proto::{ConfigChoice, ConfigField, DirectoryOptions};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub type Values = HashMap<String, String>;

fn home() -> Result<PathBuf, String> {
    std::env::var_os("HOME").map(PathBuf::from).ok_or("HOME is not set".into())
}

// Expand only a manifest's leading ~/; never expand environment variables
// or arbitrary shell expressions supplied by an untrusted extension.
pub fn expand(path: &str) -> Result<PathBuf, String> {
    if let Some(rest) = path.strip_prefix("~/") {
        Ok(home()?.join(rest))
    } else if Path::new(path).is_absolute() {
        Ok(PathBuf::from(path))
    } else {
        Err(format!("expected absolute or ~/ path: {path}"))
    }
}

pub fn config_path(data_dir: &Path) -> PathBuf { data_dir.join("config.json") }

pub fn load(data_dir: &Path) -> Values {
    std::fs::read_to_string(config_path(data_dir)).ok()
        .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

pub fn available(options: &DirectoryOptions) -> Result<Vec<ConfigChoice>, String> {
    let dir = expand(&options.root)?;
    let mut choices = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let metadata = entry.file_type().map_err(|e| e.to_string())?;
        if !metadata.is_dir() || metadata.is_symlink() { continue; }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&options.prefix) { continue; }
        // A Google Drive folder can have different subfolders on localized installs;
        // use config-provided child rather than guessing arbitrary paths.
        if !options.child.is_empty() && !entry.path().join(&options.child).is_dir() { continue; }
        choices.push(ConfigChoice {
            label: name.strip_prefix(&options.prefix).unwrap_or(&name).to_string(),
            value: name,
        });
    }
    choices.sort_by(|a, b| a.label.cmp(&b.label));
    Ok(choices)
}

pub fn field<'a>(fields: &'a [ConfigField], key: &str) -> Result<&'a ConfigField, String> {
    fields.iter().find(|f| f.name == key).ok_or(format!("unknown configuration field {key}"))
}

pub fn set_value(data_dir: &Path, fields: &[ConfigField], key: &str, value: &str) -> Result<(), String> {
    let f = field(fields, key)?;
    if !available(&f.directories)?.iter().any(|c| c.value == value) {
        return Err("choice is not in the permitted directory list".into());
    }
    std::fs::create_dir_all(data_dir).map_err(|e| e.to_string())?;
    let mut values = load(data_dir);
    values.insert(key.into(), value.into());
    let path = config_path(data_dir);
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(&values).map_err(|e| e.to_string())?;
    std::fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
    std::fs::rename(temporary, path).map_err(|e| e.to_string())
}

pub fn configured_directory(data_dir: &Path, fields: &[ConfigField], key: &str) -> Result<PathBuf, String> {
    let f = field(fields, key)?;
    let values = load(data_dir);
    let selected = values.get(key).ok_or(format!("configure {key} before enabling the extension"))?;
    // Validate every time, not just at selection, so revoked/moved accounts
    // cannot silently redirect writes to another path.
    if !available(&f.directories)?.iter().any(|c| &c.value == selected) {
        return Err("selected directory is no longer available".into());
    }
    let root = expand(&f.directories.root)?;
    let path = root.join(selected).join(&f.directories.child);
    path.canonicalize().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cannot_select_unknown_directory() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("GoogleDrive-personal")).unwrap();
        let fields = vec![ConfigField {
            name: "drive".into(), label: "Account".into(),
            directories: DirectoryOptions {
                root: root.path().display().to_string(),
                prefix: "GoogleDrive-".into(), child: "".into(),
            }
        }];
        let data = root.path().join("data");
        assert!(set_value(&data, &fields, "drive", "../bad").is_err());
        set_value(&data, &fields, "drive", "GoogleDrive-personal").unwrap();
        assert_eq!(load(&data).get("drive").unwrap(), "GoogleDrive-personal");
    }
}
