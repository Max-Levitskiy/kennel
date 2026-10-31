use kennel_guest_sdk::{host_emit, kennel_extension, EventType, Manifest, Status};

// Emits the same three `ping` events on every check(), plus one of an
// undeclared type. Every check() runs in a fresh instance, so each tick
// re-emits them: kenneld must start one run per id, not one per tick.
fn my_manifest() -> Manifest {
    Manifest {
        name: "emits-events",
        version: "0.1.0",
        description: "test fixture",
        interval_secs: 1,
        capabilities: vec!["emit"],
        privileged_commands: vec![],
        events: vec![EventType { name: "ping", description: "a test ping", example_payload: r#"{"n": 0}"# }],
        ..Default::default()
    }
}
fn my_check() -> Status {
    for n in 1..=3 {
        host_emit("ping", &format!("evt-{n}"), &format!(r#"{{"n": {n}, "title": "it's $(not) a command"}}"#));
    }
    host_emit("bogus", "evt-x", "{}");
    Status::Healthy
}
fn my_fix() {}

kennel_extension!(my_manifest, my_check, my_fix);
