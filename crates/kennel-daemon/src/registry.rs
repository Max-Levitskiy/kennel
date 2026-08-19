use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use kennel_proto::{ExtensionInfo, ExtensionManifest, MonitorStatus};
use crate::plugin::PluginFactory;

struct Entry {
    manifest: ExtensionManifest,
    enabled: bool,
    last_status: Option<MonitorStatus>,
    factory: PluginFactory,
}

#[derive(Clone)]
pub struct Registry {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
}

impl Registry {
    pub fn new() -> Self {
        Registry { inner: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn register(&self, manifest: ExtensionManifest, factory: PluginFactory) {
        let mut map = self.inner.lock().unwrap();
        map.insert(manifest.name.clone(), Entry { manifest, enabled: false, last_status: None, factory });
    }

    pub fn set_enabled(&self, name: &str, enabled: bool) -> Result<(), String> {
        let mut map = self.inner.lock().unwrap();
        let entry = map.get_mut(name).ok_or_else(|| format!("unknown extension: {name}"))?;
        entry.enabled = enabled;
        Ok(())
    }

    pub fn list(&self) -> Vec<ExtensionInfo> {
        let map = self.inner.lock().unwrap();
        map.values()
            .map(|e| ExtensionInfo { manifest: e.manifest.clone(), enabled: e.enabled, last_status: e.last_status.clone() })
            .collect()
    }

    pub fn update_status(&self, name: &str, status: MonitorStatus) {
        let mut map = self.inner.lock().unwrap();
        if let Some(entry) = map.get_mut(name) {
            entry.last_status = Some(status);
        }
    }

    // Used by the scheduler (Task 4): snapshot of (name, enabled, factory) pairs
    // to tick. Cloning the factory Arc-style would need PluginFactory to be
    // Clone, which Box<dyn Fn> isn't — so the scheduler calls back into the
    // registry to invoke a fresh plugin instead of taking factories out.
    pub fn make_plugin(&self, name: &str) -> Option<Box<dyn crate::plugin::Plugin>> {
        let map = self.inner.lock().unwrap();
        map.get(name).map(|e| (e.factory)())
    }

    pub fn enabled_names(&self) -> Vec<String> {
        let map = self.inner.lock().unwrap();
        map.iter().filter(|(_, e)| e.enabled).map(|(k, _)| k.clone()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::Plugin;

    struct FakePlugin { healthy: bool }
    impl Plugin for FakePlugin {
        fn manifest(&self) -> ExtensionManifest {
            ExtensionManifest { name: "fake".into(), version: "0.0.0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
        }
        fn check(&mut self) -> MonitorStatus {
            if self.healthy { MonitorStatus::Healthy } else { MonitorStatus::Unhealthy { detail: "nope".into() } }
        }
        fn fix(&mut self) { self.healthy = true; }
    }

    fn fake_manifest() -> ExtensionManifest {
        ExtensionManifest { name: "fake".into(), version: "0.0.0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
    }

    #[test]
    fn register_then_enable_then_list_reflects_state() {
        let reg = Registry::new();
        reg.register(fake_manifest(), Box::new(|| Box::new(FakePlugin { healthy: true })));

        let listed = reg.list();
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].enabled);

        reg.set_enabled("fake", true).unwrap();
        assert!(reg.list()[0].enabled);
    }

    #[test]
    fn enable_unknown_extension_errors() {
        let reg = Registry::new();
        assert!(reg.set_enabled("nope", true).is_err());
    }

    #[test]
    fn update_status_is_reflected_in_list() {
        let reg = Registry::new();
        reg.register(fake_manifest(), Box::new(|| Box::new(FakePlugin { healthy: true })));
        reg.update_status("fake", MonitorStatus::Unhealthy { detail: "x".into() });
        assert_eq!(reg.list()[0].last_status, Some(MonitorStatus::Unhealthy { detail: "x".into() }));
    }
}
