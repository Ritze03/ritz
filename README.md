# Ritz Game Launcher

A Linux-only Steam launch wrapper. Set one launch option and Ritz transparently
injects env vars, wrappers (gamescope/gamemode), game-only env vars, and launch
args around the real game command — solving the Linux-gaming pain of hand-managing
`RADV_PERFTEST`, gamescope flags, LSFG frame-gen quirks, and so on.

## Usage

In Steam, set the game's launch option to:

```
ritz %command%
```

That's it. On launch a splash screen appears for a few seconds:

- **Q** — cancel launch
- **W** — launch immediately
- **E** — edit this game's config
- (timeout) — launch

Run `ritz` with no arguments to open the settings GUI.

## How it works

A launch command is assembled from five blocks:

```
[ENV_VARS] [WRAPPERS] [GAME_ENV_VARS] [GAME_LAUNCH_COMMAND] [GAME_LAUNCH_ARGS]
```

- `ENV_VARS` apply to the whole chain (the child process environment).
- `WRAPPERS` (gamescope, gamemode…) are prepended left of `%command%`, sorted by
  priority — the standard `gamescope … -- %command%` idiom.
- `GAME_ENV_VARS` apply only to the game, via an `env` shim after the wrapper `--`.
- `GAME_LAUNCH_COMMAND` is the entire Steam `%command%`, preserved verbatim (the
  `SteamLinuxRuntime` container is never stripped). Steam-injected *overlays*
  (mangohud/gamemoderun) can optionally be discarded so Ritz manages them itself.
- `GAME_LAUNCH_ARGS` are appended after the game command.

Ritz stays the process Steam tracks for the whole game lifetime, supervising the
child: it forwards the exit code, fires lifecycle hooks, waits for the real game
process to appear (`OnGameReady`), live-reloads backend settings on config change,
and cleans up on exit.

### Dry-run

```
ritz --print %command%
```

prints the assembled command without launching.

## Extensions

Functionality is JSON-driven. Drop extensions into `~/.config/ritz/extensions/`:

```
simple-extension.json
complex-extension/
  complex-extension.json   # exactly one manifest
  build-env.sh             # optional scripts referenced by the manifest
```

Each extension declares a `UI` tree (sections of fields), and contributions to the
launch blocks (`ENV_VARS`, `WRAPPERS`, `GAME_ENV_VARS`, `GAME_LAUNCH_ARGS`) and/or
a built-in `Backend`. Shipped extensions: **Gamescope, AMD RADV, Proton Tweaks,
GameMode, Core** (env clearing / overlay stripping / compat fixes), and **LSFG-VK**
(backend).

Key concepts:

- **Variables** are auto-scoped per extension (short names, no prefixes). A field
  whose `Variable` is `global:<name>` is published to other extensions' *builders*
  only.
- **`Requires`** gates UI visibility and command building: a boolean expression of
  variable names with `AND` / `OR` / `!` (e.g. `enabled AND fsr_enabled`,
  `gamescope AND !native_res`). Precedence `! > AND > OR`, no parentheses.
- **Builder ops**: env vars support `set` / `append` (with `Separator`) / `unset`,
  evaluated in declaration order; `{var}` interpolates a variable's value.

## Configuration

Stored under `~/.config/ritz/` (override with `$RITZ_CONFIG_DIR`):

```
general.json            # splash timeout, default preset
presets/<name>.json     # named bundles of variable values
games/<appid>.json      # per-game overrides (named after the SteamAppId)
extensions/             # extensions (bundled set exported here on first run, + drop-ins)
plugins/                # bundled plugins (e.g. hypr-monctl.so) exported here on first run
```

All bundled resources (font, extensions, plugins) are embedded in the binary. On
startup ritz exports `extensions/` and `plugins/` into the config directory,
**never overwriting** files you've edited — so the shipped extensions become your
editable copies and the hypr-monctl plugin lands in `plugins/` automatically.

Values resolve in three layers — **extension default ← preset ← game override** —
and only explicitly set values are stored. The GUI shows each value's provenance
(default / inherited from preset / set for this game) and offers reset-to-inherit.

## Backends

Backends are extensions whose values feed a built-in handler with a process
lifecycle, rather than the command builder:

- **LSFG-VK** — writes `~/.config/lsfg-vk/conf.toml` (matching `exe = "RitzLauncher"`),
  sets `LSFG_PROCESS`, and supports an activation delay (starts at multiplier 1,
  bumps to the target after N seconds to avoid crash-on-start). Live-reloadable.
- **hypr-monctl** — *deferred seam* for per-game display vibrance via a Hyprland
  plugin (`hyprctl eval 'vibr_register{…}'`). Registered but inert until the plugin
  ships.

## Building

```
cargo build --release
```

Workspace layout:

- `crates/ritz-core` — pure, unit-tested logic: extension schema, `Requires`
  grammar, variable resolution, launch-command builder, Steam `%command%` parser,
  config storage, lsfg conf.toml transform.
- `crates/ritz-app` — the `ritz` binary: egui splash + settings GUI, process
  supervisor, runtime backends, hooks/scripts.

Linux only.
