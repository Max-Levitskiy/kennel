use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use crate::registry::Registry;

pub struct ScheduledMonitor {
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

impl ScheduledMonitor {
    pub fn stop(self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = self.handle.join();
    }
}

pub fn spawn(registry: Registry, name: String, interval_secs: u64) -> ScheduledMonitor {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let handle = thread::spawn(move || {
        while !stop_clone.load(Ordering::SeqCst) {
            if let Some(mut plugin) = registry.make_plugin(&name) {
                let status = plugin.check();
                let unhealthy = matches!(status, kennel_proto::MonitorStatus::Unhealthy { .. });
                registry.update_status(&name, status);
                if unhealthy {
                    plugin.fix();
                }
            }
            // Sleep in short slices so `stop` is noticed quickly instead of
            // blocking for the whole interval.
            let mut waited = 0u64;
            while waited < interval_secs && !stop_clone.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_secs(1));
                waited += 1;
            }
        }
    });
    ScheduledMonitor { stop, handle }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{Plugin, PluginFactory};
    use kennel_proto::{ExtensionManifest, MonitorStatus};
    use std::sync::atomic::AtomicUsize;

    struct CountingPlugin {
        checks: Arc<AtomicUsize>,
        fixes: Arc<AtomicUsize>,
    }
    impl Plugin for CountingPlugin {
        fn manifest(&self) -> ExtensionManifest {
            ExtensionManifest { name: "counting".into(), version: "0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
        }
        fn check(&mut self) -> MonitorStatus {
            self.checks.fetch_add(1, Ordering::SeqCst);
            MonitorStatus::Unhealthy { detail: "always".into() }
        }
        fn fix(&mut self) {
            self.fixes.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn ticks_call_check_and_fix_on_unhealthy() {
        let checks = Arc::new(AtomicUsize::new(0));
        let fixes = Arc::new(AtomicUsize::new(0));
        let (c1, f1) = (checks.clone(), fixes.clone());

        let registry = Registry::new();
        registry.register(
            ExtensionManifest { name: "counting".into(), version: "0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] },
            Box::new(move || Box::new(CountingPlugin { checks: c1.clone(), fixes: f1.clone() }) as Box<dyn Plugin>) as PluginFactory,
        );

        let monitor = spawn(registry, "counting".into(), 1);
        thread::sleep(Duration::from_millis(2500));
        monitor.stop();

        assert!(checks.load(Ordering::SeqCst) >= 2, "expected at least 2 ticks in 2.5s at 1s interval, got {}", checks.load(Ordering::SeqCst));
        assert_eq!(checks.load(Ordering::SeqCst), fixes.load(Ordering::SeqCst), "every unhealthy check should trigger exactly one fix");
    }
}
