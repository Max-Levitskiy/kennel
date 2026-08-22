use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use kennel_proto::{ExtensionInfo, Request, Response};

pub struct Client {
    stream: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    pub fn connect(path: &Path) -> std::io::Result<Client> {
        let stream = UnixStream::connect(path)?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Client { stream, reader })
    }

    pub fn list(&mut self) -> Result<Vec<ExtensionInfo>, String> {
        match self.roundtrip(&Request::List)? {
            Response::Extensions(list) => Ok(list),
            Response::Error { message } => Err(message),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    pub fn set_enabled(&mut self, name: &str, enabled: bool) -> Result<(), String> {
        let req = if enabled { Request::Enable { name: name.into() } } else { Request::Disable { name: name.into() } };
        match self.roundtrip(&req)? {
            Response::Ok => Ok(()),
            Response::Error { message } => Err(message),
            other => Err(format!("unexpected response: {other:?}")),
        }
    }

    fn roundtrip(&mut self, req: &Request) -> Result<Response, String> {
        let mut line = serde_json::to_string(req).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stream.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        let mut resp_line = String::new();
        self.reader.read_line(&mut resp_line).map_err(|e| e.to_string())?;
        serde_json::from_str(&resp_line).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufReader, Write, BufRead};
    use std::os::unix::net::UnixListener;

    // A tiny stand-in server: good enough to prove the client's wire format
    // without depending on kennel-daemon (would be a circular dev-dependency).
    //
    // Deviation from the brief: the brief's version read exactly one line
    // per accepted connection, then moved on to `listener.incoming()`'s next
    // connection -- but `Client` opens a single persistent connection and
    // sends both requests (list, then set_enabled) down it, matching how the
    // real kenneld handles connections (see
    // crates/kennel-daemon/src/socket.rs::handle_connection, which loops
    // over `reader.lines()` for the lifetime of one connection). The
    // brief's version deterministically broken-pipes on the second
    // round-trip. Fixed here by looping reads within a connection until it
    // closes, matching the real server's protocol.
    fn spawn_echo_server(path: &std::path::Path) {
        let _ = std::fs::remove_file(path);
        let listener = UnixListener::bind(path).unwrap();
        let path = path.to_path_buf();
        std::thread::spawn(move || {
            let _ = path; // keep the socket path alive for the listener's lifetime
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut writer = stream;
                loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => break, // connection closed
                        Ok(_) => {}
                    }
                    let req: Request = serde_json::from_str(&line).unwrap();
                    let resp = match req {
                        Request::List => Response::Extensions(vec![]),
                        Request::Enable { .. } | Request::Disable { .. } => Response::Ok,
                    };
                    let mut out = serde_json::to_string(&resp).unwrap();
                    out.push('\n');
                    if writer.write_all(out.as_bytes()).is_err() { break; }
                }
            }
        });
        std::thread::sleep(std::time::Duration::from_millis(200));
    }

    #[test]
    fn list_and_set_enabled_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("test.sock");
        spawn_echo_server(&sock);

        let mut client = Client::connect(&sock).unwrap();
        assert_eq!(client.list().unwrap().len(), 0);
        client.set_enabled("sd-keepalive", true).unwrap();
    }
}
