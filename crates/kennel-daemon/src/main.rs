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
use registry::Registry;
use scheduler_manager::SchedulerManager;

fn socket_path() -> PathBuf {
    dirs_home().join("Library/Application Support/kennel/control.sock")
}

fn state_path() -> PathBuf {
    dirs_home().join("Library/Application Support/kennel/state.json")
}

fn dirs_home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME must be set"))
}

fn main() {
    let registry = Registry::new();

    let extensions_dir = dirs_home().join("Library/Application Support/kennel/extensions");
    let registered = extensions::scan_and_register(&extensions_dir, &registry);
    println!("registered {} extension(s): {}", registered.len(), registered.join(", "));

    let manager = Arc::new(SchedulerManager::new(registry, state_path()));
    manager.start_enabled_from_state();

    let sock_path = socket_path();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent).expect("create kennel support dir");
    }
    println!("kenneld listening on {}", sock_path.display());
    if let Err(e) = socket::serve(&sock_path, manager, extensions_dir) {
        eprintln!("kenneld: {e}");
        std::process::exit(1);
    }
}
