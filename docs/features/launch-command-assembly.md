# Launch Command Assembly — turning `%command%` + extensions into an argv

Ritz sits between Steam's launch options and the actual game process. It receives
Steam's expanded `%command%`, layers each enabled extension's environment/wrapper/arg
contributions onto it in a fixed block order, and produces one spawnable command —
either to `exec` directly or to print for inspection.

## How it works

- Steam invokes `ritz %command%`; the tokens after `ritz` are the entire native (or
  Proton) launch chain, e.g. `steam-launch-wrapper -- reaper SteamLaunch AppId=730 --
  <SteamLinuxRuntime>/_v2-entry-point --verb=waitforexitandrun -- <game-exe> [args]`.
  `crates/ritz-core/src/steam.rs:SteamCommand::parse` turns this into a
  `crates/ritz-core/src/steam.rs:SteamCommand`, extracting `appid` (from
  `RITZ_APPID`/`SteamAppId` env, else the `AppId=` token via
  `crates/ritz-core/src/steam.rs:parse_appid_token`) and a default `game_name` (from the
  `steamapps/common/<DIR>/` segment via `crates/ritz-core/src/steam.rs:install_dir_name`,
  skipping runtime/proton dirs).
- *Why:* the parser **wraps, don't dissects** — `SteamCommand.raw` is kept verbatim and
  never rewritten token-by-token. Splitting is only used to *derive* metadata (appid,
  name), never to reconstruct the command; this is deliberate so a runtime-chain shape
  ritz doesn't recognize still launches correctly, it just can't be dissected.
- The resolved extension set (`crates/ritz-app/src/context.rs:AppContext::resolve_game`
  → `crates/ritz-app/src/context.rs:ResolvedGame`) is turned into a launch plan via
  `crates/ritz-app/src/context.rs:ResolvedGame::build_launch`, which calls
  `crates/ritz-core/src/builder.rs:build`. `build` takes one
  `crates/ritz-core/src/builder.rs:ExtInput` per active extension (its schema `spec`
  paired with its resolved `vars`) plus the raw game command tokens, and produces a
  `crates/ritz-core/src/builder.rs:LaunchCommand`.
- **Block order** (fixed, from `crates/ritz-core/src/builder.rs` module doc):
  `[ENV_VARS] [WRAPPERS] [GAME_ENV_VARS] [GAME_LAUNCH_COMMAND] [GAME_LAUNCH_ARGS]`.
  Each block is built independently and merged into `LaunchCommand`'s fields by
  `crates/ritz-core/src/builder.rs:build`:
  - **`ENV_VARS`** → `LaunchCommand.env_vars`, via
    `crates/ritz-core/src/builder.rs:build_env_block`. Applies to the whole chain (the
    child process environment) — every extension's `Requires`-gated `Builder` entries
    (`set`/`append`/`unset`) accumulate per variable name, in extension order, through
    `crates/ritz-core/src/builder.rs:EnvAccum`.
  - **`WRAPPERS`** → `LaunchCommand.wrappers`, via
    `crates/ritz-core/src/builder.rs:build_wrappers`. Each wrapper renders its
    `CommandSyntax` template (`{OPTIONS}` filled from its own `Builder` entries),
    tokenizes with `shlex`, then all wrappers are sorted by `(priority, extension
    order)` — lower priority number sorts first, i.e. closer to `%command%`
    (`crates/ritz-core/src/builder.rs:build_wrappers` sort call). E.g. gamemoderun
    (priority 50) ends up left of gamescope (priority 100):
    `gamemoderun gamescope -- %command%`.
  - **`GAME_ENV_VARS`** → `LaunchCommand.game_env_vars`, same accumulation logic as
    `ENV_VARS` via `build_env_block`, but scoped to only the game process.
  - **`%command%`** → `LaunchCommand.game_command`, copied verbatim from
    `SteamCommand.raw` — never touched by the builder.
  - **`GAME_LAUNCH_ARGS`** → `LaunchCommand.game_args`, via
    `crates/ritz-core/src/builder.rs:build_args`; each `Requires`-gated arg is
    interpolated and `shlex`-tokenized, then appended in extension order after the game
    command.
- *Why this order:* ENV_VARS come first because they must be visible to every process in
  the chain including the wrappers themselves (e.g. a wrapper reading `DXVK_HUD` at
  startup). WRAPPERS sit between the chain env and the game so they can intercept the
  process (gamescope re-execs the game inside a nested compositor, gamemoderun just
  wraps the exec) — *Why wrappers wrap `%command%` rather than the reverse:* Steam
  already owns everything left of `%command%` (steam-launch-wrapper, reaper, the Steam
  Linux Runtime entry point), so ritz can only insert **between** that chain and the
  game exe, and multiple wrappers must nest predictably by `Priority`, not by extension
  load order. GAME_ENV_VARS come right before `%command%` (not merged into the top-level
  env) specifically so a wrapper between them, if present, does *not* see them — see the
  `env` shim note below. GAME_LAUNCH_ARGS come last because they are genuine argv
  arguments to the game binary and must follow it, exactly like manual Steam launch
  options would.
- **The `env` shim for `GAME_ENV_VARS`**: when at least one wrapper is present,
  `game_env_vars` cannot be merged into the child process environment (the wrapper would
  inherit them too), so the builder inserts a literal `env KEY=VAL ...` shim
  immediately before `%command%` — see `crates/ritz-core/src/builder.rs:render_env_shim`
  (used by both `Display` and `exec_plan`). With **no** wrapper, there is nothing to
  shield the vars from, so `game_env_vars` is merged straight into the whole-chain env
  instead (`crates/ritz-core/src/builder.rs:LaunchCommand::exec_plan`, the
  `if self.wrappers.is_empty()` branch, and the analogous branch in
  `crates/ritz-core/src/builder.rs:LaunchCommand::display_tokens`).
- **Producing the final process**: `crates/ritz-core/src/builder.rs:LaunchCommand::exec_plan`
  turns the assembled blocks into a spawnable
  `crates/ritz-core/src/builder.rs:ExecPlan` (`program`, `args`, `env`) — the first argv
  token becomes `program`. `crates/ritz-app/src/supervisor.rs` uses this to actually
  spawn and supervise the game. `crates/ritz-core/src/builder.rs:LaunchCommand::display_tokens`
  produces the same shape as a shell-quoted string (via `crates/ritz-core/src/builder.rs:shq`)
  for human display — used by both the GUI preview and `--print`.

### Backend pre-pass

`crates/ritz-core/src/builder.rs:build` runs one more step after the four blocks above
are assembled and before `LaunchCommand` is returned: a `match input.spec.backend.as_deref()`
(~L196) over every active extension's `Backend` value, expanding the three list-backed
values into the blocks they belong to:

- `Backend: "custom-env"` — that extension's `env` list variable (a `multi_string`,
  already newline-joined by resolve) expands into `ENV_VARS` (chain-wide).
- `Backend: "custom-game-env"` — its `game_env` list variable expands into
  `GAME_ENV_VARS` (game-only).
- `Backend: "custom-args"` — its `args` list variable expands into `GAME_LAUNCH_ARGS`.

For the two env cases, each non-empty line is split on the **first** `=` into
`NAME=VALUE` (`crates/ritz-core/src/builder.rs:apply_list_env`, ~L166) and inserted as a
`Set`; a line with no `=` or an empty name is skipped, so a name can never contain a
newline. For `custom-args`, each non-empty line is pushed **verbatim** as one launch
arg — no `shlex`/word-splitting, so a value containing spaces stays one argument, unlike
the regular `GAME_LAUNCH_ARGS` builder above. An empty/unset list resolves to `""` and
the pre-pass is a no-op for that extension.

*Why a pre-pass here and not a `Backend`-trait handler:* these three don't need any
runtime lifecycle or external state — they only reshape one list into blocks this
builder already produces — so folding them in here avoids a `Backend` impl that would do
nothing but the env/argv work the builder does anyway. See
[runtime-backends.md](runtime-backends.md) for the real, stateful `Backend`-trait
handlers (`lsfg-vk`, `hypr-monctl`) this is *not* one of, and
[bundled-modules.md](bundled-modules.md) for the three list-backed modules themselves.

## Using it

- Steam launch options are set to `ritz %command%` (see the project `README.md` for
  first-time setup, including the `RITZ_APPID=<id> ritz %command%` bootstrap for
  non-Steam/unknown games).
- Normal launch: `crates/ritz-app/src/main.rs:cmd_launch` parses `%command%`, shows the
  splash (optionally the editor), resolves the extension set, and hands off to
  `crates/ritz-app/src/supervisor.rs` to run the assembled `ExecPlan`.
- **Dry run**: `ritz --print %command%` routes to
  `crates/ritz-app/src/main.rs:cmd_print`, which parses the command, loads context,
  resolves the game, builds the `LaunchCommand`, and prints diagnostics (`AppId`,
  `Game`, extension count, active `Preset`) to stderr followed by the fully assembled,
  shell-quoted command (`LaunchCommand`'s `Display` impl) to stdout — no process is
  spawned.

## Related links

- `docs/features/extension-system.md` — how `ENV_VARS`, `WRAPPERS`, `GAME_ENV_VARS`, and
  `GAME_LAUNCH_ARGS` blocks are declared per extension and resolved/gated by `Requires`
  and variables before reaching the builder.
- `docs/architecture/overview.md` — where this fits in the overall data flow.
