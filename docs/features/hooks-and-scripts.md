# Hooks & Scripts — shell extension points around the game lifecycle

Any module can attach shell scripts to the points where ritz builds, launches, and
tears down a game — plus a separate mechanism for a script to inject dynamically
computed values (env vars, args, wrappers) into the launch command itself. This is how
`resources/extensions/default/scripts/scripts.json` gives users a raw "run this before
launch"/"after launch"/"on exit" box without ritz needing to know what the command does.

## How it works

- **Four fixed stages**, declared per-module under a `Hooks` object
  (`crates/ritz-core/src/schema.rs:Hooks`) and driven from
  `crates/ritz-app/src/supervisor.rs:run` /
  `crates/ritz-app/src/supervisor.rs:supervise_loop`:
  - `PreLaunch` — fires just before the child process is spawned, after the launch
    command and `ScriptBuilders` have already run.
  - `PostSpawn` — fires immediately after `Command::spawn` returns (the child exists,
    but may not have execed the real game yet).
  - `OnGameReady` — fires once the supervise loop's process-name poll finds a match
    (or `READY_TIMEOUT` elapses), the same signal that drives backend
    `on_game_ready`. See `docs/features/process-supervisor.md` for what "ready" means.
  - `PostExit` — fires after the supervise loop returns the child's exit code, right
    before backend `post_exit` cleanup, on every exit path.
  *Why these four and not more:* they map 1:1 onto the only points in
  `crates/ritz-app/src/supervisor.rs:run` where a distinct, externally-observable
  lifecycle transition happens (command ready → process exists → game actually up →
  process gone). Adding a stage between two of these would have nothing new to hook.
- **Sequential across stages, per-extension order within a stage.**
  `crates/ritz-app/src/hooks.rs:run_stage` iterates the applicable extension list in
  order and runs each stage's hook one at a time — it does not parallelize hooks from
  different modules. *Why:* hooks commonly depend on host side effects (a `pre.sh` that
  sets up a directory another module's `pre.sh` reads), so silently running them
  concurrently would make behavior depend on scheduling luck.
- **Blocking vs background per hook.** A hook is either a bare string (`"pre.sh"`,
  blocking — `crates/ritz-core/src/schema.rs:HookSpec::Simple`) or an object
  (`{ "Script": "post.sh", "Background": true }` —
  `crates/ritz-core/src/schema.rs:HookSpec::Detailed`). Blocking hooks run via
  `crates/ritz-app/src/hooks.rs:run_one`, which waits on the child and turns a non-zero
  exit into a logged error. Background hooks run via
  `crates/ritz-app/src/hooks.rs:spawn_background`, which spawns and does not wait — the
  child outlives the call. *Why both:* `PreLaunch` and `PostExit` are naturally
  blocking (the user expects "wait for my setup/teardown script"), but a `PostSpawn`
  watcher, notification sound, or gamemode-by-pid attach must not stall the game from
  starting, so `scripts.json` marks its `post.sh` `Background: true`.
- **A hook's own script can still run several commands in parallel.** The
  `Background: true` flag only controls whether ritz waits on the *whole script*; what
  the script does internally is up to it.
  `resources/extensions/default/scripts/post.sh` backgrounds (`&`) each of the user's
  configured commands individually and `wait`s on all of them, so N post-launch
  commands run concurrently with each other, while the wrapping script itself is
  already non-blocking from ritz's point of view.
- **Env passed to every hook** — `crates/ritz-app/src/hooks.rs:var_env` sets
  `RITZ_APPID` and one `RITZ_VAR_<name>` per resolved field of *that extension*
  (toggles as `"true"`/`"false"`, `multi_string` as newline-joined entries, unset as
  `""`). The script runs with `cwd` set to the extension's own directory
  (`crates/ritz-app/src/hooks.rs:run_one`, `spawn_background`). *Why empty string and
  not `"false"` for unset:* scripts guard with `[ -n "$RITZ_VAR_x" ]`; `multi_string`
  in particular needs "no entries" to be empty, not the literal word `false`.
- **Errors are logged, never fatal.** `crates/ritz-app/src/hooks.rs:run_stage` catches
  every hook's `Result` and prints to stderr instead of propagating.
  *Why:* see `docs/features/process-supervisor.md` — a misbehaving extension script
  must not abort or crash the game the user is trying to play.
- **`ScriptBuilders` are a separate, non-lifecycle mechanism** for computing launch
  command content, not for side effects. Declared as a list of
  `{ "Block": "ENV_VARS" | "GAME_ENV_VARS" | "WRAPPERS" | "GAME_LAUNCH_ARGS", "Script": "…" }`
  (`crates/ritz-core/src/schema.rs:ScriptBuilder`) and run once per module via
  `crates/ritz-app/src/hooks.rs:apply_script_builders`, *before* `PreLaunch` fires
  (`crates/ritz-app/src/supervisor.rs:run`, step 3, ahead of step 4). Each builder gets
  the extension's resolved variables as a JSON object on stdin and the appid as
  `argv[1]`
  (`crates/ritz-app/src/hooks.rs:run_script_builder`), and is expected to print
  newline-separated contributions to stdout, which
  `crates/ritz-app/src/hooks.rs:inject` folds into the matching `LaunchCommand` field
  (`KEY=VALUE` lines for the two env blocks, `shlex`-split tokens for wrappers/args).
  *Why stdin/argv instead of the `RITZ_VAR_` env convention hooks use:* a builder's job
  is to *compute* command content from structured input, not to react to one flag —
  JSON on stdin lets it consume the whole resolved field set without parsing env names.
  No shipped module currently declares a `ScriptBuilders` block; `scripts.json` only
  uses `Hooks`.

## Using it

A module opts into hooks by adding a `Hooks` object to its JSON, one key per stage it
wants, each value either a bare script filename (blocking) or `{ "Script": ..., "Background": bool }`.
`resources/extensions/default/scripts/scripts.json` is the reference shape: it maps
three `multi_string` UI fields (`pre_command`, `post_command`, `exit_command`) straight
onto `PreLaunch` / `PostSpawn` (background) / `PostExit`, and the paired `.sh` files
(`resources/extensions/default/scripts/pre.sh`,
`resources/extensions/default/scripts/post.sh`,
`resources/extensions/default/scripts/exit.sh`) each read the corresponding
`RITZ_VAR_*` newline-joined list and `sh -c` each non-empty line, exiting immediately
(`exit 0`) when the variable is unset so an unconfigured stage costs nothing.

To add a hook to a new module:

1. Put the `.sh` file next to the module's JSON (scripts run with `cwd` = that
   directory, so relative paths inside the script resolve there).
2. Add the `Hooks` key for the stage(s) needed, background-flagging anything that
   shouldn't block game start.
3. Read `$RITZ_VAR_<name>` for any field the script needs, guarding on `-n` so an unset
   field is a no-op rather than an error.

## Options

| Config key | Default | Meaning |
| --- | --- | --- |
| `Hooks.PreLaunch` / `PostSpawn` / `OnGameReady` / `PostExit` | none | Per-stage `HookSpec` — bare script path (blocking) or `{ "Script", "Background" }`. |
| `Background` | `false` | Detailed-form only; `true` spawns and does not wait. |
| `ScriptBuilders[].Block` | — | Target launch block: `ENV_VARS` \| `GAME_ENV_VARS` \| `WRAPPERS` \| `GAME_LAUNCH_ARGS`. |
| `ScriptBuilders[].Script` | — | Script invoked with resolved vars as JSON on stdin, appid as `argv[1]`. |

## Related links

- [Process Supervisor](process-supervisor.md) — the launch/exit sequence that fires
  each hook stage, and why hook/backend errors are non-fatal.
- [Extension System](extension-system.md) — the module JSON format `Hooks` and
  `ScriptBuilders` are top-level keys of, and how `RITZ_VAR_*` values are resolved.
