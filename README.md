<div align="center">

<img src="resources/assets/logo/logo_128.png" width="96" alt="Ritz logo">

# Ritz

**A Linux Steam launch wrapper.** Set one launch option and Ritz transparently injects
environment variables, wrappers (gamescope / gamemode), game-only env vars, and launch
arguments around the real game command — so you stop hand-managing `RADV_PERFTEST`,
`gamescope` flags, `VKD3D_CONFIG`, frame-gen quirks, and the rest.

Everything is configured from a GUI, layered across **global → profile → game** scopes,
and driven by simple JSON modules you can edit or extend.

📖 **[Full documentation & guides »](https://ritze03.github.io/ritz/)**

</div>

---

## Quickstart

```sh
# 1. Build (Linux only)
cargo build --release

# 2. Put the binary on your PATH
install -Dm755 target/release/ritz ~/.local/bin/ritz   # ensure ~/.local/bin is in $PATH

# 3. In Steam: right-click the game → Properties → Launch Options:
ritz %command%
```

Launch the game. The first time Ritz sees it, a short wizard asks for a **name** and an
optional **profile**, then the game starts. From then on you get a splash screen on every
launch:

| Key | Action |
|-----|--------|
| **W** / Enter | Launch now |
| **E** | Edit this game's config (opens the settings window) |
| **Q** / Esc | Cancel the launch |
| *(timeout)* | Launches automatically |

Run **`ritz`** with no arguments anytime to open the settings GUI.

> **Non-Steam games** (or anything outside Steam): use
> `RITZ_APPID=<your-id> ritz %command%` so Ritz can give it a stable identity. Running
> `ritz %command%` on an unknown game walks you through setting this up.

## Usage

- **Settings GUI** (`ritz`): a left navigator lists **General Settings**, **Global
  Settings**, your **Profiles**, and your **Games**. Selecting any of them shows that
  scope's modules in the center; the bottom bar previews the exact launch command.
- **Scopes & colors.** Every option resolves through four layers —
  **extension default → 🔴 global → 🟢 profile → 🔵 game**. A value's color shows where it
  comes from; left-click a checkbox to set it at the current scope, right-click to reset it
  to inherited. Higher scopes win.
- **Profiles** are reusable bundles of settings (e.g. *Competitive FPS*, *Controller
  Couch*). Assign one to a game, or set a default profile for all games. Pin up to 10
  profiles for one-key selection in the new-game wizard.
- **Dry run.** `ritz --print %command%` prints the fully assembled command without
  launching — handy for checking what a config actually does.

## Modules

Functionality ships as JSON **modules** (extensions). The bundled set:

| Module | What it does |
|--------|--------------|
| **Gamescope** | The `gamescope` micro-compositor wrapper (resolution, scaling, FSR, adaptive sync, …). |
| **Proton** | Proton env toggles: sync backends (NTSync/FSYNC/ESYNC), renderer (WineD3D, D3D8/10/11), Wayland, HDR, NVIDIA, compatibility. |
| **AMD** | RADV driver tuning (`RADV_PERFTEST`: aco/gpl/nggc/sam/rt), Mesa anti-lag, experimental queues. |
| **DXVK** | D3D→Vulkan: FPS limit, HUD, GPL/async pipelines, frame latency, tear-free, HDR. |
| **VKD3D** | D3D12→Vulkan: FPS limit, `VKD3D_CONFIG` flags (descriptor heap, DXR, …), present mode. |
| **Misc** | Common env/compat: clear `LD_PRELOAD`/`VK_INSTANCE_LAYERS`, force X11 SDL, keyboard layout, GameMode (`gamemoderun`). |
| **Game Launch Args** | Free-form arguments appended after the game command. |
| **Custom Env** | Free-form environment variables (chain-wide and game-only). |
| **Scripts** | Run scripts at lifecycle hooks (pre-launch, post-spawn, on-ready, post-exit). |
| **LSFG-VK** | Lossless Scaling frame generation backend (writes `conf.toml`, activation delay). |
| **Hypr-Monctl** | Per-game display vibrance via a Hyprland plugin (only loads on Hyprland). |

Drop your own `.json` modules into `~/.config/ritz/extensions/` — see the
**[extension reference](https://ritze03.github.io/ritz/extensions.html)**.

## How it works

A launch command is assembled from five blocks:

```
[ENV_VARS]  [WRAPPERS]  [GAME_ENV_VARS]  %command%  [GAME_LAUNCH_ARGS]
```

- **`ENV_VARS`** — applied to the whole chain (the child process environment).
- **`WRAPPERS`** — gamescope, gamemoderun, … prepended left of `%command%`, ordered by
  priority (`gamescope … -- %command%`).
- **`GAME_ENV_VARS`** — applied only to the game, via an `env` shim after the wrapper `--`.
- **`%command%`** — the entire Steam command, preserved verbatim (the `SteamLinuxRuntime`
  container is never stripped).
- **`GAME_LAUNCH_ARGS`** — appended after the game command.

Ritz stays the process Steam tracks for the whole game lifetime: it forwards the exit code,
fires lifecycle hooks, waits for the real game process to appear, live-reloads backend
settings on config change, and cleans up on exit.

## Configuration

Stored under `~/.config/ritz/` (override with `$RITZ_CONFIG_DIR`):

```
general.json          # app settings (splash timeout, default profile, UI prefs)
global.json           # the 🔴 global scope (applies to every game)
profiles/<name>.json  # 🟢 named profiles (reusable setting bundles)
games/<appid>.json    # 🔵 per-game overrides (named after the SteamAppId)
extensions/           # modules (bundled set exported here on first run, + your drop-ins)
plugins/              # bundled plugins (e.g. hypr-monctl.so)
```

Bundled resources (fonts, modules, plugins) are embedded in the binary. On startup Ritz
exports any **missing** files into the config directory — it never overwrites ones you've
edited. Use **Re-Export Modules** in General Settings to restore the shipped versions, and
**Config Cleanup** to drop values for variables that no longer exist after a module update.

Only explicitly-set values are stored, so config files stay minimal and show clear
provenance per value.

## Building

```sh
cargo build --release   # Linux only
cargo test              # unit tests (ritz-core)
```

Workspace layout:

- **`crates/ritz-core`** — pure, unit-tested logic: module schema, the `Requires` grammar,
  variable resolution, the launch-command builder, the Steam `%command%` parser, config
  storage, and the lsfg `conf.toml` transform.
- **`crates/ritz-app`** — the `ritz` binary: the egui splash + settings GUI, the process
  supervisor, runtime backends, and hooks/scripts.

## License

Bundled fonts: Geist / Geist Mono (OFL) and a patched Nerd Font — see
`resources/assets/`. Project license: see repository.
