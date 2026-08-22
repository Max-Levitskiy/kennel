use kennel_guest_sdk::{host_launchctl, host_now_unix_secs, host_spawn, host_state_get, host_state_set, kennel_extension, Manifest, Status};

const GAP_THRESHOLD_SECS: u64 = 8; // matches sd-keepalive.sh's own GAP_THRESHOLD, already proven on this machine

fn my_manifest() -> Manifest {
    Manifest { name: "sketchybar-watchdog", version: "0.1.0", description: "Restarts sketchybar after the Mac wakes from sleep", interval_secs: 5, capabilities: vec!["launchctl", "spawn", "state"], privileged_commands: vec![] }
}

fn now_secs() -> u64 {
    host_now_unix_secs()
}

fn my_check() -> Status {
    let now = now_secs();
    let last_tick: u64 = host_state_get("last_tick").parse().unwrap_or(now);
    host_state_set("last_tick", &now.to_string());

    let gap = now.saturating_sub(last_tick);
    if gap > GAP_THRESHOLD_SECS {
        Status::Unhealthy(format!("system was asleep for ~{gap}s, sketchybar needs a kick"))
    } else {
        Status::Healthy
    }
}

fn my_fix() {
    let uid = host_spawn("/usr/bin/id", &["-u"]).stdout.trim().to_string();
    host_launchctl("kickstart", &["-k", &format!("gui/{uid}/homebrew.mxcl.sketchybar")]);
}

kennel_extension!(my_manifest, my_check, my_fix);
