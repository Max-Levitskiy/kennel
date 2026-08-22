use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use kennel_proto::{Request, Response};
use crate::scheduler_manager::SchedulerManager;

// `extensions_dir` is the directory Request::Rescan re-scans. It is the
// daemon's own configured path, passed in here rather than accepted from a
// client, so no socket peer can point the daemon at a directory of its choosing.
pub fn serve(path: &Path, manager: Arc<SchedulerManager>, extensions_dir: PathBuf) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    for stream in listener.incoming() {
        let stream = stream?;
        let manager = manager.clone();
        let extensions_dir = extensions_dir.clone();
        std::thread::spawn(move || handle_connection(stream, manager, extensions_dir));
    }
    Ok(())
}

fn handle_connection(stream: UnixStream, manager: Arc<SchedulerManager>, extensions_dir: PathBuf) {
    let reader = BufReader::new(stream.try_clone().expect("clone unix stream"));
    let mut writer = stream;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() { continue; }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(&manager, req, &extensions_dir),
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let mut out = serde_json::to_string(&response).expect("Response always serializes");
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
    }
}

fn handle_request(manager: &SchedulerManager, req: Request, extensions_dir: &Path) -> Response {
    match req {
        Request::List => Response::Extensions(manager.registry().list()),
        Request::Enable { name } => match manager.enable(&name, true) {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error { message },
        },
        Request::Disable { name } => match manager.disable(&name) {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error { message },
        },
        // Without this, an extension installed by the GUI stayed invisible to
        // the running daemon until someone restarted it by hand -- scanning
        // only ever happened once, at startup. scan_and_register is idempotent:
        // already-registered extensions (including enabled, actively ticking
        // ones) are left exactly as they are.
        Request::Rescan => {
            let added = crate::extensions::scan_and_register(extensions_dir, manager.registry());
            if !added.is_empty() {
                println!("rescan registered {} new extension(s): {}", added.len(), added.join(", "));
            }
            Response::Ok
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{Plugin, PluginFactory};
    use crate::registry::Registry;
    use kennel_proto::{ExtensionManifest, MonitorStatus};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    struct NoopPlugin;
    impl Plugin for NoopPlugin {
        fn manifest(&self) -> ExtensionManifest {
            ExtensionManifest { name: "noop".into(), version: "0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] }
        }
        fn check(&mut self) -> MonitorStatus { MonitorStatus::Healthy }
        fn fix(&mut self) {}
    }

    #[test]
    fn list_enable_disable_round_trip_over_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("kennel.sock");
        let state_path = dir.path().join("state.json");

        let registry = Registry::new();
        registry.register(
            ExtensionManifest { name: "noop".into(), version: "0".into(), description: "".into(), interval_secs: 1, capabilities: vec![], privileged_commands: vec![] },
            Arc::new(|| Box::new(NoopPlugin) as Box<dyn Plugin>) as PluginFactory,
        );
        let manager = Arc::new(SchedulerManager::new(registry, state_path));

        let serve_path = sock_path.clone();
        let serve_manager = manager.clone();
        let serve_extensions_dir = dir.path().join("extensions");
        std::thread::spawn(move || { let _ = serve(&serve_path, serve_manager, serve_extensions_dir); });
        std::thread::sleep(std::time::Duration::from_millis(200)); // let the listener bind

        let mut conn = UnixStream::connect(&sock_path).unwrap();
        let mut reader = BufReader::new(conn.try_clone().unwrap());

        send(&mut conn, &Request::List);
        let resp: Response = recv(&mut reader);
        match resp {
            Response::Extensions(list) => { assert_eq!(list.len(), 1); assert!(!list[0].enabled); }
            other => panic!("expected Extensions, got {other:?}"),
        }

        send(&mut conn, &Request::Enable { name: "noop".into() });
        assert!(matches!(recv::<Response>(&mut reader), Response::Ok));

        send(&mut conn, &Request::List);
        match recv::<Response>(&mut reader) {
            Response::Extensions(list) => assert!(list[0].enabled),
            other => panic!("expected Extensions, got {other:?}"),
        }

        // Prove the socket path genuinely schedules a tick, not just flips the
        // flag: interval_secs is 1 above, so a short sleep should be enough
        // for the scheduler thread to have run at least one check.
        std::thread::sleep(std::time::Duration::from_millis(1500));
        send(&mut conn, &Request::List);
        match recv::<Response>(&mut reader) {
            Response::Extensions(list) => assert_eq!(list[0].last_status, Some(MonitorStatus::Healthy), "Enable over the socket must actually schedule ticks, not just flip a flag"),
            other => panic!("expected Extensions, got {other:?}"),
        }
    }

    // I2: installing an extension from the GUI used to do nothing at all until
    // someone restarted the daemon by hand.
    #[test]
    fn rescan_picks_up_an_extension_installed_after_startup() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("kennel.sock");
        let state_path = dir.path().join("state.json");
        let extensions_dir = dir.path().join("extensions");
        std::fs::create_dir_all(&extensions_dir).unwrap();

        let manager = Arc::new(SchedulerManager::new(Registry::new(), state_path));
        let serve_path = sock_path.clone();
        let serve_manager = manager.clone();
        let serve_extensions_dir = extensions_dir.clone();
        std::thread::spawn(move || { let _ = serve(&serve_path, serve_manager, serve_extensions_dir); });
        std::thread::sleep(std::time::Duration::from_millis(200));

        let mut conn = UnixStream::connect(&sock_path).unwrap();
        let mut reader = BufReader::new(conn.try_clone().unwrap());

        send(&mut conn, &Request::List);
        match recv::<Response>(&mut reader) {
            Response::Extensions(list) => assert!(list.is_empty()),
            other => panic!("expected Extensions, got {other:?}"),
        }

        // ... the GUI installs an extension while the daemon is running ...
        let ext_dir = extensions_dir.join("always-healthy");
        std::fs::create_dir_all(&ext_dir).unwrap();
        std::fs::write(ext_dir.join("manifest.toml"), "name = \"always-healthy\"\nversion = \"0.1.0\"\ndescription = \"test\"\ninterval_secs = 1\ncapabilities = []\n").unwrap();
        std::fs::copy(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm32-unknown-unknown/release/always_healthy.wasm"),
            ext_dir.join("monitor.wasm"),
        ).unwrap();

        // Without the Rescan the daemon still knows nothing about it.
        send(&mut conn, &Request::List);
        match recv::<Response>(&mut reader) {
            Response::Extensions(list) => assert!(list.is_empty(), "the daemon can't know about it before a rescan"),
            other => panic!("expected Extensions, got {other:?}"),
        }

        send(&mut conn, &Request::Rescan);
        assert!(matches!(recv::<Response>(&mut reader), Response::Ok));

        send(&mut conn, &Request::List);
        match recv::<Response>(&mut reader) {
            Response::Extensions(list) => {
                assert_eq!(list.len(), 1, "Rescan must register the newly installed extension");
                assert_eq!(list[0].manifest.name, "always-healthy");
                assert!(!list[0].enabled, "a freshly discovered extension stays disabled until the user enables it");
            }
            other => panic!("expected Extensions, got {other:?}"),
        }

        // A second rescan must be a no-op rather than a duplicate/replacement.
        send(&mut conn, &Request::Rescan);
        assert!(matches!(recv::<Response>(&mut reader), Response::Ok));
        send(&mut conn, &Request::List);
        match recv::<Response>(&mut reader) {
            Response::Extensions(list) => assert_eq!(list.len(), 1),
            other => panic!("expected Extensions, got {other:?}"),
        }
    }

    fn send(conn: &mut UnixStream, req: &Request) {
        let mut line = serde_json::to_string(req).unwrap();
        line.push('\n');
        conn.write_all(line.as_bytes()).unwrap();
    }

    fn recv<T: serde::de::DeserializeOwned>(reader: &mut BufReader<UnixStream>) -> T {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
}
