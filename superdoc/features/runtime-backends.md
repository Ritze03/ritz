# Runtime Backends — the stateful handler behind the LSFG-VK module

`Extension.Backend` (`crates/ritz-core/src/schema.rs:Extension.backend`) currently has
four meaningful values, and they split into two unrelated mechanisms:

- **`lsfg-vk`** — a real runtime backend handler, a native Rust type implementing
  `crates/ritz-app/src/backends/mod.rs:Backend`. This page covers it.
- **`custom-env`, `custom-game-env`, `custom-args`** — *not* `Backend`-trait impls at
  all. They're a pre-pass in the generic builder that expands one `multi_string` list
  variable into N env vars / launch args at build time. See
  [launch-command-assembly.md](launch-command-assembly.md#backend-pre-pass) for how that
  pre-pass works, and [bundled-modules.md](bundled-modules.md) for the three list-backed
  modules themselves.

*Why the split:* the list-backed trio need no persistent runtime handler — no lifecycle,
no external process/file to drive — they only reshape a list into blocks the generic
builder already assembles (`ENV_VARS`/`GAME_ENV_VARS`/`GAME_LAUNCH_ARGS`), so a builder
pre-pass is enough and a `Backend`-trait impl would be pure overhead. LSFG-VK
instead declares the same `"Backend"` field but is consumed by a native Rust handler
implementing `crates/ritz-app/src/backends/mod.rs:Backend` — because what it does
(write an external tool's own config file) can't be expressed as env vars or argv at all.

*History:* a second trait-based backend, `hypr-monctl` (drove a Hyprland plugin over
`hyprctl`), was removed on 2026-10-29 because its upstream project is dead; see
[bundled-modules.md](bundled-modules.md#removed-modules).

## How it works

- `crates/ritz-app/src/backends/mod.rs:Backend` is the lifecycle trait: `pre_launch`,
  `on_game_ready`, `live_reload`, `post_exit`, plus `enabled` (checks the module's
  `enabled` toggle) and `ready_process`. `crates/ritz-app/src/backends/mod.rs:registry`
  lists the registered backends; `crates/ritz-app/src/backends/mod.rs:active` filters to
  the ones enabled for the current game.
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
    notification (carrying the app logo as its icon — see Resource Export) on
    completion. *Why:* some games briefly render broken frames or a
    black screen if frame generation is active from process start, so the delay lets
    the game reach a stable frame before lsfg-vk's multiplier kicks in.

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

## Related links

- [extension-system.md](extension-system.md) — how a manifest's `UI` fields resolve
  into the variable values these backends read via `ResolvedGame::value`/`truthy`; also
  documents the `Backend` field's four values and `ForkedFrom` metadata.
- [launch-command-assembly.md](launch-command-assembly.md#backend-pre-pass) — the
  list-backed `custom-env`/`custom-game-env`/`custom-args` builder pre-pass (the *other*
  `Backend` mechanism, not a trait impl).
- [bundled-modules.md](bundled-modules.md) — the shipped-module reference tour,
  including where LSFG-VK sits among the other modules.
