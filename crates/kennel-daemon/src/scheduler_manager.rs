use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use crate::registry::Registry;
use crate::scheduler::{self, ScheduledMonitor};
use crate::state::StateFile;

pub struct SchedulerManager {
    registry: Registry,
    state_path: PathBuf,
    scheduled: Mutex<HashMap<String, ScheduledMonitor>>,
}

impl SchedulerManager {
    pub fn new(registry: Registry, state_path: PathBuf) -> SchedulerManager {
        SchedulerManager { registry, state_path, scheduled: Mutex::new(HashMap::new()) }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    // Starts every extension already marked enabled in the state file.
    // Called once at daemon startup, after extensions are scanned/registered.
    // Does not re-persist what it just read.
    pub fn start_enabled_from_state(&self) {
        let state = StateFile::load(&self.state_path);
        for name in &state.enabled {
            let _ = self.enable(name, false);
        }
    }

    pub fn enable(&self, name: &str, persist: bool) -> Result<(), String> {
        self.registry.set_enabled(name, true)?;
        let interval = self.registry.list().into_iter()
            .find(|e| e.manifest.name == name)
            .map(|e| e.manifest.interval_secs)
            .ok_or_else(|| format!("unknown extension: {name}"))?;
        let monitor = scheduler::spawn(self.registry.clone(), name.to_string(), interval);
        let mut scheduled = self.scheduled.lock().unwrap();
        if let Some(old) = scheduled.insert(name.to_string(), monitor) {
            old.stop(); // re-enabling an already-running monitor replaces it, never leaks the old thread
        }
        drop(scheduled);
        if persist {
            self.persist();
        }
        Ok(())
    }

    pub fn disable(&self, name: &str) -> Result<(), String> {
        self.registry.set_enabled(name, false)?;
        if let Some(monitor) = self.scheduled.lock().unwrap().remove(name) {
            monitor.stop();
        }
        self.persist();
        Ok(())
    }

    fn persist(&self) {
        let state = StateFile { enabled: self.registry.enabled_names() };
        let _ = state.save(&self.state_path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{Plugin, PluginFactory};
    use kennel_proto::{ExtensionManifest, MonitorStatus};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    struct CountingPlugin {
        checks: Arc<AtomicUsize>,
    }
    impl Plugin for CountingPlugin {
        fn manifest(&self) -> ExtensionManifest {
            ExtensionManifest { name: "counting".into(), version: "0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
        }
        fn check(&mut self) -> MonitorStatus {
            self.checks.fetch_add(1, Ordering::SeqCst);
            MonitorStatus::Healthy
        }
        fn fix(&mut self) {}
    }

    fn manifest() -> ExtensionManifest {
        ExtensionManifest { name: "counting".into(), version: "0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
    }

    #[test]
    fn enable_actually_starts_ticking_and_persists_state() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");

        let checks = Arc::new(AtomicUsize::new(0));
        let checks_for_factory = checks.clone();
        let registry = Registry::new();
        registry.register(manifest(), Box::new(move || Box::new(CountingPlugin { checks: checks_for_factory.clone() }) as Box<dyn Plugin>) as PluginFactory);

        let manager = SchedulerManager::new(registry, state_path.clone());
        manager.enable("counting", true).unwrap();

        std::thread::sleep(Duration::from_millis(1500));
        assert!(checks.load(Ordering::SeqCst) >= 1, "enable() must actually schedule ticks, not just flip a flag");

        let saved = StateFile::load(&state_path);
        assert_eq!(saved.enabled, vec!["counting"], "enable() must persist to state.json");

        manager.disable("counting").unwrap();
        let count_at_disable = checks.load(Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(1500));
        assert_eq!(checks.load(Ordering::SeqCst), count_at_disable, "disable() must actually stop the scheduler thread");

        let saved = StateFile::load(&state_path);
        assert!(saved.enabled.is_empty(), "disable() must persist to state.json");
    }

    #[test]
    fn start_enabled_from_state_schedules_without_rewriting_the_file_it_just_read() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");
        StateFile { enabled: vec!["counting".into()] }.save(&state_path).unwrap();
        let before = std::fs::metadata(&state_path).unwrap().modified().unwrap();

        let checks = Arc::new(AtomicUsize::new(0));
        let checks_for_factory = checks.clone();
        let registry = Registry::new();
        registry.register(manifest(), Box::new(move || Box::new(CountingPlugin { checks: checks_for_factory.clone() }) as Box<dyn Plugin>) as PluginFactory);

        let manager = SchedulerManager::new(registry, state_path.clone());
        manager.start_enabled_from_state();

        std::thread::sleep(Duration::from_millis(1500));
        assert!(checks.load(Ordering::SeqCst) >= 1, "start_enabled_from_state() must actually schedule ticks");

        let after = std::fs::metadata(&state_path).unwrap().modified().unwrap();
        assert_eq!(before, after, "start_enabled_from_state() must not rewrite the file it just loaded from");
    }
}
