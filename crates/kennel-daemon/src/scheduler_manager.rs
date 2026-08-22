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

    // Holds `scheduled` across the entire body -- registry update, spawn/stop,
    // map mutation, and persist -- so enable() and disable() are atomic with
    // respect to each other for a given name. Without this, two concurrent
    // calls for the same name (e.g. a fast double-toggle from the GUI, or two
    // socket clients) could interleave and leave the scheduled-thread map
    // inconsistent with both the registry flag and state.json in either
    // direction: disabled-but-still-ticking, or enabled-but-nothing-running.
    // scheduler.rs's monitor threads only ever touch `registry`, never
    // `scheduled`, so holding this lock longer introduces no new lock
    // ordering / deadlock risk.
    pub fn enable(&self, name: &str, persist: bool) -> Result<(), String> {
        let mut scheduled = self.scheduled.lock().unwrap();
        self.registry.set_enabled(name, true)?;
        let interval = self.registry.list().into_iter()
            .find(|e| e.manifest.name == name)
            .map(|e| e.manifest.interval_secs)
            .ok_or_else(|| format!("unknown extension: {name}"))?;
        // Stop any already-running monitor for this name BEFORE spawning the
        // replacement (not after), so there is never a window where two
        // monitor threads for the same extension are both alive and able to
        // independently tick / call fix().
        if let Some(old) = scheduled.remove(name) {
            old.stop();
        }
        let monitor = scheduler::spawn(self.registry.clone(), name.to_string(), interval);
        scheduled.insert(name.to_string(), monitor);
        if persist {
            self.persist();
        }
        Ok(())
    }

    pub fn disable(&self, name: &str) -> Result<(), String> {
        let mut scheduled = self.scheduled.lock().unwrap();
        self.registry.set_enabled(name, false)?;
        if let Some(monitor) = scheduled.remove(name) {
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
        registry.register(manifest(), Arc::new(move || Box::new(CountingPlugin { checks: checks_for_factory.clone() }) as Box<dyn Plugin>) as PluginFactory);

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
        registry.register(manifest(), Arc::new(move || Box::new(CountingPlugin { checks: checks_for_factory.clone() }) as Box<dyn Plugin>) as PluginFactory);

        let manager = SchedulerManager::new(registry, state_path.clone());
        manager.start_enabled_from_state();

        std::thread::sleep(Duration::from_millis(1500));
        assert!(checks.load(Ordering::SeqCst) >= 1, "start_enabled_from_state() must actually schedule ticks");

        let after = std::fs::metadata(&state_path).unwrap().modified().unwrap();
        assert_eq!(before, after, "start_enabled_from_state() must not rewrite the file it just loaded from");
    }

    // The riskiest untested path: calling enable() twice for the same name
    // with no disable() in between must never leave two monitor threads
    // both ticking. Rather than approximating this from tick *counts* over a
    // wall-clock window (fragile to scheduling jitter), each check() records
    // how many check() calls are concurrently in flight, across every
    // monitor instance spawned for this name. If enable() ever spawned a
    // replacement before the old monitor had genuinely stopped, the two
    // threads' 200ms-long check() calls -- fired within microseconds of each
    // other -- would overlap and this would observe more than one
    // concurrently in-flight check().
    #[test]
    fn double_enable_without_disable_never_runs_two_monitors_concurrently() {
        struct ConcurrencyTrackingPlugin {
            checks: Arc<AtomicUsize>,
            concurrent: Arc<AtomicUsize>,
            max_concurrent: Arc<AtomicUsize>,
        }
        impl Plugin for ConcurrencyTrackingPlugin {
            fn manifest(&self) -> ExtensionManifest {
                manifest()
            }
            fn check(&mut self) -> MonitorStatus {
                let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
                self.max_concurrent.fetch_max(now, Ordering::SeqCst);
                self.checks.fetch_add(1, Ordering::SeqCst);
                // Held long enough that a leaked old monitor's check() and a
                // freshly spawned replacement's check() would provably
                // overlap in wall-clock time if both were alive at once.
                std::thread::sleep(Duration::from_millis(200));
                self.concurrent.fetch_sub(1, Ordering::SeqCst);
                MonitorStatus::Healthy
            }
            fn fix(&mut self) {}
        }

        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("state.json");

        let checks = Arc::new(AtomicUsize::new(0));
        let concurrent = Arc::new(AtomicUsize::new(0));
        let max_concurrent = Arc::new(AtomicUsize::new(0));
        let (c1, cc1, mc1) = (checks.clone(), concurrent.clone(), max_concurrent.clone());
        let registry = Registry::new();
        registry.register(
            manifest(),
            Arc::new(move || Box::new(ConcurrencyTrackingPlugin {
                checks: c1.clone(),
                concurrent: cc1.clone(),
                max_concurrent: mc1.clone(),
            }) as Box<dyn Plugin>) as PluginFactory,
        );

        let manager = SchedulerManager::new(registry, state_path);
        manager.enable("counting", true).unwrap();
        manager.enable("counting", true).unwrap(); // re-enable with nothing in between: must replace, never leave two live tickers

        std::thread::sleep(Duration::from_millis(2000));
        manager.disable("counting").unwrap();

        assert!(checks.load(Ordering::SeqCst) >= 1, "expected at least one tick across the test");
        assert_eq!(
            max_concurrent.load(Ordering::SeqCst),
            1,
            "enable() must fully stop the previous monitor before starting a new one -- observed two concurrent check() calls for the same extension"
        );
    }
}
