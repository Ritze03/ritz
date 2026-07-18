# Runtime Backends — stateful handlers behind the LSFG-VK and Hypr-Monctl modules

`Extension.Backend` (`crates/ritz-core/src/schema.rs:Extension.backend`) currently has
five meaningful values, and they split into two unrelated mechanisms:

- **`lsfg-vk`, `hypr-monctl`** — real runtime backend handlers, each a native Rust type
  implementing `crates/ritz-app/src/backends/mod.rs:Backend`. This page covers those two.
- **`custom-env`, `custom-game-env`, `custom-args`** — *not* `Backend`-trait impls at
  all. They're a pre-pass in the generic builder that expands one `multi_string` list
  variable into N env vars / launch args at build time. See
  [launch-command-assembly.md](launch-command-assembly.md#backend-pre-pass) for how that
  pre-pass works, and [bundled-modules.md](bundled-modules.md) for the three list-backed
  modules themselves.

*Why the split:* the list-backed trio need no persistent runtime handler — no lifecycle,
no external process/file to drive — they only reshape a list into blocks the generic
builder already assembles (`ENV_VARS`/`GAME_ENV_VARS`/`GAME_LAUNCH_ARGS`), so a builder
pre-pass is enough and a `Backend`-trait impl would be pure overhead. LSFG-VK and
Hypr-Monctl instead declare the same `"Backend"` field but are consumed by a native Rust
handler implementing `crates/ritz-app/src/backends/mod.rs:Backend` — because what they do
(write an external tool's own config file, drive a Hyprland plugin over IPC) can't be
expressed as env vars or argv at all.

## How it works

- `crates/ritz-app/src/backends/mod.rs:Backend` is the lifecycle trait: `pre_launch`,
  `on_game_ready`, `live_reload`, `post_exit`, plus `enabled` (checks the module's
  `enabled` toggle) and `ready_process`. `crates/ritz-app/src/backends/mod.rs:registry`
  lists both backends; `crates/ritz-app/src/backends/mod.rs:active` filters to the ones
  enabled for the current game.
- **LSFG-VK** (`crates/ritz-app/src/backends/lsfg.rs:LsfgBackend`) writes
  `~/.config/lsfg-vk/conf.toml`, the config file lsfg-vk itself reads, matching the
  running process by its `exe` field. Because Steam tracks the ritz process rather than
  the game binary, ritz always registers itself as `"RitzLauncher"`
  (`crates/ritz-core/src/lsfg_toml.rs:RITZ_EXE`) and sets `LSFG_PROCESS` to the same
  name in `pre_launch` — the game inherits frame generation from the launcher's process
  identity, not its own.
  - The actual TOML edit is a pure read-modify-write transform,
    `crates/ritz-core/src/lsfg_toml.rs:apply`, which finds-or-creates ritz's `[[game]]`
    entry (`crates/ritz-core/src/lsfg_toml.rs:ritz_game_entry`) and preserves any other
    `[[game]]` entries already in the file. `crates/ritz-app/src/backends/lsfg.rs:write_conf`
    is the single funnel for every write (pre-launch, activation-delay thread,
    live-reload), serialized by `crates/ritz-app/src/backends/lsfg.rs:conf_lock`. *Why a
    lock:* the activation-delay thread below and a live-reload triggered from the GUI
    can otherwise race on the same file and clobber each other's read-modify-write.
  - **Activation delay.** If the module's `activation_delay` field is > 0, `pre_launch`
    first writes the multiplier as `1` (pass-through, no frame gen) so the game starts
    clean, then spawns a thread that sleeps for the delay and hot-patches just the
    multiplier back up via `crates/ritz-core/src/lsfg_toml.rs:set_multiplier`
    (`crates/ritz-app/src/backends/lsfg.rs:LsfgBackend::pre_launch`), firing a desktop
    notification on completion. *Why:* some games briefly render broken frames or a
    black screen if frame generation is active from process start, so the delay lets
    the game reach a stable frame before lsfg-vk's multiplier kicks in.
- **Hypr-Monctl** (`crates/ritz-app/src/backends/hypr_monctl.rs:HyprMonctlBackend`)
  applies per-game saturation, brightness, and temperature to a monitor via a
  Hyprland plugin, driven entirely by `hyprctl` subcommands (`monctl register` /
  `monctl clear`) rather than a config file — the plugin owns focus detection, ritz
  only tells it which window class to match.
  - `pre_launch` first ensures the plugin `.so` is loaded
    (`crates/ritz-app/src/backends/hypr_monctl.rs:ensure_plugin_loaded`), checking
    `hyprctl plugin list` by `.so` file stem before loading, because
    `hyprctl plugin load` is **not idempotent** — reloading an already-loaded plugin
    errors with "Cannot load a plugin twice!". The plugin path defaults to the bundled
    `.so` ritz exports to `Paths::plugins_dir()/hypr-monctl.so`
    (`crates/ritz-app/src/backends/hypr_monctl.rs:HyprMonctlBackend::read`), overridable
    via a `plugin_path` variable.
  - `pre_launch`/`live_reload` both call
    `crates/ritz-app/src/backends/hypr_monctl.rs:HyprMonctlBackend::apply`, which clears
    *every* registered rule (`clear_cmd`) before registering the current window class
    (`register_cmd`). *Why clear first:* guarantees no stale rule from a previously
    launched game (a different window class) lingers if ritz didn't get a clean
    `post_exit` last time. `post_exit` just clears again; the plugin restores the
    monitor to neutral once nothing is registered.
  - Gated by `"RequiresDesktop": "Hyprland"` in `resources/extensions/built-in/hypr-monctl/hypr-monctl.json`,
    enforced once in `crates/ritz-app/src/context.rs:AppContext::load` via a
    `desktop_matches()` check against `$XDG_CURRENT_DESKTOP`. *Why:* the backend calls
    `hyprctl` unconditionally — on a non-Hyprland desktop those calls would just fail
    noisily (or worse, do nothing silently) every launch, so the whole module is
    dropped from the GUI, resolution, and backend registry activation on any other
    desktop rather than shipping a backend that only half-works.

## The bundled plugin `.so` — atomic install, never in-place

The `hypr-monctl.so` plugin itself ships under `resources/plugins/` and is exported to
the user's config/plugins directory by `crates/ritz-app/src/resources.rs:bootstrap` (and
re-exported on demand by `crates/ritz-app/src/resources.rs:reexport`) — see
[bundled-modules.md](bundled-modules.md) for the wider resource-export mechanism. The
export path (`crates/ritz-app/src/resources.rs:export_dir`) never opens the destination
file directly: it skips the write entirely if the bytes are already identical, and
otherwise writes to a `.tmp` sibling and `std::fs::rename`s it over the destination.
*Why:* Hyprland has `hypr-monctl.so` `dlopen`'d and mmap'd for the lifetime of the
session; an in-place `std::fs::write` truncates the same inode Hyprland is actively
executing code out of, corrupting the mapping and crashing the whole compositor on the
next call into it (observed as a SIGSEGV on window focus after a second game launch).
`rename()` swaps the directory entry to a fresh inode instead, leaving whatever
Hyprland already has mapped untouched.

## Options

| Config key (LSFG-VK) | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Master toggle; backend is inert unless truthy. |
| `multiplier` | `2` | Frame generation multiplier (`2`/`3`/`4`/`6`/`8`). |
| `flow_scale` | unset | Optical flow resolution scale (`0.25`-`1.0`). |
| `performance_mode` | `false` | Use the lighter-weight model. |
| `hdr_mode` | `false` | HDR-aware frame generation. |
| `present_mode` | unset | Overrides `experimental_present_mode` (`fifo`/`mailbox`/`immediate`). |
| `activation_delay` | `0` | Seconds to hold at pass-through (`1x`) before applying `multiplier`; `0` = immediate. |

| Config key (Hypr-Monctl) | Default | Meaning |
| --- | --- | --- |
| `enabled` | `false` | Master toggle; backend is inert unless truthy. |
| `window_class` | game name | Hyprland window class to match; falls back to the game's configured name. |
| `saturation` | `1.0` | `1.0` = neutral, higher = more vivid. |
| `brightness` | `1.0` | `1.0` = neutral. |
| `temperature` | `0.0` | `-1.0` = cool, `0` = neutral, `1.0` = warm. |
| `plugin_path` | bundled `.so` | Explicit override for the plugin path; otherwise defaults to `Paths::plugins_dir()/hypr-monctl.so` if it exists. |

## Related links

- [extension-system.md](extension-system.md) — how a manifest's `UI` fields resolve
  into the variable values these backends read via `ResolvedGame::value`/`truthy`; also
  documents the `Backend` field's five values and `ForkedFrom` metadata.
- [launch-command-assembly.md](launch-command-assembly.md#backend-pre-pass) — the
  list-backed `custom-env`/`custom-game-env`/`custom-args` builder pre-pass (the *other*
  `Backend` mechanism, not a trait impl).
- [bundled-modules.md](bundled-modules.md) — the shipped-module reference tour,
  including where LSFG-VK and Hypr-Monctl sit among the other modules.
