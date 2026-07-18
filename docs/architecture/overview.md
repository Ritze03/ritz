# Architecture Overview

The start-here page for the ritz codebase: what it is, how the two crates split the
work, and the pipeline that turns Steam's `%command%` plus a stack of JSON modules into
a supervised game process. It links out to the deep-dive pages rather than repeating
them — every feature has its own page (see the [documentation map](#documentation-map)),
and the [terminology](../meta/TERMINOLOGY.md) is defined once there, not re-explained
here.

## Big picture

Ritz is a Linux Steam launch wrapper: you put `ritz %command%` in a game's Steam launch
options, and ritz inserts itself between Steam and the game. It is a native egui desktop
app that also runs headless as a launch coordinator — the same binary is both the
settings GUI and the process that Steam tracks for the game's lifetime.

The workspace is two crates:

- **`ritz-core`** — pure domain logic, no egui and no threads. The extension schema, the
  `Requires` condition grammar, four-scope variable resolution, the launch-command
  builder, and the Steam `%command%` parser. Everything here is unit-testable in
  isolation (`crates/ritz-core/src/lib.rs`).
- **`ritz-app`** — the egui binary and everything that touches the OS: window/event
  loop, the settings GUI, the splash, process supervision, shell hooks, runtime
  backends, and embedded-resource export (`crates/ritz-app/src/main.rs`).

*Why two crates:* the value of ritz is the config→launch-command transformation, and
that logic is worth testing without spinning up a window or a real process. Keeping it in
a UI-free, thread-free crate means the entire pipeline — schema, conditions, resolution,
builder, Steam parser — is exercised by plain `cargo test` with no egui or OS in the
loop; `ritz-app` is a thin shell that renders and executes what `ritz-core` decides.

The one struct everything in the app hangs off is
`crates/ritz-app/src/context.rs:AppContext` — it holds the loaded extensions, config
paths, and general settings, and every mode (`--print`, splash, GUI, supervisor) starts
by calling `AppContext::load` and then `resolve_game`.

## Process model

The binary has five invocation modes, dispatched by `crates/ritz-app/src/main.rs:main`
on the first arg:

| Invocation | Mode | Function |
| --- | --- | --- |
| `ritz` (no args / no appid) | settings GUI manager | `main.rs:cmd_gui` |
| `ritz %command%` | launch coordinator (windowless) | `main.rs:cmd_launch` |
| `ritz --print %command%` | dry-run, print assembled command | `main.rs:cmd_print` |
| `ritz --splash <appid> -- %command%` | splash window | `main.rs:cmd_splash` |
| `ritz --edit [--launch] <appid> [-- %command%]` | settings GUI for one game | `main.rs:cmd_edit` |

*Why the split into subprocesses:* the splash and the editor each spawn as their **own
process** (`main.rs:run_splash_subprocess`, `main.rs:edit_in_subprocess`). winit permits
only one event loop per process, and a separate process guarantees the window despawns on
exit. The top-level `cmd_launch` process is windowless: it spawns the splash, optionally
the editor, then supervises the game — so ritz remains the single process Steam tracks
from click to game exit.

## Data flow

The launch pipeline is a straight line from Steam's launch token to a supervised child
process. Steam expands `%command%` into a nested-`--` runtime chain, which ritz preserves
verbatim (wrap, don't dissect) and prepends its own wrappers/env to. Loading extensions
and resolving the four config scopes produces a `ResolvedGame`; the builder folds every
active module into one argv; the supervisor runs it and stays alive for the game's
lifetime, firing lifecycle hooks and backends along the way.

```
Steam %command%  +  $SteamAppId / $RITZ_APPID
      │  nested-`--` runtime chain, preserved verbatim
      ▼
steam.rs:SteamCommand::parse            [boundary — wrap, don't dissect]
      │  appid + raw command
      ▼
context.rs:AppContext::load             [load extensions + config paths]
      │  extension.rs:load_all  →  schema.rs:Extension per JSON module
      ▼
context.rs:AppContext::resolve_game     [→ ResolvedGame]
      │  resolve.rs:resolve   ext default → global → profile → game
      ▼
context.rs:ResolvedGame::build_launch
      │  builder.rs:build   [ENV_VARS][WRAPPERS][GAME_ENV_VARS] %command% [GAME_LAUNCH_ARGS]
      ▼
supervisor.rs:run                       [spawn, wait, forward exit code + signals]
      │  fires hooks.rs stages + backends/mod.rs handlers
      ▼
game process
```

*Why resolution is separate from building:* `resolve.rs:resolve` computes each field's
effective value **and its provenance** (which of the four scopes it came from) up front,
so the same `ResolvedGame` drives both the GUI's colour-coded inheritance display and the
builder — the builder never re-derives inheritance, it just reads settled values. *Why
the builder is data-driven:* modules are JSON, not Rust, so adding a new launch tweak is
a manifest drop-in, not a recompile — `builder.rs:build` walks each extension's declared
`Builder` steps and `Requires` conditions rather than hard-coding any one module's
behaviour. The escape hatch for logic JSON can't express is a named
[runtime backend](../features/runtime-backends.md), not a special case in the builder.

## Module map

| path | role |
| --- | --- |
| `crates/ritz-app/src/main.rs` | Entry point; arg dispatch to the five modes (`main`). |
| `crates/ritz-app/src/context.rs` | `AppContext` — loads extensions + config, `resolve_game`, `ResolvedGame::build_launch`. The app's hub. |
| `crates/ritz-app/src/supervisor.rs` | `run` — spawns the game, waits, forwards exit code + signals, fires hooks/backends, polls live config reloads. |
| `crates/ritz-app/src/gui.rs` | egui settings window: `run_manager` (all games) and `run` (one game). |
| `crates/ritz-app/src/splash.rs` | Pre-launch countdown window + new-game picker + unknown-id guide. |
| `crates/ritz-app/src/hooks.rs` | Lifecycle shell hooks (`run_stage`) and dynamic ScriptBuilders. |
| `crates/ritz-app/src/backends/` | Runtime backends: `Backend` trait, `registry`, `active`; `lsfg.rs`, `hypr_monctl.rs`. |
| `crates/ritz-app/src/resources.rs` | Embedded assets; `bootstrap` copies them into the config dir on startup. |
| `crates/ritz-app/src/theme.rs` | "Graphite" theme tokens + egui style — single source of colour truth. |
| `crates/ritz-app/src/icon_center.rs` | `IconCenterCache` — measured ink-box centering for icon glyphs (`Galley::mesh_bounds`), so icon rows don't read ragged. |
| `crates/ritz-core/src/schema.rs` | The `Extension` manifest schema: fields, builders, env/wrapper/arg specs. |
| `crates/ritz-core/src/extension.rs` | Discovery, loading, validation, dir-merge (`discover`, `load_all`, `validate`). |
| `crates/ritz-core/src/condition.rs` | The `Requires` grammar parser + evaluator (`parse`, `eval_opt`). |
| `crates/ritz-core/src/resolve.rs` | Four-scope resolution → effective value + provenance (`resolve`, `Resolution`). |
| `crates/ritz-core/src/variables.rs` | Per-extension resolved-variable lookup surface for the builder. |
| `crates/ritz-core/src/builder.rs` | `LaunchCommand` assembly from resolved extensions (`build`). |
| `crates/ritz-core/src/config.rs` | On-disk config: `GameConfig`, `Preset`, `GeneralConfig`, `Paths`. |
| `crates/ritz-core/src/steam.rs` | `SteamCommand::parse` — the `%command%` parser. |

## Documentation map

Where to read about X. Every capability has its own page:

| Go here for… | Page |
| --- | --- |
| Project vocabulary (extension, scope, builder, wrapper, `%command%`…) | [meta/TERMINOLOGY.md](../meta/TERMINOLOGY.md) |
| GUI styling rules, theme tokens, do-NOT bans | [ui/STYLING-GUIDE.md](../ui/STYLING-GUIDE.md) |
| What a JSON module is and how it compiles into a launch command | [features/extension-system.md](../features/extension-system.md) |
| The four-scope inheritance chain and provenance display | [features/scoped-config.md](../features/scoped-config.md) |
| The settings window: rendering modules, live launch-command preview | [features/settings-gui.md](../features/settings-gui.md) |
| The pre-launch countdown window and the new-game wizard | [features/splash-and-new-game-wizard.md](../features/splash-and-new-game-wizard.md) |
| How ritz stays the process Steam tracks; exit codes, signals, live reload | [features/process-supervisor.md](../features/process-supervisor.md) |
| Shell hooks (PreLaunch/PostSpawn/OnGameReady/PostExit) and ScriptBuilders | [features/hooks-and-scripts.md](../features/hooks-and-scripts.md) |
| The five assembly blocks and how the argv is built | [features/launch-command-assembly.md](../features/launch-command-assembly.md) |
| The modules shipped out of the box | [features/bundled-modules.md](../features/bundled-modules.md) |
| Stateful Rust handlers behind LSFG-VK and Hypr-Monctl | [features/runtime-backends.md](../features/runtime-backends.md) |
| Embedded assets and the first-run copy into the config dir | [features/resource-export.md](../features/resource-export.md) |

## Where to look for X

Task-oriented cheat-sheet for touching the code:

- **Add a new bundled module** — drop a JSON manifest under
  `resources/extensions/default/` (or `built-in/`); it's discovered by
  `crates/ritz-core/src/extension.rs:load_all` and validated by
  `crates/ritz-core/src/extension.rs:validate`. No Rust change. See
  [bundled-modules.md](../features/bundled-modules.md).
- **Add a new field type or manifest key** — extend
  `crates/ritz-core/src/schema.rs:Extension` (and `UiField`), then teach
  `crates/ritz-core/src/resolve.rs:resolve` and the GUI renderer in
  `crates/ritz-app/src/gui.rs` how to handle it.
- **Change how the launch argv is assembled** — `crates/ritz-core/src/builder.rs:build`;
  block order and the `%command%`-preservation rule live there. See
  [launch-command-assembly.md](../features/launch-command-assembly.md).
- **Change scope precedence or provenance** — `crates/ritz-core/src/resolve.rs:resolve`
  and the `Provenance` enum. See [scoped-config.md](../features/scoped-config.md).
- **Add a stateful backend** (needs a process lifecycle, not just env/args) — implement
  `crates/ritz-app/src/backends/mod.rs:Backend` and register it in `registry`. See
  [runtime-backends.md](../features/runtime-backends.md).
- **Add a lifecycle hook stage** — `crates/ritz-app/src/hooks.rs:Stage` and
  `run_stage`; fire it from `crates/ritz-app/src/supervisor.rs:run`. See
  [hooks-and-scripts.md](../features/hooks-and-scripts.md).
- **Change supervision / exit-code / signal behaviour** —
  `crates/ritz-app/src/supervisor.rs:run`. See
  [process-supervisor.md](../features/process-supervisor.md).
- **Change the `Requires` grammar** — `crates/ritz-core/src/condition.rs:parse` /
  `eval_opt`.
- **Change what Steam's `%command%` parsing derives** —
  `crates/ritz-core/src/steam.rs:SteamCommand::parse`.
- **Change colours / theme tokens** — `crates/ritz-app/src/theme.rs`; never hard-code hex
  at call sites. See [ui/STYLING-GUIDE.md](../ui/STYLING-GUIDE.md).
- **Change config file locations** — `crates/ritz-core/src/config.rs:Paths`.
- **Change what ships embedded or how it's exported on first run** —
  `crates/ritz-app/src/resources.rs:bootstrap`. See
  [resource-export.md](../features/resource-export.md).
