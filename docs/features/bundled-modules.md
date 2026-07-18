# Bundled Modules — reference tour of the shipped extensions

Ritz ships a set of extension modules out of the box, defined as JSON manifests under
`resources/extensions/`. Each module contributes UI groups the user configures per-game,
which get rendered into env vars, command wrappers, launch args, or lifecycle hooks. See
[extension-system.md](extension-system.md) for how manifests are loaded and applied; this
page is the "what does each shipped module do" reference.

## At a glance

| Module | Manifest | Targets | Option categories |
| --- | --- | --- | --- |
| AMD | `resources/extensions/default/amd.json` | AMD RADV Vulkan driver | RADV_PERFTEST flags (NGGC, SAM), Mesa Anti-Lag layer, experimental user-mode submission queue |
| DXVK | `resources/extensions/default/dxvk.json` | DXVK (D3D8/9/10/11 → Vulkan) | FPS limit, HUD overlay, graphics pipeline library, frame latency, tearing/HDR |
| VKD3D | `resources/extensions/default/vkd3d.json` | VKD3D-Proton (D3D12 → Vulkan) | FPS limit, a dozen `VKD3D_CONFIG` flags (descriptor heap, ray tracing, PSO retention, etc.) plus a raw-flags passthrough, present mode |
| Proton | `resources/extensions/default/proton.json` | Proton compatibility layer | Sync backend (NTSync/FSYNC), renderer overrides (WineD3D, D3D8/10/11), display (Wayland, HDR, integer scaling), GPU (NVAPI, GPU-hiding), compatibility toggles, debug logging |
| Gamescope | `resources/extensions/default/gamescope.json` | Gamescope compositor | Enable/backend/scaler, output & internal resolution, refresh rate, sync & input flags, FSR upscaling — emits a command wrapper |
| Misc | `resources/extensions/default/misc.json` | Steam runtime / GameMode / desktop env | Clear LD_PRELOAD/VK_INSTANCE_LAYERS, force X11 SDL backend, keyboard layout, GameMode wrapper |
| PulseAudio | `resources/extensions/default/pulse.json` | PulseAudio / PipeWire-pulse | Client latency, output sink routing, `media.role=game` tagging |
| Scripts | `resources/extensions/default/scripts/scripts.json` | User shell commands | Pre-launch (blocking), post-spawn (background), post-exit (blocking) command hooks |
| Game Launch Args | `resources/extensions/built-in/custom-args/custom-args.json` | The game's own argv | Uncapped list of free-text launch arguments, appended verbatim after the game command |
| Custom Env | `resources/extensions/built-in/custom-env/custom-env.json` | Process environment (chain-wide) | Uncapped list of free-form `NAME=VALUE` env var pairs |
| Custom Game Env | `resources/extensions/built-in/custom-game-env/custom-game-env.json` | Process environment (game only) | Uncapped list of free-form `NAME=VALUE` env var pairs, game-scoped only (same shape as Custom Env, emitted into `GAME_ENV_VARS` instead of `ENV_VARS`) |
| Hypr-Monctl | `resources/extensions/built-in/hypr-monctl/hypr-monctl.json` | Hyprland monitor color pipeline | Per-game saturation, brightness, temperature applied while the game window is focused |
| LSFG-VK | `resources/extensions/built-in/lsfg-vk/lsfg-vk.json` | Lossless Scaling frame generation | Enable, multiplier (2x-8x), flow scale, performance mode, HDR mode, present-mode override, activation delay |

## Notes per module

- **AMD** — All options gate behind an `enabled` toggle (`Requires: "enabled"`) and build
  the `RADV_PERFTEST` env var incrementally via a `Builder` list; a separate `clear`
  toggle can reset it first before appending flags.
- **DXVK / VKD3D** — Both target Vulkan translation layers for different D3D versions and
  share a shape: an `ENV_VARS` block with `Builder` entries keyed off UI `Variable`s.
  VKD3D's `Config Flags` group is the largest single options surface in the bundle (11
  toggles plus a raw string passthrough that appends extra `VKD3D_CONFIG` flags verbatim).
- **Proton** — Purely a flat set of `PROTON_*` / `WINE_*` toggles, one env var each, no
  `Builder` composition needed since none of them combine into a single variable.
- **Gamescope** — The only module besides Misc/Scripts that emits a `WRAPPERS` entry
  instead of (or in addition to) `ENV_VARS`; its `CommandSyntax` (`gamescope {OPTIONS} --`)
  wraps the game command, with `Priority: 100` controlling wrapper ordering.
- **Misc** — A grab-bag of Steam-runtime env clears, SDL/XKB overrides, and a `gamemoderun`
  wrapper (`Priority: 200`) for feral GameMode.
- **Scripts** — Configures three lifecycle `Hooks` (`PreLaunch`, `PostSpawn`, `PostExit`)
  wired to `resources/extensions/default/scripts/pre.sh`, `post.sh`, `exit.sh`, with
  `PostSpawn` marked `Background: true` so it doesn't block game start. Each hook's UI
  field is a `multi_string` command entered by the user, not the shell script itself.
- **Game Launch Args / Custom Env / Custom Game Env** — "Escape hatch" modules, each a
  single `multi_string` UI field (`args`, `env`, `game_env` respectively) rather than a
  fixed bank of numbered slots. A `crates/ritz-core/src/builder.rs` backend pre-pass
  (`Backend: "custom-args"` / `"custom-env"` / `"custom-game-env"`) expands the list at
  launch-command build time: `custom-args` appends each non-empty line verbatim (no
  shell word-splitting) as one launch arg; `custom-env`/`custom-game-env` split each
  non-empty line on its *first* `=` into one `NAME=VALUE` env var, chain-wide for
  `custom-env` and game-only for `custom-game-env`. In the GUI, the two env modules'
  field renders as a two-column Name | Value row list
  (`crate::gui::GuiApp::render_env_pair_field`, name validated against
  `^[A-Za-z_][A-Za-z0-9_]*$`); `custom-args` renders as a single-column growing list
  (`crate::gui::GuiApp::render_multi_string_field`). *Why:* `multi_string` is a
  variable-length list UI primitive the schema already has for other modules (e.g.
  Scripts' hooks) — no cap, and no new manifest concept needed, replacing the earlier
  fixed 16-slot (`arg_1`..`arg_16` / `env_N_name`+`env_N_value`) numbered-field
  workaround from before `multi_string`-backed lists existed.
- **Hypr-Monctl / LSFG-VK** — These two declare a `Backend` field (`"hypr-monctl"`,
  `"lsfg-vk"`) instead of only `ENV_VARS`/`WRAPPERS`, meaning their options are consumed by
  a native Rust backend rather than the generic env/wrapper builder. See
  [runtime-backends.md](runtime-backends.md) for how those backends are implemented and
  wired to the manifest's `Variable` values. Hypr-Monctl also carries
  `"RequiresDesktop": "Hyprland"`, gating it out of the extension list entirely on other
  desktops (see the RequiresDesktop project memory / `AppContext::load` filter).

## Adding or extending options

New option fields for these modules are tracked in `MODULE-OPTIONS-BACKLOG.md` at the repo
root rather than duplicated here — check it before adding a new field to any manifest above
to avoid clashing with a planned addition.

## Related links

- [extension-system.md](extension-system.md) — how manifests are loaded, validated, and
  turned into env vars / wrappers / launch args.
- [runtime-backends.md](runtime-backends.md) — the Rust-side backends for Hypr-Monctl and
  LSFG-VK.
