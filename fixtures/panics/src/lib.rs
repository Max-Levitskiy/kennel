use kennel_guest_sdk::{kennel_extension, Manifest, Status};

fn my_manifest() -> Manifest {
    Manifest { name: "panics", version: "0.1.0", description: "test fixture", interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
}
fn my_check() -> Status { panic!("intentional test panic") }
fn my_fix() {}

kennel_extension!(my_manifest, my_check, my_fix);
