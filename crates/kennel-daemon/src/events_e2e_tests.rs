// End-to-end tests for events, subscriptions and runs, driven only through the
// control socket: a real kenneld (registry, scheduler, event service) on a temp
// dir, the `emits-events` wasm fixture, and `sh` commands that write what they
// received to files. Commands go through `/bin/sh -c`, never the developer's
// login shell.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use kennel_proto::{Request, Response, Run, RunStatus, Subscription};
use crate::events::{EventService, PlainShell};
use crate::registry::Registry;
use crate::scheduler_manager::SchedulerManager;

const FIXTURE_MANIFEST: &str = r#"name = "emits-events"
version = "0.1.0"
description = "test fixture"
interval_secs = 1
capabilities = ["emit"]

[[events]]
name = "ping"
description = "a test ping"
example_payload = '{"n": 0}'
"#;

// Lays out `root` the way ~/Library/Application Support/kennel looks, with the
// emits-events fixture installed but not enabled.
fn install_fixture(root: &Path) {
    let ext_dir = root.join("extensions/emits-events");
    std::fs::create_dir_all(&ext_dir).unwrap();
    std::fs::write(ext_dir.join("manifest.toml"), FIXTURE_MANIFEST).unwrap();
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/wasm32-unknown-unknown/release/emits_events.wasm"),
        ext_dir.join("monitor.wasm"),
    )
    .unwrap();
}

// Starts a daemon on `root` listening on `root/<socket_name>` and connects to it.
// Starting a second one on the same root with another socket name is how the
// tests model a daemon restart.
fn start_daemon(root: &Path, socket_name: &str) -> Client {
    let events = EventService::open(root, Arc::new(PlainShell));
    let registry = Registry::new();
    let extensions_dir = root.join("extensions");
    crate::extensions::scan_and_register(&extensions_dir, &registry, Some(events.sink()));
    let manager = Arc::new(SchedulerManager::new(registry, root.join(format!("{socket_name}.state.json"))));
    let sock_path = root.join(socket_name);
    let serve_path = sock_path.clone();
    std::thread::spawn(move || {
        let _ = crate::socket::serve(&serve_path, manager, extensions_dir, events);
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(conn) = UnixStream::connect(&sock_path) {
            let reader = BufReader::new(conn.try_clone().unwrap());
            return Client { conn, reader };
        }
        assert!(Instant::now() < deadline, "daemon never started listening");
        std::thread::sleep(Duration::from_millis(20));
    }
}

struct Client {
    conn: UnixStream,
    reader: BufReader<UnixStream>,
}

impl Client {
    fn call(&mut self, req: &Request) -> Response {
        self.call_raw(&serde_json::to_string(req).unwrap())
    }

    // Sends a raw line, the way `echo '…' | nc -U` would.
    fn call_raw(&mut self, json: &str) -> Response {
        self.conn.write_all(format!("{json}\n").as_bytes()).unwrap();
        let mut out = String::new();
        self.reader.read_line(&mut out).unwrap();
        serde_json::from_str(&out).unwrap()
    }

    fn put(&mut self, command: &str, timeout_secs: u64) -> Subscription {
        let sub = Subscription { id: String::new(), extension: "emits-events".into(), event: "ping".into(), command: command.into(), timeout_secs, enabled: true };
        match self.call(&Request::PutSubscription { subscription: sub }) {
            Response::Subscription(saved) => saved,
            other => panic!("expected Subscription, got {other:?}"),
        }
    }

    fn subscriptions(&mut self) -> Vec<Subscription> {
        match self.call(&Request::ListSubscriptions) {
            Response::Subscriptions(subs) => subs,
            other => panic!("expected Subscriptions, got {other:?}"),
        }
    }

    fn enable(&mut self) {
        assert!(matches!(self.call(&Request::Enable { name: "emits-events".into() }), Response::Ok));
    }

    fn disable(&mut self) {
        assert!(matches!(self.call(&Request::Disable { name: "emits-events".into() }), Response::Ok));
    }

    fn runs(&mut self) -> Vec<Run> {
        match self.call(&Request::ListRuns) {
            Response::Runs(runs) => runs,
            other => panic!("expected Runs, got {other:?}"),
        }
    }

    // Polls ListRuns until `n` runs have finished.
    fn wait_for_finished_runs(&mut self, n: usize, within: Duration) -> Vec<Run> {
        let deadline = Instant::now() + within;
        loop {
            let runs = self.runs();
            if runs.iter().filter(|r| r.status != RunStatus::Running).count() >= n {
                return runs;
            }
            assert!(Instant::now() < deadline, "expected {n} finished runs within {within:?}, have {runs:?}");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
}

fn read(path: PathBuf) -> String {
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn an_emitted_event_runs_the_subscribed_command_once_with_payload_on_stdin_and_env() {
    let dir = tempfile::tempdir().unwrap();
    install_fixture(dir.path());
    let out = dir.path().join("out");
    std::fs::create_dir_all(&out).unwrap();
    let mut client = start_daemon(dir.path(), "kennel.sock");

    let sub = client.put(&format!("cat > {o}/$KENNEL_EVENT_ID.json; env | grep '^KENNEL_' | LC_ALL=C sort > {o}/$KENNEL_EVENT_ID.env", o = out.display()), 600);
    assert!(!sub.id.is_empty(), "the daemon assigns an id when none is given");
    client.enable();

    let runs = client.wait_for_finished_runs(3, Duration::from_secs(10));
    assert!(runs.iter().all(|r| r.status == RunStatus::Ok && r.exit_code == Some(0) && r.subscription == sub.id), "{runs:?}");

    let stdin: serde_json::Value = serde_json::from_str(&read(out.join("evt-2.json"))).unwrap();
    assert_eq!(stdin, serde_json::json!({"n": 2, "title": "it's $(not) a command"}), "the payload arrives on stdin, verbatim");
    assert_eq!(read(out.join("evt-2.env")), "KENNEL_EVENT=ping\nKENNEL_EVENT_ID=evt-2\nKENNEL_EXTENSION=emits-events\n");

    // The fixture re-emits all three ids on every 1s tick; none may run twice.
    std::thread::sleep(Duration::from_millis(2500));
    let mut ids: Vec<String> = client.runs().into_iter().map(|r| r.event_id).collect();
    ids.sort();
    assert_eq!(ids, vec!["evt-1", "evt-2", "evt-3"]);

    // The undeclared `bogus` event the fixture also emits was rejected.
    match client.call_raw("\"ListEvents\"") {
        Response::Events(events) => {
            let mut seen: Vec<(String, String)> = events.into_iter().map(|e| (e.event, e.id)).collect();
            seen.sort();
            assert_eq!(seen, vec![("ping".into(), "evt-1".into()), ("ping".into(), "evt-2".into()), ("ping".into(), "evt-3".into())]);
        }
        other => panic!("expected Events, got {other:?}"),
    }
}

#[test]
fn an_event_id_seen_before_a_restart_never_runs_again() {
    let dir = tempfile::tempdir().unwrap();
    install_fixture(dir.path());
    let log = dir.path().join("runs.log");
    let mut first = start_daemon(dir.path(), "first.sock");
    first.put(&format!("echo $KENNEL_EVENT_ID >> {}", log.display()), 600);
    first.enable();
    first.wait_for_finished_runs(3, Duration::from_secs(10));
    first.disable();

    // A new daemon on the same support dir: same subscriptions, same event log.
    let mut second = start_daemon(dir.path(), "second.sock");
    assert_eq!(second.runs().len(), 3, "the run log survives the restart");
    second.enable();
    std::thread::sleep(Duration::from_millis(2500)); // two more ticks re-emitting evt-1..3
    second.disable();

    assert_eq!(second.runs().len(), 3, "no new runs after the restart");
    assert_eq!(read(log), "evt-1\nevt-2\nevt-3\n");
}

#[test]
fn runs_of_one_subscription_are_serial_in_event_order() {
    let dir = tempfile::tempdir().unwrap();
    install_fixture(dir.path());
    let log = dir.path().join("order.log");
    let mut client = start_daemon(dir.path(), "kennel.sock");
    client.put(&format!("echo begin $KENNEL_EVENT_ID >> {l}; sleep 0.3; echo end $KENNEL_EVENT_ID >> {l}", l = log.display()), 600);
    client.enable();
    client.wait_for_finished_runs(3, Duration::from_secs(10));

    assert_eq!(read(log), "begin evt-1\nend evt-1\nbegin evt-2\nend evt-2\nbegin evt-3\nend evt-3\n");
}

#[test]
fn different_subscriptions_run_in_parallel() {
    let dir = tempfile::tempdir().unwrap();
    install_fixture(dir.path());
    let marks = dir.path().join("marks");
    std::fs::create_dir_all(&marks).unwrap();
    let mut client = start_daemon(dir.path(), "kennel.sock");
    // Each command marks itself as started, then succeeds only if the other
    // subscription's command for the same event starts within 5s. Serial
    // execution across subscriptions would make the first one wait forever.
    let waits_for = |me: &str, other: &str| {
        format!(
            "touch {m}/{me}-$KENNEL_EVENT_ID; for i in $(seq 50); do [ -f {m}/{other}-$KENNEL_EVENT_ID ] && exit 0; sleep 0.1; done; exit 1",
            m = marks.display()
        )
    };
    client.put(&waits_for("a", "b"), 600);
    client.put(&waits_for("b", "a"), 600);
    client.enable();

    let runs = client.wait_for_finished_runs(6, Duration::from_secs(40));
    assert!(runs.iter().all(|r| r.status == RunStatus::Ok), "{runs:?}");
}

#[test]
fn a_command_over_its_timeout_has_its_process_group_killed() {
    let dir = tempfile::tempdir().unwrap();
    install_fixture(dir.path());
    let pids = dir.path().join("pids");
    std::fs::create_dir_all(&pids).unwrap();
    let mut client = start_daemon(dir.path(), "kennel.sock");
    // The background sleep is a child of the shell, not the shell itself:
    // killing only the shell's pid would leave it running.
    client.put(&format!("echo started; sleep 30 & echo $! > {p}/$KENNEL_EVENT_ID; wait", p = pids.display()), 1);
    client.enable();

    let runs = client.wait_for_finished_runs(3, Duration::from_secs(20));
    for run in &runs {
        assert_eq!(run.status, RunStatus::Timeout, "{run:?}");
        assert_eq!(run.exit_code, None);
        assert_eq!(run.output, "started\n");
        let pid = read(pids.join(&run.event_id)).trim().to_string();
        let alive = std::process::Command::new("/bin/kill").args(["-0", &pid]).stderr(std::process::Stdio::null()).status().unwrap().success();
        assert!(!alive, "the command's background child {pid} survived the timeout");
    }
}

#[test]
fn a_failing_command_records_its_exit_code_and_output_tail() {
    let dir = tempfile::tempdir().unwrap();
    install_fixture(dir.path());
    let mut client = start_daemon(dir.path(), "kennel.sock");
    client.put("echo to-stdout; echo to-stderr >&2; exit 3", 600);
    client.enable();

    let runs = client.wait_for_finished_runs(3, Duration::from_secs(10));
    for run in &runs {
        assert_eq!((run.status, run.exit_code), (RunStatus::Failed, Some(3)), "{run:?}");
        assert!(run.output.contains("to-stdout\n") && run.output.contains("to-stderr\n"), "{:?}", run.output);
        assert!(run.finished_at.is_some_and(|f| f >= run.started_at));
    }
}

#[test]
fn subscriptions_are_managed_over_the_socket_and_kept_in_subscriptions_toml() {
    let dir = tempfile::tempdir().unwrap();
    install_fixture(dir.path());
    let mut client = start_daemon(dir.path(), "kennel.sock");

    // Shaped the way a user would type it into nc: no id, default timeout.
    let saved = match client.call_raw(r#"{"PutSubscription":{"subscription":{"extension":"emits-events","event":"ping","command":"true"}}}"#) {
        Response::Subscription(sub) => sub,
        other => panic!("expected Subscription, got {other:?}"),
    };
    assert_eq!((saved.timeout_secs, saved.enabled), (600, true), "defaults: 10 minute timeout, enabled");

    let mut edited = saved.clone();
    edited.command = "echo edited".into();
    edited.enabled = false;
    assert!(matches!(client.call(&Request::PutSubscription { subscription: edited.clone() }), Response::Subscription(_)));
    assert_eq!(client.subscriptions(), vec![edited.clone()], "a put with an existing id replaces it");

    // Typos are refused rather than saved as a subscription that never fires.
    for (extension, event) in [("no-such-extension", "ping"), ("emits-events", "pong")] {
        let sub = Subscription { id: String::new(), extension: extension.into(), event: event.into(), ..edited.clone() };
        assert!(matches!(client.call(&Request::PutSubscription { subscription: sub }), Response::Error { .. }), "{extension}/{event}");
    }

    // A hand edit is picked up on Rescan.
    let path = dir.path().join("subscriptions.toml");
    let good = read(path.clone()).replace("echo edited", "echo by hand");
    std::fs::write(&path, &good).unwrap();
    assert!(matches!(client.call(&Request::Rescan), Response::Ok));
    assert_eq!(client.subscriptions()[0].command, "echo by hand");

    // A broken file is only logged; the loaded subscriptions stay.
    std::fs::write(&path, "this is [not toml").unwrap();
    assert!(matches!(client.call(&Request::Rescan), Response::Ok));
    assert_eq!(client.subscriptions().len(), 1);
    std::fs::write(&path, &good).unwrap();

    // What was saved is what a restarted daemon loads.
    let mut second = start_daemon(dir.path(), "second.sock");
    assert_eq!(second.subscriptions()[0].command, "echo by hand");

    assert!(matches!(second.call_raw(&format!(r#"{{"DeleteSubscription":{{"id":"{}"}}}}"#, saved.id)), Response::Ok));
    assert!(second.subscriptions().is_empty());
    assert!(!read(path).contains(&saved.id), "the delete reached subscriptions.toml");
    assert!(matches!(second.call(&Request::DeleteSubscription { id: saved.id }), Response::Error { .. }), "deleting twice is an error");
}
