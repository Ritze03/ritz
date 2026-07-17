# Scoped Config — four-layer inheritance with provenance

Every extension variable is resolved through four stacked scopes — extension default, global, profile, game — so a value set once at a broad scope flows down to everything below it, while a narrower scope can override or explicitly disable it. The GUI then shows *where* each effective value came from, colour-coded by scope and by how deep in the profile chain it was inherited.

*Why layered scopes:* you set a knob once at the level it belongs (a machine-wide default in **global**, a "competitive FPS" tuning in a **profile**, a one-off fix per **game**) instead of re-entering it on every game. Each narrower layer only stores what it changes.

## Layers, lowest → highest priority

The precedence order is fixed in `crates/ritz-core/src/resolve.rs:Provenance` and applied in `crates/ritz-core/src/resolve.rs:resolve_one` — the first layer (searched highest-first) that holds a stored value wins:

| Priority | Scope | Stored in | `Provenance` |
| --- | --- | --- | --- |
| 4 (highest) | **Game** | `games/<appid>.json` | `Provenance::Game` |
| 3 | **Profile** | `profiles/<name>.json` (a `Preset`) | `Provenance::Preset` |
| 2 | **Global** | `global.json` (a `Preset`) | `Provenance::Global` |
| 1 (lowest) | **Extension default** | the field's `Default` in the extension schema | `Provenance::Default` |

`resolve_one` walks `game → preset → global → default`, taking the first `Some` value; if none has a stored value it falls back to the field's `Default`. `crates/ritz-core/src/resolve.rs:resolve` runs this for every field of every extension and returns a `Resolution` the launch builder and the GUI both read.

*Why "profile" and "preset" are the same thing:* the on-disk type is `crates/ritz-core/src/config.rs:Preset` and the code says `preset`, but the UI, the config directory (`profiles/`), and this doc call it **profile**. Same object — treat the words as synonyms.

### The profile layer is itself a chain

A profile may name a `Parent` (`crates/ritz-core/src/config.rs:Preset.parent`). Before resolution, the parent chain is flattened with the child's own values layered on top (`collect_parent_chain` + `merge_modules`, wired in `crates/ritz-app/src/gui.rs:resolve_for_editing`), so a grandparent value is visible unless a nearer profile overrides it. This is why provenance alone isn't enough for profiles — the GUI also tracks *depth* within that chain (see below).

## How a value is stored, overridden, and disabled

Storage convention (documented at the top of `crates/ritz-core/src/config.rs`), applied per variable in the nested layout `Modules → <Author> → <ExtName> → <var> = value`:

- **present, non-null** = an explicit override (the field is on, with that value)
- **`null`** = explicitly *disabled* — turns off a value that a lower layer had turned on
- **absent** = inherit from the layer below

*Why the null-vs-absent distinction:* deleting a key means "inherit", but sometimes a game needs to force a knob **off** that global or a profile turned on. A stored `null` is that override — `crates/ritz-core/src/resolve.rs:resolve_one` reads it as `Provenance::Game` (or whichever layer holds it) but `enabled = false`, so it beats the inherited-on value without erasing the chain (test `null_disables_inherited`).

Writes and resets go through the scope-appropriate methods, dispatched by the active `NavSel` in `crates/ritz-app/src/gui.rs:set_raw_var` / `crates/ritz-app/src/gui.rs:unset_raw_var`:

- Override a field: `Preset::set_value` (global + profile) or `crates/ritz-core/src/config.rs:GameConfig::set_value`.
- Reset-to-inherit: `crates/ritz-core/src/config.rs:GameConfig::unset_value`, which removes the key **and prunes now-empty author/ext maps** (test `unset_prunes_empty_maps`) so a reset leaves no stale scaffolding behind.

*Why store only the delta:* because absent = inherit, a layer's file contains exactly the variables that layer changes and nothing else — the file stays a readable diff against what it inherits, and a later change to a lower layer automatically shows through.

## Provenance display

The resolved `Provenance` drives the colour a field (and its tree row) is tinted with, via `crates/ritz-app/src/theme.rs:scope_color`:

| Provenance | Colour token | Hex |
| --- | --- | --- |
| `Default` | `COL_DEFAULT` (neutral gray) | `#464D57` |
| `Global` | `COL_GLOBAL` | `#E15554` |
| `Preset` (profile) | `COL_PROFILE` | `#6CC551` |
| `Game` | `COL_GAME` | `#4D9DE0` |

*Why gray for Default and not the game blue:* a field nobody has touched should read as "untouched", not as an override — `scope_color` deliberately maps `Provenance::Default` to neutral gray so real overrides stand out.

A value set *at the layer currently being edited* renders in that layer's editing colour (`crates/ritz-app/src/gui.rs:editing_scope_color`), and the row carries an icon — `ICON_EDIT` (pencil) when the value is set here, `ICON_INHERIT` (arrow) when it comes from a lower scope.

### Profile-chain depth indicators

When the effective value is inherited from a profile, its depth in the parent chain is shown too — controlled by `GeneralConfig::show_field_inheritance` and rendered per the `crates/ritz-core/src/config.rs:InheritanceDisplayMode`:

- **`Color`** — `crates/ritz-app/src/gui.rs:profile_depth_color` shifts the profile hue darker one step per level up the chain, so a direct-parent value and a grandparent value are visibly distinct.
- **`Numbers`** — a numeric badge (1, 2, …) for the depth, in `COL_PROFILE`.
- **`ArrowsOnly`** — plain inheritance arrows, no depth encoding.

The depth is computed from the profile's `Parent` links in `render_field` (`crates/ritz-app/src/gui.rs`, `preset_depth` around line 657) and in the nav tree (`profile_parents`, around line 2046: direct parent = 1, grandparent = 2, …).

## Export on startup: missing-only, never overwrite

Bundled extensions and plugins are shipped into the config dir on startup, but user files are sacrosanct:

- `crates/ritz-app/src/resources.rs:bootstrap` calls `export(paths, false)` — it **creates only files that don't already exist** and leaves any existing file untouched.
- `crates/ritz-app/src/resources.rs:reexport` (`export(paths, true)`) overwrites, and is reachable **only** through the explicit, confirmed "Re-Export Modules and Plugins" action (`ConfirmAction::ReExportResources` in `crates/ritz-app/src/gui.rs`).

*Why export missing-only:* the launcher must ship working default modules on first run, but a user who has edited or added their own config must never have it silently clobbered on the next startup — so bootstrap is strictly additive.

*Why never write a bundled file in place:* even `reexport` writes a temp file and renames it over the target rather than truncating the existing inode. A bundled `.so` (e.g. `hypr-monctl.so`) may be `mmap`'d live by Hyprland; rewriting its inode in place would corrupt that mapping and crash it. The rename swaps the directory entry to a fresh inode and leaves the mapped one intact (`export_dir` in `crates/ritz-app/src/resources.rs`).

## Related links

- [Extension System](extension-system.md) — how extensions declare fields, defaults, and `global:` variables that this model resolves.
- [Settings GUI](settings-gui.md) — the inheritance-display settings (mode, show-field-inheritance) and the module/nav tree that render provenance.
- [Architecture Overview](../architecture/overview.md) — where config resolution sits in the launch pipeline.
