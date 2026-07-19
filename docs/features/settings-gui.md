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
  below), lays out the panels for the current `Mode` (see Layout below), then persists if
  anything changed. It doesn't diff
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

### Two modes: `Mode::Config` and `Mode::Ide`

`crates/ritz-app/src/gui.rs:Mode` selects which *shape* the window takes. It is
**orthogonal to `NavSel`**, held in memory only, and never written to any config file
(reopening the settings window always lands in `Config`).

*Why a separate axis and not another `NavSel` variant:* `NavSel` answers "which config
scope am I editing"; `Mode` answers "which shape is the window in". Folding the two
together would force every exhaustive `match self.nav_sel` in `gui.rs` to grow an arm
with nothing to say about scopes — and would collide with `NavSel::ModuleEditor`, which
IDE Mode *uses* as its "which module is open" carrier. IDE mode maintains the invariant
`nav_sel == NavSel::ModuleEditor(_)` at all times (re-established at the top of
`GuiApp::ui` if anything clears it, which is what makes the editor's Close button read
as "discard and reload" there).

### Title bar (`render_title_bar`) — the mode-aware chip

The title bar's rounded chip (`breadcrumb_chip`, right of the "Ritz" wordmark) shows
different text depending on which of the three category-box destinations is active,
computed once per frame by `GuiApp::title_chip_text` (2026-07-19; the chip used to
always show `self.game_config.game.name`, regardless of `mode`/`nav_sel` — reported as
"the current game display doesn't update when I go into IDE or settings mode"):

| Destination | Chip text |
| --- | --- |
| Profiles / Games (`Mode::Config`, `nav_sel != GeneralSettings`) | the ambient game's name — unchanged from before |
| Settings (`Mode::Config`, `nav_sel == GeneralSettings`) | the literal `"Settings"` |
| IDE Mode (`Mode::Ide`) | the open module's declared `Name` (`EditorHeaderInfo::name`, the same string the editor header row shows) |

`GlobalSettings` and `Profile(_)` both fall under the first row (they live under the
Profiles/Games category tab, see the tab table above) and keep showing the ambient game's
name, not `"Settings"`.

*Why IDE Mode can safely read `editor_header_info(id)` without a load-order race:*
`GuiApp::update` sets `nav_sel == ModuleEditor(id)` and calls `ensure_draft(&id)` several
lines before `render_title_bar` runs later the same frame, so the draft is already synced
to `id` on every ordinary frame by the time the chip is computed. The `"IDE Mode"`
fallback text only shows when `all_specs` is empty (no modules loaded — there is nothing
to be "the current module"); it is a fixed string, not a re-derivation of the previous
value, so it can't flicker between two names on a module switch.

*Sizing* (2026-07-19): the chip has always sized itself to its text plus fixed padding,
with no cap — fine while the only text it ever showed was a game name, but a
user-authored module `Name` has no length guarantee. `breadcrumb_chip` now wraps its text
in a `LayoutJob` capped at `BREADCRUMB_MAX_TEXT_W` (260px) with an ellipsis overflow
character (the same idiom `render_editor_header_description` uses for the editor header's
description line), so an unusually long name elides instead of growing the chip enough to
crowd the title bar's right-aligned Launch/Cancel cluster. The full text is available as a
hover tooltip when elided. The chip has never been clickable (`Sense::hover` only) in any
mode, so this didn't need a click-behavior decision.

### Layout: `Mode::Config` — four panels per frame

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

### Layout: `Mode::Ide` — module tree | manifest editor | read-only preview

```
┌───────────── titlebar (render_title_bar) ─────────────────────────┐
├────────────────┬─────────────────────────────────────────────────┤
│ ┌────────────┐ │ ide_module_header — name v… by …   [Fork][🗑][✕][Save] │
│ │Prof·IDE·Set│ │   description, one line, elided…                       │
│ └────────────┘ ├──────────────────────┬─────────────────────────┤
│                │  manifest editor      │  PREVIEW (read-only)     │
│ ⚠ errors banner│  (render_module_      │  (render_module_settings │
│ [module tree,  │   editor, full-width) │   _body, read_only=true) │
│  all_specs]    │←──── exactly half ───→│←──── exactly half ──────→│
├────────────────┼──────────────────────┼─────────────────────────┤
│ Group by Author│  ide_editor_band      │  ide_launch_band         │
│ [+ New Module ]│  (198px, declared     │  (launch command, nested │
│ [Open Folder →]│   but empty)          │   inside the preview)    │
│  extensions/   │                       │                          │
└────────────────┴──────────────────────┴─────────────────────────┘
```

**Panel declaration order is load-bearing** — egui hands each panel the rect left over
by the panels declared before it (both `SidePanel::show` and `TopBottomPanel::show`
start from `ctx.available_rect()`), so `GuiApp::ui` declares, in this order: titlebar →
`SidePanel::left("nav")` → `TopBottomPanel::top("ide_module_header")` →
`SidePanel::right("ide_preview_split")` → `TopBottomPanel::bottom("ide_editor_band")` →
`CentralPanel`. Two slots in that order carry weight:

- Declaring the **right panel before the bottom band** is what makes `ide_editor_band`
  span only the editor column instead of the whole window.
- Declaring the **header after the nav but before the right panel** is what makes it
  start at the nav's right edge (not the window's) *and* span the editor **and** preview
  columns (not just the editor's half). Moving it after `render_ide_preview_panel` would
  silently shrink it back to the editor column — no compile error, just the old layout.

### IDE-mode header band (`ide_module_header`)

The module header — name, version, author, and the `[Fork] [Delete] [✕ Close] [Save]`
(+ `Rename`) cluster — spans the **full width right of the nav**, above both columns.
Rendered by the free function `render_editor_header_row`; see "Header / status / body
split" under the manifest editor for how the state reaches it across panel closures.

*Save/Close hidden for a bundled module in IDE mode* (2026-07-19): `render_editor_header_row`
takes an `ide_mode: bool` (`true` from this band, `false` from Config mode's inline call in
`render_module_editor`) and skips Save and Close entirely when `!info.editable && ide_mode`
— leaving only `[Fork]`. Save can never be enabled on a bundled module (nothing to save —
the fields render disabled) and, in IDE mode specifically, Close is not "leave the editor":
the `Mode::Ide` invariant (`nav_sel` is always `NavSel::ModuleEditor(_)`) reopens whatever
the tree has selected on the very next frame (the guard in `GuiApp::ui` right after the
draft-drop guard), so Close in IDE mode only ever discards-and-reloads the current
selection — see `GuiApp::dispatch_top_action`'s `TopAction::Close` arm. On a bundled module
there is nothing to discard, so the reload was a no-op; hiding the button removes a control
that did nothing observable. **Config mode keeps both for bundled modules, unchanged**:
Close there genuinely returns to the previous view (`editor_exit_target`), and Save's
disabled "Fork to edit" tooltip is the established way Config teaches the fork gesture —
this fix does not touch that path.

*Why full-width and not inside the editor column* (2026-07-19): the action cluster plus
the heading needs roughly 500pt of row. Inside the editor's half — `(window − 280) / 2`
— that made the **button row the thing setting the editor column's minimum usable
width**: below about a 1300pt-wide window the right-to-left cluster overflowed its
region and drew over the module name. (The editor's *field rows* were never the
constraint: `editor_control_width` floors at `MIN_CONTROL_W`, so they degrade gracefully
at any width.) Spanning the full width right of the nav roughly halves that floor and
decouples the editor column's width from the button row outright, rather than moving
the problem somewhere else.

- **`exact_height(IDE_HEADER_H)`**, where
  `IDE_HEADER_H = 14.0 + 23.0 + 7.0 + 16.0 + 10.0 = 70.0` — top inset, the
  `interact_size.y` heading/button row, one `item_spacing.y`, the
  `IDE_HEADER_DESC_H` description slot, bottom inset. The frame's `inner_margin` is
  `{ left: 14.0, right: 16.0, top: 14.0, bottom: 10.0 }`. *Where 70 comes from*
  (2026-07-19): the first cut was one row (`6 + 23 + 8 = 37`), then two rows at
  `6 + 23 + 7 + 16 + 8 = 60` (top/bottom inset `6`/`8`) — both read as cramped beside
  Config mode's module header. That header (`render_module_detail_header`) is **not**
  fixed-height — it is natural-sized — but its structure is: `6` space (on top of the
  enclosing `CentralPanel`'s own 8px margin — 14 total), the same 23pt heading row, an
  edit-context line, the description, `10` space, a separator. The band reproduces it
  minus the two pieces it does not own — the edit-context line (IDE Mode has no edit
  scope to name; the tree shows the selection) and the trailing separator
  (`show_separator_line` draws that) — which leaves heading row + description, now at
  the same 14px top inset as Config's fixed same-day fix (see "Header top/left
  symmetry" under the manifest editor's "Module editor" section). The frame's *left*
  margin was raised from 8 to 14 to match, for the same top-vs-left symmetry reason,
  and the *bottom* margin was raised from 8 to 10 to match Config's `add_space(10.0)`
  gap between the description and its trailing separator — the closest analogue to
  this frame's own bottom margin. *Why pinned rather than auto-sized:*
  the band spans half the window, so any height change reflows both columns; and on the
  frames where `GuiApp::editor_header_info` returns `None` (a module switch, before
  `ensure_draft` catches up) an auto-sized band would collapse to nothing and snap back,
  which reads as a flicker. `None` renders an **empty band of the same height** instead.
- **The status lines did NOT move up with the header.** They stay at the top of the
  editor `CentralPanel`. *Why:* only the first line ("Unsaved changes" / "All changes
  saved") is unconditional — the validation, `Requires` and pending-identity lines appear
  and disappear **as you type**. In the full-width band they would resize it mid-keystroke
  and shove the editor *and* the preview column down; kept in the editor column, any
  height change stays confined to where it already was before the header moved. This
  preserves the focus-stability contract documented under the manifest editor.
- **The description renders as a fixed one-line second row**, full band width, under the
  name and left of nothing — the action cluster owns the right end of row *one*, so a row
  of its own can never collide with it. Painted by the free function
  `render_editor_header_description` in `theme::DIM`, matching how
  `render_module_detail_header` shows the same field in Config mode. **Its height cannot
  vary**, which `IDE_HEADER_H`'s budget depends on, and three things had to be closed off
  to say that: a module with **no** description still allocates the slot (the rect is
  reserved before the text is looked at, so walking the tree never twitches the band); a
  **long** description elides rather than wraps (`LayoutJob` with `max_rows = 1` and an
  ellipsis overflow character — this is why the text is painted from a hand-built job
  instead of `ui.label`, which would size the `Ui` from its galley); and a **narrow
  window** only moves the ellipsis, since wrapping was the only route from width to
  height. Elided text keeps the full string on hover.
- **Fill is `theme::PANEL`** — the same fill as the nav column, the editor `CentralPanel`
  and the preview panel, so the header reads as the top of one continuous surface rather
  than as a separate toolbar. (The first cut used `theme::PANEL2`, the 198px bands' fill,
  for a toolbar look; seen in place, it read as a strip bolted on top of the editor.) With
  the band and the columns beneath it now sharing a fill, `show_separator_line(true)` — the
  `theme::BORDER` hairline — is the only thing marking the edge, so it stays; a hand-drawn
  bottom border on top of it would just double the line.
- **The click is dispatched after the CentralPanel**, not at the point of the click:
  `ide_action` is hoisted out of the panel closure and fed to
  `GuiApp::dispatch_top_action` once the preview column and editor body have rendered.
  Acting on Save/Close inside the header closure would drop the draft mid-frame and leave
  everything downstream rendering a "no longer available" flash against state the header
  had already invalidated.
- **Config mode is untouched** — the whole band is inside `if self.mode == Mode::Ide`, and
  `render_module_editor(.., header_inline = true)` keeps drawing the header inline there.

- The `ext_list` column is **dropped entirely** in IDE mode (its tree moves into the nav
  column); a second copy would be redundant and would eat the width the editor/preview
  split needs.
- The full-width `preview` footer is **skipped entirely**, not emptied. *Why:* a
  zero-content bottom panel still reserves height across the full window width, which
  would stack a dead strip under the editor column on top of `ide_editor_band`.
- `ide_editor_band` is deliberately **declared but empty**, reserved for a future
  diagnostics pane. Its `exact_height(198.0)` matches the nav footer band so the two
  bottom edges align.
- `ide_preview_split` is **`resizable(false)` with an `exact_width` of exactly half** of
  everything right of the fixed `NAV_W` (280px) nav column:
  `((ctx.screen_rect().width() - NAV_W) * 0.5).max(0.0)`, recomputed every frame so the
  halves track a window resize instead of holding a width captured at first paint.

  *Why static and not a clamped drag range* (2026-07-19): the splitter could be dragged
  far enough left that the preview slid **behind** the nav column and disappeared.
  Clamping would have papered over that; removing the handle removes the failure mode
  outright, and a static half-and-half is what was asked for. The `PREVIEW_MIN_W` /
  `EDITOR_MIN_W` / `width_range` / `default_width` machinery existed only to bound that
  drag and was deleted rather than left inert — half a window clears either column's
  usability floor (preview 400px, editor 340px) on any display this app is usable on.

  *It is half to within one pixel, not exactly half:* `SidePanel` paints its separator
  line out of the space **outside** `exact_width`, so the editor's central panel ends up
  ~1px narrower. Compensating would make the columns provably unequal the other way; a
  1px asymmetry from a hairline is the smaller lie.
- The panel id is `ide_preview_split` rather than `ide_preview` for a reason that is now
  **historical**: `default_width` only applied the first time egui saw a panel id, and
  eframe persists egui memory, so the rename was needed to retire a stale stored width.
  `exact_width` + `resizable(false)` ignores stored width entirely, so the hazard is
  gone; the id stays only because renaming again would be pure churn. The
  `push_id("ide_preview")` widget namespace inside the panel is unrelated and unchanged.
- The editor column renders at `body_max_width(ui, full_width = true)`: the 743px clamp
  exists for Config mode's wider unsplit central panel and would only add a dead gutter
  here.

## Navigation panel

`crate::gui::GuiApp::render_nav_panel` splits into a header, a scrollable body, and a
fixed-height (198px) bottom band. The header is constant across both modes; the body and
the band swap on `Mode`.

- **Header — the category tab bar** (`GuiApp::render_nav_category_box`): a bordered
  frame (the `editor_card` border idiom, rebuilt locally — `editor_card` itself is
  specialised for the manifest editor) holding three tabs **side by side in one
  horizontal row**: **Profiles**, **IDE Mode**, **Settings**, in that left-to-right
  order (2026-07-19 — was **Settings**, **IDE Mode**, **Profiles**; reordered so
  **Profiles**, the app's startup destination, sits first). It replaced the old single
  `header_label("Profiles / Games")`. *Why a box and not two top-level categories:* it
  is the cheapest change that splits the old "Profiles / Games" destination in two — the
  nav's existing "fixed header box above a scrolling tree" shape is reused verbatim and
  only the body's data source swaps. Clicks are collected as a `NavCategory` and applied
  after the render closure by `GuiApp::select_nav_category` (the closure already holds
  `&mut self`).

  *Why a horizontal tab strip and not the original stacked rows* (2026-07-19): three
  full-width rows plus an uppercase `GENERAL` caption spent four lines of vertical space
  on navigation that the module tree needs, and read as a list of *items* rather than as
  a mode switch. One row of tabs is the conventional shape for "pick one of N views" and
  costs one line. The caption was removed with it — the tabs label themselves — and the
  labels shortened (`General Settings` → **Settings**, `Games / Profiles` →
  **Profiles**). The border stays; it is what still groups the three as one control.

  **The bar is the *only* control for these three destinations, and it is always
  truthful.** `is_ide` / `is_general` / `is_games` are recomputed from `mode` and
  `nav_sel` on every frame — there is no cached "current category" that could drift, so
  any other code path that moves `nav_sel` (Close, the IDE tree, the exit interlock) is
  followed by the box automatically. *Why the reorder above didn't need a matching
  change to the startup default:* which tab reads as selected is derived from state
  (`mode`/`nav_sel`), never from draw order — `GuiApp::new` starts in
  `Mode::Config` / `NavSel::Game(appid)`, which makes `is_games` true regardless of
  where the Profiles tab sits in the row, so the app still lands on **Profiles** at
  startup.

  Each category owns what the list below it shows:

  | Tab | List below the bar |
  | --- | --- |
  | Settings | **empty** — the page renders in the central panel |
  | IDE Mode | all modules (`render_ide_module_tree` over `all_specs`) |
  | Profiles | Global Profile · Profiles ▸ · Games ▸ |

  *Why Settings shows an empty list:* the tree lists config **scopes**, and
  General Settings is not one — it edits app-wide preferences. Drawing the scope tree
  under it would offer a selection the right-hand panel isn't showing.
  - *Profiles* from IDE mode calls `exit_module_editor`, which restores the
    remembered view (else the ambient game) and drops the draft. Coming from **General
    Settings** it lands on the ambient game (`NavSel::Game(appid)`, the same destination
    `editor_exit_target` defaults to) — leaving `nav_sel` on `GeneralSettings` would make
    the box snap straight back to that category and the click look dead.
  - *IDE Mode* opens `all_specs[ide_selected]` in the editor, establishing the
    `nav_sel == ModuleEditor(_)` invariant IDE mode's columns rely on.
  - **Tab rendering** (`crate::gui::nav_category_tab`): a glyph + label built as a
    `LayoutJob` and hand-painted into a cell of exactly `tab_w` points. *Why not
    `full_selectable`,* which the stacked-row version used: it is `top_down_justified`,
    i.e. deliberately full-width — the wrong primitive once three tabs share one row —
    and its plain rounded selection fill reads in a horizontal strip as "one cell is
    slightly lighter" rather than "this is the open tab".

    **Each tab now owns its own colour** (2026-07-19; all three used to share `ACCENT`
    when selected): `nav_category_tab` takes an `accent: Color32` parameter — **Profiles**
    passes `COL_PROFILE` (the same green the settings tree already uses for the Profile
    scope, so the tab ties back to a meaning the user already associates with that word;
    it reads acceptably against `PANEL2`, if anything helping the three tabs stay
    distinguishable at a glance), **IDE Mode** keeps `ACCENT` (the app's primary authoring
    surface, unchanged), **Settings** passes `theme::DIM` (deliberately *not*
    `COL_DEFAULT`, which already means "no override anywhere" in the scope palette and
    would be a confusing second meaning for the same colour; `DIM` is also visibly
    brighter than the `FAINT` the *unselected* icon already renders in, so selecting the
    tab still reads as a state change).

    **Selected state layers two cues, both derived from that per-tab `accent`:** a fill +
    border from `theme::selection_tint(accent)` — the same ~16%/~42% alpha formula
    `SEL`/`SELBD` are hand-expanded from (`selection_tint(ACCENT)` reproduces them
    byte-for-byte, pinned by a unit test), rounded on **all four corners** at the same
    `8.0` radius every other rounded container in the app uses (`Rounding::same(8.0)`,
    per [STYLING-GUIDE.md](../ui/STYLING-GUIDE.md)); and ink — glyph in `accent`, label
    `TEXT`. Unselected stays identical across all three tabs — transparent (a `HOV` wash
    on hover only), glyph `FAINT`, label `DIM` — so exactly one tab reads as "live" and
    resting state doesn't look like an undecided 3-colour legend; the colour only shows up
    once a tab is actually selected. *Why colour and not a bold weight:* the bold family
    (`FontFamily::Name("bold")`) is reserved for the logo wordmark, and a weight switch
    would reflow the label inside a fixed-width cell.

    *Why no underline anymore* (2026-07-19): the selected cell used to round only its
    top two corners and sit on a separate 2px `ACCENT` underline across its bottom edge,
    so it would read as "sitting on a strip." In practice this read as a flat outline
    bar rather than a rounded, selected tab. Rounding all four corners and dropping the
    underline lets the fill/border plus the accent icon + bright label carry "this one
    is selected" on their own, matching the uniform rounding every other control in the
    app uses.

    **Width and text size are measured, not chosen.** The nav column is a fixed `NAV_W`
    280px; the header frame's 16px side margins and the category frame's 8px inner
    margins leave **232px**, so each tab is `(232 − 2×TAB_GAP) / 3 ≈ 74.7px` (`TAB_GAP`
    = 4px, tighter than the global 7px `item_spacing.x`). Worst case is `mono_ui` (the
    default), where Geist Mono makes every glyph 0.6em wide and all three labels are
    exactly 8 characters:

    | size | glyph + gap + label (mono) | fits 74.7px? |
    | --- | --- | --- |
    | 13px (`Body`) | 8 + 8 + 64 = **80px** | no — clips |
    | 12px | 7 + 7 + 56 = **70px** | only just — 4.7px slack |
    | 11px (`Small`) | 6 + 7 + 48 = **61px** | yes — 13.7px slack |

    Hence `TAB_TEXT_SIZE = 11.0`, which is also an existing step of the type scale rather
    than an invented size. *Consequence worth knowing:* the fit is driven by **label
    length**, not by the size — any label longer than 8 characters clips again, so the
    three labels cannot grow without revisiting this table. *Why not icon-only with
    tooltips,* which would fit at full 13px: a tooltip is not discoverable, and top-level
    navigation is exactly where a first-time reader should not have to hover to find out
    where a button goes.

    Glyphs are `\u{f013}` (cog, reused verbatim from the retired tree row),
    `theme::ICON_EDIT` = `\u{f044}` (pencil) and `\u{f4ff}` (person). The pencil replaced
    `\u{eef4}` on 2026-07-19 — that glyph's ink is too fine to read at this size — and is
    deliberately the *same* pencil the module detail header uses for its Edit
    affordance, since both mean "author this thing". All three resolve from
    `resources/assets/GeistMonoNerdFont-Regular.otf` via the `Proportional` family's icon
    fallback, so they share one font path. They go through a plain `LayoutJob`, **not**
    `IconCenterCache`, which corrects glyph ink for a square `icon_button` and has
    nothing to say about a glyph that is simply the first run of a text line.
- **Body** — `Mode::Config`: `GuiApp::render_nav_tree`, wrapped in the `nav_live`
  disable-while-dirty gate — **unless** `nav_sel == GeneralSettings`, which renders
  nothing at all (see the table above). `Mode::Ide`: `GuiApp::render_ide_module_tree` — the
  load-error banner (rehomed from `ext_list`, which IDE mode drops; losing it in an
  *authoring* mode would mean writing broken JSON and getting silence) over
  `render_ext_tree` across the unfiltered `all_specs`, with `show_inheritance = false`
  (IDE mode edits manifests, not scopes, so inheritance badges describe nothing).
  **The IDE tree is deliberately NOT gated by `add_enabled_ui`** — it is the primary
  navigation there, and locking it on a dirty draft would mean you can never leave the
  module you just typed into. Per-row dirty markers replace locking (S4).
  Selection lives in `ide_selected` (an index into `all_specs`, distinct from
  `selected_ext`, which indexes `cur_specs` — different lengths and orderings). `leaf`
  writes the index directly and never opens the editor, so the click is turned into an
  `open_module_editor` call *after* the tree renders.
- **Bottom band** — `Mode::Config`: `GuiApp::render_nav_settings`, context-specific
  controls for whatever's selected. `Mode::Ide`: `GuiApp::render_ide_nav_footer` —
  Group by Author, a full-width **+ New Module**, and Open Folder. It deliberately
  omits **Show Inheritance** (no scope in IDE mode) and **Clear Settings** (it clears
  stored *config*; IDE Mode edits manifests and must never touch config).

  *Why "+ New Module" now has only one place it lives* (2026-07-19): the Config-mode
  `ext_list` header used to carry its own glyph-only **+** button calling the same
  `GuiApp::open_create_dialog` handler. IDE Mode has taken over module creation and
  editing, so that second entry point was removed as dead weight — `open_create_dialog`
  itself is unchanged and still backs this IDE-mode button.

  **Open Folder here opens `Paths::user_extensions()`** (`~/.config/ritz/extensions/`),
  not `games_dir()` — IDE Mode edits module *manifests*, and that is where they live
  (2026-07-19). Note the Config-mode module footer (`GuiApp::render_modules_footer`) has
  an identical-looking Open Folder button that still opens `games_dir()`; that is a
  **pre-existing target bug on a different screen**, left alone deliberately because
  changing it is a separate behaviour change needing its own decision. The two buttons
  therefore open different folders on purpose — this is not drift.

- **`NavSel`** (`crates/ritz-app/src/gui.rs:NavSel`) is the four things the tree can
  select: `GeneralSettings`, `GlobalSettings`, `Profile(name)`, `Game(appid)`. Switching
  it clears `text_buffers`/`multi_edit` (in-progress edits) and reloads whichever config
  the new selection needs.
- **Tree rows** — Global Profile (label only; the `NavSel` variant, file, and scope are
  still `GlobalSettings`/`global.json`/global scope, see
  [TERMINOLOGY.md](../meta/TERMINOLOGY.md)) is the always-visible top row;
  below it, two `egui::CollapsingHeader`s list **Profiles** (pinned ones first, by
  slot 1–10, via `crate::gui::GuiApp::assign_pin`) and **Games**. `+ Add profile` /
  `+ Add game` switch the bottom band into a create-name form
  (`GuiApp::creating_profile` / `GuiApp::creating_game`).
  There is **no General Settings row in the tree** — it is reachable only from the
  category tab bar. *Why removed:* with the bar present the row was a second, competing
  control for the same destination. Selecting it left the bar reading "Profiles"
  while the panel showed General Settings, and picking any profile or game afterwards
  silently flipped the bar's category. One destination, one control. This is a
  deliberate Config-mode rendering change (user-confirmed) and supersedes the earlier
  "Config mode stays pixel-identical" constraint for this row alone.
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
  - *General Settings/Global Profile* — empty; their controls live in the central panel
    instead.

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

*Header top/left symmetry* (2026-07-19): `render_module_detail_header` sits inside
`CentralPanel::default()`'s frame, which gives an 8px margin on every side; the header
then adds its own `add_space(6.0)` above the heading, putting its top inset at 14px
against an unrelated 8px left inset — the header read closer to the left edge than the
top, i.e. not square in its corner. Fixed with a local `egui::Frame` (6px left-only
inner margin) wrapped around just the header + settings-body call in `GuiApp::ui`,
bringing the left inset to 14px too — without touching the shared `CentralPanel` frame
the General Settings and Config-mode module-editor branches render through, which were
never part of the complaint. `IDE_HEADER_H`, below, imports this same left = top = 14
spacing to give IDE mode's header band the same breathing room the fix gave Config
mode's.

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
whole app. The detected value is written through `crate::gui::GuiApp::set_scoped`, the
single `nav_sel`→config-store map shared with `set_current`. *Why routed rather than
written directly:* `poll_detect` previously wrote `game_config` unconditionally, so
detecting while editing a Profile or Global put the value in the wrong store — and
since `persist` saves *by scope*, the edited scope never received it while the text
buffer still displayed it (a silent wrong-scope write). Every new writer must go
through `set_scoped` for the same reason.

**Cancel on navigate**: a pending detection is dropped if the user changes the
navigation selection before the 3 seconds elapse — `crate::gui::GuiApp::start_detect`
snapshots `nav_sel` into `Detect::nav`, and
`crate::gui::GuiApp::cancel_stale_detect` clears `detect` as soon as the live `nav_sel`
differs. It is called twice per frame: once right after the nav side panel renders (so
no frame draws a "Detecting… Ns" countdown for a scope the user has left) — this catches
game/profile create too, since those `nav_sel` assignments live inside
`crate::gui::GuiApp::render_nav_panel` — and again at the top of `poll_detect` —
*before* its `Waiting` early-return, or an in-flight detection would not be
re-examined until it completed — which catches the nav sites outside the nav panel
(module dialogs, editor entry/exit, game/profile delete).
*Why cancel rather than apply to the snapshotted scope:* that target may no longer
exist — `editing_preset_buf` is swapped on a profile switch and `game_config` is
replaced on a game switch — so "write it where it started" would recreate the very
wrong-scope bug `set_scoped` exists to prevent. Cancelling also matches user intent:
they navigated away from the field they were configuring. *Why a snapshot comparison
rather than `detect = None` at each nav site:* `nav_sel` is assigned in a dozen-odd
places, so a per-site clear is only as complete as the next author's memory; comparing
against the snapshot cannot be defeated by a new assignment. The spawned thread still
runs to completion and writes into its `Arc<Mutex<DetectStatus>>`; nothing reads that
handle once `detect` is cleared, and it is freed with the dropped `Detect`.

**Who a field write belongs to**: `crate::gui::GuiApp::set_current` /
`crate::gui::GuiApp::unset_current` (and the `crate::gui::GuiApp::apply_tri` dispatcher
that forwards to them) take the module as a `&Extension` parameter supplied by the call
site — `render_field` and the two list-field renderers all already hold the `spec` they
are drawing. *Why not look it up from `cur_specs[selected_ext]`:* that index is a single
*global* selection, so it is only ever right while exactly one module's fields exist on
screen; a frame drawing two modules would funnel every edit into whichever one the index
happened to point at. Taking identity from the caller makes the writer correct by
construction instead of by coincidence.

`set_scoped` / `unset_scoped` treat `NavSel::GeneralSettings` as an unreachable no-op
(`debug_assert`), not as the game scope. *Why a no-op:* General Settings' own controls
write `general_config`, never module values, so there is no honest scope to pick;
folding it into the game arm (as it used to be) would silently divert any future module
write there into `games/<appid>.json`. `persist` still maps `GeneralSettings` to
`save_game`; that is only a harmless re-save of an unchanged game config, not evidence
of a module write path.

**Why the arm is genuinely unreachable** — the proof needs *two* legs, and the render
one alone is not sufficient. `set_scoped`/`unset_scoped` have exactly three callers:
`set_current`, `unset_current`, and `poll_detect`.

1. *Render path.* `set_current`/`unset_current` are only ever called from `render_field`
   and the list-field renderers it dispatches to, and no module field renders on the
   General Settings screen — the central panel returns right after
   `render_general_settings_panel`, and the module list panel is skipped for that
   selection entirely.
2. *Async path.* `poll_detect` is a completion writer decoupled from rendering: it is
   called from `update()` **outside** both of those gates, so it runs unconditionally
   every frame for every `NavSel`. It is barred instead by `cancel_stale_detect` (see
   "Cancel on navigate" above), plus the fact that a detection can only be *started*
   from `render_field` and therefore never carries `GeneralSettings` as its snapshot.

*Why leg 2 matters:* `NavSel::ModuleEditor` is equally off the render path (its central
panel also returns early) yet correctly stays on the game arm — precisely because it is
a real edit scope reachable through this async path. A render-only argument would have
mis-classified it too.

### Manifest editor — `render_module_editor` (`NavSel::ModuleEditor`)

Opening a module in the **IDE Mode** tab routes through `GuiApp::open_module_editor`,
which records the current view in `GuiApp::editor_return` and switches `nav_sel` to
`NavSel::ModuleEditor(id)`, whose central panel is a full editor for the module's
*manifest* (not its config values). *Why the IDE Mode tab is now the only route*
(2026-07-19): the Config-mode module detail header used to carry an **✎ Edit** icon
button calling the same handler, and **Ctrl+E** used to open the editor for
`cur_specs[selected_ext]`. Both were removed as redundant entry points now that IDE
Mode owns module authoring — the Edit button because a second door into the editor
from a read-only view was confusing, Ctrl+E because it was keyed off `selected_ext`,
an index that only exists because the Config-mode `ext_list` column exists (see
`docs/brainstorm/ide-mode.md`, which staged it as "removed for now, not ported"). The
keybinding may be revisited once IDE Mode has its own notion of a selected module.

- **Entering / leaving** — `GuiApp::open_module_editor(id)` remembers the prior `nav_sel`
  in `editor_return` (unless already in an editor) so Close can restore it.
  If the selection moves off the editor by *any* route while a draft is still open
  (a left-nav click, the IDE tree, …), an early guard in `GuiApp::ui` (after keybind
  handling, before anything reads the draft) drops `module_draft` as an implicit
  Close/discard, so a dirty draft can never keep the config-autosave interlock engaged and
  strand later config writes — the frame-end `flush_config_writes_if_clean` then releases
  the interlock. `GuiApp::exit_module_editor` drops `module_draft` and restores
  `editor_return` via the pure `editor_exit_target` (falls back to the ambient `Game(appid)` when the stored view
  is missing or itself an editor, so no dangling editor selection remains). The header's
  always-present **✕ Close** button (`TopAction::Close`) exits even with nothing to save; it
  doubles as Discard — with a *dirty* editable draft it opens the `ConfirmAction::DiscardEdits`
  modal first and only `discard_module()`s on confirm. **Save** also exits on success so the
  user lands back on the real module. *Why no separate Discard button:* Close already means
  "leave without saving"; two buttons for one outcome, with the warning attached to only one
  of them, was the confusing part.
- **Locked trees while editing** — the nav-away guard above is now a *backstop*, not the
  normal path: while a draft exists the **MODULES tree** is wrapped
  in `ui.add_enabled_ui(module_draft.is_none(), …)`, so it greys out and can't swap the
  module out mid-edit; while the draft is **dirty** the **left nav** (Profiles / Games /
  General / Global) is likewise disabled, so a stray click can't silently discard unsaved
  edits. *Why the left nav is only locked when dirty:* it retargets the editor's live
  launch preview, which is useful — the lock exists to protect unsaved edits, not to pin
  the preview. Exit stays Close / Save (or Ctrl+S).
- **Keybinds** (handled at the top of `GuiApp::ui`, before panels render) — **Ctrl+S**
  triggers Save when the editor is open and `ModuleDraft::save_enabled` holds (same gate
  as the button), a no-op otherwise; **Ctrl+R** hot-reloads extensions and configs. The
  `command` modifier means text editing is never swallowed. *There is deliberately no
  Ctrl+E* — see the entry-point note at the top of this section.
- **Focus stability (no reflow steal)** — the dirty/identity/validation status lines that
  appear on the first keystroke must not knock the focused `TextEdit` out of focus. Two
  mechanisms guarantee this: (1) the "unsaved changes / All changes saved" line is
  **always rendered** (greyed when clean) so the clean→dirty transition never inserts a
  line above the fields; (2) the body `ScrollArea` carries an explicit `id_salt`
  ("module_editor_body") and **every** editor `TextEdit` a stable `.id_salt` keyed on its
  structural path (section/field/block index + role), so a focused box keeps the same egui
  id — and thus focus — even if a banner above it appears and shifts its screen position.

- **Header / status / body split** (2026-07-19) — `render_module_editor` is assembled from
  three pieces rather than one straight-line function:
  - `GuiApp::editor_header_info(id) -> Option<EditorHeaderInfo>` snapshots the name,
    version, author and every gate flag (`editable`, `dirty`, `save_on`, `has_identity`,
    `identity_err`, `sections_unique`, `validate_err`, `req_ok`) out of the draft. It takes
    `&self` only, so it can run **before any panel is declared**.
  - `render_editor_header_row(ui, cache, info, ide_mode) -> TopAction` draws the
    name/version/author heading and the `[Fork] [Delete] [✕] [Save]` (+ `Rename`)
    cluster, and *returns* the click rather than acting on it. `ide_mode` hides
    Save/Close for a bundled module in IDE mode only — see "IDE-mode header band"
    above.
  - `render_editor_status_lines(ui, info)` draws the dirty/validation/identity lines.

  *Why the snapshot struct rather than passing `&ModuleDraft`:* IDE mode renders the header
  row in a **different panel closure** from the body (see "IDE-mode header band"), while the
  body holds `&mut self.module_draft` for its whole scope. Handing the header owned, plain
  data means neither closure borrows the draft while the other runs — that decoupling is
  what makes the full-width header band possible at all. The two free functions take the
  `IconCenterCache` directly for the same reason.

  `render_module_editor`'s `header_inline: bool` parameter selects between the two layouts:
  Config mode passes `true` (header drawn in the editor column, action dispatched there);
  IDE mode passes `false` and owns both itself. **The status lines are drawn by
  `render_module_editor` in both modes** — see the reflow note under "IDE-mode header band"
  for why they did *not* move with the header.

  `GuiApp::dispatch_top_action(action, id, dirty, editable)` carries out the click and is
  shared by both paths, so Save / Close-as-Discard / Fork / Rename / Delete can never drift
  between modes.

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
  `editable`), laid out **[Fork] [Delete] [Close] [Save]** left→right (plus **Rename** when an
  identity edit is staged). In **IDE mode**, a bundled (non-`editable`) module drops
  Close and Save too, leaving just **[Fork]** — see "IDE-mode header band" above; this
  is unchanged in Config mode. Creating a new module is reached only via IDE Mode's
  **+ New Module** button (see the nav-panel bottom band above) — the Config-mode
  `ext_list` header's own glyph-only **+** button and the read-only module detail
  view's **✎ Edit** button were both removed on 2026-07-19 as redundant now that IDE
  Mode owns module creation and editing; forking is reached from inside the editor, so
  there is one place to do it. Fork/Create open `GuiApp::module_dialog`
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
- **Bordered cards with the title inside** —
  `editor_card(ui, cache, fill, title, actions, add) -> RowAction` draws a 1px `theme::BORDER`
  frame and renders `theme::section_label(title)` as the **first widget inside the frame's
  inner margin**, then 4px, then the body. *Why not a fieldset/legend notch:* the border
  stays a plain continuous stroke — the title is simply the first row inside it, which is
  cheaper and doesn't fight egui's layout. Every block header ("UI Sections", "ENV_VARS",
  "GAME_ENV_VARS", "WRAPPERS", "GAME_LAUNCH_ARGS") is the title of the card that *contains*
  its entries, rather than a label floating above the box. An empty `title` skips the
  header row. `actions: Option<(idx, len)>` puts the `[↑][↓][🗑]` cluster on the title row.
- **Nesting shades** — `editor_card`'s `fill` takes a per-level shade from the
  `theme::EDIT_L0..L3` ramp so the hierarchy reads at a glance: module card and the four
  output-block cards = `EDIT_L0`, section / ENV-var / wrapper / arg entry cards = `EDIT_L1`,
  field cards = `EDIT_L2`, builder-step cards = `EDIT_L3` (each one step lighter than base
  panel). *Why keep the shades on top of the borders:* ritz nests four levels deep
  (module → block → section/field → builder row); the border alone doesn't say *how deep*.
- **Fixed column layout** — editor rows reuse the fixed-column idiom `render_field` uses
  for normal module controls (pad the cursor out to a constant x, then let the control take
  the remainder). `editor_row_label(ui, label, reserve)` draws the label, pads to
  `EDITOR_LABEL_W` (96px — `render_field`'s 260px reserve at editor scale) and returns the
  control width from the pure `editor_control_width(remaining, reserve)`, so every textbox
  in a card starts and ends at the same x instead of each sizing to its leftover space.
- **Row alignment** — `row_actions` pushes itself to the row's right edge and then
  allocates exactly `ACTION_COL_W`, so section, field and builder rows share one right edge
  and one vertical center. Cards nest, so a deeper card's edge is inset by its parents'
  margins — that inset is the hierarchy, not misalignment.
- **Icon centering** — all icon affordances go through `icon_button(ui, cache, icon, label,
  style, enabled)`, which reserves a fixed square cell and positions the glyph via
  `crate::icon_center::IconCenterCache` (measured `Galley::mesh_bounds` ink box, cached per
  glyph+size+family). *Why:* `Align2::CENTER_CENTER` centers a glyph's *layout* box —
  identical line height for every glyph, advance width including side bearings — so a gear
  and a trash can look centered by different amounts and a row of icons reads ragged.
  Every call site must anchor `Align2::LEFT_TOP`; the cache already returns a corrected
  top-left, and re-centering would double-correct.
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

Two different panels depending on `Mode`. In `Mode::Ide` the full-width footer below is
not declared at all — see **IDE-mode preview column** further down.

The bottom `preview` panel (in `GuiApp::ui`, `Mode::Config` only) is either a fixed 198px band or, if
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

## IDE-mode preview column (`GuiApp::render_ide_preview_panel`)

The right-hand `ide_preview` `SidePanel` in `Mode::Ide`: a WYSIWYG render of the module
under edit — `render_module_settings_body(.., full_width = true, read_only = true)` —
over a nested `TopBottomPanel::bottom("ide_launch_band")`. The band is declared **before**
the inner `CentralPanel`; nested panels obey declaration order exactly like top-level
ones. *Why nest it here instead of a full-width footer:* the launch string belongs to the
preview, and the space under the editor column is the reserved diagnostics band.

- **Strictly read-only.** `read_only = true` gates every write at the `editable` flag in
  `render_field` / `render_value_editor` / `render_multi_string_field` /
  `render_env_pair_field` (including the `window_class` **Detect** button), so nothing
  from this column can reach `set_scoped` — whose `ModuleEditor(_)` arm writes the *real*
  game config. A `debug_assert!` on the returned `changed` bool guards the invariant.
- **One spec list for both halves.** `ide_specs` = `cur_specs` with the draft snapshot
  spliced over its entry, or **appended** when the module isn't in `cur_specs` at all.
  *Why the append:* the IDE tree browses the unfiltered `all_specs`, so opening a module
  that doesn't apply to the ambient game would otherwise show a preview that silently
  ignores everything you type. The body resolves via
  `GuiApp::resolve_specs_for_editing(&ide_specs)` and the band assembles from the same
  list, so the two can never disagree.
- **`resolve_specs_for_editing`** (new in S3b) is `resolve_for_editing` with the spec list
  as a parameter; `resolve_for_editing` delegates to it with `cur_specs`. *Why it was
  needed:* every arm previously said `&self.cur_specs` literally, ignoring any list the
  caller had in hand — inert while the only consumers used `cur_specs`, but it silently
  blanks badges and whole preview bodies for any module outside it. `render_ext_tree` now
  resolves against its own `specs` parameter for the same reason.
- **No "Previewing against: {game}" line**, unlike the Config-mode footer: IDE Mode is
  specified to resolve against nothing by default, so naming a game would advertise a
  binding the design doesn't want. (The empty scratch-layer `preview_config` that makes
  that literally true is S5; S3b still resolves against the ambient game underneath.)
- **`ui.push_id("ide_preview", ..)` wraps the entire body.** The editor and preview
  columns render the same module in the same frame, and the body mints position-derived
  auto-ids — plus `ComboBox::from_id_salt(&field.variable)`, salted by variable name
  *alone* — so without a namespace the two columns fight over widget state. It must wrap
  the whole body, not just the combo: the multi_string and env-pair renderers auto-id
  their `TextEdit`s, `+ Add` buttons and `icon_button`s too. This is the only `push_id`
  in `gui.rs`; S4 replaces the per-widget salts properly.
- If the module resolves to no `ExtResolution`, the column renders an **explicit notice**
  rather than staying blank — a blank column reads as "this module has no settings",
  which is a very different (and wrong) message.

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
2. Pick a target in the **left nav panel** — a game, a profile, Global Profile, or
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
7. Switch to the **IDE Mode** tab to open a module's manifest editor; inside it,
   **Ctrl+S** saves (when the Save gate holds) and **✕ Close** leaves.

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
