use kennel_guest_sdk::{host_notify, host_spawn, host_state_get, host_state_set, kennel_extension, Manifest, Status};

const CONFIRM_FAILURES: u32 = 2;
const GRACE_AFTER_START_SECS: u64 = 120;

fn my_manifest() -> Manifest {
    Manifest { name: "gdrive-watchdog", version: "0.1.0", description: "Detects Google Drive FileProvider stalls and restarts Drive", interval_secs: 30, capabilities: vec!["spawn", "notify", "state"], privileged_commands: vec![] }
}

fn drive_domains() -> Vec<String> {
    let result = host_spawn("/bin/sh", &["-c", "ls -d $HOME/Library/CloudStorage/GoogleDrive-* 2>/dev/null"]);
    result.stdout.lines().map(|s| s.to_string()).collect()
}

fn probe_stalled() -> bool {
    for domain in drive_domains() {
        let result = host_spawn("/bin/ls", &["-1", &domain]);
        // A wasmtime epoch timeout on a hung `ls` surfaces as a nonzero-exit
        // spawn result (the host's Command::output returns once the child is
        // reaped or errors), so exit_code != 0 covers both "stalled" and
        // "domain briefly unreadable".
        if result.exit_code != 0 {
            return true;
        }
    }
    false
}

fn my_check() -> Status {
    if drive_domains().is_empty() {
        return Status::Healthy; // Drive isn't running -- nothing to watch, matches the original script's behavior
    }
    if !probe_stalled() {
        host_state_set("consecutive_stalls", "0");
        return Status::Healthy;
    }
    let consecutive: u32 = host_state_get("consecutive_stalls").parse().unwrap_or(0) + 1;
    host_state_set("consecutive_stalls", &consecutive.to_string());
    if consecutive < CONFIRM_FAILURES {
        Status::Healthy // not confirmed yet, matches CONFIRM_FAILURES in the original
    } else {
        Status::Unhealthy(format!("{consecutive} consecutive stalled probes"))
    }
}

fn my_fix() {
    host_notify("Google Drive frozen", "Restarting Drive…");
    host_spawn("/usr/bin/osascript", &["-e", "tell application \"Google Drive\" to quit"]);
    host_spawn("/bin/sleep", &["3"]);
    host_spawn("/usr/bin/pkill", &["-9", "-f", "Google Drive.app/Contents/MacOS/Google Drive"]);
    host_spawn("/usr/bin/open", &["-a", "/Applications/Google Drive.app"]);
    host_state_set("consecutive_stalls", "0");
    host_notify("Google Drive restarted", "Watch for recovery on the next check.");
}

kennel_extension!(my_manifest, my_check, my_fix);
