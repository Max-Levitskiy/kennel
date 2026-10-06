# kennel

A macOS watchdog host. `kenneld` runs small WebAssembly extensions on a schedule;
each one exports `check()` (is the thing I watch healthy?) and `fix()` (repair it),
and gets only the host capabilities its `manifest.toml` declares (`spawn`,
`launchctl`, `state`, `log`, ...). A panicking or hung extension can't take the
daemon down: every call runs in a fresh wasm instance with a timeout.

`kennel-gui` lives in the menu bar: the icon is colored by overall health, and its
window lists installed extensions and installs new ones from an extension index.

## Docs

- [Using kennel](docs/usage.md): install, the menu bar app, stores, the control
  socket, files and logs, troubleshooting
- [Writing an extension](docs/writing-extensions.md): a tutorial, the runtime
  rules, the host API, and publishing to a store

## Layout

| Crate | What it is |
|---|---|
| `crates/kennel-daemon` | `kenneld`: wasmtime host, scheduler, control socket |
| `crates/kennel-gui` | menu bar app and extension store |
| `crates/kennel-guest-sdk` | what extensions link against (`kennel_extension!` macro, host imports) |
| `crates/kennel-proto` | wire types shared by daemon and GUI |
| `crates/kennel-barprobe` | asks the window server whether sketchybar is on every display |
| `fixtures/` | test extensions used by the daemon's tests |

Extensions live in [kennel-extensions](https://github.com/Max-Levitskiy/kennel-extensions),
whose `index.toml` is the GUI's default store.

## Install

```bash
packaging/install.sh     # build, assemble ~/Applications/Kennel.app, register LaunchAgents
packaging/uninstall.sh   # remove them again (--purge also deletes data and logs)
```

Requires macOS and a Rust toolchain. Re-running `install.sh` is the upgrade path. Logs go to `~/Library/Logs/kennel`,
extensions and state to `~/Library/Application Support/kennel`.

## Test

```bash
rustup target add wasm32-unknown-unknown
cargo build --release --target wasm32-unknown-unknown -p always-healthy -p unhealthy-then-fixed -p panics -p hangs
cargo test --workspace
```

## License

[MIT](LICENSE)
