mod events;
#[cfg(test)]
mod events_e2e_tests;
mod extensions;
mod plugin;
mod registry;
mod scheduler;
mod scheduler_manager;
mod socket;
mod state;
mod wasm_host;

use std::path::PathBuf;
use std::sync::Arc;
use events::{EventService, LoginShell};
use registry::Registry;
use scheduler_manager::SchedulerManager;

fn support_dir() -> PathBuf {
    dirs_home().join("Library/Application Support/kennel")
}

fn dirs_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
}

fn main() {
    let support_dir = support_dir();
    std::fs::create_dir_all(&support_dir).expect("create kennel support dir");
    let events = EventService::open(&support_dir, Arc::new(LoginShell));
    let registry = Registry::new();

    let extensions_dir = support_dir.join("extensions");
    let registered = extensions::scan_and_register(&extensions_dir, &registry, Some(events.sink()));
    println!("registered {} extension(s): {}", registered.len(), registered.join(", "));

    let manager = Arc::new(SchedulerManager::new(registry, support_dir.join("state.json")));
    manager.start_enabled_from_state();

    let sock_path = support_dir.join("control.sock");
    println!("kenneld listening on {}", sock_path.display());
    if let Err(e) = socket::serve(&sock_path, manager, extensions_dir, events) {
        eprintln!("kenneld: {e}");
        std::process::exit(1);
    }
}
