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
tinted by `GuiApp::editing_scope_color`), then iterates `spec.ui` (an
`IndexMap<section, Vec<UiField>>`, order-preserving so section/field order matches the
manifest), skips sections with no currently-visible field, and calls
`crate::gui::GuiApp::render_field` per visible field — including for the `custom-env`,
`custom-game-env`, and `custom-args` backends, which are plain manifests with a single
`multi_string` field (`env`, `game_env`, `args` respectively; see
[bundled-modules.md](bundled-modules.md#notes-per-module)) rather than a special-cased
dispatch. `render_field` special-cases `FieldType::MultiString`, and within that
special-cases the two env backends' field once more for its two-column widget:

- **`custom-env` / `custom-game-env`'s `env`/`game_env` field** →
  `crate::gui::GuiApp::render_env_pair_field`, a growing list of **Name | Value** row
  pairs. Each stored entry (a `"NAME=VALUE"` string) is split on its first `=`
  (`crate::gui::parse_env_row`) into the two boxes and reassembled as `"{name}={value}"`
  on write. The Name box is live-validated against the POSIX env-var charset
  (`crate::gui::is_valid_env_name`, `^[A-Za-z_][A-Za-z0-9_]*$`): an invalid name gets a
  red outline, and a row with an empty/invalid name is dropped when the list is
  persisted, so a bad pair can never reach a stored env var. The Value box is
  unrestricted (may itself contain `=`).
- **`custom-args`'s `args` field** (and every other `multi_string` field) →
  `crate::gui::GuiApp::render_multi_string_field`, a single-column growing list of text
  rows (one literal argument per row, no NAME=value split), each with a delete icon
  plus a `+ Add` button that appends an empty row.
- **Everything else** → the normal one-row-per-field layout described below.

Both list widgets store their working copy in `entries`/`multi_edit`, persist the
non-empty, trimmed entries as a JSON array at the current scope on any change, and drop
the field entirely when the list empties out.

At launch-command build time, `crates/ritz-core/src/builder.rs`'s backend pre-pass
expands each of these single `multi_string` values (`entries.join("\n")`) into N
outputs: `custom-env` splits each non-empty line on its first `=` into one chain-wide
`ENV_VARS` entry, `custom-game-env` does the same into `GAME_ENV_VARS`, and
`custom-args` appends each non-empty line verbatim (no shell word-splitting) as one
`GAME_LAUNCH_ARGS` entry.

*Why list-backed replaced the old fixed numbered-slot renderers* (deleted
`render_custom_env_backend`/`render_custom_args_backend`, `MAX_SLOTS`): those slots
existed only because the schema had no variable-length list UI primitive, capping
free-form args/env at 16. `multi_string` is that primitive now — no cap, and no new
manifest concept was needed: the backend pre-pass just expands one `multi_string` list
into N vars/args at build time instead of the GUI faking the list with numbered fields.

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

### Manifest editor — `render_module_editor` (`NavSel::ModuleEditor`)

Clicking **✎ Edit** on a module (or **Ctrl+E** on the selected module) routes through
`GuiApp::open_module_editor`, which records the current view in `GuiApp::editor_return`
and switches `nav_sel` to `NavSel::ModuleEditor(id)`, whose central panel is a full editor
for the module's *manifest* (not its config values).

- **Entering / leaving** — `GuiApp::open_module_editor(id)` remembers the prior `nav_sel`
  in `editor_return` (unless already in an editor) so Close can restore it;
  `GuiApp::enter_editor_for_selection` (Ctrl+E) opens the editor for
  `cur_specs[selected_ext]`, a no-op in the editor or on General Settings.
  If the selection moves off the editor by *any* route while a draft is still open
  (a left-nav click, Ctrl+E elsewhere, …), an early guard in `GuiApp::ui` (after keybind
  handling, before anything reads the draft) drops `module_draft` as an implicit
  Close/discard, so a dirty draft can never keep the config-autosave interlock engaged and
  strand later config writes — the frame-end `flush_config_writes_if_clean` then releases
  the interlock. `GuiApp::exit_module_editor` drops `module_draft` and restores
  `editor_return` via the pure `editor_exit_target` (falls back to the ambient `Game(appid)` when the stored view
  is missing or itself an editor, so no dangling editor selection remains). The header's
  always-present **✕ Close** button (`TopAction::Close`) exits even with nothing to save
  (dropping in-memory edits); **Save** and **Discard** also exit on success so the user
  lands back on the real module.
- **Keybinds** (handled at the top of `GuiApp::ui`, before panels render) — **Ctrl+E**
  enters the editor for the selected module; **Ctrl+S** triggers Save when the editor is
  open and `ModuleDraft::save_enabled` holds (same gate as the button), a no-op otherwise;
  **Ctrl+R** still hot-reloads. The `command` modifier means text editing is never
  swallowed.
- **Focus stability (no reflow steal)** — the dirty/identity/validation status lines that
  appear on the first keystroke must not knock the focused `TextEdit` out of focus. Two
  mechanisms guarantee this: (1) the "unsaved changes / All changes saved" line is
  **always rendered** (greyed when clean) so the clean→dirty transition never inserts a
  line above the fields; (2) the body `ScrollArea` carries an explicit `id_salt`
  ("module_editor_body") and **every** editor `TextEdit` a stable `.id_salt` keyed on its
  structural path (section/field/block index + role), so a focused box keeps the same egui
  id — and thus focus — even if a banner above it appears and shifts its screen position.

- **Draft state** — `GuiApp::module_draft: Option<ModuleDraft>`, (re)synced each frame by
  `GuiApp::ensure_draft`. A `ModuleDraft` holds the module `id`, its `manifest` path, an
  `editable` flag, the `Extension` (minus UI sections), the UI `sections` as an ordered
  `Vec<(name, fields)>` (folded back via `ModuleDraft::snapshot`), a `baseline` JSON value
  (the on-disk manifest) for the `dirty()` check, a `baseline_vars` set, a `name_error`
  (on-disk-identity collision, feeds the Save gate), and a `PendingIdentity` (staged
  Author/Name/Variable edits) + its `identity_error`. *Why sections live in a `Vec`, not the
  `IndexMap`:* reorder/rename/remove become plain `Vec` ops with no mutable-key problem.
- **Editability** — a module is editable only when its manifest is *not* one of the
  bundled sets (`GuiApp::module_editability` checks the `default/` / `built-in/` rel-dir
  roots; those are bootstrapped into the user config dir but stay inspect-only until
  forked). Bundled modules render the same widgets **disabled**, Save shows a
  "Fork to edit" tooltip.
- **Staged identity edits vs Save** (Phase 3 stage 2b) — for an editable module, Author,
  Name and every *existing* field's `Variable` are editable, but their edits go into a
  separate `PendingIdentity` (`identity.author` / `identity.name` /
  `identity.var_edits[current-name]`), **never** `ext.meta` or `field.variable`. Because
  `ModuleDraft::snapshot` reads only `ext`/`sections`, identity edits never mark the draft
  `dirty()`, never enable Save, and never hold the config-autosave interlock — they commit
  *only* through the explicit **Rename** action. A *newly-added* field's `Variable` is still
  edited directly (no config to orphan). Version stays fixed. `ModuleDraft::name_error`
  (on-disk identity, `GuiApp::refresh_draft_name_error`) feeds the Save gate and stays
  `None` for an existing module; the *pending* identity's validity is a separate
  `identity_error` (`GuiApp::refresh_identity_state` → `ModuleDraft::compute_identity_error`).
- **Rename / identity migration** (Phase 3 stage 2b, `GuiApp::perform_rename`) — a **Rename**
  header button, enabled only when `has_pending_identity() && identity_error.is_none()`.
  `identity_error` rejects an empty/colliding (Author, Name) (`name_collides`, Version-blind,
  self excluded by path) and any bad var rename via the pure `validate_var_renames`: an
  empty target, a **chained** rename (a new name equal to another live variable — would
  strand config, do them one at a time), or two vars renamed to the same name. On commit,
  in **exactly** this order: (1) compute `from` = on-disk `(Author,Name)`, `to` = new pair,
  `var_rename` = changed existing-field vars; (2) **scope sweep FIRST** —
  `config::migrate_renamed_module` moves stored config across every scope; on error, abort
  **without** touching the manifest; (3) build the new manifest from `snapshot()` — apply
  the new Author/Name and rewrite in-module references via `schema::apply_var_renames`
  (each `{old}` interpolation token → `{new}`, each exact `old` identifier in a `Requires`
  → `new`, plus each field's own `Variable`; exact-token match so `old_thing` / `global:old`
  survive); (4) write the manifest **LAST, in place** (file never renamed; `id()` comes from
  JSON meta) via `config::write_atomic`. *Why this order:* the sweep is idempotent and the
  manifest is written last, so a crash between (2) and (4) leaves the manifest on the OLD
  identity — re-pressing Rename re-runs cleanly (already-moved scopes no-op). No WAL/journal.
  Dropped vars reuse the `carryover_report` banner; the editor reopens on the new `id`.
- **Fork / Create / Delete** (Phase 3 stage 2a) — the editor header carries a **Fork**
  button (on *any* module, bundled or user) and a **Delete** button (only when
  `editable`); the module-list header carries **+ New**, and the module detail view offers
  **Fork** next to **✎ Edit**. Fork/Create open `GuiApp::module_dialog`
  (`ModuleDialog` — Author + Name, Fork adds a "Copy saved settings" checkbox), rendered by
  `GuiApp::render_module_dialog` with live red/green uniqueness feedback
  (`name_collides`, whole loaded set) and the confirm button disabled while colliding or
  Name empty. On confirm:
  - **Fork** (`GuiApp::perform_fork`) deep-copies the parent `Extension`, sets the new
    Author/Name + `ForkedFrom = "<parentAuthor>::<parentName>"`, writes it to the user
    extensions dir under `sanitize(Author)__sanitize(Name).json` (suffixed `-2`, `-3`, …
    on slug clash — `unique_slug_path`/`uniquify_slug`) via `config::write_atomic`, and if
    "Copy saved settings" is on calls `config::snapshot_config_to_fork`. It then reloads
    and opens the fork in the editor (now an editable user module; the bundled parent is
    untouched on disk).
  - **Create** (`GuiApp::perform_create`) writes a minimal valid template (meta + one empty
    `General` UI section, no builders) that passes `extension::validate`, then reloads and
    opens it.
  - **Delete** (`GuiApp::delete_module`, gated behind a `ConfirmAction::DeleteModule`
    dialog with an "Also purge saved settings" checkbox, default OFF) removes the manifest
    file; when purge is checked it runs `GuiApp::config_cleanup` after reload so the now
    undeclared vars are swept across every scope. Bundled modules are never deletable.
- **Dropped-var report** — a fork snapshot (and later rename) returns
  `(scope-label, dropped-vars)` entries; `GuiApp::set_carryover_report` formats a non-empty
  report into `GuiApp::carryover_report`, shown as a dismissible banner atop the editor.
  (A clean copy carries everything, so this is usually empty.)
- **Invalid-save reason** — when Save is greyed for a schema problem the editor shows the
  reason: duplicate section names (checked via `ModuleDraft::sections_unique`, since they
  collapse in the `IndexMap` before `validate` runs) or the `extension::validate` error
  text (duplicate `Variable`, empty variable, …).
- **Structural edits** are collected as path-addressed `Deferred` actions during render
  and applied by `apply_deferred` **after** the render loop, never mid-iteration. One
  reusable `row_actions` widget (`[↑][↓][🗑]`) returns a `RowAction` the caller turns into
  the container-correct op (`apply_row` = `Vec::swap`/`remove`).
- **Nesting shades** — `editor_card(ui, fill, add)` takes a per-level fill from the
  `theme::EDIT_L0..L3` ramp so the hierarchy reads at a glance: module card = `EDIT_L0`,
  section / ENV / WRAPPER / arg block cards = `EDIT_L1`, field cards = `EDIT_L2`,
  builder-step rows = `EDIT_L3` (each one step lighter than base panel).
- **Labels & placeholder color** — every editor box has a leading `field_label` ("Section",
  "Name", "Variable", "Description", "Requires", "Value", …) and its placeholder is a gray
  `gray_hint` (`theme::FAINT`) while entered text stays the normal foreground. `requires_edit`
  colors *entered* text `theme::TEXT` (red only on a parse error), not dim.
- **Requires** fields are live-validated via `condition::parse` (`requires_edit`): red
  text on a parse error, the wordy error shown only once the field loses focus. Any
  unparseable `Requires` blocks Save.
- **Explicit Save** (`GuiApp::save_module`) is gated by `ModuleDraft::save_enabled`
  (`save_gate`: `dirty && valid && all Requires parse && !name_error`, and `editable`).
  It serializes `snapshot()` and writes via `config::write_atomic`, then reloads
  extensions so the draft resets clean **and calls `exit_module_editor`** so the user
  returns to the normal view showing the saved module. **No manifest autosave.**
  `GuiApp::discard_module` drops the draft and also exits (via `exit_module_editor`).
- **Config-autosave interlock** — while a module draft is dirty (`config_autosave_held`),
  the normal config write at the end of `ui()` is held (`pending_config_write`); the
  in-memory config still updates, and the held write is flushed by
  `flush_config_writes_if_clean` once the draft is saved, discarded, or hand-reverted.
  *Why:* a config value could reference a field that only exists in the unsaved draft.

The **live preview** (below) splices the in-memory draft over the module's on-disk entry
before resolving, so the launch command reflects unsaved edits.

## Launch-command preview footer

The bottom `preview` panel (in `GuiApp::ui`) is either a fixed 198px band or, if
`GeneralConfig::dynamic_preview` is on, sized to its content. It shows the **About**
box (repo link + credits, `crate::gui::GuiApp::render_about`) when `NavSel` is
`GeneralSettings` — there's no launch command to preview there — otherwise it calls
`crate::context::assemble_launch` with whichever resolution matches the panel: when
editing a **Profile**, the edit-scope resolution (so the preview reflects the profile
being edited even if it's not the active one for any game); when in the **manifest
editor** (`NavSel::ModuleEditor`), the draft-spliced spec set (the edited module's entry
replaced by `ModuleDraft::snapshot`, resolved via `GuiApp::resolve_specs_for_game` — one
clone/frame, no disk, assembler signature unchanged) plus a red `set_set_collisions`
lint listing env vars two modules both `Set`; otherwise the game resolution. The
assembled command is rendered with `crates/ritz-app/src/gui.rs:command_job`,
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
7. Press **Ctrl+E** (or **✎ Edit**) to open the selected module's manifest editor;
   inside it, **Ctrl+S** saves (when the Save gate holds) and **✕ Close** leaves.

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
