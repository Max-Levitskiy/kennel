use kennel_guest_sdk::{kennel_extension, Manifest, Status};
use std::sync::atomic::{AtomicBool, Ordering};

static FIXED: AtomicBool = AtomicBool::new(false);

fn my_manifest() -> Manifest {
    Manifest { name: "unhealthy-then-fixed", version: "0.1.0", description: "test fixture", interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
}
fn my_check() -> Status {
    if FIXED.load(Ordering::SeqCst) { Status::Healthy } else { Status::Unhealthy("not fixed yet".into()) }
}
fn my_fix() { FIXED.store(true, Ordering::SeqCst); }

kennel_extension!(my_manifest, my_check, my_fix);
