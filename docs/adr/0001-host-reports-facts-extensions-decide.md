# The host reports facts; extensions decide what they mean

The host gives extensions raw observations of the machine, such as the displays, the space each one shows, and the windows of a named owner, through capabilities like `window_server`. Any rule about what those observations mean belongs in the extension. Examples: "a sketchybar window at least half a display wide is a bar", or "a fullscreen space needs no bar".

The first version of sketchybar-watchdog broke this. Its verdict lived in `kennel-barprobe`, a native binary built into Kennel.app. When fullscreen spaces turned out to need no bar, fixing that rule meant rebuilding and reinstalling Kennel.app, not just releasing the extension. Now the host changes only when an extension needs a fact it can't see yet. A change to a rule ships as an extension release through `index.toml`.

## Consequences

- An extension that needs a new capability needs a kenneld that has it. An older daemon fails to load the extension, because both the manifest's unknown capability and the wasm's unknown import are rejected. So the kennel release goes out before the extension that uses it.
- Host replies travel through the 64 KiB scratch buffer, so a capability that returns observations takes a filter (such as `window_server`'s owner) instead of returning everything.
