# Process Supervisor — ritz stays the process Steam tracks

When Steam launches a game through ritz, ritz spawns the assembled game command
and then **stays alive as that game's process for its whole lifetime** — waiting
on the child, forwarding its exit code back to Steam, firing lifecycle hooks,
reloading config on the fly, and cleaning up backends when the game quits.

*Why:* Steam tracks the process it launched (`ritz %command%`) as "the game". If
ritz forked the game and exited immediately (fire-and-forget), Steam would
believe the game closed the instant it opened — the library would show it as not
running, playtime wouldn't accrue, and the "Stop" button couldn't reach it. By
remaining the tracked process and mirroring the child's exit state, ritz is
transparent: Steam sees exactly what it would have seen launching the game
directly.

## How it works

The supervisor is `crates/ritz-app/src/supervisor.rs:run`, called from
`crates/ritz-app/src/main.rs:cmd_launch` once the splash/editor flow decides to
launch. Its return value (the game's exit code) is propagated all the way out to
`main`'s `ExitCode` so Steam reflects the game's true result.

`run` proceeds in order:

- **Build the launch command** via `crates/ritz-app/src/context.rs:ResolvedGame::build_launch`.
- **Backend pre-launch** — each active backend
  (`crates/ritz-app/src/backends/mod.rs:active`) gets `Backend::pre_launch` and
  may inject game-only env (e.g. plugin setup, monitor rules).
- **ScriptBuilders** — `crates/ritz-app/src/hooks.rs:apply_script_builders`
  injects dynamic block content (extra env vars, wrappers, args) into the command.
- **`PreLaunch` hooks**, then **spawn** the child with the resolved program, args
  and env (`EnvAction::Set` / `EnvAction::Unset`).
- **`PostSpawn` hooks** fire immediately after the child exists.
- **Supervise** — hand off to `crates/ritz-app/src/supervisor.rs:supervise_loop`.
- **`PostExit` hooks + backend `post_exit` cleanup** run after the loop returns,
  regardless of how the game exited.

*Why:* hook and backend-callback errors are logged and swallowed, never
propagated — a misbehaving extension or a failed cleanup must not abort or crash
the game the user is trying to play. Cleanup (`post_exit`) runs on every exit path
so a backend that installed system state (a loaded Hyprland plugin, a monitor
override) always gets a chance to undo it.

### The supervise loop

`supervise_loop` is a single-threaded 500 ms (`POLL`) polling loop that owns four
concerns at once:

- **Exit detection & code forwarding.** `child.try_wait()` is checked each tick;
  when the child has exited, its code is returned. A normally-exited process
  forwards `status.code()`; a signal-killed process forwards `128 + signal`.
  *Why:* `128 + signum` is the standard shell convention for reporting a
  signal-terminated process, so ritz's exit status is indistinguishable from
  running the game directly under a shell.
- **Termination forwarding.** SIGTERM and SIGINT are registered against an
  `AtomicBool` flag via `signal_hook`. When set, the loop calls
  `crates/ritz-app/src/supervisor.rs:forward_term`, which sends SIGTERM to the
  child's PID (once, guarded by a `forwarded` flag). *Why:* when Steam's "Stop
  Game" or a Ctrl-C kills ritz, the actual game process must die with it —
  otherwise the game would be orphaned while Steam thinks it stopped it.
- **Game-ready detection.** The backends and the Steam command contribute a list
  of process names to await (`Backend::ready_process` + `SteamCommand.game_name`).
  Each tick `crates/ritz-app/src/supervisor.rs:process_running` runs `pgrep -x`
  against them; the first match fires the `OnGameReady` hook stage and every
  backend's `on_game_ready`. If no matching process appears within
  `READY_TIMEOUT` (60 s) the loop gives up and marks ready as fired. *Why:* the
  spawned process is often a launcher/wrapper (Proton, a Steam reaper) that execs
  the real game later, so "the game is up" can't be equated with "spawn
  returned" — ritz watches for the real executable by name. The timeout keeps a
  game whose process name never matches from blocking the ready stage forever.
- **Live config reload.** At startup the loop snapshots the mtimes of the game's
  config file and every preset in its inheritance chain (walking `parent`
  pointers, guarded by a `seen` set against cycles). Each tick it re-`stat`s them;
  on any change it re-resolves the game with
  `crates/ritz-app/src/context.rs:AppContext::resolve_game` and calls each
  backend's `live_reload` with the updated resolution. *Why:* the user can edit a
  module or swap a preset while the game is running and have backends (e.g.
  monitor saturation/brightness) re-apply without relaunching. Polling mtimes
  reuses the loop that already exists rather than pulling in an inotify watcher —
  the cost is one `stat` per watched file every 500 ms.

## Using it

The supervisor has no direct UI — it runs invisibly inside the windowless launch
coordinator. A user experiences it as: the game launches, ritz's chosen settings
apply, edits made mid-session take effect live, and when the game (or Steam's Stop
button) ends the session, Steam's library correctly returns to "not running" with
the right exit state.

## Options

| Config key | Default | Meaning |
| --- | --- | --- |
| `POLL` (const) | 500 ms | Supervise-loop tick interval — exit check, signal forward, ready poll, mtime re-stat. |
| `READY_TIMEOUT` (const) | 60 s | How long to wait for a matching game process before giving up on the `OnGameReady` stage. |

Both are compile-time constants in `crates/ritz-app/src/supervisor.rs`, not
user-facing config.

## Related links

- [Architecture Overview](../architecture/overview.md) — where the supervisor sits in the launch flow.
- [Hooks & Scripts](hooks-and-scripts.md) — the `PreLaunch` / `PostSpawn` / `OnGameReady` / `PostExit` stages and ScriptBuilders the supervisor drives.
- [Runtime Backends](runtime-backends.md) — the `Backend` trait callbacks (`pre_launch`, `on_game_ready`, `live_reload`, `post_exit`) the loop invokes.
