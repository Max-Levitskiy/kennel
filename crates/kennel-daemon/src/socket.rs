use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use kennel_proto::{Request, Response};
use crate::registry::Registry;

pub fn serve(path: &Path, registry: Registry) -> std::io::Result<()> {
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    for stream in listener.incoming() {
        let stream = stream?;
        let registry = registry.clone();
        std::thread::spawn(move || handle_connection(stream, registry));
    }
    Ok(())
}

fn handle_connection(stream: UnixStream, registry: Registry) {
    let reader = BufReader::new(stream.try_clone().expect("clone unix stream"));
    let mut writer = stream;
    for line in reader.lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() { continue; }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(req) => handle_request(&registry, req),
            Err(e) => Response::Error { message: format!("bad request: {e}") },
        };
        let mut out = serde_json::to_string(&response).expect("Response always serializes");
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
    }
}

fn handle_request(registry: &Registry, req: Request) -> Response {
    match req {
        Request::List => Response::Extensions(registry.list()),
        Request::Enable { name } => match registry.set_enabled(&name, true) {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error { message },
        },
        Request::Disable { name } => match registry.set_enabled(&name, false) {
            Ok(()) => Response::Ok,
            Err(message) => Response::Error { message },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{Plugin, PluginFactory};
    use kennel_proto::{ExtensionManifest, MonitorStatus};
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    struct NoopPlugin;
    impl Plugin for NoopPlugin {
        fn manifest(&self) -> ExtensionManifest {
            ExtensionManifest { name: "noop".into(), version: "0".into(), description: "".into(), interval_secs: 60, capabilities: vec![], privileged_commands: vec![] }
        }
        fn check(&mut self) -> MonitorStatus { MonitorStatus::Healthy }
        fn fix(&mut self) {}
    }

    #[test]
    fn list_enable_disable_round_trip_over_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock_path = dir.path().join("kennel.sock");

        let registry = Registry::new();
        registry.register(
            ExtensionManifest { name: "noop".into(), version: "0".into(), description: "".into(), interval_secs: 60, capabilities: vec![], privileged_commands: vec![] },
            Box::new(|| Box::new(NoopPlugin) as Box<dyn Plugin>) as PluginFactory,
        );

        let serve_path = sock_path.clone();
        std::thread::spawn(move || { let _ = serve(&serve_path, registry); });
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
