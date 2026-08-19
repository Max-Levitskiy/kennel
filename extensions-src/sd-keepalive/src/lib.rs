use kennel_guest_sdk::{host_log, host_now_unix_secs, host_write_file, kennel_extension, Manifest, Status};

const MOUNT_MARKER: &str = "/Volumes/Vault/.keepalive";

fn my_manifest() -> Manifest {
    Manifest {
        name: "sd-keepalive",
        version: "0.1.0",
        description: "Keeps the GL9755 SD reader's PCIe link awake",
        interval_secs: 2,
        capabilities: vec!["write_file", "log"],
        privileged_commands: vec![],
    }
}

fn my_check() -> Status {
    let now = host_now_unix_secs();
    let ok = host_write_file(MOUNT_MARKER, now.to_string().as_bytes());
    if ok {
        Status::Healthy
    } else {
        host_log("warn", "Vault not mounted, could not touch .keepalive");
        Status::Unhealthy("/Volumes/Vault is not mounted".into())
    }
}

fn my_fix() {
    host_log("info", "sd-keepalive: card not mounted, nothing kennel can do until it's reseated");
}

kennel_extension!(my_manifest, my_check, my_fix);
