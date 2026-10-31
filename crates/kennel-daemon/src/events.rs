// Events, subscriptions and runs.
//
// An extension emits an event from check() (wasm_host's `emit` import, which
// lands in `EventSink::accept`). kenneld drops ids it has already accepted from
// that extension, records the event, and queues it on every enabled
// subscription for that extension and event type. Each subscription has one
// worker thread, so its runs are serial in event order while different
// subscriptions run in parallel.
//
// Everything lives in kennel's support directory:
//   events.jsonl        accepted events, one per line (also the seen-id set)
//   runs.jsonl          run records; a run is re-appended on every change and
//                       the last line for an id wins
//   subscriptions.toml  the user's subscriptions; kenneld owns it, hand edits
//                       are picked up on Rescan

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};
use kennel_proto::{EventRecord, Run, RunStatus, Subscription};
use serde::{Deserialize, Serialize};

// How much of a run's combined stdout/stderr is kept, from the end.
const OUTPUT_TAIL_BYTES: usize = 64 * 1024;

// How long to keep collecting output after the command itself has exited.
// Bounded because a command can leave a background child holding the pipes.
const OUTPUT_DRAIN_GRACE: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmitOutcome {
    Accepted,
    Duplicate,
    // Couldn't be written to events.jsonl. Not marked seen and nothing runs,
    // so the extension's next emit of the same id gets another chance.
    NotRecorded,
}

// Where the wasm host hands events that passed its checks (capability,
// declared type, payload shape).
pub trait EventSink: Send + Sync {
    fn accept(&self, extension: &str, event: &str, id: &str, payload: serde_json::Value) -> EmitOutcome;
}

// Builds the process a subscription's command runs in. Injected so tests run
// commands through `/bin/sh -c` instead of the developer's login shell.
pub trait ShellLauncher: Send + Sync {
    fn command(&self, script: &str) -> Command;
}

// Production: the user's login shell, so tools on their normal PATH (Homebrew,
// uv, gh) are found. The LaunchAgent's own PATH is only /usr/bin:/bin:….
pub struct LoginShell;

impl ShellLauncher for LoginShell {
    fn command(&self, script: &str) -> Command {
        let shell = std::env::var("SHELL").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/bin/zsh".into());
        let mut cmd = Command::new(shell);
        cmd.arg("-lc").arg(script);
        if let Ok(home) = std::env::var("HOME") {
            cmd.current_dir(home);
        }
        cmd
    }
}

#[cfg(test)]
pub struct PlainShell;

#[cfg(test)]
impl ShellLauncher for PlainShell {
    fn command(&self, script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c").arg(script);
        cmd
    }
}

#[derive(Default, Serialize, Deserialize)]
struct SubscriptionsFile {
    #[serde(default, rename = "subscription")]
    subscriptions: Vec<Subscription>,
}

#[derive(Clone)]
pub struct EventService {
    shared: Arc<Shared>,
}

struct Shared {
    events_path: PathBuf,
    runs_path: PathBuf,
    subscriptions_path: PathBuf,
    launcher: Arc<dyn ShellLauncher>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    seen: HashMap<String, HashSet<String>>, // extension -> accepted event ids
    events: Vec<EventRecord>,
    runs: Vec<Run>, // ascending id
    next_run_id: u64,
    subscriptions: Vec<Subscription>,
    // subscription id -> its queue. Never removed: a deleted subscription's
    // worker just finds nothing to run, and one re-added under the same id
    // reuses it, so its runs can't overlap a run still in progress.
    workers: HashMap<String, Sender<EventRecord>>,
}

impl EventService {
    pub fn open(support_dir: &Path, launcher: Arc<dyn ShellLauncher>) -> EventService {
        let shared = Shared {
            events_path: support_dir.join("events.jsonl"),
            runs_path: support_dir.join("runs.jsonl"),
            subscriptions_path: support_dir.join("subscriptions.toml"),
            launcher,
            state: Mutex::new(State::default()),
        };
        {
            let mut state = shared.state.lock().unwrap();
            for event in read_jsonl::<EventRecord>(&shared.events_path) {
                state.seen.entry(event.extension.clone()).or_default().insert(event.id.clone());
                state.events.push(event);
            }
            let mut runs: HashMap<u64, Run> = HashMap::new();
            for run in read_jsonl::<Run>(&shared.runs_path) {
                runs.insert(run.id, run);
            }
            state.runs = runs.into_values().collect();
            state.runs.sort_by_key(|r| r.id);
            state.next_run_id = state.runs.last().map_or(1, |r| r.id + 1);
            match load_subscriptions(&shared.subscriptions_path) {
                Ok(subs) => state.subscriptions = subs,
                Err(e) => eprintln!("kenneld: {e}; starting with no subscriptions"),
            }
        }
        EventService { shared: Arc::new(shared) }
    }

    pub fn sink(&self) -> Arc<dyn EventSink> {
        Arc::new(self.clone())
    }

    pub fn list_subscriptions(&self) -> Vec<Subscription> {
        self.shared.state.lock().unwrap().subscriptions.clone()
    }

    // Creates or replaces a subscription. The file is re-read first so a hand
    // edit made since the last Rescan isn't overwritten.
    pub fn put_subscription(&self, mut sub: Subscription) -> Result<Subscription, String> {
        if sub.command.trim().is_empty() {
            return Err("command must not be empty".into());
        }
        if sub.timeout_secs == 0 {
            return Err("timeout_secs must be at least 1".into());
        }
        let mut state = self.shared.state.lock().unwrap();
        let mut subs = load_subscriptions(&self.shared.subscriptions_path)?;
        if sub.id.is_empty() {
            sub.id = new_subscription_id(&subs);
        }
        match subs.iter_mut().find(|s| s.id == sub.id) {
            Some(existing) => *existing = sub.clone(),
            None => subs.push(sub.clone()),
        }
        save_subscriptions(&self.shared.subscriptions_path, &subs)?;
        state.subscriptions = subs;
        Ok(sub)
    }

    pub fn delete_subscription(&self, id: &str) -> Result<(), String> {
        let mut state = self.shared.state.lock().unwrap();
        let mut subs = load_subscriptions(&self.shared.subscriptions_path)?;
        let before = subs.len();
        subs.retain(|s| s.id != id);
        if subs.len() == before {
            return Err(format!("unknown subscription: {id}"));
        }
        save_subscriptions(&self.shared.subscriptions_path, &subs)?;
        state.subscriptions = subs;
        Ok(())
    }

    // Re-reads subscriptions.toml (Rescan). A file that doesn't parse leaves
    // the current subscriptions in place.
    pub fn reload_subscriptions(&self) -> Result<(), String> {
        // Read under the lock, like put/delete, so a racing Put can't be
        // overwritten by an older read of the file.
        let mut state = self.shared.state.lock().unwrap();
        state.subscriptions = load_subscriptions(&self.shared.subscriptions_path)?;
        Ok(())
    }

    pub fn list_events(&self) -> Vec<EventRecord> {
        self.shared.state.lock().unwrap().events.clone()
    }

    // Newest first.
    pub fn list_runs(&self) -> Vec<Run> {
        let mut runs = self.shared.state.lock().unwrap().runs.clone();
        runs.reverse();
        runs
    }
}

impl EventSink for EventService {
    fn accept(&self, extension: &str, event: &str, id: &str, payload: serde_json::Value) -> EmitOutcome {
        let mut state = self.shared.state.lock().unwrap();
        if state.seen.get(extension).is_some_and(|ids| ids.contains(id)) {
            return EmitOutcome::Duplicate;
        }
        let record = EventRecord { extension: extension.into(), event: event.into(), id: id.into(), payload, received_at: unix_now() };
        // An event that isn't on disk would run again after a restart, so it
        // isn't accepted at all.
        if let Err(e) = append_jsonl(&self.shared.events_path, &record) {
            eprintln!("[{extension}] could not record event {event}/{id}, dropped it: {e}");
            return EmitOutcome::NotRecorded;
        }
        state.seen.entry(extension.into()).or_default().insert(id.into());
        state.events.push(record.clone());
        println!("[{extension}] accepted event {event}/{id}");

        let matching: Vec<String> = state
            .subscriptions
            .iter()
            .filter(|s| s.enabled && s.extension == extension && s.event == event)
            .map(|s| s.id.clone())
            .collect();
        for sub_id in matching {
            let shared = self.shared.clone();
            let queue = state.workers.entry(sub_id.clone()).or_insert_with(|| spawn_worker(shared, sub_id.clone()));
            let _ = queue.send(record.clone());
        }
        EmitOutcome::Accepted
    }
}

fn spawn_worker(shared: Arc<Shared>, sub_id: String) -> Sender<EventRecord> {
    let (tx, rx) = mpsc::channel::<EventRecord>();
    std::thread::Builder::new()
        .name(format!("kennel-sub-{sub_id}"))
        .spawn(move || {
            for event in rx {
                shared.run(&sub_id, &event);
            }
        })
        .expect("spawn subscription worker");
    tx
}

impl Shared {
    fn run(&self, sub_id: &str, event: &EventRecord) {
        // Looked up now, not at queue time, so an edit or disable made while
        // the event waited in the queue is respected.
        let (sub, mut run) = {
            let mut state = self.state.lock().unwrap();
            let Some(sub) = state.subscriptions.iter().find(|s| s.id == sub_id && s.enabled).cloned() else { return };
            let run = Run {
                id: state.next_run_id,
                subscription: sub.id.clone(),
                extension: event.extension.clone(),
                event: event.event.clone(),
                event_id: event.id.clone(),
                started_at: unix_now(),
                finished_at: None,
                status: RunStatus::Running,
                exit_code: None,
                output: String::new(),
            };
            state.next_run_id += 1;
            self.record_run(&mut state, run.clone());
            (sub, run)
        };

        let mut cmd = self.launcher.command(&sub.command);
        cmd.env("KENNEL_EXTENSION", &event.extension).env("KENNEL_EVENT", &event.event).env("KENNEL_EVENT_ID", &event.id);
        let stdin = serde_json::to_vec(&event.payload).unwrap_or_default();
        let outcome = execute(cmd, stdin, Duration::from_secs(sub.timeout_secs));

        run.finished_at = Some(unix_now());
        run.status = outcome.status;
        run.exit_code = outcome.exit_code;
        run.output = outcome.output;
        println!("[{}] run {} of subscription {} for {}/{}: {:?}", run.extension, run.id, run.subscription, run.event, run.event_id, run.status);
        let mut state = self.state.lock().unwrap();
        self.record_run(&mut state, run);
    }

    fn record_run(&self, state: &mut State, run: Run) {
        if let Err(e) = append_jsonl(&self.runs_path, &run) {
            eprintln!("kenneld: could not record run {}: {e}", run.id);
        }
        match state.runs.iter_mut().find(|r| r.id == run.id) {
            Some(existing) => *existing = run,
            None => state.runs.push(run),
        }
    }
}

struct Outcome {
    status: RunStatus,
    exit_code: Option<i32>,
    output: String,
}

// Runs `cmd` in its own process group with `stdin` piped in, keeping the tail
// of its combined output. Past `timeout` the whole group is killed, so a
// command's children (`sleep 600 &`, a pipeline) die with it.
fn execute(mut cmd: Command, stdin: Vec<u8>, timeout: Duration) -> Outcome {
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).process_group(0);
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return Outcome { status: RunStatus::Failed, exit_code: None, output: format!("could not start the command: {e}") },
    };
    let pgid = child.id() as i32;

    // A command that never reads stdin must not block us on a full pipe.
    if let Some(mut pipe) = child.stdin.take() {
        std::thread::spawn(move || {
            let _ = pipe.write_all(&stdin);
        });
    }

    let tail = Arc::new(Mutex::new(Vec::new()));
    let (done_tx, done_rx) = mpsc::channel();
    let readers: Vec<Box<dyn Read + Send>> = [
        child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
        child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .collect();
    let reader_count = readers.len();
    for mut pipe in readers {
        let tail = tail.clone();
        let done = done_tx.clone();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = pipe.read(&mut buf) {
                if n == 0 {
                    break;
                }
                let mut tail = tail.lock().unwrap();
                tail.extend_from_slice(&buf[..n]);
                if tail.len() > 2 * OUTPUT_TAIL_BYTES {
                    let excess = tail.len() - OUTPUT_TAIL_BYTES;
                    tail.drain(..excess);
                }
            }
            let _ = done.send(());
        });
    }

    // Ok(None) means it timed out.
    let deadline = Instant::now() + timeout;
    let exit: std::io::Result<Option<std::process::ExitStatus>> = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(Some(status)),
            Ok(None) if Instant::now() >= deadline => {
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
                let _ = child.wait();
                break Ok(None);
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(e) => {
                unsafe { libc::killpg(pgid, libc::SIGKILL) };
                let _ = child.wait();
                break Err(e);
            }
        }
    };

    let grace = Instant::now() + OUTPUT_DRAIN_GRACE;
    for _ in 0..reader_count {
        if done_rx.recv_timeout(grace.saturating_duration_since(Instant::now())).is_err() {
            break;
        }
    }
    let output = {
        let tail = tail.lock().unwrap();
        let start = tail.len().saturating_sub(OUTPUT_TAIL_BYTES);
        String::from_utf8_lossy(&tail[start..]).into_owned()
    };

    match exit {
        Ok(None) => Outcome { status: RunStatus::Timeout, exit_code: None, output },
        Ok(Some(status)) if status.success() => Outcome { status: RunStatus::Ok, exit_code: Some(0), output },
        Ok(Some(status)) => Outcome { status: RunStatus::Failed, exit_code: status.code(), output },
        Err(e) => Outcome { status: RunStatus::Failed, exit_code: None, output: format!("{output}\n[kenneld: lost track of the command, killed it: {e}]") },
    }
}

fn load_subscriptions(path: &Path) -> Result<Vec<Subscription>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => toml::from_str::<SubscriptionsFile>(&text)
            .map(|f| f.subscriptions)
            .map_err(|e| format!("{} is not valid: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(format!("could not read {}: {e}", path.display())),
    }
}

// Written to a temp file and renamed over the old one, so a crash mid-write
// can't leave a half-written file behind.
fn save_subscriptions(path: &Path, subs: &[Subscription]) -> Result<(), String> {
    let text = toml::to_string_pretty(&SubscriptionsFile { subscriptions: subs.to_vec() }).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, text).and_then(|_| std::fs::rename(&tmp, path)).map_err(|e| format!("could not write {}: {e}", path.display()))
}

fn new_subscription_id(existing: &[Subscription]) -> String {
    let mut n = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    loop {
        let id = format!("sub-{n:x}");
        if !existing.iter().any(|s| s.id == id) {
            return id;
        }
        n += 1;
    }
}

fn read_jsonl<T: serde::de::DeserializeOwned>(path: &Path) -> Vec<T> {
    let Ok(text) = std::fs::read_to_string(path) else { return Vec::new() };
    text.lines().filter(|l| !l.trim().is_empty()).filter_map(|l| serde_json::from_str(l).ok()).collect()
}

fn append_jsonl<T: Serialize>(path: &Path, value: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(value).map_err(std::io::Error::other)?;
    line.push(b'\n');
    let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    file.write_all(&line)
}

fn unix_now() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}
