use std::collections::HashSet;
use std::path::Path;
use kennel_proto::{Capability, ExtensionManifest};
use crate::registry::Registry;
use crate::wasm_host::WasmPlugin;

#[derive(serde::Deserialize)]
struct ManifestToml {
    name: String,
    version: String,
    description: String,
    interval_secs: u64,
    #[serde(default)]
    capabilities: Vec<Capability>,
    #[serde(default)]
    privileged_commands: Vec<String>,
}

pub fn scan_and_register(dir: &Path, registry: &Registry) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let ext_dir = entry.path();
        if !ext_dir.is_dir() { continue; }
        let manifest_path = ext_dir.join("manifest.toml");
        let wasm_path = ext_dir.join("monitor.wasm");
        let (Ok(manifest_text), true) = (std::fs::read_to_string(&manifest_path), wasm_path.exists()) else { continue };
        let Ok(parsed) = toml::from_str::<ManifestToml>(&manifest_text) else { continue };

        let manifest = ExtensionManifest {
            name: parsed.name.clone(),
            version: parsed.version,
            description: parsed.description,
            interval_secs: parsed.interval_secs,
            capabilities: parsed.capabilities.clone(),
            privileged_commands: parsed.privileged_commands,
        };
        let capabilities: HashSet<Capability> = parsed.capabilities.into_iter().collect();
        let data_dir = ext_dir.join("data");
        let manifest_for_factory = manifest.clone();
        let wasm_path_for_factory = wasm_path.clone();

        registry.register(manifest, Box::new(move || {
            Box::new(WasmPlugin::load(&wasm_path_for_factory, manifest_for_factory.clone(), capabilities.clone(), data_dir.clone())
                .expect("extension failed to load")) as Box<dyn crate::plugin::Plugin>
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_a_directory_with_one_valid_extension() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("always-healthy");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(ext_dir.join("manifest.toml"), r#"
name = "always-healthy"
version = "0.1.0"
description = "test"
interval_secs = 1
capabilities = []
"#).unwrap();
        let wasm_src = fixture_wasm_path();
        std::fs::copy(&wasm_src, ext_dir.join("monitor.wasm")).unwrap();

        let registry = Registry::new();
        scan_and_register(dir.path(), &registry);

        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].manifest.name, "always-healthy");
    }

    fn fixture_wasm_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm32-unknown-unknown/release/always_healthy.wasm")
    }
}
