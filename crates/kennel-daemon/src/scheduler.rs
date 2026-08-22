use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use crate::registry::Registry;

// Floor for the gap between two fix() calls for the same monitor, regardless of
// how short its interval is.
const MIN_FIX_COOLDOWN: Duration = Duration::from_secs(60);

// How long a monitor must wait after calling fix() before it may call it again,
// even if check() keeps reporting Unhealthy.
//
// fix() is a real, disruptive action -- gdrive-watchdog's pkills and relaunches
// Google Drive -- and a genuinely stuck condition stays Unhealthy on every
// single tick, so without this a 30s-interval monitor would restart the user's
// app roughly every 30s for as long as the stall lasted. 5x the extension's own
// interval keeps the ratio sane for slow monitors, with a 60s floor so a
// fast-ticking monitor (sd-keepalive ticks every 2s) can't turn its fix into a
// hot loop either. In-memory and per monitor thread on purpose: a restarted
// daemon, or an operator toggling the extension off and on, is an explicit
// "try again now" signal.
fn fix_cooldown(interval_secs: u64) -> Duration {
    MIN_FIX_COOLDOWN.max(Duration::from_secs(interval_secs.saturating_mul(5)))
}

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
    spawn_with_fix_cooldown(registry, name, interval_secs, fix_cooldown(interval_secs))
}

fn spawn_with_fix_cooldown(registry: Registry, name: String, interval_secs: u64, fix_cooldown: Duration) -> ScheduledMonitor {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let handle = thread::spawn(move || {
        let mut last_fix: Option<Instant> = None;
        while !stop_clone.load(Ordering::SeqCst) {
            if let Some(mut plugin) = registry.make_plugin(&name) {
                let status = plugin.check();
                let unhealthy = matches!(status, kennel_proto::MonitorStatus::Unhealthy { .. });
                // Status is always published, cooldown or not, so the GUI and
                // the control socket always show current health -- only the
                // fix() *action* is rate-limited.
                registry.update_status(&name, status);
                if unhealthy {
                    let due = last_fix.map_or(true, |last| last.elapsed() >= fix_cooldown);
                    if due {
                        last_fix = Some(Instant::now());
                        plugin.fix();
                    }
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

    fn counting_registry(checks: &Arc<AtomicUsize>, fixes: &Arc<AtomicUsize>) -> Registry {
        let (c1, f1) = (checks.clone(), fixes.clone());
        let registry = Registry::new();
        registry.register(
            ExtensionManifest { name: "counting".into(), version: "0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] },
            Arc::new(move || Box::new(CountingPlugin { checks: c1.clone(), fixes: f1.clone() }) as Box<dyn Plugin>) as PluginFactory,
        );
        registry
    }

    #[test]
    fn ticks_call_check_and_fix_on_unhealthy() {
        let checks = Arc::new(AtomicUsize::new(0));
        let fixes = Arc::new(AtomicUsize::new(0));
        let registry = counting_registry(&checks, &fixes);

        let monitor = spawn(registry.clone(), "counting".into(), 1);
        thread::sleep(Duration::from_millis(2500));
        monitor.stop();

        assert!(checks.load(Ordering::SeqCst) >= 2, "expected at least 2 ticks in 2.5s at 1s interval, got {}", checks.load(Ordering::SeqCst));
        assert!(fixes.load(Ordering::SeqCst) >= 1, "an unhealthy check must still trigger a fix");
        // Every tick reports Unhealthy, so status must track that on every tick
        // even while fix() is being rate-limited.
        assert!(matches!(registry.list()[0].last_status, Some(MonitorStatus::Unhealthy { .. })));
    }

    // I6: fix() used to run on EVERY unhealthy tick. For gdrive-watchdog (30s
    // interval, fix() = pkill + relaunch Google Drive) a persistent stall meant
    // killing the user's app every 30-90s indefinitely -- the stated reason that
    // extension can't be enabled long-term.
    #[test]
    fn repeated_unhealthy_ticks_only_fix_once_within_the_cooldown() {
        let checks = Arc::new(AtomicUsize::new(0));
        let fixes = Arc::new(AtomicUsize::new(0));
        let registry = counting_registry(&checks, &fixes);

        // Default cooldown for a 1s interval is the 60s floor -- far longer than
        // this test's window, so every tick after the first must be suppressed.
        let monitor = spawn(registry, "counting".into(), 1);
        thread::sleep(Duration::from_millis(3500));
        monitor.stop();

        assert!(checks.load(Ordering::SeqCst) >= 3, "expected several unhealthy ticks, got {}", checks.load(Ordering::SeqCst));
        assert_eq!(fixes.load(Ordering::SeqCst), 1, "fix() must be rate-limited to once per cooldown no matter how many ticks report Unhealthy");
    }

    #[test]
    fn fix_runs_again_once_the_cooldown_has_elapsed() {
        let checks = Arc::new(AtomicUsize::new(0));
        let fixes = Arc::new(AtomicUsize::new(0));
        let registry = counting_registry(&checks, &fixes);

        // Same code path as production, with a cooldown short enough to observe
        // it expiring (the real one is >= 60s).
        let monitor = spawn_with_fix_cooldown(registry, "counting".into(), 1, Duration::from_millis(1500));
        thread::sleep(Duration::from_millis(3500));
        monitor.stop();

        let fixes = fixes.load(Ordering::SeqCst);
        assert!(fixes >= 2, "fix() must resume after the cooldown elapses, got {fixes}");
        assert!(fixes < checks.load(Ordering::SeqCst), "...but still not once per tick");
    }

    #[test]
    fn fix_cooldown_is_five_intervals_with_a_sixty_second_floor() {
        assert_eq!(fix_cooldown(2), Duration::from_secs(60), "sd-keepalive's 2s interval must not turn fix() into a hot loop");
        assert_eq!(fix_cooldown(30), Duration::from_secs(150), "gdrive-watchdog: 5 x 30s");
        assert_eq!(fix_cooldown(0), Duration::from_secs(60));
        assert_eq!(fix_cooldown(u64::MAX), Duration::from_secs(u64::MAX), "saturating, not overflowing");
    }
}
