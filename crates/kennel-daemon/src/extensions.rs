use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use kennel_proto::{Capability, EventType, ExtensionManifest};
use crate::registry::Registry;
use crate::events::EventSink;
use crate::wasm_host::WasmPlugin;

// I7: `interval_secs = 0` makes scheduler.rs's `while waited < interval_secs`
// wait-loop exit immediately, i.e. a tight busy-loop with no sleep at all. That
// value is no longer purely self-inflicted now that manifests can arrive from a
// remote (and less trusted) extension repo, so it is rejected at scan time.
// 1s is the floor because the wait loop's resolution is one second.
const MIN_INTERVAL_SECS: u64 = 1;

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
    #[serde(default)]
    events: Vec<EventType>,
    #[serde(default)]
    fs_subscriptions: Vec<kennel_proto::FsSubscription>,
    #[serde(default)]
    config_fields: Vec<kennel_proto::ConfigField>,
}

// Scans `dir` for extensions and registers any that aren't registered yet,
// returning the names it added. Safe to call repeatedly (see Request::Rescan):
// an extension that is already registered is left completely alone -- its
// existing plugin (and, if enabled, its running scheduler thread) must not be
// disturbed by re-registration.
//
// `event_sink` receives the events extensions emit from check().
pub fn scan_and_register(dir: &Path, registry: &Registry, event_sink: Option<Arc<dyn EventSink>>) -> Vec<String> {
    let mut added = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return added };
    for entry in entries.flatten() {
        let ext_dir = entry.path();
        if !ext_dir.is_dir() { continue; }
        let manifest_path = ext_dir.join("manifest.toml");
        let wasm_path = ext_dir.join("monitor.wasm");
        let (Ok(manifest_text), true) = (std::fs::read_to_string(&manifest_path), wasm_path.exists()) else { continue };
        let parsed = match toml::from_str::<ManifestToml>(&manifest_text) {
            Ok(parsed) => parsed,
            Err(e) => {
                eprintln!("skipping {}: unreadable manifest.toml: {e}", ext_dir.display());
                continue;
            }
        };

        // The example payload is piped to a command by Test, so it has to be
        // real JSON; catching a typo here beats a confusing failed test run.
        if let Some(bad) = parsed.events.iter().find(|e| serde_json::from_str::<serde_json::Value>(&e.example_payload).is_err()) {
            eprintln!("skipping {}: event type {:?} has an example_payload that is not valid JSON", parsed.name, bad.name);
            continue;
        }

        if !parsed.fs_subscriptions.is_empty()
            && !parsed.capabilities.contains(&Capability::FsWatch) {
            eprintln!("skipping {}: filesystem subscriptions need fs_watch", parsed.name);
            continue;
        }
        if !parsed.config_fields.is_empty()
            && !parsed.capabilities.contains(&Capability::FsDiscover) {
            eprintln!("skipping {}: directory choices need fs_discover", parsed.name);
            continue;
        }
        if parsed.fs_subscriptions.iter().any(|s|
            s.id.is_empty() || s.id.contains(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
            || !crate::filesystem::valid_pattern(&s.pattern)
            || crate::ext_config::expand(&s.path).is_err())
        {
            eprintln!("skipping {}: invalid filesystem subscription", parsed.name);
            continue;
        }
        if parsed.config_fields.iter().any(|f|
            f.name.is_empty()
            || !f.name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            || crate::ext_config::expand(&f.directories.root).is_err()
            || f.directories.child.contains("..")
            || f.directories.child.starts_with('/'))
        {
            eprintln!("skipping {}: invalid config field", parsed.name);
            continue;
        }
        if parsed.interval_secs < MIN_INTERVAL_SECS {
            eprintln!("skipping {}: interval_secs = {} is below the {MIN_INTERVAL_SECS}s minimum", parsed.name, parsed.interval_secs);
            continue;
        }
        // Already known: skip before doing any work at all -- re-loading would
        // needlessly build a second Engine/Module/ticker for an extension that
        // already has one, and re-registering would swap the live factory out
        // from under a running monitor.
        if registry.contains(&parsed.name) { continue; }

        let manifest = ExtensionManifest {
            name: parsed.name.clone(),
            version: parsed.version,
            description: parsed.description,
            interval_secs: parsed.interval_secs,
            capabilities: parsed.capabilities.clone(),
            privileged_commands: parsed.privileged_commands,
            events: parsed.events,
            fs_subscriptions: parsed.fs_subscriptions,
            config_fields: parsed.config_fields,
        };
        let capabilities: HashSet<Capability> = parsed.capabilities.into_iter().collect();
        let data_dir = ext_dir.join("data");

        // C1: load ONCE, here -- not inside the factory, which the scheduler
        // calls on every single tick. WasmPlugin::load builds a wasmtime
        // Engine, compiles the module and spawns an epoch ticker thread; doing
        // that per tick leaked a thread and an Engine+Module every time. The
        // factory now just clones the already-built handle (cheap: Engine and
        // Module are Arc-backed internally), and the genuinely per-tick part --
        // a fresh Store+Instance so guest state can't persist across ticks --
        // still happens inside check()/fix().
        //
        // Failures are reported and skipped here rather than surfacing as an
        // `.expect()` panic inside the factory on some later tick: a wasm that
        // can't even load (or whose declared capabilities disagree with its
        // manifest.toml -- see WasmPlugin::verify_guest_manifest) must not be
        // registered at all.
        let plugin = match WasmPlugin::load(&wasm_path, manifest.clone(), capabilities, data_dir) {
            Ok(plugin) => match &event_sink {
                Some(sink) => plugin.with_event_sink(sink.clone()),
                None => plugin,
            },
            Err(e) => {
                eprintln!("skipping {}: {e}", manifest.name);
                continue;
            }
        };

        added.push(manifest.name.clone());
        registry.register(manifest, Arc::new(move || Box::new(plugin.clone()) as Box<dyn crate::plugin::Plugin>));
    }
    added
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
        scan_and_register(dir.path(), &registry, None);

        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].manifest.name, "always-healthy");
    }

    fn fixture_wasm_path() -> std::path::PathBuf {
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm32-unknown-unknown/release/always_healthy.wasm")
    }

    // Writes an extension directory (manifest.toml + monitor.wasm) into `root`,
    // built on the always-healthy fixture, whose own manifest() declares no
    // capabilities.
    fn write_extension(root: &Path, name: &str, interval_secs: u64) {
        let ext_dir = root.join(name);
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("manifest.toml"),
            format!("name = \"{name}\"\nversion = \"0.1.0\"\ndescription = \"test\"\ninterval_secs = {interval_secs}\ncapabilities = []\n"),
        )
        .unwrap();
        std::fs::copy(fixture_wasm_path(), ext_dir.join("monitor.wasm")).unwrap();
    }

    // C1 regression test, end to end through the path the bug actually lived on:
    // scan_and_register -> Registry::make_plugin (once per scheduler tick, see
    // scheduler.rs) -> check(). Before the fix each of those ticks called
    // WasmPlugin::load again, building a new wasmtime Engine + Module and
    // spawning an epoch ticker thread that could never be stopped or joined --
    // ~37 leaked OS threads per minute in production, hitting macOS's ~8192
    // thread ceiling within hours (and then poisoning the registry mutex when
    // thread::spawn finally failed inside the factory, which the registry used
    // to call while holding its lock).
    #[test]
    fn ticking_a_registered_extension_never_rebuilds_its_engine() {
        let dir = tempfile::tempdir().unwrap();
        let name = "engine-build-count-registry";
        write_extension(dir.path(), name, 1);

        let before = crate::wasm_host::engine_builds(name);
        let registry = Registry::new();
        assert_eq!(scan_and_register(dir.path(), &registry, None), vec![name.to_string()]);
        assert_eq!(crate::wasm_host::engine_builds(name), before + 1, "the scan itself builds exactly one Engine");

        for _ in 0..10 {
            let mut plugin = registry.make_plugin(name).expect("registered extension");
            assert_eq!(plugin.check(), kennel_proto::MonitorStatus::Healthy);
        }

        assert_eq!(
            crate::wasm_host::engine_builds(name),
            before + 1,
            "10 scheduler ticks must reuse the one Engine built at scan time, not build (and leak a ticker thread for) one per tick",
        );
    }

    // I2: Rescan calls this again on a live registry.
    #[test]
    fn rescanning_adds_only_new_extensions_and_leaves_existing_ones_untouched() {
        let dir = tempfile::tempdir().unwrap();
        write_extension(dir.path(), "first-extension", 1);

        let registry = Registry::new();
        assert_eq!(scan_and_register(dir.path(), &registry, None), vec!["first-extension".to_string()]);
        registry.set_enabled("first-extension", true).unwrap();
        let builds_after_first_scan = crate::wasm_host::engine_builds("first-extension");

        // A second extension appears on disk (the GUI just installed it).
        write_extension(dir.path(), "second-extension", 1);
        assert_eq!(scan_and_register(dir.path(), &registry, None), vec!["second-extension".to_string()], "a rescan must report only genuinely new extensions");

        let mut listed: Vec<_> = registry.list().into_iter().map(|e| (e.manifest.name, e.enabled)).collect();
        listed.sort();
        assert_eq!(listed, vec![("first-extension".to_string(), true), ("second-extension".to_string(), false)]);
        assert_eq!(
            crate::wasm_host::engine_builds("first-extension"),
            builds_after_first_scan,
            "rescanning must not re-load (or re-register) an extension that is already registered and possibly running",
        );
    }

    // I7
    #[test]
    fn an_extension_with_interval_secs_zero_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_extension(dir.path(), "busy-loop", 0);

        let registry = Registry::new();
        assert!(scan_and_register(dir.path(), &registry, None).is_empty());
        assert!(registry.list().is_empty(), "interval_secs = 0 busy-loops the scheduler and must never be registered");
    }

    // I5: a manifest.toml that grants capabilities its wasm doesn't declare
    // (or vice versa) must not be registered at all. The always-healthy fixture
    // declares none; this manifest.toml grants write_file.
    #[test]
    fn an_extension_whose_manifest_disagrees_with_its_wasm_is_not_registered() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("mismatched");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("manifest.toml"),
            "name = \"mismatched\"\nversion = \"0.1.0\"\ndescription = \"test\"\ninterval_secs = 1\ncapabilities = [\"write_file\"]\n",
        )
        .unwrap();
        std::fs::copy(fixture_wasm_path(), ext_dir.join("monitor.wasm")).unwrap();

        let registry = Registry::new();
        assert!(scan_and_register(dir.path(), &registry, None).is_empty());
        assert!(registry.list().is_empty(), "a capability mismatch between manifest.toml and the wasm must fail the scan, not register silently");
    }

    // The load rule covers event types too: the always-healthy fixture declares
    // none, this manifest.toml declares `ping`.
    #[test]
    fn an_extension_whose_event_types_disagree_with_its_wasm_is_not_registered() {
        let dir = tempfile::tempdir().unwrap();
        let ext_dir = dir.path().join("mismatched-events");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(
            ext_dir.join("manifest.toml"),
            "name = \"mismatched-events\"\nversion = \"0.1.0\"\ndescription = \"test\"\ninterval_secs = 1\ncapabilities = []\n\n[[events]]\nname = \"ping\"\ndescription = \"a ping\"\nexample_payload = '{\"n\": 1}'\n",
        )
        .unwrap();
        std::fs::copy(fixture_wasm_path(), ext_dir.join("monitor.wasm")).unwrap();

        let registry = Registry::new();
        assert!(scan_and_register(dir.path(), &registry, None).is_empty());
        assert!(registry.list().is_empty(), "an event-type mismatch between manifest.toml and the wasm must fail the scan");
    }
}
