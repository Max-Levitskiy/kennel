use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime};
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

// Source of "now" for cooldown accounting. Injected only so tests can express a
// long sleep without sleeping; production always passes `SystemTime::now`.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

// Whether a monitor whose last fix() ran at `last_fix` may fix again at `now`.
//
// I7: this deliberately reads a wall clock rather than `Instant`. On macOS
// `Instant` is backed by CLOCK_UPTIME_RAW, which *stops* while the machine is
// asleep, so it measures awake time only. Half of what kennel watches for is
// caused by sleep -- sketchybar-watchdog's entire health condition is "a wall
// clock gap appeared, so the Mac was asleep" -- and gating that fix on awake
// time silently swallowed the fix for the one wake that mattered. The cooldown
// has to be counted on the same clock the check itself uses.
fn fix_is_due(last_fix: Option<SystemTime>, now: SystemTime, cooldown: Duration) -> bool {
    match last_fix {
        None => true,
        // A backwards jump (NTP correction, user setting the clock) makes the
        // elapsed time unknowable. Fail open: fixing once too often is a
        // nuisance, never fixing again is the outage this guard caused.
        Some(last) => now.duration_since(last).map_or(true, |elapsed| elapsed >= cooldown),
    }
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
    spawn_with_clock(registry, name, interval_secs, fix_cooldown, Arc::new(SystemTime::now))
}

fn spawn_with_clock(registry: Registry, name: String, interval_secs: u64, fix_cooldown: Duration, now: Clock) -> ScheduledMonitor {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_clone = stop.clone();
    let handle = thread::spawn(move || {
        let mut last_fix: Option<SystemTime> = None;
        while !stop_clone.load(Ordering::SeqCst) {
            if let Some(mut plugin) = registry.make_plugin(&name) {
                let status = plugin.check();
                let unhealthy = matches!(status, kennel_proto::MonitorStatus::Unhealthy { .. });
                // Status is always published, cooldown or not, so the GUI and
                // the control socket always show current health -- only the
                // fix() *action* is rate-limited.
                registry.update_status(&name, status);
                if unhealthy {
                    let at = now();
                    if fix_is_due(last_fix, at, fix_cooldown) {
                        last_fix = Some(at);
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
    use std::sync::Mutex;

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

    // A clock the test drives by hand, so a "the Mac slept for ten minutes" can be
    // expressed without the test itself taking ten minutes. Advancing it models
    // exactly what macOS does across a sleep: wall time jumps by the whole sleep
    // while the uptime clock behind `Instant` barely moves.
    #[derive(Clone)]
    struct FakeClock(Arc<Mutex<SystemTime>>);

    impl FakeClock {
        fn new() -> Self {
            FakeClock(Arc::new(Mutex::new(SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000))))
        }
        fn advance(&self, by: Duration) {
            let mut t = self.0.lock().unwrap();
            *t += by;
        }
        fn as_clock(&self) -> Clock {
            let inner = self.0.clone();
            Arc::new(move || *inner.lock().unwrap())
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

    // I7: the cooldown used to be measured with `Instant`, which on macOS stops
    // while the machine is asleep. sketchybar-watchdog exists *because* of sleep:
    // on 2026-09-01 it kicked sketchybar at 16:36:31, the Mac slept, and it woke
    // at 16:47:59 still needing a kick -- but only ~11 seconds of *awake* time
    // had passed, so the 60s cooldown had not expired and the fix was swallowed.
    // The next tick was healthy again, so nothing ever retried: the bar stayed
    // missing for 41 hours until a human noticed. The cooldown has to be counted
    // on the same clock the health check itself uses -- wall time.
    #[test]
    fn a_sleep_longer_than_the_cooldown_is_still_fixed_on_the_next_wake() {
        let checks = Arc::new(AtomicUsize::new(0));
        let fixes = Arc::new(AtomicUsize::new(0));
        let registry = counting_registry(&checks, &fixes);
        let clock = FakeClock::new();

        let monitor = spawn_with_clock(registry, "counting".into(), 1, Duration::from_secs(60), clock.as_clock());
        thread::sleep(Duration::from_millis(1200));
        assert_eq!(fixes.load(Ordering::SeqCst), 1, "the first unhealthy tick must fix, and the one right after it must be suppressed");

        // The Mac sleeps for ten minutes and wakes still unhealthy. Real time in
        // this test moves by ~1.5s, so an `Instant`-based cooldown would still
        // read ~1.5s elapsed and suppress the fix -- which is the bug.
        clock.advance(Duration::from_secs(600));
        thread::sleep(Duration::from_millis(1500));
        monitor.stop();

        assert_eq!(fixes.load(Ordering::SeqCst), 2, "a 10-minute sleep outlasts the 60s cooldown, so the tick after the wake must fix");
    }

    #[test]
    fn fix_is_due_only_once_the_cooldown_has_actually_elapsed() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let cooldown = Duration::from_secs(60);

        assert!(fix_is_due(None, base, cooldown), "a monitor that has never fixed must be allowed to");
        assert!(!fix_is_due(Some(base), base + Duration::from_secs(59), cooldown));
        assert!(fix_is_due(Some(base), base + Duration::from_secs(60), cooldown), "the boundary counts as elapsed");
        assert!(fix_is_due(Some(base), base + Duration::from_secs(600), cooldown), "time the machine spent asleep counts too");
    }

    // NTP corrections and a user changing the clock can move wall time backwards,
    // which makes "how long since the last fix" unanswerable. Fail open: fixing
    // once too often is a nuisance, never fixing again is the outage above.
    #[test]
    fn a_backwards_clock_jump_lets_fix_run_rather_than_wedging_it_shut() {
        let base = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let jumped_back = base - Duration::from_secs(3600);
        assert!(fix_is_due(Some(base), jumped_back, Duration::from_secs(60)));
    }

    #[test]
    fn fix_cooldown_is_five_intervals_with_a_sixty_second_floor() {
        assert_eq!(fix_cooldown(2), Duration::from_secs(60), "sd-keepalive's 2s interval must not turn fix() into a hot loop");
        assert_eq!(fix_cooldown(30), Duration::from_secs(150), "gdrive-watchdog: 5 x 30s");
        assert_eq!(fix_cooldown(0), Duration::from_secs(60));
        assert_eq!(fix_cooldown(u64::MAX), Duration::from_secs(u64::MAX), "saturating, not overflowing");
    }
}
