# Typed filesystem subscriptions and extension configuration (preview)

Adds a filesystem event source for Kennel extensions. Existing check()/fix() and user-created event-to-shell subscriptions remain unchanged.

## SDK

A guest declares its watched paths, matching file patterns, and typed directory picker in Manifest. With kennel_fs_extension!, a Rust Handler<FsEvent<MyFileMarker>> exports on_event and on_error. The compiler validates the handler signature.

The compiled WASM manifest is cross-checked with manifest.toml at load time. Generation of manifest.toml by a proc macro is deferred; two-file store packaging remains.

## Filesystem events

On each scheduler tick, Kennel scans and reconciles the directory with a persistent snapshot. Matching files must be at least 60 seconds old; matching changes arrive as created or modified events. This is polling, **not yet a native FSEvents watcher**. Future FSEvents integration can trigger the same reconciler.

## Dynamic account choices

DirectoryOptions declares a root, prefix and optional required child directory. Kennel lists only names/types of immediate matching subdirectories, without granting general access to their contents. GUI renders a select control for each field. SetConfig accepts only listed values and revalidates on every copy. Config is persisted in the extension data directory.

## Copying

host_fs_copy(source, folder) queues a native copy outside WASM. Kennel validates the source against the declared subscriptions and destinations against the selected configuration account. Copying uses temporary files and versioned names, preserves originals, and retries after reported failures. on_error is optional. Queued does not imply cloud upload completed.

## Permissions and limitations

- fs_watch, fs_discover and fs_copy require enable-time approval.
- Full Disk Access may be needed for Apple's Voice Memos container.
- Symlinks and obvious path traversal are rejected. This preview is not yet a descriptor-anchored sandbox against adversarial concurrent directory renames.
- Native filesystem notifications are not implemented; polling reconciles after restarts.
- In-flight copies may finish after an extension is disabled.
- Only the first directory picker is used as a copy destination currently.
- Typed events from other extensions and automatic release manifest generation are follow-on work.
- Embedded Apple transcripts are preserved as part of the original M4A bytes; separately exporting them requires an additional decoder.

## Test

On macOS, install the WASM target, build the fixture crates and run cargo test --workspace.
