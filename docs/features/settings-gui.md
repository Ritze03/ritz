# Settings GUI — the window that renders extensions and previews the launch command

The settings manager is the whole visible surface of ritz: a native egui window with a
navigation panel (games/profiles/general/global), a module tree, a dynamically-rendered
field editor for whichever module is selected, and a live launch-command preview.
Everything lives in `crates/ritz-app/src/gui.rs` (~3.4k lines), one `GuiApp` struct
implementing `eframe::App`.

## How it works

- **Entry points** — `crates/ritz-app/src/gui.rs:run_manager` opens the editor on the
  first saved game (or a scratch config); `crates/ritz-app/src/gui.rs:run` opens it
  focused on a specific game and, in *launch mode* (`launch_mode: true`, set when opened
  from the splash screen), shows Launch/Cancel actions in the title bar instead of just
  closing. Both build a `crates/ritz-app/src/gui.rs:GuiApp` via
  `crates/ritz-app/src/gui.rs:GuiApp::new` and hand it to `eframe::run_native`.
- **State** — `GuiApp` (`crates/ritz-app/src/gui.rs:GuiApp`, struct at line ~101) holds:
  loaded extension specs (`all_specs` = every extension, `cur_specs` = only those
  applicable to the current game per `Extension::applies_to`), the current
  `NavSel` (which panel is being edited), the loaded `GameConfig`/`Preset`/global
  `Preset`, in-progress text-edit buffers, and a `Detect` handle for the async
  window-class detector. It is built once per window open and mutated in place every
  frame — there's no separate view-model, the render functions read and write `self`
  directly.
- **Frame loop** — `crates/ritz-app/src/gui.rs:GuiApp::ui` (the `eframe::App::update`
  body) runs once per frame: handles Ctrl+R hot-reload, resolves the config twice (see
  below), lays out four panels (title bar, nav panel, module-tree panel, launch-preview
  footer, central field editor), then persists if anything changed. It doesn't diff
  state — `changed` is a `bool` threaded through every render call, and `true` at the
  end triggers `crate::gui::GuiApp::persist` unconditionally. *Why:* egui is
  immediate-mode with no widget change events outside the frame, so "did anything
  change" has to be collected explicitly from every widget's `.changed()`/`.clicked()`.
- **Two resolutions per frame** — `crate::gui::GuiApp::resolve_for_editing` resolves
  against whichever scope `NavSel` currently points at (so field values in the editor
  reflect that scope); `crate::gui::GuiApp::resolve_for_game` always resolves against the
  *actual* game (so the launch-command preview reflects what would really launch, even
  while editing an unrelated profile or global settings). Editing a profile that's also
  the game's *active* profile makes `resolve_for_game` read the live in-memory
  `editing_preset_buf` instead of the on-disk copy, so the preview reflects unsaved
  edits. See [scoped-config.md](scoped-config.md) for what `Provenance`/`Resolution`
  mean and how the four scopes (default/global/profile/game) stack.

### Layout: four panels per frame

```
┌───────────── titlebar (render_title_bar) ─────────────────┐
├──────────┬───────────┬──────────────────────────────────┤
│   nav    │  ext_list │        central panel               │
│  panel   │  (module  │   (render_field per visible field, │
│(render_  │   tree,   │    or the custom-env/custom-args   │
│nav_panel)│ render_   │    backend, or General Settings)   │
│          │ ext_tree) │                                    │
├──────────┴───────────┴──────────────────────────────────┤
│         preview footer (launch command / About box)       │
└─────────────────────────────────────────────────────────┘
```

Built in `crate::gui::GuiApp::ui`: `egui::SidePanel::left("nav")` (280px, always shown),
`egui::SidePanel::left("ext_list")` (280px, hidden while `NavSel::GeneralSettings` is
selected — General Settings has no modules to browse), a bottom
`egui::TopBottomPanel::bottom("preview")`, and an `egui::CentralPanel` for the field
editor. *Why hide the module list for General Settings:* that screen edits app-wide
preferences, not extension variables, so a module tree would have nothing to select.

## Navigation panel

`crate::gui::GuiApp::render_nav_panel` splits into a header ("Profiles / Games"), a
scrollable tree (`crate::gui::GuiApp::render_nav_tree`), and a fixed-height (198px)
bottom settings band (`crate::gui::GuiApp::render_nav_settings`) that shows
context-specific controls for whatever's selected.

- **`NavSel`** (`crates/ritz-app/src/gui.rs:NavSel`) is the four things the tree can
  select: `GeneralSettings`, `GlobalSettings`, `Profile(name)`, `Game(appid)`. Switching
  it clears `text_buffers`/`multi_edit` (in-progress edits) and reloads whichever config
  the new selection needs.
- **Tree rows** — General Settings and Global Settings are always-visible top rows;
  below them, two `egui::CollapsingHeader`s list **Profiles** (pinned ones first, by
  slot 1–10, via `crate::gui::GuiApp::assign_pin`) and **Games**. `+ Add profile` /
  `+ Add game` switch the bottom band into a create-name form
  (`GuiApp::creating_profile` / `GuiApp::creating_game`).
- **Inheritance indicators in the tree** — each profile/game row can carry an
  inheritance icon or number showing how deep in the profile parent chain it sits,
  driven by `GeneralConfig::inheritance_display`
  (`crates/ritz-app/src/gui.rs:profile_depth_color` computes the per-depth color shade).
  See [scoped-config.md](scoped-config.md#profile-chain-depth-indicators) for the
  chain-depth model this visualizes.
- **Bottom band per selection** (`crate::gui::GuiApp::render_nav_settings`):
  - *Profile* — rename (Enter commits, `crate::gui::GuiApp::rename_preset`), pin
    toggle/slot picker, **Parent** combo (any other profile; a selection that would
    introduce a cycle is shown disabled and red via
    `crate::context::chain_would_have_cycle` rather than rejected server-side), Delete
    and Duplicate buttons.
  - *Game* — editable AppID (Enter renames the on-disk file via
    `crate::gui::GuiApp::rename_game_appid`), editable Name, a **Profile** combo
    assigning which preset the game uses, Delete button.
  - *General/Global Settings* — empty; their controls live in the central panel instead.

## Module tree (`ext_list` panel)

`crate::gui::GuiApp::render_ext_tree` renders `cur_specs` (only extensions applicable to
the current game) either **grouped by Author** (`group_by_author: true`, the default —
a flat `CollapsingHeader` per author) or **grouped by folder** (mirrors the nested
`extensions/` directory layout on disk via `crates/ritz-app/src/gui.rs:TreeNode`, a
simple trie built from each spec's `rel_dir`). *Why folder mode collapses single-leaf
folders* (`crates/ritz-app/src/gui.rs:render_node`): a folder that only contains its own
extension's manifest (a "folder extension", `is_folder_ext`) would otherwise show a
redundant header wrapping a single row.

Extensions whose manifest declares a `backend` (custom-env, custom-args, hypr-monctl,
lsfg-vk — see [bundled-modules.md](bundled-modules.md)) are pulled out of both grouping
modes and shown under a trailing **Built-In** header.

Each row (`crates/ritz-app/src/gui.rs:leaf`) can carry up to a few small icons showing
where the module's value comes from — global inheritance, profile-chain inheritance
(color/number/arrows-only, per `InheritanceDisplayMode`), and a pencil for "edited at
this scope" — computed per extension per frame in `render_ext_tree`'s `icon_lists`
closure. A field only counts as "configured" if its variable is still declared by the
loaded schema *and* its `Requires` gate currently evaluates visible
(`field_visible`) — a stored value behind a closed gate, or for a since-removed
variable, doesn't light up the icon.

## Module editor (central panel)

For the selected extension, `GuiApp::ui`'s central-panel closure renders a header
(name, version, author, description, and an "Editing Game/Profile/Global: X" label
tinted by `GuiApp::editing_scope_color`), then dispatches on the spec's `backend`:

- **`backend: "custom-env"`** → `crate::gui::GuiApp::render_custom_env_backend`, two
  16-slot NAME=value sections (`env_N_name`/`env_N_value`, then the game-scoped
  `game_env_N_*` set) via `crate::gui::GuiApp::render_custom_env_section`.
- **`backend: "custom-args"`** → `crate::gui::GuiApp::render_custom_args_backend`, one
  16-slot `arg_N` list, no NAME=value split.
- **Everything else** → iterate `spec.ui` (an `IndexMap<section, Vec<UiField>>`,
  order-preserving so section/field order matches the manifest), skip sections with no
  currently-visible field, and call `crate::gui::GuiApp::render_field` per visible field.

*Why custom-env/custom-args get dedicated render paths instead of going through the
generic `UiField` loop:* their manifests declare a fixed bank of numbered slots
(`arg_1..arg_16`, `env_N_name`/`env_N_value`, doubled for the game-scoped set) because
the schema has no variable-length/add-remove-row UI primitive — see
[bundled-modules.md](bundled-modules.md#notes-per-module). The custom backends add
that missing add/remove/commit behavior in Rust instead: each slot's `NAME=value` text
box only commits to config on `lost_focus()`, and a fresh `+ Add` slot lives only in
`text_buffers` (a "pending" slot, not yet in `used`) until it's typed into and loses
focus, so an empty just-added row doesn't get written to disk.

### Field rendering — `render_field`

`crate::gui::GuiApp::render_field` draws one row per field inside a scope-tinted
"card" (`Frame` with 8px rounding, a 3px left color bar matching the field's resolved
scope color from [scoped-config.md](scoped-config.md#provenance-display)):

1. Compute `Tri` state (`crates/ritz-app/src/gui.rs:tri_state`: `Unset` when
   provenance isn't the scope being edited, else `Enabled`/`Disabled`) and the row's
   scope color — normally the resolved `Provenance`'s color, except a value set *at the
   currently-edited layer* always resolves as `Provenance::Game` internally (the layer
   is loaded as a "fake game", see `preset_as_fake_game`), so `render_field` re-maps
   that case to the real editing-scope color (Game blue / Profile green / Global red)
   via `editing_scope_color`.
2. A `MultiString` field is diverted immediately to
   `crate::gui::GuiApp::render_multi_string_field` — it renders as its own growing
   list-of-rows card rather than the shared one-row layout, since its value is a JSON
   array, not a scalar.
3. Otherwise: `scope_checkbox` draws the tri-state toggle (left-click cycles
   enabled/disabled, right-click resets to inherit — `Tri::Unset`), reserving a fixed
   260px label column so every field's value editor lines up at the same x regardless
   of label length. Non-`Toggle` fields also draw a value editor to their right, even
   when not editable at the current scope (greyed out, showing the *effective*
   inherited value read-only) — so the user always sees what would actually apply.

### Value editors — `render_value_editor`

`crate::gui::GuiApp::render_value_editor` switches on `FieldType`:

| `FieldType` | Widget | Notes |
| --- | --- | --- |
| `Selection` | `egui::ComboBox` | Manually placed in an explicit `Rect` rather than laid out normally — *Why:* egui's `ComboBox` button draws a few px taller/lower than the space it reserves, so normal layout mis-centers it in the fixed-height row; `render_value_editor` computes the centered rect itself. |
| `Integer` / `Float` | `crates/ritz-app/src/gui.rs:scope_slider` | Custom-painted track + `DragValue` spinner, not `egui::Slider` — gives the scope-colored fill/handle and a fixed-width spinner that doesn't grow when the user starts typing a longer number. |
| `String` | `egui::TextEdit::singleline` | The `window_class` field additionally gets a **Detect** button (see below). Buffered per-field in `GuiApp::text_buffers`, re-synced from the resolved value only while the widget isn't focused — *Why:* without that guard, switching profiles mid-frame (nav action applied before `resolve_for_editing` reruns) would show a stale value that persists until the next click, per the inline comment at `crates/ritz-app/src/gui.rs:1193`. |
| `Toggle` | (none here) | Rendered entirely by the checkbox in `render_field`; no separate value widget. |
| `MultiString` | (none here) | Rendered entirely by `render_multi_string_field`. |

**Window-class detection**: clicking **Detect** on the `window_class` field
(`crate::gui::GuiApp::start_detect`) spawns a background thread that sleeps 3 seconds
then calls `crates/ritz-app/src/gui.rs:detect_active_window_class` (shells out to
`hyprctl activewindow -j`, preferring `initialClass` over `class`). The GUI thread polls
it every frame via `crate::gui::GuiApp::poll_detect`, requesting a repaint every 200ms
while waiting so the "Detecting… Ns" countdown updates without user input. *Why a
thread instead of blocking the UI:* the sleep gives the user time to alt-tab to the
game window before its class is queried; blocking `update()` for 3s would freeze the
whole app.

## Launch-command preview footer

The bottom `preview` panel (in `GuiApp::ui`) is either a fixed 198px band or, if
`GeneralConfig::dynamic_preview` is on, sized to its content. It shows the **About**
box (repo link + credits, `crate::gui::GuiApp::render_about`) when `NavSel` is
`GeneralSettings` — there's no launch command to preview there — otherwise it calls
`crate::context::assemble_launch` with whichever resolution matches the panel: when
editing a **Profile**, the edit-scope resolution (so the preview reflects the profile
being edited even if it's not the active one for any game); otherwise the game
resolution. The assembled command is rendered with `crates/ritz-app/src/gui.rs:command_job`,
which highlights every `%command%` token in the accent color. See
[launch-command-assembly.md](launch-command-assembly.md) for what `assemble_launch`
does with the resolved fields.

## General Settings panel

Selecting `NavSel::GeneralSettings` swaps the whole central+preview area:
`crate::gui::GuiApp::render_general_settings_panel` lists app-wide preferences (splash
timeout, default profile, editor-close action, monospace UI font, touch mode, full-width
layout, dynamic preview size, inheritance display mode, show-field-inheritance) via two
helper row layouts, `crates/ritz-app/src/gui.rs:settings_value_row` and
`crates/ritz-app/src/gui.rs:settings_toggle_row`, plus two destructive maintenance
actions (Clean Up Configs → `crate::gui::GuiApp::config_cleanup`; Re-Export Modules and
Plugins → `crate::resources::reexport`) that route through the shared confirmation
dialog rather than firing immediately.

## Confirmation dialog

Any destructive action (delete game/profile, clear settings, re-export resources,
config cleanup) is staged into `GuiApp::confirm: Option<ConfirmAction>`
(`crates/ritz-app/src/gui.rs:ConfirmAction`) instead of executing inline, and carried
out by `crate::gui::GuiApp::render_confirm_dialog` only after the user clicks Confirm in
a modal `egui::Window` (a full-screen `egui::Area` backdrop swallows clicks so only the
dialog is interactive). *Why route everything through one enum instead of five ad-hoc
confirm flags:* one dialog renderer handles the modal chrome/backdrop once; each variant
just supplies its title/message and its own commit logic.

## Using it

1. Open from the splash (launch mode, Launch/Cancel bar visible) or standalone via
   `run_manager` (Continue always wins on close).
2. Pick a target in the **left nav panel** — a game, a profile, Global Settings, or
   General Settings.
3. Pick a module in the **module tree** (second column) — grouped by author or by
   folder, toggle via **Group by Author** / **Show Inheritance** in the tree's footer.
4. Toggle/edit fields in the **central panel**; left-click a field's checkbox to
   enable/disable it at the current scope, right-click to reset to inherited. The
   colored left bar and checkbox tint show which scope the effective value is coming
   from.
5. Watch the **bottom preview** update live with the resulting launch command.
6. Use **Ctrl+R** to hot-reload both extensions and configs from disk without
   restarting the window (`crate::gui::GuiApp::reload_extensions` +
   `crate::gui::GuiApp::reload_configs`).

## Related links

- [scoped-config.md](scoped-config.md) — the four-layer resolution model
  (`Provenance`, tri-state, profile-chain depth) that `render_field` visualizes.
- [extension-system.md](extension-system.md) — how a manifest's `UI`/`Requires`/field
  types are parsed, which this page's field renderer walks.
- [bundled-modules.md](bundled-modules.md) — what the custom-env/custom-args/backend
  modules this GUI special-cases actually do.
- [launch-command-assembly.md](launch-command-assembly.md) — what the preview footer's
  `assemble_launch` call produces.
- [../ui/STYLING-GUIDE.md](../ui/STYLING-GUIDE.md) — the theme/color-role rules
  `render_field` and friends follow (`theme::scope_color`, `theme::ACCENT`, button
  helpers).
