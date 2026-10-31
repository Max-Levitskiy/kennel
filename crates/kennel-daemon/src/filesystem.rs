use kennel_proto::{FsKind, FsSubscription};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, mpsc::{sync_channel, SyncSender, TrySendError}};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::ext_config;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileEvent {
    pub subscription: String,
    pub path: String,
    pub change: String,
}

#[derive(Debug, Clone)]
pub struct CopyError {
    pub source: String,
    pub detail: String,
}

#[derive(Clone)]
pub struct CopyService {
    sender: SyncSender<CopyTask>,
    errors: Arc<Mutex<Vec<CopyError>>>,
}

struct CopyTask {
    source: PathBuf,
    destination: PathBuf,
    allowed_root: PathBuf,
}

impl CopyService {
    pub fn new() -> Self {
        let (tx, rx) = sync_channel::<CopyTask>(128);
        let errors: Arc<Mutex<Vec<CopyError>>> = Arc::new(Mutex::new(Vec::new()));
        let worker_errors = errors.clone();
        std::thread::spawn(move || {
            while let Ok(task) = rx.recv() {
                if let Err(error) = copy_stable(&task.source, &task.destination, &task.allowed_root) {
                    worker_errors.lock().unwrap().push(CopyError {
                        source: task.source.to_string_lossy().into_owned(),
                        detail: error,
                    });
                }
            }
        });
        Self { sender: tx, errors }
    }

    pub fn enqueue(&self, source: PathBuf, destination: PathBuf, allowed_root: PathBuf) -> Result<(), String> {
        match self.sender.try_send(CopyTask { source, destination, allowed_root }) {
            Ok(()) => Ok(()),
            Err(TrySendError::Full(_)) => Err("copy queue full; retry on next scan".into()),
            Err(TrySendError::Disconnected(_)) => Err("copy worker unavailable".into()),
        }
    }

    pub fn take_errors(&self) -> Vec<CopyError> {
        std::mem::take(&mut *self.errors.lock().unwrap())
    }
}

// Pure, small glob subset: "*.suffix", "prefix*", or exact basename.
// Invalid patterns are rejected at manifest load, never silently broadened.
pub fn valid_pattern(pattern: &str) -> bool {
    !pattern.is_empty()
        && !pattern.contains('/')
        && !pattern.contains('\\')
        && !pattern.contains("..")
        && pattern.matches('*').count() <= 1
        && !pattern.contains('?')
        && !pattern.contains('[')
}

pub fn matches(pattern: &str, name: &str) -> bool {
    if let Some((prefix, suffix)) = pattern.split_once('*') {
        name.len() >= prefix.len() + suffix.len()
            && name.starts_with(prefix) && name.ends_with(suffix)
    } else {
        pattern == name
    }
}

fn stable_stamp(path: &Path) -> Result<String, String> {
    let meta = fs::symlink_metadata(path).map_err(|e| e.to_string())?;
    if !meta.is_file() || meta.file_type().is_symlink() { return Err("not a regular file".into()); }
    let modified = meta.modified().map_err(|e| e.to_string())?
        .duration_since(UNIX_EPOCH).map_err(|e| e.to_string())?;
    Ok(format!("{}:{}", meta.len(), modified.as_nanos()))
}

fn gather(root: &Path, pattern: &str, kind: &FsKind, recursive: bool, out: &mut Vec<PathBuf>) -> Result<(), String> {
    if !root.exists() { return Err(format!("watch folder is absent: {}", root.display())); }
    for entry in fs::read_dir(root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let typ = entry.file_type().map_err(|e| e.to_string())?;
        if typ.is_symlink() { continue; }
        let path = entry.path();
        if typ.is_dir() && recursive {
            gather(&path, pattern, kind, recursive, out)?;
        }
        let selected = match kind { FsKind::File => typ.is_file(), FsKind::Directory => typ.is_dir() };
        if selected && matches(pattern, &entry.file_name().to_string_lossy()) {
            out.push(path);
        }
    }
    Ok(())
}

fn snapshot_path(data_dir: &Path, subscription_id: &str) -> PathBuf {
    data_dir.join(format!("fs-snapshot-{subscription_id}.json"))
}

fn read_snapshot(path: &Path) -> HashMap<String, String> {
    fs::read_to_string(path).ok()
        .and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default()
}

fn write_snapshot(path: &Path, map: &HashMap<String, String>) -> Result<(), String> {
    let bytes = serde_json::to_vec(map).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("tmp");
    fs::write(&temporary, bytes).map_err(|e| e.to_string())?;
    fs::rename(temporary, path).map_err(|e| e.to_string())
}

// The same filtered snapshot-based reconciler can later be triggered from
// FSEvents. Polling ensures no events are lost across sleep/restart.
pub fn reconcile<F>(sub: &FsSubscription, data_dir: &Path, mut deliver: F) -> Result<(), String>
where F: FnMut(FileEvent) -> Result<(), String> {
    let root = ext_config::expand(&sub.path)?;
    let mut files = Vec::new();
    gather(&root, &sub.pattern, &sub.kind, sub.recursive, &mut files)?;
    let state = snapshot_path(data_dir, &sub.id);
    let mut previous = read_snapshot(&state);
    for path in files {
        let stamp = if sub.kind == FsKind::File {
            let meta = fs::metadata(&path).map_err(|e| e.to_string())?;
            if meta.len() == 0 { continue; }
            // A newly downloaded iCloud file may still be written. Also do
            // not process files with future timestamps caused by clock skew.
            if SystemTime::now().duration_since(meta.modified().map_err(|e| e.to_string())?)
                .unwrap_or_default().as_secs() < 60 { continue; }
            stable_stamp(&path)?
        } else {
            "dir".to_string()
        };
        let key = path.to_string_lossy().into_owned();
        if previous.get(&key) == Some(&stamp) { continue; }
        deliver(FileEvent {
            subscription: sub.id.clone(),
            path: key.clone(),
            change: if previous.contains_key(&key) { "modified" } else { "created" }.into(),
        })?;
        previous.insert(key, stamp);
        // Commit per file, so a crash doesn't repeat every successful event.
        write_snapshot(&state, &previous)?;
    }
    Ok(())
}

pub fn invalidate(subs: &[FsSubscription], data_dir: &Path, source: &str) {
    for sub in subs {
        let path = snapshot_path(data_dir, &sub.id);
        let mut previous = read_snapshot(&path);
        if previous.remove(source).is_some() {
            let _ = write_snapshot(&path, &previous);
        }
    }
}

pub fn approved_source(source: &Path, subs: &[FsSubscription]) -> Result<PathBuf, String> {
    if !source.is_absolute() || source.is_symlink() { return Err("source must be a regular absolute file".into()); }
    let file = source.canonicalize().map_err(|e| e.to_string())?;
    if !file.is_file() { return Err("source is not a file".into()); }
    let name = file.file_name().ok_or("source has no basename")?.to_string_lossy();
    let allowed = subs.iter().filter(|s| s.kind == FsKind::File).any(|sub| {
        let Ok(root) = ext_config::expand(&sub.path).and_then(|p| p.canonicalize().map_err(|e| e.to_string())) else {
            return false;
        };
        let Ok(relative) = file.strip_prefix(root) else { return false };
        (sub.recursive || relative.components().count() == 1) && matches(&sub.pattern, &name)
    });
    if !allowed { return Err("source is outside declared filesystem subscriptions".into()); }
    Ok(file)
}

fn copy_stable(source: &Path, destination_dir: &Path, allowed_root: &Path) -> Result<(), String> {
    fs::create_dir_all(destination_dir).map_err(|e| e.to_string())?;
    let canonical = destination_dir.canonicalize().map_err(|e| e.to_string())?;
    if !canonical.starts_with(allowed_root.canonicalize().map_err(|e| e.to_string())?) {
        return Err("destination resolved outside selected directory".into());
    }
    let before = stable_stamp(source)?;
    let source_name = source.file_name().ok_or("missing source name")?.to_string_lossy();
    let suffix = before.split(':').nth(1).ok_or("missing timestamp")?;
    let (stem, ext) = source_name.rsplit_once('.').ok_or("missing extension")?;
    let filename = format!("{}--{}.{}", stem, suffix, ext);
    let output = destination_dir.join(filename);
    if output.is_file() { return Ok(()); }
    let input = OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW)
        .open(source).map_err(|e| e.to_string())?;
    let tmp = destination_dir.join(format!(".kennel-copy-{}-{}.tmp",
        std::process::id(), SystemTime::now().duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?.as_nanos()));
    let result = (|| -> Result<(), String> {
        let mut output_file = OpenOptions::new().write(true).create_new(true)
            .custom_flags(libc::O_NOFOLLOW).open(&tmp).map_err(|e| e.to_string())?;
        let mut input = input;
        std::io::copy(&mut input, &mut output_file).map_err(|e| e.to_string())?;
        output_file.sync_all().map_err(|e| e.to_string())?;
        if stable_stamp(source)? != before {
            return Err("recording changed during copy".into());
        }
        if output.is_file() { return Ok(()); }
        fs::rename(&tmp, &output).map_err(|e| e.to_string())
    })();
    let _ = fs::remove_file(&tmp);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn patterns_are_narrow() {
        assert!(valid_pattern("*.m4a"));
        assert!(matches("*.m4a", "audio.m4a"));
        assert!(!matches("*.m4a", "audio.wav"));
        assert!(!valid_pattern("../../*"));
        assert!(!valid_pattern("*/*"));
    }

    #[test]
    fn reconciliation_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("watched");
        fs::create_dir(&watched).unwrap();
        let memo = watched.join("a.m4a");
        fs::write(&memo, b"recording").unwrap();
        // Uses an old timestamp because the watcher intentionally skips
        // recent writes. On macOS this test is exercised in CI.
        let sub = FsSubscription {
            id: "audio".into(), path: watched.to_string_lossy().into_owned(),
            pattern: "*.m4a".into(), kind: FsKind::File, recursive: true
        };
        let mut got = Vec::new();
        reconcile(&sub, dir.path(), |e| { got.push(e); Ok(()) }).unwrap();
        assert_eq!(got.len(), 0, "fresh files must not be processed");
    }

    #[test]
    fn copy_preserves_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("a.m4a");
        let destination = dir.path().join("out");
        fs::write(&source, b"sample").unwrap();
        copy_stable(&source, &destination, dir.path()).unwrap();
        assert_eq!(fs::read(source).unwrap(), b"sample");
        assert_eq!(fs::read_dir(destination).unwrap().count(), 1);
    }
}
