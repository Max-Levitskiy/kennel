mod extensions;
mod plugin;
mod registry;
mod scheduler;
mod socket;
mod state;
mod wasm_host;

use std::path::PathBuf;
use registry::Registry;

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
    extensions::scan_and_register(&extensions_dir, &registry);

    let state = state::StateFile::load(&state_path());
    let mut monitors = Vec::new();
    for name in &state.enabled {
        if registry.set_enabled(name, true).is_ok() {
            let manifest = registry.list().into_iter().find(|e| &e.manifest.name == name);
            if let Some(info) = manifest {
                monitors.push(scheduler::spawn(registry.clone(), name.clone(), info.manifest.interval_secs));
            }
        }
    }

    let sock_path = socket_path();
    if let Some(parent) = sock_path.parent() {
        std::fs::create_dir_all(parent).expect("create kennel support dir");
    }
    println!("kenneld listening on {}", sock_path.display());
    socket::serve(&sock_path, registry).expect("socket server crashed");
}
