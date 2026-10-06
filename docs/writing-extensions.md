# Writing a kennel extension

An extension watches one thing and knows how to repair it. It is a Rust crate compiled to WebAssembly (`wasm32-unknown-unknown`) against [`kennel-guest-sdk`](../crates/kennel-guest-sdk/src/lib.rs). It ships as two files:

- `monitor.wasm` is the code. It exports `check()` ("is the thing healthy?") and `fix()` ("repair it").
- `manifest.toml` is the permission grant. It names the host capabilities the code may use, such as running commands, writing files or keeping state.

The wasm runs sandboxed. It can touch the outside world only through host functions, and only the ones its manifest grants. A panic, an infinite loop or a hung command fails that one call and never takes the daemon down.

This guide builds a complete extension, then covers the runtime rules, the host API reference and publishing.

## Tutorial: a low disk space warning

`disk-space-watchdog` checks free space on the startup disk every 5 minutes and sends a notification when it drops below 10 GiB.

### 1. Create the crate

```bash
rustup target add wasm32-unknown-unknown
cargo new --lib disk-space-watchdog
cd disk-space-watchdog
```

`Cargo.toml`:

```toml
[package]
name = "disk-space-watchdog"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
kennel-guest-sdk = { git = "https://github.com/Max-Levitskiy/kennel", branch = "main" }
serde_json = "1"
```

`crate-type = ["cdylib"]` makes cargo produce a `.wasm` module. `serde_json` is required because the SDK's `kennel_extension!` macro uses it in your crate.

### 2. Write the code

`src/lib.rs`:

```rust
use kennel_guest_sdk::{host_log, host_notify, host_spawn, kennel_extension, Manifest, Status};

const MIN_FREE_GIB: u64 = 10;

fn my_manifest() -> Manifest {
    Manifest {
        name: "disk-space-watchdog",
        version: "0.1.0",
        description: "Warns when the startup disk has less than 10 GiB free",
        interval_secs: 300,
        capabilities: vec!["spawn", "notify", "log"],
        privileged_commands: vec![],
    }
}

// `df -k /` prints a header line, then one line whose 4th column is the
// free space in KiB.
fn free_gib(df_stdout: &str) -> Option<u64> {
    let line = df_stdout.lines().nth(1)?;
    let kib: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(kib / 1024 / 1024)
}

fn my_check() -> Status {
    let df = host_spawn("/bin/df", &["-k", "/"]);
    if df.exit_code != 0 {
        return Status::Unhealthy(format!("df failed: {}", df.stderr.trim()));
    }
    match free_gib(&df.stdout) {
        Some(gib) if gib >= MIN_FREE_GIB => Status::Healthy,
        Some(gib) => Status::Unhealthy(format!("only {gib} GiB free on /")),
        None => Status::Unhealthy("could not parse df output".into()),
    }
}

fn my_fix() {
    host_log("warn", "startup disk is low on space");
    host_notify("Disk almost full", "Less than 10 GiB free on the startup disk.");
}

kennel_extension!(my_manifest, my_check, my_fix);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_free_space_from_df() {
        let out = "Filesystem 1024-blocks Used Available Capacity Mounted on\n\
                   /dev/disk3s1s1 971350180 10566728 52428800 17% /\n";
        assert_eq!(free_gib(out), Some(50));
    }
}
```

`kennel_extension!(manifest_fn, check_fn, fix_fn)` generates the exports the daemon calls. It defines functions named `manifest`, `check` and `fix` itself, so your own functions must be named something else (`my_check` and so on). Otherwise the build fails with `E0428: the name is defined multiple times`.

Here `fix()` can't free disk space. It tells you, and that is a fine fix. kennel's [fix cooldown](#how-kennel-runs-it) stops it from notifying on every tick.

### 3. Write the manifest

`manifest.toml`, next to `Cargo.toml`:

```toml
name = "disk-space-watchdog"
version = "0.1.0"
description = "Warns when the startup disk has less than 10 GiB free"
interval_secs = 300
capabilities = ["spawn", "notify", "log"]
```

The capability list has to match `my_manifest()` exactly. The daemon compares the two when it loads the extension and refuses to load it if they differ (see [Manifest](#manifest)).

### 4. Test and build

```bash
cargo test                                                # native unit tests
cargo build --release --target wasm32-unknown-unknown
```

The module is at `target/wasm32-unknown-unknown/release/disk_space_watchdog.wasm`. Cargo turns the dashes in the crate name into underscores.

Host functions exist only inside `kenneld`, so tests can't call `my_check()` or anything else that reaches `host_*`. Put the logic in plain functions like `free_gib` and test those.

### 5. Install it locally and enable it

```bash
EXT="$HOME/Library/Application Support/kennel/extensions/disk-space-watchdog"
SOCK="$HOME/Library/Application Support/kennel/control.sock"
mkdir -p "$EXT"
cp target/wasm32-unknown-unknown/release/disk_space_watchdog.wasm "$EXT/monitor.wasm"
cp manifest.toml "$EXT/manifest.toml"
echo '"Rescan"' | nc -U -w 2 "$SOCK"
```

The extension now shows up in the menu bar app's **Installed** tab. Tick it there, or enable it from the shell:

```bash
echo '{"Enable":{"name":"disk-space-watchdog"}}' | nc -U -w 2 "$SOCK"
echo '"List"' | nc -U -w 2 "$SOCK"   # "last_status":"Healthy"
```

`"Rescan"` loads only extensions the daemon hasn't seen yet. After you rebuild one it already loaded, copy the files again and restart the daemon with `launchctl kickstart -k gui/$(id -u)/com.max.kenneld`.

### 6. Watch it

`host_log` output goes to `~/Library/Logs/kennel/kenneld.out.log`, prefixed with the extension name:

```bash
tail -f ~/Library/Logs/kennel/kenneld.out.log
```

To see the unhealthy path, raise `MIN_FREE_GIB` above your free space, rebuild, copy the wasm over and restart the daemon.

## How kennel runs it

- **Load.** At startup and on `Rescan`, the daemon goes through `extensions/<name>/`. A directory is loaded only if it holds both `manifest.toml` and `monitor.wasm`, the manifest parses, `interval_secs >= 1`, and the wasm's `manifest()` declares the same capabilities as `manifest.toml`. Anything else is skipped, and the reason is written to `kenneld.err.log`. A newly loaded extension stays disabled until someone enables it.
- **Tick.** Every `interval_secs` the daemon calls `check()`. The result is published as the extension's status, which drives the GUI and `List`.
- **Fix.** `fix()` runs only after `check()` returned `Unhealthy`, and then at most once per cooldown: 5 × `interval_secs`, but never less than 60 s. A 2 s extension can fix at most once a minute, and a 5 min one at most every 25 min. The cooldown resets when the daemon restarts or the extension is re-enabled.
- **Fresh instance per call.** Every `check()` and `fix()` runs in a brand new wasm instance, so `static`s, globals and anything else in memory are gone by the next call. Use `host_state_get` / `host_state_set` to carry data between calls (gdrive-watchdog counts consecutive stalls this way).
- **Time limits.**
  - Each host command (`spawn`, `privileged_spawn`, `launchctl`, `notify`) is killed after **8 s**, and the guest sees `exit_code: -1`.
  - Each `check()` / `fix()` call has **13 s** of wall-clock time in total, host calls included. Past that it is stopped, and `check()` reports `Errored`.
- **Errors.** If `check()` panics, times out or returns output that isn't valid, the status is `Errored`, which the GUI counts as unhealthy. `fix()` is not called for `Errored`, only for `Unhealthy`.

## Manifest

| Field | Type | Meaning |
|---|---|---|
| `name` | string | Identity: what the daemon registers it as. Keep it equal to the directory name (the GUI's installer requires it to match the store entry). Use `[a-z0-9-]`. |
| `version` | string | Shown in the GUI. |
| `description` | string | Shown in the GUI and in the **Allow …?** dialog. |
| `interval_secs` | integer ≥ 1 | Seconds between `check()` calls. |
| `capabilities` | array of strings | Host functions this extension may call. Defaults to `[]`. |
| `privileged_commands` | array of strings | Absolute paths `privileged_spawn` may run as root. Defaults to `[]`. |

The manifest lives in two places: `manifest.toml` and the `Manifest` returned by your `my_manifest()`.

- **`manifest.toml` decides what the extension gets.** It is the file the daemon grants capabilities from and the GUI shows the user.
- **The wasm's copy is checked against it.** At load time the daemon checks that the two capability lists are the same set and refuses to load the extension if they're not. A capability you forgot in `manifest.toml` fails the load. It is never silently denied at runtime.
- **Name, version and description are not cross-checked by the daemon.** The kennel-extensions release workflow does check that the tag, `Cargo.toml` and `manifest.toml` versions agree.

## Host API

All of these are in `kennel_guest_sdk`. A function whose capability isn't granted does nothing and returns its empty or failure value; it doesn't trap.

| Function | Capability | What it does |
|---|---|---|
| `host_spawn(cmd, args) -> SpawnResult` | `spawn` | Runs `cmd` directly with `args`, with no shell. Returns `{ exit_code, stdout, stderr }`. Use `"/bin/sh", &["-c", "..."]` when you need `$HOME`, globs or pipes. |
| `host_privileged_spawn(cmd, args) -> SpawnResult` | `privileged_spawn` | Runs `sudo -n cmd args`. `cmd` must be listed in `privileged_commands`, and the user must have installed a `NOPASSWD` sudoers rule for it; the GUI shows the rule. Without the rule, sudo fails with a non-zero exit code instead of prompting. |
| `host_launchctl(action, args)` | `launchctl` | Runs `/bin/launchctl action args…`. Returns nothing; use `spawn` if you need the output. |
| `host_write_file(path, data) -> bool` | `write_file` | Writes `data` to any path **except** inside `~/Library/Application Support/kennel`. Returns `false` on failure or denial. |
| `host_state_get(key) -> String` | `state` | Reads a value from the extension's own storage (`extensions/<name>/data/<key>`). A missing key returns `""`. |
| `host_state_set(key, value)` | `state` | Writes it. Keys are sanitized to `[A-Za-z0-9_-]`. |
| `host_notify(title, body)` | `notify` | Shows a macOS notification. |
| `host_log(level, msg)` | `log` | Writes `[<name>] <level>: <msg>` to `kenneld.out.log`. `level` is free text (`info`, `warn`, …). |
| `host_now_unix_secs() -> u64` | none | Current wall-clock time. Always available, because wasm has no clock of its own. |

`read_file` is a valid capability and the daemon implements it, but the SDK has no wrapper for it yet. Until it does, read files with `host_spawn("/bin/cat", &[path])`.

**Output limit.** Values coming back from the host (spawn output as JSON, state values) travel through a 64 KiB buffer, and anything larger is cut off. If a spawned command prints more than about 64 KiB, its result can't be parsed and comes back as `exit_code: -1` with `stderr: "bad host response"`. Filter the output in the command itself, with `head`, `grep` or `awk`.

**Environment.** Commands run as you, with the LaunchAgent's environment. `PATH` is only `/usr/bin:/bin:/usr/sbin:/sbin`, so use absolute paths (`/usr/bin/pgrep`, `/opt/homebrew/bin/…`).

### Status

`check` returns `Status::Healthy` or `Status::Unhealthy(String)`. The string is shown in the GUI, so make it say what's wrong ("only 4 GiB free on /"), not just "unhealthy".

A check that would flap on one bad probe can require several failures in a row before reporting `Unhealthy`. Count them in `state`, like `gdrive-watchdog` and `sketchybar-watchdog` do.

## Publishing

### To kennel-extensions

[kennel-extensions](https://github.com/Max-Levitskiy/kennel-extensions) is the default store. To add an extension:

1. Put the crate at `<name>/` in that repo, with `manifest.toml` next to its `Cargo.toml`.
2. Add `<name>` to the workspace `members`.
3. Open a PR.
4. Once it's merged, a maintainer pushes the tag `<name>-v<version>`. CI then builds it, attaches `monitor.wasm` and `manifest.toml` to a GitHub Release, and adds the entry to `index.toml`.

### Your own store

A store is any URL serving an `index.toml`:

```toml
[[extensions]]
name = "disk-space-watchdog"
version = "0.1.0"
wasm_url = "https://example.com/disk-space-watchdog/0.1.0/monitor.wasm"
manifest_url = "https://example.com/disk-space-watchdog/0.1.0/manifest.toml"
sha256 = "<shasum -a 256 monitor.wasm>"
manifest_sha256 = "<shasum -a 256 manifest.toml>"
```

All six fields are required. The GUI refuses to install when:

- either file's hash doesn't match, or
- the downloaded `manifest.toml` names a different extension than the entry.

A store with no extensions is `extensions = []`. Add the store's URL under **Settings** in the GUI. The easiest way to run a store is to fork kennel-extensions: its release workflow and `scripts/update_index.py` keep the index in sync for you.

## Examples

These extensions are worth reading:

- [`sd-keepalive`](https://github.com/Max-Levitskiy/kennel-extensions/tree/main/sd-keepalive) is the smallest real one: `write_file` on a 2 s tick.
- [`gdrive-watchdog`](https://github.com/Max-Levitskiy/kennel-extensions/tree/main/gdrive-watchdog) uses `spawn` probes, `state` to confirm a stall over consecutive ticks, and a disruptive `fix()`.
- [`sketchybar-watchdog`](https://github.com/Max-Levitskiy/kennel-extensions/tree/main/sketchybar-watchdog) keeps its decision logic in pure functions and tests it natively, calls a helper binary, and restarts a LaunchAgent with `launchctl`.
