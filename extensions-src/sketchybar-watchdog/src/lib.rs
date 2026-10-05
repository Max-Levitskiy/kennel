// sketchybar-watchdog -- restarts sketchybar when the bar is not actually on screen.
//
// This used to infer health from a wall-clock gap ("a gap appeared, so the Mac
// slept, so kick sketchybar"). That model was wrong in both directions. It fired
// after every wake whether or not anything was broken, and -- the reason it was
// rewritten -- it was blind to the failure that actually loses the bar: a display
// being connected or disconnected. During one such outage sketchybar was running,
// answering `--query bar`, and reporting drawing=on/hidden=off, with the bar
// nowhere on screen. Process liveness, socket liveness and the bar's own flags
// were all green for 41 hours.
//
// So health is now measured the only way that matches what a human sees: ask the
// window server for sketchybar's actual window geometry, per display. That covers
// the wake case too, without having to guess at causes.

use kennel_guest_sdk::{
    host_launchctl, host_log, host_spawn, host_state_get, host_state_set, kennel_extension,
    Manifest, Status,
};

// The probe lives next to kenneld inside Kennel.app. Spawned through `sh -c` so
// $HOME expands -- guest code has no environment of its own.
const PROBE: &str = "$HOME/Applications/Kennel.app/Contents/MacOS/kennel-barprobe";

// A display change legitimately leaves the bar absent for a moment while
// sketchybar rebuilds one window per item. Requiring two consecutive bad ticks
// (~10s at a 5s interval) keeps the watchdog from kickstarting sketchybar every
// time a monitor is plugged in, which would be worse than the bug.
const CONFIRM_TICKS: u32 = 2;

const STATE_KEY: &str = "consecutive_missing";

fn my_manifest() -> Manifest {
    Manifest {
        name: "sketchybar-watchdog",
        version: "0.2.0",
        description: "Restarts sketchybar when the bar is not actually on screen",
        interval_secs: 5,
        capabilities: vec!["launchctl", "spawn", "state", "log"],
        privileged_commands: vec![],
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Probe {
    pub displays: u32,
    pub with_bar: u32,
}

// Reads the probe's first line: `displays=2 with_bar=1`. Anything else is treated
// as no answer rather than as a healthy answer -- see `decide`.
pub fn parse_probe(stdout: &str) -> Option<Probe> {
    let line = stdout.lines().next()?;
    let mut displays = None;
    let mut with_bar = None;
    for field in line.split_whitespace() {
        match field.split_once('=') {
            Some(("displays", v)) => displays = v.parse().ok(),
            Some(("with_bar", v)) => with_bar = v.parse().ok(),
            _ => {}
        }
    }
    Some(Probe { displays: displays?, with_bar: with_bar? })
}

#[derive(Debug, PartialEq)]
pub enum Decision {
    // Nothing to do; reset the confirmation counter.
    Healthy,
    // The bar is missing but not yet confirmed. Carries the new counter value.
    Confirming(u32),
    // Missing on `CONFIRM_TICKS` consecutive ticks -- restart, with this detail.
    Restart(String),
    // The probe could not be trusted. Deliberately NOT a restart: acting on no
    // information is how a watchdog turns into a restart loop.
    Unknown(String),
}

pub fn decide(exit_code: i32, stdout: &str, consecutive: u32) -> Decision {
    if exit_code != 0 {
        return Decision::Unknown(format!("probe exited {exit_code}"));
    }
    let probe = match parse_probe(stdout) {
        Some(p) => p,
        None => return Decision::Unknown("probe printed no parseable summary".into()),
    };
    // No active displays at all (lid shut, every screen asleep). A bar is
    // legitimately absent here, and restarting sketchybar at a dark machine
    // would just churn.
    if probe.displays == 0 {
        return Decision::Healthy;
    }
    if probe.with_bar >= probe.displays {
        return Decision::Healthy;
    }
    let n = consecutive + 1;
    if n < CONFIRM_TICKS {
        Decision::Confirming(n)
    } else {
        Decision::Restart(format!(
            "bar missing on {} of {} display(s) for {} consecutive checks",
            probe.displays - probe.with_bar,
            probe.displays,
            n
        ))
    }
}

fn my_check() -> Status {
    let result = host_spawn("/bin/sh", &["-c", PROBE]);
    let consecutive: u32 = host_state_get(STATE_KEY).parse().unwrap_or(0);

    match decide(result.exit_code, &result.stdout, consecutive) {
        Decision::Healthy => {
            if consecutive > 0 {
                host_log("info", "bar is back on every display");
            }
            host_state_set(STATE_KEY, "0");
            Status::Healthy
        }
        Decision::Confirming(n) => {
            host_state_set(STATE_KEY, &n.to_string());
            // Still reported Healthy: this is the debounce window, and a display
            // change that resolves on its own must not show up as a fault.
            host_log("info", &format!("bar missing (check {n} of {CONFIRM_TICKS}); {}", first_line(&result.stdout)));
            Status::Healthy
        }
        Decision::Restart(detail) => {
            host_state_set(STATE_KEY, &(consecutive + 1).to_string());
            // The full per-display geometry goes to the log, so the next time this
            // fires there is a record of what the window server actually reported.
            host_log("warn", &format!("{detail}\n{}", result.stdout.trim()));
            Status::Unhealthy(detail)
        }
        Decision::Unknown(why) => {
            host_log("warn", &format!("cannot tell whether the bar is up: {why}; {}", result.stderr.trim()));
            Status::Healthy
        }
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

fn my_fix() {
    let uid = host_spawn("/usr/bin/id", &["-u"]).stdout.trim().to_string();
    host_log("warn", "restarting sketchybar");
    host_launchctl("kickstart", &["-k", &format!("gui/{uid}/homebrew.mxcl.sketchybar")]);
    // Start the confirmation over, so the restart gets a full debounce window to
    // take effect before another one is considered.
    host_state_set(STATE_KEY, "0");
}

kennel_extension!(my_manifest, my_check, my_fix);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_probe_summary_line() {
        let p = parse_probe("displays=2 with_bar=1\ndisplay id=1 ...\n").unwrap();
        assert_eq!(p, Probe { displays: 2, with_bar: 1 });
    }

    #[test]
    fn garbage_output_is_not_mistaken_for_a_healthy_answer() {
        assert!(parse_probe("").is_none());
        assert!(parse_probe("segfault").is_none());
        assert!(parse_probe("displays=2").is_none(), "a half-written line is not an answer");
    }

    #[test]
    fn a_bar_on_every_display_is_healthy() {
        assert_eq!(decide(0, "displays=2 with_bar=2", 0), Decision::Healthy);
        assert_eq!(decide(0, "displays=1 with_bar=1", 5), Decision::Healthy, "recovery resets, whatever the counter was");
    }

    // The reported failure: fine on one screen, gone on the other.
    #[test]
    fn a_bar_missing_on_one_of_two_displays_is_confirmed_then_restarted() {
        assert_eq!(decide(0, "displays=2 with_bar=1", 0), Decision::Confirming(1));
        match decide(0, "displays=2 with_bar=1", 1) {
            Decision::Restart(d) => assert!(d.contains("1 of 2"), "{d}"),
            other => panic!("expected a restart on the second consecutive check, got {other:?}"),
        }
    }

    // Plugging in a monitor blanks the bar for a moment while sketchybar rebuilds
    // its windows. One bad tick must never be enough.
    #[test]
    fn a_single_bad_tick_never_restarts_anything() {
        assert_eq!(decide(0, "displays=2 with_bar=0", 0), Decision::Confirming(1));
    }

    // A probe that cannot read the window server must not be read as "the bar is
    // gone" -- that is a restart loop, not a fix.
    #[test]
    fn an_unusable_probe_never_triggers_a_restart() {
        assert!(matches!(decide(2, "", 0), Decision::Unknown(_)));
        assert!(matches!(decide(0, "", 9), Decision::Unknown(_)));
        assert!(matches!(decide(1, "displays=2 with_bar=0", 9), Decision::Unknown(_)),
                "a nonzero exit wins over whatever happens to be on stdout");
    }

    // Lid shut, or every display asleep. Nothing is broken.
    #[test]
    fn no_active_displays_is_healthy_not_a_missing_bar() {
        assert_eq!(decide(0, "displays=0 with_bar=0", 3), Decision::Healthy);
    }
}
