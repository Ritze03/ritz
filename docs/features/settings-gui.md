# Settings GUI — the window that renders extensions and previews the launch command

The settings manager is the whole visible surface of ritz: a native egui window with a
navigation panel (games/profiles/general/global), a module tree, a dynamically-rendered
field editor for whichever module is selected, and a live launch-command preview.
Everything lives in `crates/ritz-app/src/gui.rs` (~14.4k lines as of 2026-07-19), one
`GuiApp` struct implementing `eframe::App`.

> The line count is stamped with the date it was measured rather than left bare: it
> has been wrong twice already (the header claimed ~3.4k while the file was ~8.7k, and
> was ~14.4k by the time that was noticed). A dated figure tells you how much to trust
> it; an undated one just rots. Re-measure with `wc -l`, don't guess.

## How it works

- **Entry points** — `crates/ritz-app/src/gui.rs:run_manager` opens the editor on the
  first saved game (or a scratch config); `crates/ritz-app/src/gui.rs:run` opens it
  focused on a specific game and, in *launch mode* (`launch_mode: true`, set when opened
  from the splash screen), shows Launch/Cancel actions in the title bar instead of just
  closing. Both build a `crates/ritz-app/src/gui.rs:GuiApp` via
  `crates/ritz-app/src/gui.rs:GuiApp::new` and hand it to `eframe::run_native`.
- **State** — `GuiApp` (`crates/ritz-app/src/gui.rs:GuiApp`, struct at line ~101) holds:
  loaded extension specs (`all_specs` = every extension, `cur_specs` = the modules the
  **current screen** lists — see "Which modules a screen lists" below), the current
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
  reflect that scope); the second, built in `GuiApp::ui` as
  `resolve_specs_for_game(&game_specs())`, always resolves the *actual* game's module set
  (so the launch-command preview reflects what would really launch, even while editing an
  unrelated profile or global settings). Editing a profile that's also
  the game's *active* profile makes that resolution read the live in-memory
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
with nothing to say about scopes. As of S4b (2026-07-19) the two are fully orthogonal:
entering or leaving IDE Mode never touches `nav_sel` at all — "which module has focus" is
carried by `GuiApp::focused_module: Option<String>` instead, a plain field nothing else
aliases. IDE mode maintains the invariant `GuiApp::focused_module().is_some()` at all
times (re-established at the top of `GuiApp::ui` if anything clears it, which is what
makes the editor's Close button read as "discard and reload" there). *Pre-S4b* `Mode::Ide`
ran with the now-deleted `nav_sel == NavSel::ModuleEditor(_)` as its own "which module is
open" carrier, which is what made the two axes collide in the first place — see the
`## Applied` entry at the bottom of `../brainstorm/ide-mode.md` for what changed and why.

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
`GuiApp::update` sets `focused_module` and calls `ensure_draft(&id)` several
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

Built in `crate::gui::GuiApp::ui`: `egui::SidePanel::left("nav")` (`NAV_W`, always
shown), `egui::SidePanel::left("ext_list")` (also `NAV_W`, hidden while
`NavSel::GeneralSettings` is selected — General Settings has no modules to browse), a
bottom
`egui::TopBottomPanel::bottom("preview")`, and an `egui::CentralPanel` for the field
editor. *Why hide the module list for General Settings:* that screen edits app-wide
preferences, not extension variables, so a module tree would have nothing to select.

Both left panels size themselves from the one `NAV_W` constant (280.0) rather than
their own literals (2026-07-19, issue #15). They are the same affordance — the left
column of their respective modes — and are meant to stay the same width; `ext_list`
previously carried a duplicate `280.0`, which would have let a tweak to `NAV_W`
silently desync the Config-mode column from the IDE-mode one. `NAV_W` also feeds IDE
Mode's 50/50 split, which sizes itself from "everything right of the nav", so three
call sites now share the single constant.

### Layout: `Mode::Ide` — module tree | manifest editor | interactive preview

```
┌─────────────────── titlebar (render_title_bar) ───────────────────┐
├────────────────┬──────────────────────────────────────────────────┤
│ ┌────────────┐ │ ide_module_header — name v… by …                 │
│ │Prof·IDE·Set│ │   [Rename][Fork][Del][Close][Save]               │
│ └────────────┘ │   description, one line, elided…                 │
│                ├────────────────────────┬─────────────────────────┤
│                │  manifest editor       │  PREVIEW (interactive)  │
│ ⚠ errors banner│  (render_module_       │  (render_module_settings│
│ [module tree,  │   editor, full-width)  │   _body → preview_cfg)  │
│  all_specs]    │←──── exactly half ────→│←──── exactly half ─────→│
├────────────────┼────────────────────────┼─────────────────────────┤
│ Group by Author│  ide_editor_band       │  ide_launch_band        │
│ Preview against│  (BOTTOM_BAND_H,       │  (launch command,       │
│ [ None      ▾ ]│   DIAGNOSTICS)         │   nested in the preview)│
│ [+ New Module ]│                        │                         │
│ [Open Folder →]│                        │                         │
│  extensions/   │                        │                         │
└────────────────┴────────────────────────┴─────────────────────────┘
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

The module header — name, version, author, and the `[Fork] [Delete] [✕ Discard] [Save]`
(+ `Rename`) cluster — spans the **full width right of the nav**, above both columns.
Rendered by the free function `render_editor_header_row`; see "Header / status / body
split" under the manifest editor for how the state reaches it across panel closures.

*Save/Close hidden for a bundled module in IDE mode* (2026-07-19): `render_editor_header_row`
takes an `ide_mode: bool` (`true` from this band, `false` from Config mode's inline call in
`render_module_editor`) and skips Save and Close entirely when `!info.editable && ide_mode`
— leaving only `[Fork]`. Save can never be enabled on a bundled module (nothing to save —
the fields render disabled) and, in IDE mode specifically, that button is not "leave the
editor":
the `Mode::Ide` invariant (a module is always focused, `GuiApp::focused_module().is_some()`)
reopens whatever the tree has selected on the very next frame (the guard in `GuiApp::ui`
right after the draft-drop guard), so Close in IDE mode only ever discards-and-reloads the
current selection — see `GuiApp::dispatch_top_action`'s `TopAction::Close` arm. On a bundled
module there is nothing to discard, so the reload was a no-op; hiding the button removes a
control that did nothing observable. **Config mode keeps both for bundled modules,
unchanged** (though this whole call path is currently unreachable — see "Manifest editor"
below): Save's disabled "Fork to edit" tooltip is the established way Config teaches the
fork gesture — this fix does not touch that path.

*The button is labelled **Discard** in IDE mode and **Close** in Config mode*
(2026-07-19, S5): both raise the same `TopAction::Close` and run the same
`GuiApp::dispatch_top_action` arm, including the dirty-draft confirmation — only the
wording differs, because only the meaning does. `TopAction::Close` calls
`GuiApp::discard_module`, but the `Mode::Ide` invariant reopens the tree's selected module on
the next frame, so in IDE mode it can only ever mean "throw away my edits and reload the
draft". Labelling it Close there promised an exit that structurally cannot happen.
`render_editor_header_row` picks the label from its existing `ide_mode` parameter; no
behaviour changed.

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

  ```
  IDE_HEADER_H = IDE_HEADER_MARGIN.top (8)   top inset
               + IDE_HEADER_ROW_H     (27)   heading + button row (button-driven)
               + 7.0                         item_spacing.y
               + IDE_HEADER_DESC_H    (17)   one-line description slot
               + IDE_HEADER_MARGIN.bottom (11)  bottom inset
               = 70.0
  ```

  The frame's `inner_margin` is `IDE_HEADER_MARGIN`
  (`{ left: 14.0, right: 16.0, top: 8.0, bottom: 11.0 }`) — a **shared constant**, not a
  literal at the panel, because two of its four numbers are terms of the sum that sizes
  the panel. Written twice, a margin edit that failed to reach the sum would reintroduce
  the black bar below with no compile error and no test failure (verified: changing only
  the panel's copy left every test green). The test
  `ide_header_content_is_exactly_ide_header_h` renders the real two rows headlessly,
  with the real bundled font and `theme::apply`, and asserts the laid-out content plus
  those margins equals `IDE_HEADER_H` on the nose.

  *The band briefly held the status lines, and no longer does* (2026-07-19, issue #26,
  revised the same day): commit `7f6109c` hoisted the draft's status messages into this
  band as a third row, in a slot reserved for the four-line worst case, which put
  `IDE_HEADER_H` at `134.0` and the bottom margin at `12`. It was rejected on sight —
  *"It's way too large."* Only the first status line is normally present, so three of the
  four reserved rows (~42pt, a third of the band) were permanently empty. Shrinking the
  reservation to one line and letting the band grow per message was considered and
  dropped in favour of the user's own better suggestion: *"We could move it into the
  diagnostic view?"* **The status lines now live in the diagnostics band** — see
  "Diagnostics band" below for why that surface absorbs them for free. This band is back
  to exactly the two rows and the `70.0` it had before `7f6109c`, bottom margin included
  (the `12` existed only because an 11pt `Small` status row sits 1pt closer to its slot's
  bottom edge than the 13pt description does; with the description last again, the point
  goes back).

  *Where the original 70 came from*
  (2026-07-19): the first cut was one row (`6 + 23 + 8 = 37`), then two rows at
  `6 + 23 + 7 + 16 + 8 = 60` (top/bottom inset `6`/`8`) — both read as cramped beside
  Config mode's module header. That header (`render_module_detail_header`) is **not**
  fixed-height — it is natural-sized — but its structure is: `6` space (on top of the
  enclosing `CentralPanel`'s own 8px margin — 14 total), the same button-driven heading
  row, an edit-context line, the description, `10` space, a separator. The band
  reproduces it minus the two pieces it does not own — the edit-context line (IDE Mode
  has no edit scope to name; the tree shows the selection) and the trailing separator
  (`show_separator_line` draws that) — which leaves heading row + description, now at
  the same 14px top inset as Config's fixed same-day fix (see "Header top/left
  symmetry" under the manifest editor's "Module editor" section). The frame's *left*
  margin was raised from 8 to 14 to match, for the same top-vs-left symmetry reason,
  and the *bottom* margin was raised from 8 to 10 to match Config's `add_space(10.0)`
  gap between the description and its trailing separator — the closest analogue to
  this frame's own bottom margin.

  *Same-day black-bar fix* (2026-07-19, later the same day): the `14 + 23 + 7 + 16 + 10`
  version above turned out to have two wrong terms. The heading/button row is
  **button-driven** — `max(interact_size.y, Body galley + 2*button_padding.y)`
  (`egui::Button`'s own sizing) — and with this app's `theme.rs` numbers and the
  bundled Geist Mono font that comes out to `27.0`, not the `interact_size.y` value of
  `23.0` the original derivation assumed (the Fork button renders on every path
  through the row, so the button term always applies; confirmed with a headless
  `egui::Context` render of `secondary_button("Fork")`, which measured exactly
  `27.0`pt). The description slot was also a point short of its own text: Geist
  Mono's row height is `size * 1.3`, so a 13pt Body row measures `16.9` → `17.0`, not
  the `16.0` `IDE_HEADER_DESC_H` had. Because `exact_height` clips the panel's *fill*
  to `70.0` but egui still returns the frame's full (larger) laid-out rect and
  advances the cursor from there, the real `74.0`pt of content (using the wrong `27`
  row but still the old `16` slot) left a `70..74` strip nothing painted — the
  framebuffer clear colour showed through as a black bar under the separator hairline.
  Naming the row height `IDE_HEADER_ROW_H = 27.0` and correcting
  `IDE_HEADER_DESC_H` to `17.0` fixed the content itself, but their honest sum
  (`14 + 27 + 7 + 17 + 10 = 75`) is one point taller than the original bug, not
  shorter — so to hold `IDE_HEADER_H` at `70.0` (no reflow of the editor/preview
  columns) the frame's margins absorbed the full 5-point difference:
  `top: 14.0 → 8.0`, `bottom: 10.0 → 11.0`. The two are **deliberately unequal**, not a
  rounding accident — the visual gap a margin produces also depends on font
  ascent/cap-height above the heading and ascent below the description, and measured
  ink-to-edge distance only comes out even (top 14.61, left 14.72, bottom 14.94, within
  0.35pt) when the bottom margin is about 2.67pt more than the top. Do not "simplify"
  these back to equal numbers.

  *Why pinned rather than auto-sized:*
  the band spans half the window, so any height change reflows both columns — the hazard
  that ultimately sent the status lines to the diagnostics band rather than letting this
  band grow; and on the
  frames where `GuiApp::editor_header_info` returns `None` (a module switch, before
  `ensure_draft` catches up) an auto-sized band would collapse to nothing and snap back,
  which reads as a flicker. `None` renders an **empty band of the same height** instead.
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
- `ide_editor_band` is the **diagnostics band** (`crate::gui::render_ide_diagnostics_band`)
  — declared but empty until 2026-07-19, when the env-overwrite lint moved into it from
  the launch band, joined later the same day by the open draft's status lines (issue #26).
  Its `exact_height(BOTTOM_BAND_H)` matches the nav footer band so the two bottom
  edges align, and it stays `exact_height` however much it ends up showing: a
  variable-height band here would reflow the editor column above it every time the warning
  count changed. That fixed height plus the scroll area inside it is exactly why the status
  lines could move in — however many appear as you type, the band does not move a pixel.
  See "Diagnostics band" below.
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
  - *Profiles* from IDE mode drops the one focused draft (clean or already-confirmed-dirty
    — see `nav_category_drops_draft`) and clears focus; `nav_sel` needs no restoring (S4b),
    since focusing a module never moved it away from a real config scope in the first place.
    Coming from **General Settings** it lands on the ambient game (`NavSel::Game(appid)`) —
    leaving `nav_sel` on `GeneralSettings` would make the box snap straight back to that
    category and the click look dead.
  - *IDE Mode* focuses `all_specs[ide_selected]` (`GuiApp::focus_module`), establishing the
    `GuiApp::focused_module().is_some()` invariant IDE mode's columns rely on. `nav_sel` is
    deliberately left untouched. It also re-runs `GuiApp::refresh_games` so the preview
    column's game selector offers the current set.

    **The mode switch is deliberately *not* gated on a non-empty `all_specs`**
    (2026-07-19, issue #4). With no modules on disk the invariant cannot fire and IDE mode
    comes up with nothing focused — but that is the one state from which the nav footer's
    **+ New Module** button (which renders for `Mode::Ide` regardless of focus) is the only
    route to authoring a first module. Refusing entry would close that route and turn a
    bare screen into a real dead end. What the screen was missing was *copy*: the central
    panel fell through to Config mode's "No extensions apply to this game", which names a
    game IDE mode does not have and blames a game filter that is not why the list is empty.
    `crate::gui::empty_central_message` now picks per mode — a free function rather than a
    string inline in a `ui.label`, so the choice is assertable without driving a render
    pass (same seam as `save_gate` / `nav_category_drops_draft`).

    *`refresh_games` on mode entry only is sufficient, not a staleness bug* (2026-07-19,
    issue #7): the game set can be changed only from Config-mode controls (create / rename
    / delete on the nav tree), and reaching them means leaving IDE mode — so re-entry, which
    refreshes, is unavoidable between any edit and the next look at the selector. The one
    remaining window is a game file changed **on disk by something else** while IDE mode is
    open, and `Ctrl+R` covers that: it calls `reload_configs`, which calls `refresh_games`.
    Nothing inside IDE mode itself touches the game set — `delete_module`'s `config_cleanup`
    rewrites values inside existing game files, never their membership. Per-frame directory
    scanning was rejected as cost with no window to close.
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
  writes the index directly and never focuses the editor, so the click is turned into a
  `GuiApp::focus_module` call *after* the tree renders.

  **`ide_selected` is re-anchored by module id across every reload** (2026-07-19, issue
  #37), the same treatment `rebuild_cur_specs` has always given `selected_ext`.
  *Why:* `context::load_extensions` re-sorts alphabetically by `meta.name` on every call,
  so any reload that adds, removes or renames a module renumbers the rows under a raw
  index. `GuiApp::reload_extensions` therefore remembers `all_specs[ide_selected].id()`
  before the load and calls `GuiApp::anchor_ide_selection` after it; the three operations
  that move focus to a module which did **not** exist under that id a moment ago —
  `perform_fork`, `perform_create`, `perform_rename` — re-anchor a second time on the new
  id, because the id `reload_extensions` remembered is the pre-operation one. (Rename is in
  that list because it changes `meta.name`, the sort key itself: the module changes rows
  without its file moving.)

  *Why the fallback for a vanished id is "keep the index, clamped" rather than "snap to
  row 0":* the id disappears on **delete**, and there the row that shifted into the old
  index is exactly the deleted module's next neighbour — where a list selection should
  land. Row 0 would throw the user back to the top of the tree from wherever they were
  working. The clamp is what keeps the index in range so the `Mode::Ide` reopen invariant
  can still find a module to reopen after the last row is deleted.

  What this fixes, concretely: the tree highlighting a different row than the editor is
  editing after Fork/Create/Rename, and — because `save_module` clears focus and lets the
  reopen invariant re-focus `all_specs[ide_selected]` — **Save** landing the user on an
  arbitrary neighbour instead of the module they just saved.
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
  (2026-07-19). The Config-mode module footer (`GuiApp::render_modules_footer`) has an
  identical-looking Open Folder button; as of 2026-07-19 **it opens the same folder**,
  with the same "Open the user extensions folder" hover tooltip. It used to open
  `games_dir()`, which had nothing to do with the module tree it sits under — both
  buttons live beneath a module tree, so both mean the same thing.

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
  Selecting anything in the tree drops those half-finished states through the single
  `GuiApp::reset_nav_transients` helper (2026-07-19, issue #14), which clears
  `creating_profile`, `duplicating_preset`, `creating_game` and `text_buffers`. *Why one
  method:* the three `NavAction::Select*` arms each repeated those four lines, so a new
  nav-transient field had to be remembered in three places. The two `StartCreate*` arms
  deliberately do **not** call it — they *enter* a transient state rather than leave
  one, and `StartCreateProfile` must preserve `duplicating_preset`, which the "Duplicate
  profile" context-menu item sets just before raising the same `creating_profile` flag.
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
    and Duplicate buttons. Rename and Delete both re-point **every** stored reference to
    the profile — games' `Preset`, other profiles' `Parent`, the general-config
    `DefaultPreset` — via the shared `crate::gui::GuiApp::retarget_preset_references`; see
    [scoped-config.md](scoped-config.md#renaming-or-deleting-a-profile-sweeps-every-reference-to-it)
    for why a leftover reference resurrects when the name is reused (issue #36).
  - *Game* — editable AppID (Enter renames the on-disk file via
    `crate::gui::GuiApp::rename_game_appid`), editable Name, a **Profile** combo
    assigning which preset the game uses, Delete button.
  - *General Settings/Global Profile* — empty; their controls live in the central panel
    instead.

## Which modules a screen lists (`cur_specs`)

`cur_specs` (+ the index-aligned `cur_dirs` / `cur_is_folder_ext`) is the module set the
**currently showing screen** lists, rebuilt by `GuiApp::rebuild_cur_specs` from
`all_specs`. The filter it applies comes from `GuiApp::cur_specs_filter`:

| Screen | Filter |
| --- | --- |
| Game (`NavSel::Game`), manifest editor, General Settings | `Extension::applies_to(appid)` — the ambient game |
| **Global Profile** (`NavSel::GlobalSettings`), **Profile** (`NavSel::Profile`) | none — every loaded module |

*Why the Global Profile and Profile screens take no filter* (2026-07-19): those scopes
apply **across** games. Filtering them by whichever game happened to be selected last is
meaningless — the ambient game is not what the screen is configuring — and it *hid*
modules the user has to be able to set a global/profile value for. Before this, the
filter was applied unconditionally because `rebuild_cur_specs` only ever ran from
`switch_game`.

Mechanics worth knowing:

- **When it rebuilds** — `GuiApp::ui` compares the stored `cur_specs_filter` against
  `cur_specs_filter()` once per frame and rebuilds on a change, so a screen switch costs
  exactly one rebuild. `switch_game` / `reload_extensions` still rebuild eagerly (they
  change the underlying data, not just the filter). *Why the check lives in the frame
  loop* rather than beside each assignment: `nav_sel` moves from a dozen call sites, and
  the next one added would forget it.
- **Selection safety** — `selected_ext` is a raw *index* into `cur_specs`, so a list that
  changes length would otherwise re-point it at a different module. `rebuild_cur_specs`
  re-finds the previously selected module by `Extension::id()` and only falls back to
  index 0 when it is genuinely absent from the new list.
- **This only ever widens the list.** `RequiresDesktop`-gated modules are dropped much
  earlier — at load time in `crate::context::load_extensions`, shared by
  `AppContext::load` and the GUI hot-reload — so they never reach `all_specs` and
  dropping the `AppIds` filter cannot resurrect them.
- **The launch preview keeps the game filter regardless** — `GuiApp::game_specs()` is
  `all_specs` filtered by the ambient `appid` on *every* screen, and it (not `cur_specs`)
  is what the launch-command preview and `ide_specs` assemble from. *Why:* a launch
  command is by definition for one concrete game, so assembling it from modules that can
  never apply to that game would print a command that will never be run. The two lists
  are equal everywhere except the Global Profile / Profile screens.

## Module tree (`ext_list` panel)

`crate::gui::GuiApp::render_ext_tree` renders `cur_specs` (the modules the current
screen lists — see "Which modules a screen lists") either **grouped by Author** (`group_by_author: true`, the default —
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
never part of the complaint. `IDE_HEADER_H`, below, originally imported this same
left = top = 14 spacing to give IDE mode's header band the same breathing room the fix
gave Config mode's, and the *left* margin still is 14 today. Its *top* margin is not,
though: a same-day-later fix (2026-07-19, see the `exact_height(IDE_HEADER_H)` bullet
below) found the band's real content laid out four points taller than `exact_height`
assumed, painting a black bar of framebuffer clear colour under the panel's separator
line. Correcting that dropped the band's top margin to 8 (and raised its bottom to 11) —
deliberately unequal to the 14/10 pair this paragraph describes, because matching
*measured ink-to-edge distance* across the band's edges needs different margin numbers
than matching Config mode's did.

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

**Whose countdown is it** (2026-07-19, issue #2): the "Detecting… Ns" button that
replaces **Detect** is drawn from `crate::gui::GuiApp::detect_countdown_secs`, which
returns `Some(secs)` only when the in-flight `Detect` targets *this* module and field —
`Detect::ext_id`/`var` matched against the rendering module's, plus `target`/`mode`
matched against the live ones (`crate::gui::Detect::targets`). It used to ask nothing
but "is some detection waiting, and is this variable called `window_class`". *Why that
was worth fixing while still latent:* only `hypr-monctl` declares `window_class` among
the shipped modules, so there is never a second field on screen to mis-render into —
but a user's own module under `~/.config/ritz/extensions/` can declare it too, and then
the countdown draws on the user's field while `poll_detect` writes the result to
hypr-monctl. `target`/`mode` are in the comparison because Config mode and the IDE
preview draw the *same* module's fields and only those two axes tell them apart — see
"Preview write guards" below and `cancel_stale_detect`.

**A detection failure cannot take the window down** (2026-07-19, issue #3). Two
independent measures, deliberately belt-and-braces:

1. `start_detect`'s thread computes first and locks second — the guard is never held
   across `detect_active_window_class`, which spawns `hyprctl` and parses its JSON. It
   used to be one statement (`*handle.lock().unwrap() = DetectStatus::Done(detect…())`),
   so a panic in there unwound with the guard held and **poisoned** the mutex.
2. Both GUI-thread readers go through `crate::gui::Detect::status`, which resolves a
   poisoned lock to `DetectStatus::Done(None)` instead of `.unwrap()`-ing.

*Why tolerate poisoning rather than fail fast:* both readers are on the GUI thread —
`detect_countdown_secs` every frame a field renders, `poll_detect` every frame a
detection is live — so `.unwrap()` converts any panic in the detector thread into a
panic in the UI thread, i.e. a failed `hyprctl` costs the user the settings window and
every unsaved draft in it. The poisoning is also not evidence of half-written shared
state: the slot holds one enum and its only writer assigns it whole. *Why `Done(None)`
and not `Waiting`:* a poisoned slot still literally contains `Waiting` (the writer never
got to store), so propagating the contents would strand the countdown at "Detecting… 0"
with no route back to the Detect button; `Done(None)` is the existing "found nothing"
outcome and flows through `poll_detect`'s clear-and-write-nothing path.

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

*Why leg 2 matters:* before S4b deleted `NavSel::ModuleEditor`, that variant was equally
off the render path (its central panel also returns early) yet correctly stayed on the
game arm — precisely because it was a real edit scope reachable through this async path. A
render-only argument would have mis-classified it too. Post-S4b there is nothing left to
misclassify — every real `NavSel` variant now genuinely names a config scope.

### Manifest editor — `render_module_editor`

Opening a module in the **IDE Mode** tab routes through `GuiApp::focus_module`, which sets
`GuiApp::focused_module: Option<String>` — a plain field, independent of `nav_sel` (S4b,
2026-07-19; see "Two modes" above) — whose central panel is a full editor for the module's
*manifest* (not its config values). *Why the IDE Mode tab is now the only route*
(2026-07-19): the Config-mode module detail header used to carry an **✎ Edit** icon
button calling the same handler, and **Ctrl+E** used to open the editor for
`cur_specs[selected_ext]`. Both were removed as redundant entry points now that IDE
Mode owns module authoring — the Edit button because a second door into the editor
from a read-only view was confusing, Ctrl+E because it was keyed off `selected_ext`,
an index that only exists because the Config-mode `ext_list` column exists (see
`docs/brainstorm/ide-mode.md`, which staged it as "removed for now, not ported"). The
keybinding may be revisited once IDE Mode has its own notion of a selected module.

- **Entering / leaving** — `GuiApp::focus_module(id)` sets `focused_module = Some(id)`,
  immediately builds that module's draft via `GuiApp::sync_focused_draft`, and
  does not touch `nav_sel` at all (S4b) — there is no "prior view" to remember any more,
  because focusing a module never moves `nav_sel` away from whatever real config scope was
  already selected. Which module the editor is *focused* on is read through the single
  accessor `GuiApp::focused_module() -> Option<&str>`; drafts themselves are keyed
  independently (see **Multi-draft** below), so leaving the editor no longer destroys
  anything but the focused draft. `GuiApp::close_focused_draft` (renamed from
  `exit_module_editor` in S4b, which is what pre-S4b restored a remembered `nav_sel` via
  `editor_return`/`editor_exit_target` — both deleted, nothing left to restore) removes
  that one entry and clears focus. The header's
  always-present close button (labelled **Discard** in IDE mode, **✕ Close** in Config mode;
  both `TopAction::Close`) acts on the *focused* draft only — with an editable draft that
  holds unsaved work (`has_unsaved_work`, so a staged rename counts, issue #34) it
  opens the `ConfirmAction::DiscardEdits` modal first and only `discard_module()`s on
  confirm, which in IDE mode re-seeds that draft from disk and stays on the same module.
  **Save** also clears focus on success (`GuiApp::close_focused_draft`), so the
  `Mode::Ide` reopen invariant lands the user back on the real (now-saved) module the very
  next frame.

  *Why `focus_module` builds the draft on the spot instead of leaving it to the next
  frame's top-of-frame sync* (2026-07-19, issue #5): both interactive routes that move
  focus — a click on an IDE tree row, and entering IDE mode via `select_nav_category` —
  run inside `render_nav_panel`, which `GuiApp::ui` declares **after** its top-of-frame
  `sync_focused_draft` but **before** the header band, the editor column and the preview
  column. Setting focus alone therefore left those three rendering against a
  `focused_module` with no draft behind it for exactly one frame: `editor_header_info`
  returns `None`, the editor column prints "Loading…" and the preview column blanks, all
  self-healing on the next frame — a visible flash on the mode's most common gesture.
  Syncing at the point of the focus change removes the window outright rather than racing
  the panel declaration order, so the frame-order comments in `ui()` (load-bearing for the
  *panel rects*) have one less thing riding on them. `sync_focused_draft` bundles
  `ensure_draft` + `refresh_draft_name_error` + `refresh_identity_state` because those
  three must stay together — a draft whose two error states were not refreshed is a draft
  the Save gate reads stale. *Why no separate Discard button:* Close already means
  "leave without saving"; two buttons for one outcome, with the warning attached to only one
  of them, was the confusing part.
- **Leaving the editor with unsaved edits always asks first** (2026-07-19) — the three
  **category tabs** (Profiles / IDE Mode / Settings) sit *above* both locked trees and
  used to switch instantly, so clicking Profiles or Settings from a dirty editor let the
  frame-start nav-away guard destroy the draft without a word. That was silent data loss,
  and it is now gated: `GuiApp::render_nav_panel` raises
  `ConfirmAction::DiscardEdits { then: DiscardThen::Nav(cat) }` instead of applying the
  click. Confirm discards the draft **and completes the switch**; Cancel leaves the editor
  and the draft exactly as they were.
  - **The prompt names exactly what it destroys, and destroys no more than the
    destination requires** (2026-07-19, issue #35). Originally the dialog listed
    `GuiApp::unsaved_module_names` (*every* unsaved draft) while
    `GuiApp::apply_discard_edits` ran `drafts.clear()` (*every* draft, unsaved or not) —
    two different sets, neither of which matched what any destination actually needed.
    Three things changed, and they are one principle applied three times:
    - `DiscardThen::Nav(cat)` **no longer clears the map**. It calls
      `GuiApp::select_nav_category(cat)` and lets the destination drop what it drops, so
      the confirmed path and the plain (nothing-unsaved) path are the same code and
      cannot drift.
    - **`NavCategory::GeneralSettings` no longer prompts at all.** `select_nav_category`
      keeps every draft there and only clears `focused_module` — so the old prompt was
      offering a "yes" that `drafts.clear()` then made expensive. *Why this is the whole
      bug in miniature:* the confirmation created a loss the destination never required,
      and Cancel was the only non-destructive answer. A destination that costs nothing
      must not ask a question.
    - **`NavCategory::GamesProfiles` asks about, and destroys, the focused draft only.**
      Its destination calls `GuiApp::close_focused_draft` — one draft — so
      `nav_category_drops_draft` now takes `GuiApp::focused_draft_unsaved` instead of
      `!unsaved_drafts.is_empty()`. A background draft the click would never touch is no
      longer a reason to prompt.

    The dialog's list comes from `GuiApp::discard_names(then)`, keyed on the
    destination, not from `unsaved_module_names`: focused-draft-only for `CloseEditor`
    and `Nav(GamesProfiles)`, empty for the two destinations that drop nothing, every
    unsaved draft for `ExitApp`. *Why `ExitApp` still clears the whole map while naming
    only the unsaved ones:* a clean, unstaged draft is byte-identical to its manifest and
    is rebuilt by reopening the module, so dropping it destroys nothing a user could
    notice — naming it would pad the prompt with modules that are not at risk. The
    map-clear is a cache eviction for those entries and a real loss only for the ones
    listed.
  - *Why the destination is a payload on the existing variant* rather than three
    variants: all three cases render the identical dialog and differ only in what runs
    after Confirm. `DiscardThen::CloseEditor` is the editor's own Close button (Close
    doubling as Discard), which drops the focused draft via `discard_module` and clears
    focus — there is no separate "remembered view" to land on any more (S4b); in IDE mode
    the reopen invariant picks the tree's current selection up again the next frame.
    `DiscardThen::ExitApp` is the whole window closing — see "Closing the window" below.
  - **Only when there is unsaved work.** A clean draft is byte-identical to disk *and* has
    no staged rename, so dropping it loses nothing and prompting on every tab click would
    be pure noise. The gate is `ModuleDraft::has_unsaved_work`, not `dirty()` — see
    "Unsaved work vs. dirty" below. The pure predicate
    `nav_category_drops_draft` decides *which* clicks even qualify: **Profiles** does
    (it drops the focused draft), while **General Settings** and **IDE Mode** never do —
    since S4a made `drafts` a keyed map, switching modules inside IDE mode destroys
    nothing, and General Settings keeps every draft too, so a prompt on either would be
    pure noise. (Pre-#35 both config destinations prompted on `any_unsaved`; see "The
    prompt names exactly what it destroys" above.)
  - The prompt **names the modules at risk** — "These modules have unsaved changes:
    Author::Name." — from `GuiApp::discard_names`, which returns a list even though
    S3b only ever holds one draft, so multi-draft (S4) grows the list without rewording
    the dialog. Names come from the on-disk identity (`ext.meta`), not the staged rename
    buffers: a half-typed rename is not what the user recognises the module by.
  - Confirm runs `GuiApp::apply_discard_edits`, a method (not an inline match arm) *so it
    can be tested* — inline in `render_confirm_dialog` it is only reachable through a
    live egui frame.
  - **Still unguarded, by design:** switching modules *within* the IDE tree replaces a
    dirty draft silently. That tree is deliberately not lockable (see above) and per-row
    dirty markers are the S4 answer; likewise Fork/Create from a dirty draft drops it.
- **Closing the window with unsaved edits asks the same question** (2026-07-19, issue
  #33) — closing the settings window used to drop every open draft in silence. It now
  raises **the same `ConfirmAction::DiscardEdits` dialog** the Profiles-tab switch does:
  same title, same "These modules have unsaved changes: …" list, same buttons, carrying
  `DiscardThen::ExitApp(outcome)`. *Why the same dialog and not a bespoke "quit anyway?"
  prompt:* the user asked for exactly that — "when I close, it shows the same discard
  dialogue that I get when I would switch to profiles" — and a second prompt would be a
  second thing to keep in step with the first.
  - **Both close entry points are guarded**, which is the whole point. There are exactly
    two in `gui.rs`:
    1. the **OS close** — X button, Alt+F4, window-manager close — intercepted by
       `GuiApp::handle_close_request`, which reads `close_requested()` near the top of
       `GuiApp::ui` (right after the frame's `refresh_unsaved_drafts`, before any panel
       renders) and answers with `egui::ViewportCommand::CancelClose`;
    2. the launch-mode title-bar **"Launch Game" / "Cancel Launch"** buttons, which now
       call `GuiApp::request_app_close` instead of sending `Close` themselves.

    *Why (2) matters as much as (1):* it is the more frequently clicked path, and this
    codebase already had three bypasses of exactly this shape (Ctrl+S around the Save
    button's guard, Ctrl+E around its button, Ctrl+R around the modal). So the file now
    has **one** `ViewportCommand::Close` send site — the tail of `GuiApp::ui`, driven by
    the `GuiApp::pending_close` flag. Everything that wants the window shut sets that
    flag; there is no second door to leave unguarded. (`splash.rs` has its own two
    `Close` sends; the splash holds no drafts and is a separate window.)
  - **The gate is `has_unsaved_work()`, never `dirty()`** — the pure predicate
    `close_needs_confirm(any_unsaved, already_approved)` reads the same frame-cached
    `unsaved_drafts` set the tab guard does. A staged-but-uncommitted rename is unsaved
    work and `apply_discard_edits` destroys it along with the draft; asking `dirty()`
    here would reintroduce issue #34's blind spot on the *one* path where the loss is
    permanent, because the process exits straight afterwards. **No unsaved work → the
    window closes on one click**, no dialog: the common case must not pay for the guard.
  - **The close genuinely happens after Confirm.** `CancelClose` makes eframe skip
    setting its internal `close` flag, so nothing closes the window afterwards unless it
    is asked again — and `ViewportCommand::Close` only arrives back as `close_requested()`
    on the *following* frame. Confirm therefore sets `pending_close`, whose two jobs are
    (a) making the tail of `ui()` re-send `Close` and (b) making `close_needs_confirm`
    wave that re-issued close through, so the guard doesn't cancel its own approved close
    forever.
  - **Launch mode still launches.** The outcome (`EditOutcome::Continue` / `Cancel`) is
    carried *in* `DiscardThen::ExitApp` and applied only when Confirm runs — not when the
    button is clicked. *Why:* a user who clicks "Launch Game", sees the prompt and cancels
    must not be left with `outcome` silently flipped to `Continue`, or the next plain
    X-close would launch the game instead of honouring their "Editor closing action"
    setting. *Why Launch is guarded at all, given the issue is about closing:* launching
    **is** closing — the window goes away and the drafts go with it, so the loss is
    identical. Auto-saving drafts on launch was rejected instead: Save has its own gate
    (validation, name collisions, the pending-rename prompt), and writing a half-valid
    manifest unasked at launch time is worse than one question.
  - **Re-entrancy: an existing dialog wins.** If a close is requested while any
    `ConfirmAction` is already on screen, the close is cancelled and the pending dialog is
    left untouched rather than overwritten. *Why:* clobbering a half-made decision (say a
    pending "Delete Module") would silently cancel something the user had already chosen,
    and the modal backdrop means only one question can be answered at a time anyway.
    Pressing Alt+F4 again after answering works.
  - **What is not covered by tests:** the viewport round-trip itself. `close_requested()`
    / `CancelClose` / `Close` exist only inside a live eframe event loop, which needs a
    window server. The tests cover the *decision* — `close_needs_confirm`, the prompt
    `request_app_close` raises (including for a rename-only draft), what
    `apply_discard_edits(ExitApp)` does to drafts/outcome/`pending_close`, Cancel leaving
    the app usable, and the re-entrancy rule.
- **Locked trees while editing** — the nav-away guard above is now a *backstop*, not the
  normal path: while a draft exists the **MODULES tree** is wrapped
  in `ui.add_enabled_ui(module_draft.is_none(), …)`, so it greys out and can't swap the
  module out mid-edit; while the draft holds **unsaved work** the **left nav** (Profiles /
  Games / General / Global) is likewise disabled, so a stray click can't silently discard
  unsaved edits. *Why the left nav is only locked then:* it retargets the editor's live
  launch preview, which is useful — the lock exists to protect unsaved edits, not to pin
  the preview. Exit stays Close / Save (or Ctrl+S).
- **Keybinds** (handled at the top of `GuiApp::ui`, before panels render) — **Ctrl+S**
  triggers Save when the editor is open and `ModuleDraft::save_enabled` holds (same gate
  as the button), a no-op otherwise; **Ctrl+R** hot-reloads extensions and configs
  through `GuiApp::request_reload`. The `command` modifier means text editing is never
  swallowed. *There is deliberately no Ctrl+E* — see the entry-point note at the top of
  this section.

  Both shortcuts are **guarded on `confirm.is_none()`** (Ctrl+R since 2026-07-19, issue
  #38 — Ctrl+S always was). *Why:* the confirmation dialog's backdrop eats **pointer**
  input only, so a keyboard shortcut fires straight through an open modal; the toolbar
  buttons that run the same two reloads individually are genuinely blocked by the
  backdrop and need no guard, which is exactly the inconsistency #38 closed. *Why it
  matters even though it cannot corrupt anything* — and it can't: `perform_rename`
  re-checks `identity_error` at execution time and re-derives its target from the live
  draft, so a dialog's captured state is advisory. The cost is a smaller data loss:
  `reload_configs` → `switch_game` clears `text_buffers` and `multi_edit`, and any row
  that has not yet reached `cleaned` — a freshly added blank multi_string row, an env
  pair with an empty or invalid NAME — lives *only* in `multi_edit`.
- **Focus stability (no reflow steal)** — the dirty/identity/validation status lines that
  appear on the first keystroke must not knock the focused `TextEdit` out of focus. Two
  mechanisms guarantee this: (1) the "unsaved changes / All changes saved" line is
  **always rendered** (greyed when clean) so the clean→dirty transition never inserts a
  line above the fields — and in IDE mode, where the lines now live in the diagnostics
  band, they are out of the editor column altogether and cannot shift the body at all
  (issue #26); (2) the body `ScrollArea` carries an explicit `id_salt`
  ("module_editor_body") and **every** editor `TextEdit` a stable `.id_salt` keyed on its
  structural path (section/field/block index + role), so a focused box keeps the same egui
  id — and thus focus — even if a banner above it appears and shifts its screen position.

- **Header / status / body split** (2026-07-19) — `render_module_editor` is assembled from
  three pieces rather than one straight-line function:
  - `GuiApp::editor_header_info(id) -> Option<EditorHeaderInfo>` snapshots the name,
    version, author and every gate flag (`editable`, `dirty`, `unsaved`, `save_on`,
    `has_identity`, `identity_err`, `sections_unique`, `validate_err`, `req_ok`) out of the
    draft. `dirty` is the body-vs-disk check that explains the Save gate; `unsaved` is
    `has_unsaved_work` and is what the state line and Close read. It takes
    `&self` only, so it can run **before any panel is declared**.
  - `render_editor_header_row(ui, cache, info, ide_mode) -> TopAction` draws the
    name/version/author heading and the `[Fork] [Delete] [✕] [Save]` (+ `Rename`)
    cluster, and *returns* the click rather than acting on it. `ide_mode` hides
    Save/Close for a bundled module in IDE mode only — see "IDE-mode header band"
    above.
  - `editor_status_lines(info) -> Vec<StatusLine>` builds the dirty/validation/identity
    messages **once**, and two surfaces lay that one list out two ways:
    `render_editor_status_lines(ui, info)` stacks them as ordinary labels in the
    Config-mode editor body, painting by `StatusLine::config_color`; IDE mode feeds them
    through `ide_diagnostic_entries` into the diagnostics band, painting by
    `StatusLine::severity` (issue #26). *Why an entry carries both a colour and a
    severity:* Config's state line is green when dirty, grey when clean and red when
    bundled — three colours for what is one informational message under the severity
    vocabulary, so severity cannot be mapped back onto Config's palette without changing
    Config mode. Splitting the list from its layout is what keeps the two surfaces from
    drifting — each only ever shows one mode's version, so a divergence would be
    invisible in use.

  *Why the snapshot struct rather than passing `&ModuleDraft`:* IDE mode renders the header
  row in a **different panel closure** from the body (see "IDE-mode header band"), while the
  body holds `&mut self.module_draft` for its whole scope. Handing the header owned, plain
  data means neither closure borrows the draft while the other runs — that decoupling is
  what makes the full-width header band possible at all. The two free functions take the
  `IconCenterCache` directly for the same reason.

  `render_module_editor`'s `header_inline: bool` parameter selects between the two layouts:
  Config mode passes `true` (header drawn in the editor column, action dispatched there);
  IDE mode passes `false` and owns both itself. **The status lines follow the header row**
  (2026-07-19, issue #26): `render_module_editor` draws them only on the `header_inline`
  path, and IDE mode draws them in its diagnostics band instead — rendering both would put
  them on screen twice. Gated on `header_inline` rather than `self.mode` because `header_inline` is
  what actually means "this column owns the header"; the two agree today, and if a third
  call site ever disagrees, the header and its status lines should still travel together.

  `GuiApp::dispatch_top_action(action, id, unsaved, editable)` carries out the click and is
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
- **Optional-text write-backs are gated on `Response::changed()`** (2026-07-19). Every
  `Option<String>` in the manifest — `Description`, `Backend`, a field's `Name`/`Requires`,
  an env builder step's `Value`/`Separator` — is edited through a scratch `String` that
  collapses `None` into `""` on the way in (`opt_text_edit`, `requires_edit`, and the
  inline `Name`/`Value`/`Separator` rows). The write-back back out of that scratch runs
  **only when egui reports the widget actually changed**.
  *Why:* writing it back unconditionally re-encoded `Some("")` as `None` on every frame,
  including the first — so simply *opening* a module dirtied its draft before the user
  touched anything. Three bundled manifests ship an empty-string value (`"Value": ""` in
  `amd.json` and `misc.json`, `"Requires": ""` in `dxvk.json`), so those three, and only
  those three, grew a spurious dirty marker in the IDE tree plus an "unsaved changes"
  prompt on the way out — on modules that are read-only and cannot be edited at all
  (`add_enabled_ui(false, ..)` greys a widget but does not stop the plain assignment after
  it). It is *not* a `changed()`-free normalisation because `""` and absent are genuinely
  different for `Separator`: `builder.rs` reads an absent separator as `","`. Regression:
  `rendering_a_draft_without_touching_it_leaves_it_clean` renders every bundled manifest
  headless with no input and asserts the draft stays clean.
- **Rendering a Selection field must not create its `Options` list** (2026-07-19, the
  structural sibling of the rule above). The Selection arm of the field editor reads its
  option rows through `selection_options(&mut field.options)`, which yields an **empty
  slice** when `Options` is absent or holds an `OptionsSpec::Range`. The mutating
  `ensure_list` — which installs `Some(Options: [])` — is now reachable **only** from
  `Deferred::FieldOptAdd`, i.e. a click on "Add option".
  *Why a shape change and not a `changed()` gate:* the text write-backs mutate *after*
  their widget, so there is a `Response` to gate on; `ensure_list` had to run *before* the
  option rows could be drawn, because it was what produced the `&mut Vec<String>` to draw
  from. There was nothing to gate. So the renderer stopped needing the list to exist.
  No bundled manifest reaches either trigger state (every shipped Selection field has an
  array `Options`), which is why this one stayed latent: only a user-authored module — a
  Selection driven by `DisplayOptions` alone, or one just switched over from Integer and
  still carrying a `Range` — would have opened dirty. Regression:
  `rendering_a_selection_field_never_materialises_its_options`.
- **Staged identity edits vs Save** (Phase 3 stage 2b) — for an editable module, Author,
  Name and every *existing* field's `Variable` are editable, but their edits go into a
  separate `PendingIdentity` (`identity.author` / `identity.name` /
  `identity.var_edits[current-name]`), **never** `ext.meta` or `field.variable`. Because
  `ModuleDraft::snapshot` reads only `ext`/`sections`, identity edits never mark the draft
  `dirty()` and never enable Save — they commit
  *only* through the explicit **Rename** action. A *newly-added* field's `Variable` is still
  edited directly (no config to orphan). Version stays fixed. `ModuleDraft::name_error`
  (on-disk identity, `GuiApp::refresh_draft_name_error`) feeds the Save gate and stays
  `None` for an existing module; the *pending* identity's validity is a separate
  `identity_error` (`GuiApp::refresh_identity_state` → `ModuleDraft::compute_identity_error`).
- **Module identity is always trimmed — at every boundary** (2026-07-19) — Author, Name
  and field `Variable` are stripped of surrounding whitespace when they are **loaded**,
  when they are **saved**, and when they are **read out of a text box**. The three
  boundaries:
  - **Load & save (structural).** `ritz_core::schema` gives `ExtensionMeta::name`,
    `ExtensionMeta::author` and `UiField::variable` a trimming `deserialize_with`
    (`de_trimmed`) *and* `serialize_with` (`ser_trimmed`). Every manifest read is
    therefore trimmed no matter who wrote it, and every manifest write is trimmed no
    matter what is in memory. This is one place, not a `.trim()` per call site.
  - **Read-from-textbox.** The live buffers are read through
    `PendingIdentity::staged_author()` / `staged_name()` / `staged_var_changed()`.
    Everything downstream goes through them: `has_pending_identity`,
    `compute_identity_error`, `draft_identity_collides`, `refresh_identity_state`,
    `perform_rename`, and the
    `(rename pending)` notes under Author / Name / Variable. The Fork/Create dialog
    already trimmed at its own boundary (`render_module_dialog`).

  *Why:* trailing whitespace is **invisible**, but `==` sees it. `has_pending_identity`
  used to compare the raw buffers, so one typed space made a module report a pending
  rename it did not have — which lit the `(rename pending)` note, the diagnostics-band
  warning and the Rename button, put the module into `unsaved_drafts`, and made the
  close/nav discard prompt claim work that did not exist. In the other direction a
  hand-edited manifest carrying `"Name": "Foo "` loaded as a module that could never
  match itself: permanently pending, with config stored under a key nothing would ever
  resolve. Neither failure showed the user anything to look at.

  > **Do not "simplify" this into an in-place trim.** `identity.author` / `identity.name`
  > and the `var_edits` buffers are live `TextEdit` buffers, and module names
  > legitimately contain interior spaces ("LSFG VK"). Normalising the buffer each frame
  > deletes the space the instant it is typed, so the second word can never be reached
  > and the field becomes impossible to type into. **Trim on read; never mutate the
  > buffer.** Regressions: `whitespace_only_identity_edit_is_not_a_pending_rename` (which
  > also asserts an interior space survives),
  > `untrimmed_manifest_loads_trimmed_and_presents_as_clean`,
  > `saving_writes_trimmed_identity_even_if_memory_is_untrimmed`.

  **Section names** are the same class and are handled the same way: `ModuleDraft::snapshot`
  folds them into `ext.ui` trimmed, and `sections_unique` compares them trimmed. *Why:*
  `ext.ui` is an `IndexMap`, so `"General"` and `"General "` are two visually identical
  keys — untrimmed, one of them silently disappeared on Save instead of being reported as
  the duplicate it is.

  **`Requires` expressions** are the third member of the class (2026-07-19, issue #29).
  `requires_edit` decided `None`-vs-`Some` on `s.trim()` but stored the raw `s`, so `"  "`
  correctly became `None` while `" enabled "` kept its padding. `gui::normalize_requires`
  now trims every `Requires` in the manifest — `UiField`, `ENV_VARS`/`GAME_ENV_VARS` and
  their builder steps, `WRAPPERS` and their builder steps, `GAME_LAUNCH_ARGS` — in the same
  `snapshot()` fold as section names.

  *Why it is data hygiene and not a behaviour fix:* `condition::tokenize` skips whitespace,
  so `" enabled "` and `"enabled"` always parsed to the same AST. What the padding broke was
  `dirty()`, which is a **serialized-value** comparison — two logically identical drafts
  compared unequal, re-triggering the spurious-dirty class `6f47e5a` fixed — and the padding
  also landed verbatim in the written manifest.

  *Why at the snapshot fold rather than in `requires_edit`'s write-back*, which is what the
  issue proposed: `val` **is** the live `TextEdit` buffer, so trimming on write-back eats a
  trailing space the instant it is typed and the user can never get from `enabled` to
  `enabled AND !clear` — the identical hazard the block-quote above describes for identity
  buffers. Trim on read, mutate the buffer never.

  > **`normalize_requires` trims but must never collapse `Some("")` to `None`.** Bundled
  > manifests ship an explicit `"Requires": ""` (`dxvk.json`), so `Some("")` is a real
  > on-disk state; nulling it makes `snapshot()` differ from `baseline` and merely *opening*
  > DXVK marks it unsaved with nothing on screen to change back. Empty and absent both mean
  > "always true" to `condition::eval_opt`, so keeping them distinct costs nothing and keeps
  > the round-trip byte-exact. Caught by the existing
  > `rendering_a_draft_without_touching_it_leaves_it_clean` and
  > `an_empty_string_in_an_optional_slot_survives_a_render`; new regression:
  > `requires_padding_is_canonicalised_at_the_snapshot_boundary`.
- **Save and Rename are independent operations** (2026-07-19) — **Save writes the body
  and only the body; Rename writes the identity and only the identity.** Either can be
  pressed with the other's work still pending, in either order, with no coupling gate
  and nothing discarded. Save additionally *notifies* about a staged rename before
  writing — an advisory prompt, not a coupling; see "the advisory rename notice" below.

  *Why (the user's reasoning, verbatim):* "Pressing the Rename button currently also
  saves the whole config, which is wrong. Those two should be independent, like rename
  should actually only do the renaming, and saving should actually only do the saving."
  Offered "fully separate" against two merging alternatives, the user chose fully
  separate.

  What each half does with the other's pending work:
  - **Save** (`GuiApp::save_module`) writes `snapshot()` and leaves `identity`
    untouched — not committed, not cleared, not discarded. With an identity staged it
    also **does not drop the draft and does not close the editor**: the draft *is*
    where the staged rename lives, so closing would destroy it. Instead the draft is
    re-based in place (`baseline` / `baseline_vars` ← what was just written), leaving
    `dirty() == false`, `has_pending_identity() == true`, `has_unsaved_work() == true`.
    With nothing staged, Save keeps its historical outcome (drop draft, reload, close).
  - **Rename** (`GuiApp::perform_rename`) — see the next bullet.
  - The Save button and Ctrl+S share one entry point,
    **`GuiApp::request_save_module`**, which carries both the "not while a modal is up"
    guard (the dialog backdrop eats pointer input but not key presses, so a held Ctrl+S
    would otherwise write under an open confirmation) and the rename notice below. *Why
    one funnel:* the button and the shortcut diverging was a real bug once — whatever
    Save has to say, it must say from both.
  - Config mode is unaffected: `focused_module` is only ever set under `Mode::Ide`, so
    `draft()` is `None` and `save_module` returns immediately.
  - Regressions: `save_with_a_staged_rename_writes_the_body_and_leaves_the_identity_staged`,
    `save_with_a_valid_staged_rename_prompts_instead_of_writing`,
    `confirming_the_rename_prompt_applies_the_rename_and_not_the_body`,
    `cancelling_the_rename_prompt_leaves_both_edits_pending`,
    `save_with_an_invalid_staged_rename_still_writes_only_the_body`,
    `save_without_a_staged_rename_does_not_prompt`,
    `rename_writes_the_new_identity_and_leaves_the_body_edit_pending`,
    `rename_succeeds_with_an_invalid_body_in_progress`,
    `rename_with_an_invalid_identity_does_nothing_and_keeps_the_editor_open` (all but
    the last two use real files in a scratch base dir — the point of the split is what
    lands on disk, so a mocked writer would only re-assert the mock).

  **The advisory rename notice** (`ConfirmAction::SaveWithPendingRename`, 2026-07-19) —
  pressing Save while `ModuleDraft::identity` holds a staged Author/Name/`Variable` edit
  raises a modal instead of writing:

  - **Committable staged rename** → non-destructive **"Apply Rename"**, which runs
    `GuiApp::perform_rename` and **stops there**. The body edits stay pending and the
    user presses Save themselves. *Why not a rename-then-save combo:* that combo
    (`GuiApp::rename_and_save`, deleted) is exactly the coupling the user rejected. The
    user's acceptance criterion, verbatim: "you got to first save your rename and then
    you can save your changes".
  - **Uncommittable staged rename** (`identity_error.is_some()` — empty Author/Name, an
    Author+Name collision, a bad var rename) → the affirmative becomes **"Save
    Changes"**, a body-only `save_module`, and the dialog names the blocking reason.
    *Why the affirmative flips rather than being disabled:* `perform_rename` filters on
    `identity_error.is_none()`, so an "Apply Rename" button here would appear to work
    and silently do nothing — worse than no button. Flipping it also keeps the
    **body-only escape reachable**: a user with an unresolved name collision staged must
    never be locked out of saving legitimate body work. It is not destructive — the
    staged rename survives the save, ready to fix.
  - **Cancel** → nothing happens; the staged rename and the body edits both stay exactly
    as they were.
  - **No staged identity** → no dialog at all; Save writes immediately. The notice is
    about one specific situation, not a new confirmation on the editor's commonest
    gesture.
  - **No `blocked` payload.** The variant is a unit variant; `identity_error` is read
    **live** off the draft at render *and* at confirm time (`GuiApp::apply_save_with_pending_rename`,
    shared by the renderer and the tests so the two cannot drift). *Why:* the ancestor
    variant captured it as `SaveWithPendingRename { blocked }`, and captured values going
    stale is a known hazard here — `DiscardEdits` reads its module names live for the same
    reason. The modal backdrop means nothing can move between render and confirm anyway,
    so a copy buys nothing and can only be wrong.

  *Why the prompt exists at all now that it guards nothing (2026-07-19):* it is
  **advisory, not protective**. Its ancestor prevented real data loss — Save wrote the
  body under the **old** identity and then `drafts.shift_remove(&manifest)` deleted the
  draft, taking the staged rename with it, *silently* (`PendingIdentity` sits outside
  `snapshot()`, so the Save gate is opened purely by *body* edits and never sees it).
  That hazard is gone: `save_module` now keeps the draft. The prompt was briefly deleted
  along with it, and the user asked for it back — verbatim: "It should still behave the
  same way when I press save and there is a rename open, it should notify me, you got to
  first save your rename and then you can save your changes, just like before. The only
  part that I didn't like was when I press rename it also automatically saved, and
  that's gone now." So only the *auto-save coupling* was unwanted; the *notification*
  was wanted. The reason it still earns its place: because the identity edit lives
  outside `snapshot()`, a Save that silently ignores it looks — to someone who just
  typed a new name — like the rename went out with it. The modal keeps the user oriented
  about which of their two pending edits this button commits.

  The `confirm_label` / `destructive` overrides in `render_confirm_dialog` exist for this
  arm: it is the one variant whose button is neither "Confirm" nor red.
- **Rename / identity migration** (Phase 3 stage 2b, `GuiApp::perform_rename`) — a **Rename**
  header button, enabled only when `has_pending_identity() && identity_error.is_none()`.
  `identity_error` rejects an empty/colliding (Author, Name) (`name_collides`, Version-blind,
  self excluded by path) and any bad var rename via the pure `validate_var_renames`: an
  empty target, a **chained** rename (a new name equal to another live variable — would
  strand config, do them one at a time), or two vars renamed to the same name. On commit,
  in **exactly** this order: (1) compute `from` = on-disk `(Author,Name)`, `to` = new pair,
  `var_rename` = changed existing-field vars; (2) **scope sweep FIRST** —
  `config::migrate_renamed_module` moves stored config across every scope; on error, abort
  **without** touching the manifest; (3) build the new manifest from the **on-disk body**
  (`ModuleDraft::baseline`, *not* `snapshot()` — see below) — apply
  the new Author/Name and rewrite in-module references via `schema::apply_var_renames`
  (each `{old}` interpolation token → `{new}`, each exact `old` identifier in a `Requires`
  → `new`, plus each field's own `Variable`; exact-token match so `old_thing` / `global:old`
  survive); (4) write the manifest **LAST, in place** (file never renamed; `id()` comes from
  JSON meta) via `config::write_atomic`. *Why this order:* the sweep is idempotent and the
  manifest is written last, so a crash between (2) and (4) leaves the manifest on the OLD
  identity — re-pressing Rename re-runs cleanly (already-moved scopes no-op). No WAL/journal.
  Dropped vars reuse the `carryover_report` banner; the editor stays open on the new `id`.
  **`perform_rename` returns `Result<(), RenameError>`** (2026-07-19, issue #30), with one
  variant per failure mode: `NotApplicable` (the gate refused — nothing was attempted, and
  nothing on disk or in memory was touched), `Migration(String)` (step 2 failed; the
  manifest was deliberately not written) and `ManifestWrite(String)` (step 4 failed, after
  the sweep had already landed — idempotent, so re-pressing Rename retries cleanly).
  *Why an enum and not the `Result<(), String>` the issue suggested:* "refused" and "tried
  and broke" are genuinely different answers for a caller deciding whether to fall back to
  another action, and a string cannot carry that distinction without the caller matching on
  prose. **The banner is unchanged and is not replaced by the return value** — it is the
  *user's* channel and every failure still surfaces there (or on stderr) exactly as before;
  `RenameError` is the *caller's* channel. Before this, the only signal was the banner, so a
  caller had to infer the outcome from how the draft happened to be left afterwards — an
  inference that worked by coincidence rather than contract, and that stopped holding once
  075183b changed the post-rename draft bookkeeping.
  This step order is load-bearing and must not be reordered by anything that reuses it.

  **Rename writes the identity only** (2026-07-19 — the independence decision above).
  Step (3) used to build the manifest from `snapshot()`, i.e. the whole *in-memory*
  body, so Rename silently saved the body too. It now builds from
  `ModuleDraft::baseline`. *Why `baseline` and not a re-read of the file:* `baseline`
  **is** the on-disk state by definition — it is the value `ModuleDraft::dirty` measures
  against. A re-read would be a second source of truth that can disagree with it (an
  external edit, a text-vs-serde round-trip difference), and step (6) re-bases the draft
  onto exactly these bytes, so a disagreement would leave `dirty()` answering about
  state the write never saw. `from` (the identity the scope sweep migrates *away* from)
  is read off the same value for the same reason.

  **The `!dirty() || save_enabled()` gate is gone.** It was added earlier the same day
  because Rename wrote the body and could therefore commit unsaved — even
  `extension::validate`-rejecting — edits behind the user's back. Rename no longer
  writes the body, so a dirty or invalid body is not a reason to refuse a rename;
  renaming a module with a half-finished field in progress must work. The surviving
  gates are `editable && has_pending_identity() && identity_error.is_none()`.

  **Step (6), the draft bookkeeping** — the subtle half. After the write, disk holds the
  NEW identity and the OLD body. The draft used to be dropped and re-seeded from disk
  via `ensure_draft` (correct only while disk and draft agreed, i.e. while Rename also
  wrote the body); re-seeding now would throw the pending body edits away. So the entry
  is **re-based in place** instead — it never needed re-keying, since the manifest is
  rewritten in place and `drafts` is keyed by path. Updated: `id`, `baseline`,
  `baseline_vars`, and `identity` (re-seeded from the new on-disk identity, so nothing
  reads as pending). The live body travels across the rename too — its Author/Name are
  set to the new pair and `apply_var_renames` runs over it. *Why rename the live body's
  variables as well:* otherwise a later Save would write the OLD variable names back,
  undoing the manifest half of the rename while the config sweep had already moved the
  stored values under the new ones. Its UI fields make that trip through **one synthetic
  section** and are split back by the original per-section counts, because
  `Extension::ui` is an `IndexMap` and folding `sections` into it would collapse two
  identically-named sections — legal in a draft mid-edit, blocked only at the Save gate.
  Resulting state: `has_pending_identity()` false, `dirty()` unchanged (body edits
  against an equally-renamed baseline), `has_unsaved_work()` therefore exactly "there
  are body edits". Pinned by
  `rename_writes_the_new_identity_and_leaves_the_body_edit_pending`.
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
  A label wider than the reserve does **not** clip — `(EDITOR_LABEL_W - used).max(0.0)`
  simply stops padding — it silently pushes that one row's control right and breaks the
  alignment. `every_editor_row_label_fits_the_label_column` measures every label in both
  UI fonts against the 96px reserve so that can't land unnoticed; widest today is
  "Description" at 73px (mono, the wider font). Add a row, add its label to the test's
  `EDITOR_ROW_LABELS`. *Naming (2026-07-19):* the env-var row says **"Var Name"**, not
  "Name" — three other editor rows already say "Name" for a module / section / field
  label — and the builder step says **"Operation"**, not "Op", which next to "Value" and
  "Separator" read as a third noun rather than the verb it is.
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
  unparseable `Requires` blocks Save. The stored value is left untrimmed while it is being
  typed and canonicalised at the `snapshot()` boundary instead — see "Module identity is
  always trimmed", `Requires` expressions (issue #29).
- **Explicit Save** (`GuiApp::save_module`) is gated by `ModuleDraft::save_enabled`
  (`save_gate`: `dirty && valid && all Requires parse && !name_error`, and `editable`).
  It serializes `snapshot()` and writes via `config::write_atomic`, then reloads
  extensions so the draft resets clean **and calls `GuiApp::close_focused_draft`** so the
  reopen invariant puts the user back in the normal view showing the saved module. **No
  manifest autosave.** Saving one module removes only its own draft entry; every other open
  draft survives.
- **No config-autosave interlock (S4b, 2026-07-19).** Earlier revisions of this doc
  described a "config-autosave interlock" (`config_autosave_held` / `pending_config_write` /
  `flush_config_writes_if_clean`) that withheld the normal end-of-frame config write to disk
  while any module draft was dirty. That machinery was deleted — it only ever guarded
  against IDE mode's field writes reaching a real config scope, and they never do: the
  manifest editor mutates the open `ModuleDraft` directly, and the interactive preview column
  runs exclusively inside `with_preview_writes` (`WriteTarget::Preview`, see below). In its
  place, `GuiApp::persist` is now a **no-op under `Mode::Ide`** outright — strictly stronger
  than holding-and-flushing, and it also closes a path the interlock never covered: confirming
  a destructive dialog (`DeleteModule`/`DiscardEdits`) makes `render_confirm_dialog` return
  `true` unconditionally, which used to fall through to a harmless re-save of an unmodified
  `game_config` — now it's a genuine no-op.
- **Status-line wording caught up (2026-07-19).** Both unsaved arms of `editor_status_lines`
  used to read "…config autosave paused until you Save/Rename or Discard.", describing the
  interlock removed above. They now read "Unsaved changes — Save or Discard to apply." and
  "Unsaved rename — Rename or Discard to apply." — the mechanism is gone and autosave is not
  planned to come back, so the clause was deleted rather than the interlock restored.

### Unsaved work vs. dirty (2026-07-19, issue #34)

Two predicates on `ModuleDraft`, deliberately not the same thing:

| predicate | means | asked by |
| --- | --- | --- |
| `dirty()` | the **body** (`snapshot()`) differs from the on-disk manifest | `save_enabled()`, and only it |
| `has_unsaved_work()` | `dirty() \|\| has_pending_identity()` — anything at all would be lost by dropping the draft | every "will the user lose something?" gate |

*Why `PendingIdentity` stays outside `snapshot()`.* Staged Author/Name/`Variable` edits are
held on the draft but **not** folded into `snapshot()`, so `dirty()` cannot see them. That
asymmetry is load-bearing: it is what lets the editor tell "the body changed" (→ Save
writes it) apart from "a rename is staged" (→ only `perform_rename` may commit it, after
migrating stored config across every scope). Folding identity into the snapshot would
collapse the distinction and let a plain Save write a half-typed identity with no scope
migration. **So the fix for issue #34 is at the predicate layer, not the snapshot.**

*Why one predicate and not four patches.* An audit found four gates asking bare `dirty()`,
and with a clean body plus a staged rename **all four** failed at once, each in a way that
looked like a separate bug:

1. `dispatch_top_action`'s `TopAction::Close` — discarded with no prompt, and since Save is
   also `dirty()`-gated, Close was the only lit button in that state.
2. `nav_category_drops_draft`'s `any_unsaved` argument — clicking the Profiles tab
   `shift_remove`d the draft silently.
3. `unsaved_module_names` — the confirm dialog could never name a rename-only draft, in a
   prompt that then `clear()`s the whole map.
4. `editor_status_lines` — the state line, which read "All changes saved."

They are one question with one answer, so they route through one predicate.
`a_rename_only_draft_reports_unsaved_work_at_every_gate` asserts all four together, for
that reason.

*Why `GuiApp::unsaved_drafts` (the per-frame set) became unsaved-work-based rather than
leaving it `dirty()`-based with callers switching:* `apply_discard_edits` destroys **whole
drafts** (`shift_remove`, or `drafts.clear()` on the exit path), staged identity included.
Which drafts, per destination, is `GuiApp::discard_names`' job (issue #35). A dirty-based set
would let the confirm dialog name strictly fewer modules than confirming it destroys —
exactly the silent-loss shape the prompt exists to prevent. The set and the destruction now
describe the same thing. Everything else reading that set (nav lock, tree markers, preview
splice) wants the same answer anyway; the preview splice is unaffected either way, since a
rename-only draft's `snapshot()` is byte-identical to the spec it would replace.

*Why `save_enabled()` did **not** change.* Save writes `snapshot()`, and a rename-only
draft's snapshot equals disk — Save would rewrite the file with identical bytes under the
**old** identity, applying nothing. The rename's affordance is the **Rename** button,
which is gated on the identity being valid and does not look at the body at all (see
"Save and Rename are independent operations"). Net effect: a rename-only draft reports
unsaved work everywhere the user is *asked about losing it*, while Save stays greyed and
Rename is the lit control.

### Multi-draft — `GuiApp::drafts` (S4a, 2026-07-19)

Unsaved manifest edits survive switching modules. `GuiApp::drafts` is an
`IndexMap<PathBuf, ModuleDraft>` — one entry per module the user has opened — replacing
the single `Option<ModuleDraft>` slot that every module switch used to overwrite.

- **Keyed by manifest path, not `Extension::id()`.** *Why:* `GuiApp::perform_rename`
  rewrites the manifest **in place** (the file is never renamed; `id()` is derived from the
  JSON meta), so the path is stable across a rename while the id is not. Keying on the path
  makes re-keying a non-event and removes the whole class of "a missed re-key silently
  orphans a draft" bugs the plan flagged as this stage's main risk. The id rides along as a
  plain `ModuleDraft::id` field.
- **Accessors.** `focused_module()` reads `GuiApp::focused_module: Option<String>`, a plain
  field independent of `nav_sel` (S4b, 2026-07-19 — pre-S4b this read `nav_sel`'s
  `NavSel::ModuleEditor(id)` variant); `draft_key(id)` maps an id to its
  manifest path (via `module_editability`, falling back to a scan of `drafts` so orphans
  stay reachable); `draft()` / `draft_mut()` resolve the focused entry.
- **`ensure_draft` is insert-if-absent and never clears another entry.** It also no longer
  drops a draft whose spec has left `all_specs` — with a map that would be a *silent* loss
  of unsaved work.
- **Orphaned drafts are retained and surfaced.** A draft whose module was deleted outside
  the app, or whose manifest stopped validating across a Ctrl+R, is kept (losing unsaved
  work is the worse failure) and listed by `GuiApp::orphan_draft_names` in a warning banner
  above the IDE module tree (`render_orphan_draft_banner`). *Why a banner:* the tree renders
  `all_specs`, which by definition can no longer give the orphan a row. Saving it recreates
  the manifest at its remembered path.

  This is also the answer to **issue #6** ("a Ctrl+R hot reload removes the open module")
  — which the multi-draft rework closed before the issue was reached (2026-07-19). Under
  the pre-S4a single `module_draft` slot, `ensure_draft` dropped the draft the moment its
  spec left `all_specs`, and both columns fell to "module not found" with nothing to click.
  With `drafts` keyed by manifest path, `draft_key`'s scan-by-id fallback keeps the entry
  reachable, so the editor goes on rendering the module, the banner names it, and **Save is
  the recovery path** (it writes the manifest back). No reopen-on-reload logic is wanted
  here: reopening would mean choosing some *other* module while the user's unsaved work sat
  invisible.
- **Per-frame unsaved set.** `ModuleDraft::dirty()` clones the draft, rebuilds an `IndexMap`
  and serializes it, so it is far too expensive to ask per query — it already ran ~4–6× per
  frame with one draft, and per-row dirty markers would make that N× per frame.
  `GuiApp::refresh_unsaved_drafts` computes `GuiApp::unsaved_drafts: HashSet<PathBuf>`
  **once** at the top of `ui()` (and again at the frame tail, so the tree's per-row markers
  and the exit-confirmation gate see edits the frame just made, not stale state from the top
  of the frame); the tree, the exit gate, the preview splice and `unsaved_module_names` all
  read the cached set.
- **Dirty markers in the tree.** `theme::ICON_DIRTY` (a filled dot, `theme::ACCENT`) is
  prepended into the existing `icon_lists` glyph column in `render_ext_tree`, which already
  lays coloured glyphs into the row's `LayoutJob` with correct gaps. *Never appended to the
  label string* — the label is the module's name, and a baked-in glyph would flow as
  ordinary text with no independent colour. `render_ext_tree` takes a `dirty_flags: &[bool]`
  index-aligned with `specs`; Config mode passes `&[]` (its tree renders `cur_specs`, which
  has no manifest alignment, and locks while any draft exists).
- **Cross-draft identity collisions.** `name_collides` compares against **disk** only, so
  two dirty drafts could both stage `(Author, Name)` and neither would see the other — the
  second Save would silently clobber the first module's config namespace.
  `GuiApp::draft_identity_collides(author, name, exclude)` scans the other drafts' *pending*
  identities (excluded by manifest path) and is OR-ed into all three gates:
  `refresh_draft_name_error`, `refresh_identity_state`, and the Fork/Create dialog (which
  passes no exclusion, since a new module excludes nothing).
- **`identity_error` is meaningful on the FOCUSED draft only** (2026-07-19, issue #32).
  `refresh_identity_state` writes through `draft_mut()`, so a background draft's marker is
  whatever it held when it last lost focus and can be arbitrarily stale — while
  `draft_identity_collides` *reads* every draft's staged identity. That asymmetry is real
  but **not** reachable: every read of `identity_error` goes through `draft()` (the
  confirm-dialog label and its blocked-reason branch, the `perform_rename` gate,
  `editor_header_info`), `ui()` refreshes it at the top of every frame against all drafts'
  current staged identities, and an unfocused draft's identity cannot change while unfocused
  because only the focused draft has an open editor. So a stale marker never reaches the
  screen and **cannot let `perform_rename` commit a colliding rename** — no on-disk
  consequence. Left as-is rather than refreshing every draft per frame (`dirty()`-adjacent
  work, N× per frame, for a value nothing reads).

  > The invariant is "**never read `identity_error` off a draft that is not focused**". A
  > future per-row error marker in the module tree would break it — such a caller must
  > refresh every draft or call `draft_identity_collides` live at the read. Pinned by
  > `identity_error_is_refreshed_on_refocus`.
- **`identity.var_edits` is seeded in UI declaration order** (2026-07-19, issue #31). Both
  seeding sites — `ensure_draft` and the in-place re-base at the tail of `perform_rename` —
  used to build this `IndexMap` by iterating the `HashSet<String>` of baseline vars, making
  the order of an *order-preserving* map nondeterministic across runs. Nothing read it in
  order yet (`changed_var_renames` walks `sections` and looks each key up), so the only
  symptom would have been a future UI list or diff report silently reordering itself between
  launches — close to unreproducible once shipped. Both sites now seed from an ordered walk
  of `spec.ui`; `baseline_vars` stays a `HashSet` because every consumer only asks
  `contains`. Regression: `ensure_draft_seeds_var_edits_in_declaration_order`.
- **Lifecycle.** `perform_fork` sources the **focused draft's** `snapshot()` when one is
  open, falling back to disk — pre-S4a it always read disk, so forking a module you had just
  edited silently produced a fork of the *saved* version. The original draft keeps its edits
  (multi-draft makes carrying them into the fork *and* keeping them free).
  `perform_create` no longer clears drafts. `delete_module` removes only that manifest's
  entry. `perform_rename` **re-bases its entry in place** after `reload_extensions` —
  the draft's `id`, `baseline`, `baseline_vars` and staged `identity` all describe the
  pre-rename manifest and are updated field by field. (It used to drop and re-seed from
  disk; once Rename stopped writing the body, re-seeding would have discarded the
  still-pending body edits — see "Rename / identity migration", step (6).)
- **Exit confirmation.** `nav_category_drops_draft` returns `false` for
  the **IDE** tab unconditionally (switching modules inside IDE mode costs nothing, so a
  prompt on the mode's primary gesture would be pure noise). At the time it returned
  `any_dirty` for the two config destinations, which cleared the whole map on confirm;
  **issue #35 later narrowed both** — General Settings to `false` and Profiles to the
  focused draft — see "The prompt names exactly what it destroys" above. The dialog
  wording, already plural, is unchanged throughout.

**Two pre-existing bugs fixed alongside** (2026-07-19):

- `perform_rename` was gated on `editable && has_pending_identity && identity_error.is_none()`
  but **not** on `save_enabled()`, while writing `snapshot()` — the whole in-memory body — to
  disk. Rename therefore committed unsaved body edits behind the user's back, including ones
  that fail `extension::validate` and so could never have gone out via Save. It was then
  also gated on `!dirty() || save_enabled()` (renaming a clean draft stays allowed).
  **Superseded later the same day:** Rename stopped writing the body at all, which
  removed the reason for the gate along with the bug — the gate is gone again. See
  "Save and Rename are independent operations".
- `perform_rename` never remapped `GuiApp::preview_config`, the IDE preview's in-memory
  scratch layer. `config::remap_all_scopes` walks *files*, so it could not reach it, and the
  scratch values silently orphaned under the old identity. `config::remap_one_scope` is now
  `pub` and is called on `preview_config` immediately after the disk sweep, so the in-memory
  and on-disk layers can never describe different identities.

The **live preview** (below) splices every *dirty* draft over its on-disk entry in
`ide_specs` before resolving, so the launch command reflects unsaved edits to any open
module. Only the **focused** draft may be *appended* when it is absent from that list
entirely: appending a non-focused draft would inject a module that could never run for the
game, and `push` puts it at the **end** of the fold order, silently changing
`lossy_env_overwrites` attribution and wrapper `Priority` ordering.

## Launch-command preview footer

Two different panels depending on `Mode`. In `Mode::Ide` the full-width footer below is
not declared at all — see **IDE-mode preview column** further down.

The bottom `preview` panel (in `GuiApp::ui`, `Mode::Config` only) is either a fixed 198px band or, if
`GeneralConfig::dynamic_preview` is on, sized to its content. It shows the **About**
box (repo link + credits, `crate::gui::GuiApp::render_about`) when `NavSel` is
`GeneralSettings` — there's no launch command to preview there — otherwise it calls
`crate::context::assemble_launch` with whichever resolution matches the panel: when
editing a **Profile**, the edit-scope resolution (so the preview reflects the profile
being edited even if it's not the active one for any game); when `GuiApp::focused_module()`
is `Some` (the draft-spliced spec set, the edited module's entry
replaced by `ModuleDraft::snapshot`, resolved via `GuiApp::resolve_specs_for_game` — one
clone/frame, no disk, assembler signature unchanged) plus the red `lossy_env_overwrites`
lint (see "Diagnostics band") — **this branch is currently unreachable**, since focus is
only ever set under `Mode::Ide`, which declares no `preview` panel at all (see "Manifest
editor" above); otherwise the game resolution. *Why Config mode keeps the
lint in its launch band* while IDE mode moved it out (2026-07-19): there is no editor band
in Config mode to move it to — `ide_editor_band` is declared only under `Mode::Ide`. The
assembled command is rendered with `crates/ritz-app/src/gui.rs:command_job`,
which highlights every `%command%` token in the accent color. See
[launch-command-assembly.md](launch-command-assembly.md) for what `assemble_launch`
does with the resolved fields.

## IDE-mode preview column (`GuiApp::render_ide_preview_panel`)

The right-hand `ide_preview` `SidePanel` in `Mode::Ide`: a WYSIWYG render of the module
under edit — `render_module_settings_body(.., full_width = true, read_only = false,
show_legend = preview_game.is_some())` — over a nested
`TopBottomPanel::bottom("ide_launch_band")`. The band is declared **before**
the inner `CentralPanel`; nested panels obey declaration order exactly like top-level
ones. *Why nest it here instead of a full-width footer:* the launch string belongs to the
preview, and the space under the editor column is the diagnostics band.

The launch band shows the assembled command and **nothing else**. The env-overwrite lint
used to render here too; it moved to `ide_editor_band` on 2026-07-19 so the same warning
isn't on screen twice, and so this panel stays about what the launch *is* rather than
what's wrong with it.

## Diagnostics band (`crate::gui::render_ide_diagnostics_band`)

The contents of `ide_editor_band`: a `DIAGNOSTICS` heading row over a framed, scrolling
list, styled to match `ide_launch_band` beside it exactly (same `FIELD` fill, `BORDER`
stroke, 8px rounding, 13/11 inner margins, same fill-the-band `available_height - 22`
arithmetic). The two 198px bands are peers and have to read as peers.

The list holds two sources, assembled in display order by
`crate::gui::ide_diagnostic_entries`:

1. **The open draft's status messages** (`crate::gui::editor_status_lines`) — the state
   line, then whatever is blocking Save or Rename.
2. **The launch-preview lints** (`crate::gui::preview_lints`) — three checks over the
   assembled preview, described below: `lossy_env_overwrites`,
   `wrapper_priority_ties` and `variable_reference_lints`.

It was laid out as a **down payment** on the full ok/warning/error diagnostics panel the
IDE plan calls for, and issue #26 spent part of it: the heading is a row with a
right-aligned tally opposite the title (adding an ok count appends to that row and moves
nothing below it), and the list scrolls inside the fixed band rather than growing it.

### Severity vocabulary (`crate::gui::DiagSeverity`)

Three ranks, each an icon + colour pair:

| rank | icon | colour | what it means |
|---|---|---|---|
| `Info` | `` circled *i* | `theme::ACCENT` (blue) | state, not a problem — the draft's dirty / clean / read-only line |
| `Warning` | `` triangle | `theme::COL_WARN` (amber) | unfinished or lossy, but nothing is being refused — an env overwrite, a staged-but-unapplied rename |
| `Error` | `` circled *!* | `theme::COL_ERROR` (the danger red) | the reason an action is refused — every `Draft::save_enabled` gate, plus a blocked Rename |

**`Warning` and `Error` used to share `theme::COL_GLOBAL`**, separated by icon alone,
because `theme.rs` had no amber and a `Color32` literal at the call site would have broken
the styling guide's top-level rule. That was tolerable while the band held one or two
warnings. It stopped being tolerable when 60a56b3 added three lints that all rank as
`Warning`: a draft with a few undeclared references painted the whole band danger-red, so
a screen where **nothing** was refused read exactly like a screen where Save was blocked.
Fixed 2026-07-19 (issue #39) the way that note always pointed at — as real palette tokens
in `theme.rs`, where palette decisions belong. See `docs/ui/STYLING-GUIDE.md`,
"Diagnostic severity colours", for the two hex values and why they are those values.

The band's **tally row** ("3 warnings", "1 error · 2 warnings") is coloured by the worst
rank it summarises — `COL_ERROR` when there is at least one error, `COL_WARN` otherwise.
It was unconditionally red, which was the same misread one level up: the summary claimed
something was refused when nothing was.

### The pinned info line

The draft's **state line is always first, always `Info`, and always blue.** It is
unconditional — `editor_status_lines` emits exactly one of "Unsaved changes…" /
"Unsaved rename…" / "All changes saved." / "Bundled module — read-only" on every path, and
nothing else in the list is ever `Info`. The test
`status_lines_have_exactly_one_pinned_info_line` walks all 256 flag combinations to hold
both halves of that.

**A staged rename counts as unsaved** (2026-07-19, issue #34). With a clean body and a
pending Author/Name/`Variable` edit the state line reads
*"Unsaved rename — Rename or Discard to apply."*, not
"All changes saved." — the user's words: *"to the user, that's just wrong."*

- *Why its own arm rather than reusing the "Unsaved changes" wording:* the button that
  commits it is **Rename**, not Save. `ModuleDraft::save_enabled` stays `dirty()`-gated
  (see "Unsaved work vs. dirty" below), so naming Save here would point at a greyed
  control.
- *Why the "Pending identity change — press Rename to migrate saved settings and apply it."
  `Warning` still fires alongside it:* the two entries do different jobs. The `Info` line
  says **that** there is unsaved work; the `Warning` says **what** is unfinished about it
  (staged, settings not yet migrated). Merging them would either lose the state line in the
  "body dirty *and* rename staged" case or duplicate it in the rename-only case. The
  invariant is unaffected: the state line grew a third *arm*, not a second entry.

*Why pinned at the top rather than sorted in* (2026-07-19, issue #26, user's call): it
answers "is my work saved", and that question should not slide down the list as errors
appear above it. Everything after it keeps source order rather than sorting by severity —
sorting would scramble `editor_status_lines`' deliberate sequence (state → why Save is
blocked → why Rename is blocked), and with the whole list in a scroll area there is no
truncation for a sort to protect against.

**Why the status lines are here at all** (2026-07-19, issue #26): they spent one commit
(`7f6109c`) in the IDE header band, in a fixed slot reserved for the four-line worst case
— see "IDE-mode header band" above. That band spans both columns and is `exact_height`, so
the reservation was the price of not reflowing half the window mid-keystroke, and the
reservation read as far too tall. This band has neither problem: it is *already* a fixed
198px, always visible, and its list already scrolls, so any number of messages costs zero
height and nothing moves. Most of them are diagnostics on the merits, too — a `validate`
error, a section-name collision and an unparseable `Requires` are exactly what this
surface is for. **Config mode is unchanged**: it has no diagnostics band, so it still
renders the same messages inline under its header via `render_editor_status_lines`, in
the per-message palette it has always used (`StatusLine::config_color`). Both surfaces
draw from the one `editor_status_lines` builder, so they cannot drift.

**The tally** counts errors and warnings, never `Info`, and omits a rank at zero
(`3 warnings`, not `0 errors · 3 warnings`). *Why info is excluded:* exactly one info
entry is always present, so counting it would mean the band never reads below "1" and a
clean draft would claim to have an issue. The tally answers "how much is wrong", and the
state line is by definition not wrong. Errors are named before warnings, in the order
they have to be dealt with.

**Empty state:** a quiet `✔ No issues.` in `FAINT`, keyed to the **problem** count rather
than the entry count — so a clean draft reads `All changes saved.` (blue) then
`✔ No issues.`, which states the clean case twice rather than leaving an empty box under a
lone blue line. *Why an explicit line and not a blank box:* the band is fixed-height and
always visible, so "nothing is wrong" and "the lint never ran" would look identical if it
rendered empty — precisely the distinction a diagnostics surface exists to make. `FAINT`
rather than a success green because the absence of a problem shouldn't be the loudest
thing on screen.

### The preview-lint boundary (`crate::gui::preview_lints`)

All three launch-preview checks return `Vec<DiagEntry>` from one producer, which the band
concatenates after the draft's status lines. **Each check chooses its own severity**; no
caller re-derives one.

*Why one typed boundary* (2026-07-19, issue #11): before it, every check returned a
different shape — `lossy_env_overwrites` a `Vec<EnvOverwrite>`, `validate` a `Result<()>`,
`name_error` / `identity_error` bare `Option<String>`, `all_requires_parse` /
`sections_unique` bare `bool` — and nothing could aggregate them. Commit `62eb53d` had
already solved **half** of it: `DiagSeverity` + `DiagEntry` exist, and the draft-side
checks all flow through `editor_status_lines`, which attaches a severity to each message.
The stragglers were the *preview* lints, which reached the band as bespoke structs with
the band stamping `DiagSeverity::Warning` on every one at the call site — meaning a check
structurally **could not** pick a different rank even where it should. Issue #9 and #10
would each have added a third and fourth bespoke shape. `preview_lints` collapses the
family to one `-> Vec<DiagEntry>` boundary, so adding a fifth check is an append there
rather than a new arm at every consumer.

`lossy_env_overwrites` keeps its structured `EnvOverwrite` return and converts at the
boundary: its tests assert on the individual `var` / `culprit` / `victims` fields, which a
pre-formatted string would discard.

**Config mode's inline copy of these lints** now paints from `DiagSeverity::icon()` /
`color()` instead of the hardcoded warning glyph + `COL_GLOBAL` it used. That pair was
correct while `lossy_env_overwrites` was the only source (every entry a `Warning`); with
several checks each picking a rank, hardcoding one would mislabel any future non-warning.
It renders identically today.

### What `wrapper_priority_ties` detects (issue #9)

Wrappers from **two or more different modules** declared at the same `Priority`.
`ritz_core::builder::build_wrappers` sorts by `(priority, extension_index)`, so a tie
falls through to **module load order** — which neither manifest declares. Enabling a
module, renaming one, or shipping a new bundled one can reorder the assembled chain with
nothing in either JSON having changed. The defect is not that the order is wrong; it is
that the order is **not authored**.

**Severity: `Warning`.** A tie is frequently harmless — two wrappers that do not interact
genuinely do not care which runs first, and demanding a total order over things whose
order is irrelevant is busywork. It is deliberately **not** `Error`: this band's `Error`
rank means *the reason an action is refused* (a `save_enabled` gate or a blocked Rename),
and an `Error` here would make the heading tally claim a blocker while Save sits enabled.
`Info` is unavailable — `editor_status_lines` guarantees **exactly one** `Info` entry, the
pinned state line, and both the pinning and the tally depend on that invariant. `Warning`
is therefore both the accurate rank and the only non-blocking one the vocabulary offers.

**Scope: across all enabled modules; same-module ties are deliberately silent.**

- Across modules is where a tie bites, because that is where the chain is assembled and
  where the tiebreak is invisible. The editor edits one manifest at a time, but this lint
  runs over the same resolved spec set the launch preview beside it uses, so it sees the
  real chain rather than a fragment.
- Within one manifest the tiebreak is **declaration order**: `sort_by` is stable and both
  entries carry the same `extension_index`, so two tied wrappers keep the order they were
  written in. That is authored intent — exactly like a single module's `Builder` step
  sequence, which `lossy_env_overwrites` declines to police for the same reason.

Wrappers whose `Requires` does not pass are skipped: a gated-off wrapper never reaches
`build_wrappers`, so its priority cannot collide with anything.

### What `variable_reference_lints` detects (issue #10)

Both directions of the link between a `UiField`'s `Variable` and the places that read it.
Misspelling a name on either side fails **silently** — an undeclared name is falsy in a
`Requires` forever and interpolates to the empty string — so the symptom is a control that
does nothing rather than an error anyone sees.

| direction | severity | message |
|---|---|---|
| referenced in a `Requires`, declared by nothing | `Warning` | *"no field declares `x`, referenced in a condition — that condition is never true."* |
| interpolated as `{x}`, declared by nothing | `Warning` | *"…referenced in an interpolation — it expands to nothing."* |
| declared, referenced by nothing | `Warning`, behind a much stricter rule | *"nothing in this module references `x` — the field may have no effect."* |

**What counts as declared.** Locals resolve per module (`Resolution::var_store` fills the
store from that extension's own resolved fields and nothing else), so a bare name is
checked against **this module's** `UiField.variable` set. A `global:`-prefixed name
resolves out of the shared global pool, so it is checked against the `global:` fields of
**any** loaded module — a reference to `global:hdr_on` satisfied by a different manifest is
correct usage and must not be reported. This mirrors `VarStore::get`, which strips the
prefix and looks in `global`.

**Why the two directions share `Warning` despite very different confidence.** An
undeclared reference is near-certainly a defect: nothing else in the system can define the
name. An unreferenced field is far weaker evidence — a module may legitimately expose a
control no JSON block reads. The vocabulary has no rank quieter than `Warning` available
(`Info` is reserved for the single pinned state line), so **the noise comes out of the
rule instead of the rank** — see the suppression below. A fourth, quieter `Hint` rank is
the honest fix and would let these split cleanly; it is not invented here for the same
reason issue #39 leaves the palette alone.

**Why the dead-field half is suppressed for backend / hook / script modules**
(`crate::gui::has_opaque_var_consumer`). Measured against the bundled set: with no
suppression, that half fires on **six of thirteen** bundled manifests — `custom-args`,
`custom-env`, `custom-game-env`, `hypr-monctl`, `lsfg-vk` and `scripts`. Every one is a
false positive, and they share a cause: their fields are consumed by a **Rust backend
handler**, a **hook script**, or a **`ScriptBuilders` script** — none of which the walk can
see, because none is expressed in the manifest. A lint wrong about half the project's own
modules is worse than no lint, so a module carrying any of those three opaque consumers is
exempt from the dead-field half entirely. The undeclared-reference half stays on for them:
an opaque consumer explains a field with no *reader*, but nothing explains a `Requires`
naming a variable that does not exist.

**Two extraction details that keep it honest.** `{OPTIONS}` in a wrapper's `CommandSyntax`
is `build_wrappers`' own placeholder, substituted *before* interpolation runs, so it is
never a variable reference (flagging it would fire on Gamescope). And identifiers come
from the real readers — `condition::parse` for `Requires`, and
`crate::gui::interpolated_vars`, which mirrors `VarStore::interpolate` exactly — never a
regex. A regex mishandles `{{escaped}}` braces, which are literals rather than references.

### All three lints are silent on every bundled module

Pinned by `preview_lints_are_silent_on_every_bundled_module`, which loads
`resources/extensions/` through `core_extension::discover` and asserts an empty result. As
of 2026-07-19 the bundled set has **no wrapper `Priority` ties** (Gamescope at 100,
`gamemoderun` at 200) and **no undeclared references at all**; the dead-field half is
silenced by the opaque-consumer exemption above.

*Why this is a test and not a note:* a lint that fires on the manifests shipped as the
reference examples is not a lint, it is noise — and the user would learn to ignore the band
before it ever caught a real bug. If that test fails after a bundled manifest changes, the
first question is whether the **lint** is wrong, not the manifest.

### What `lossy_env_overwrites` detects

Env vars where one module's `Set` or `Unset` discards a value an **earlier, different**
module already contributed. Derived by reading `ritz_core::builder::build_env_block`,
which folds one accumulator per var name across modules in declaration order:

| earlier | later | accumulator effect | warns? |
|---|---|---|---|
| `Set` | `Set` | value replaced | **yes** |
| `Set` | `Append` | concatenated | no |
| `Append` | `Append` | concatenated | no |
| `Append` | `Set` | value replaced, the append is gone | **yes** |
| `Set`/`Append` | `Unset` | value cleared | **yes** |
| `Unset` | `Set`/`Append` | writes into an already-empty accumulator | no |

Two judgement calls, both deliberate:

- **`Append` then `Set` warns; `Set` then `Append` does not.** The pair is lossy in only
  one order. `build_env_block` folds in module declaration order, so the order the lint
  sees is the order that will run — the asymmetry is real, not an artefact. Silently
  swallowing another module's append would be worse than the false positive this replaced.
- **`Unset` then `Set` does not warn.** An `Unset` carries no value, so a later write
  destroys nothing; only an intent is overridden. The lint is about data loss, and firing
  on overridden intent would put it back to crying wolf.

Loss is reported **across modules only** — a single module's own step sequence is authored
deliberately. `ENV_VARS` and `GAME_ENV_VARS` are analysed independently, because
`build_env_block` is called once per block and the same name in the two is two variables.

*Why this replaced `set_set_collisions`* (2026-07-19, user-reported against
`RADV_PERFTEST`): the old lint recovered per-module attribution by re-running
`assemble_launch` on each module **in isolation** and looking for `EnvAction::Set`. That
cannot work — `EnvAction` has only `Set(String)` and `Unset`, so the fold reports anything
holding a value as `Set(final)`. A module whose only op is `Append` has nothing to append
to when assembled alone and came back as `Set`, so **every `Append` looked like a `Set`**
and two modules appending to one var — the correct, lossless way to share it — raised a
data-loss warning. It also missed real losses: `Set` then `Unset` assembles to one `Set`
and one `Unset`, only one "setter", so it stayed silent. The replacement models the
accumulator directly instead of asking the assembler, which is the only way to see the op
kind and the order that `LaunchCommand` has already erased.

*Why it lives in `ritz-app` and not `ritz-core`:* it needs nothing that isn't already
public (`Resolution::var_store`, `VarStore::interpolate`/`lookup_fn`,
`condition::eval_opt`), and it is an opinion about what to warn a module author about —
not a rule the launcher enforces. Core owns what the fold *does*; the GUI owns what the
editor *says about it*.

### Interactive, into a scratch layer that never reaches disk (S5a)

The column is **fully editable**, and every write it makes lands in
`GuiApp::preview_config` — an ordinary `GameConfig` (`"__preview__"` / `"None"`) handed to
the resolver as the game layer, exactly the way `preset_as_fake_game` does for profiles.
Nothing persists it: no code path hands it to `save_game`, and `persist()` has no arm that
can reach it.

The routing flag is `GuiApp::write_target: WriteTarget { Scope, Preview }`. **It is a
second, orthogonal axis and not a `NavSel` arm, deliberately.** Even before S4b deleted
`NavSel::ModuleEditor`, `nav_sel` in IDE mode was the *same* value whether the manifest
editor column or the preview column was rendering — so `nav_sel` structurally could not
tell "editor writing config" from "preview writing scratch". Since S4b, IDE mode does not
touch `nav_sel` at all (see "Two modes" above), so it has even less to say about which
column is rendering. `set_scoped`, `unset_scoped` and
`current_scope_value` therefore branch on `write_target` **before** their `nav_sel` match.
(Same reasoning that made `Mode` a separate axis from `NavSel`.)

Three guards keep a preview toggle off the user's disk, and all three are load-bearing:

1. **`GuiApp::with_preview_writes`** wraps the body call and restores
   `WriteTarget::Scope` after it. *Why a closure rather than an assignment after the
   call:* the restore lives inside the wrapper, so no `return` in the caller's closure
   body can skip it — and both `render_module_settings_body` and the `push_id` block
   around it contain early returns. A `write_target` stuck on `Preview` would be the
   worst failure mode: the *next* Config-mode edit would silently vanish into scratch.
2. **The `changed` bool from the preview call is explicitly discarded.** Outside IDE mode
   that bool feeds `ui()`'s `changed`, which calls `persist()`. `persist()` is itself a
   no-op under `Mode::Ide` (S4b), but letting `changed` escape this closure would still be
   wrong to rely on — nothing outside the scratch layer changed, so there is nothing to
   report upward regardless.
3. **A snapshot assertion.** `serde_json::to_value` over **all three** config stores —
   `game_config`, `global_config` and `editing_preset_buf` — is taken before the body call
   and `debug_assert_eq!`d after. This replaces the old `debug_assert!(!changed)` (which
   could not survive an interactive preview) with an assertion of the property that
   actually matters — it checks the *destination*, not the messenger, so it stays
   meaningful however the write path is later refactored.

   *Why all three and not `game_config` alone* (2026-07-19, issue #39): it started as
   `game_config.config.modules`, then widened to the whole `GameConfig`, but `game_config`
   is only the store a field write lands in when `nav_sel` is `NavSel::Game(_)`. The whole
   premise of S4b is that IDE mode leaves `nav_sel` wherever the user last had it, so this
   preview legitimately renders with `GlobalSettings` or `Profile(_)` selected — and a leak
   would then land in `global_config` or `editing_preset_buf` and sail straight past a
   game-only snapshot. It would not even stay in memory: the next unrelated edit anywhere
   calls `persist()`, which writes `global.json`. Snapshotting every reachable store means
   the guard no longer silently depends on which scope happens to be selected.

`poll_detect` is the one writer outside that wrapped region: it runs at the frame tail,
after the restore, so `Detect` captures the `write_target` in force when the detection
started and `poll_detect` re-establishes it for the write. It also returns `changed` only
for `WriteTarget::Scope`, for guard #2's reason. Without this, the `window_class`
**Detect** button — which becomes live along with the rest of the preview — would land its
result in the real game config, bypassing all three guards.

#### Inheritance shading in the preview (2026-07-19, issue #8)

`write_target` is also what picks the **parent-preset chain** that inheritance depth
badges shade against. `crate::gui::GuiApp::field_chain` — extracted from the inline
`match` that used to sit in `render_module_settings_body` — returns
`preview_preset_chain` under `WriteTarget::Preview` and dispatches on `nav_sel`
otherwise.

*Why `Preview` needs its own arm at all:* S4b stopped forcing `nav_sel` away from
`Game`/`Profile` while the preview renders, so without the arm a preview opened from a
Profile screen would shade against **that screen's** chain — a layer the preview is not
resolving through.

*Why the arm returns a real chain and not `Vec::new()`:* empty was the conservative
first cut and it was wrong in one reachable case. `set_preview_game` points the preview
at a game whose profile may have parents, and `resolve_specs_for_preview` genuinely
resolves fields to `Provenance::Preset` through that chain — so hard-empty dropped the
depth badge for exactly the values Config mode's Game view badges. Same value, two
screens, two answers. With **no** preview game selected the chain is empty and the depth
stays `None`, which remains correct: there is nothing to point at.

*Why `preview_preset_chain` is a second copy of what `preview_preset` already holds:*
`preview_preset` is pre-**merged** (one `Preset` with the parent chain folded in), which
is what the resolver wants and what the badge cannot use — merging is precisely what
destroys the "which link set this" information the badge exists to show. Both are cached
by `set_preview_game` rather than rebuilt in the render path, because rebuilding costs
one `load_preset` per link and rendering happens every frame.

Pinned by `ide_preview_shades_against_its_own_preset_chain_not_nav_sels`, which asserts
all three: a real deepest-first chain for a preview game with a parented profile, an
empty one for no preview game, and that neither is `nav_sel`'s.

#### Test coverage of the guards (2026-07-19, issue #16)

`set_scoped`/`unset_scoped`'s routing has been pinned since `ec4422b`
(`preview_write_target_routes_to_preview_config_not_game_config` and its `Scope`
counterpart). Issue #16 closed the remaining three holes — the tripwire in guard #3 is a
`debug_assert_eq!`, which **compiles out in release**, so a regression in an untested guard
would panic for a developer and silently corrupt config for a release user:

- `buffer_scope_tag_namespaces_preview_away_from_every_real_scope` — the preview tag
  differs from every real scope's, the *composed* key (not just the tag) differs, no two
  real scopes collide with each other, and the `Preview` early return wins over every
  `nav_sel` arm below it.
- `with_preview_writes_restores_the_previous_target_even_on_early_return` — restore holds
  for a straight-line closure, for a closure that returns **early** (the case the wrapper
  exists for), when entered from `Preview` already (the case that distinguishes
  save-and-restore from a hardcoded `= Scope`), and two calls deep.
- `poll_detect_restores_the_captured_target_and_reports_changed_only_for_scope` — a
  `Preview` detection lands in the scratch layer under the `"preview"` buffer key, leaves
  `game_config` untouched, restores `write_target`, and reports `changed = false`; the
  `Scope` case is asserted in the mirror image.

Each was proved non-vacuous by breaking the guard, watching the test fail, and restoring
it — including the subtle variants (hardcoding the restore to `Scope`, dropping
`poll_detect`'s re-establish, returning `changed` unconditionally). *Why that mattered
here specifically:* these guards are all save/restore pairs, and a test that only ever
enters from the default state passes with and without the restore — implying coverage that
does not exist.

### Buffer-key namespacing (`GuiApp::buffer_scope_tag`)

`text_buffers` keys used to be `"{spec.id()}::{var}"` with **no** scope tag, and both
`multi_edit` scope-tag sites mapped the (now-deleted) `NavSel::ModuleEditor(_)` onto
`"game:{appid}"` — so
the IDE preview and Config mode's Game view computed **identical** keys for the same
module/variable, over two different resolution bases. That was inert only while the
preview never wrote; making it interactive fires it. Every `text_buffers` / `multi_edit`
key now goes through `buffer_scope_tag()`, which returns `"preview"` when
`write_target == Preview` and the old value otherwise. Sites: the `String` arm of
`render_value_editor` (insert *and* `entry`), `render_multi_string_field`,
`render_env_pair_field`, `poll_detect`, and `unset_current`. Pinned by
`buffer_scope_tag_namespaces_preview_away_from_every_real_scope` (see "Test coverage of
the guards" above).

The same change also fixes an `egui::Id`: the `String` arm mints
`egui::Id::new(("text_edit", &key))`, an **absolute** id that `push_id` does not
namespace and that becomes reachable the moment the preview's fields turn editable.
Folding the tag into `key` fixes the id and the buffer key in one move.

### `show_legend` is its own parameter, not `!read_only`

The scope legend and the trailing colour-hint footer used to ride on `!read_only`.
Flipping `read_only` to `false` would have **silently resurrected them**, so
`render_module_settings_body` takes a separate `show_legend: bool`. Config mode passes
`true`; the preview passes `preview_game.is_some()` — the colours describe layers that are
only actually participating once a real game is selected, so with **None** the palette
would be decorative.

### `read_only` has no live caller — kept, and pinned by a test

Both callers of `render_module_settings_body` pass `read_only: false`: Config mode always
did, and the IDE preview has been deliberately interactive since S5a. That left four
correctly-written guards completely unexercised in production. Issue #39 posed the choice
as delete-or-pin; **kept and pinned** (2026-07-19), by
`read_only_true_makes_the_body_inert`.

*Why kept:* it is **not** a redundant second copy of `WriteTarget::Preview`. That guard
controls the write's *destination* (redirect into the scratch layer); `read_only` controls
*interactivity* — greyed widgets and no write-back at all. Deleting it would remove the
only way to render this form genuinely inert, a capability with an obvious future claimant
(inspecting a bundled, un-forkable module), and the deletion is wide and delicate: the flag
reaches eight functions plus `scope_checkbox`, through the most intricate rendering code in
`gui.rs`, for zero runtime benefit. S5a deliberately *split* it from `editable` (see the
section above) so the two could not be confused; unpicking that trades a real risk for a
tidiness gain.

*How it is pinned without synthesising input:* the fixture stores a `multi_string` value
containing a blank entry (`["a", ""]`). The list renderer filters blanks out and persists
the difference (`if !read_only && cleaned != stored`), so that value is rewritten on the
first frame **unprompted** — the one guarded path reachable with no input events at all,
which avoids the brittle coordinate arithmetic a synthetic click would need. The test
asserts the body reports no change, that all three config stores are byte-identical, and
that `multi_edit` / `text_buffers` were neither seeded nor consumed (read-only *clones* its
working copy instead of taking and re-inserting it). A companion control test,
`read_only_false_does_rewrite_the_stored_list`, proves the fixture actually reaches the
write — without it, an inertness test whose fixture never triggers a write would pass
vacuously. Both were verified non-vacuous by removing the `!read_only` guard and watching
the inertness test fail.

### Preview game selector (S5b)

A `ComboBox` in the **IDE nav footer** (`GuiApp::render_ide_nav_footer`), in the space
above Open Folder / New Module, labelled "Preview against". It lists **None** (first, and
the default) plus every configured game from `self.games`, refreshed by `refresh_games()`
on entry to IDE mode (`select_nav_category`'s `NavCategory::Ide` arm). Picks are collected
as a deferred `Option<Option<String>>` intent and applied after the panel closure, which
already holds `&mut self`.

- **Never remembered.** There is deliberately no `GeneralConfig` field behind it and
  nothing written to disk; every app start begins at None. (User requirement, stated
  twice.)
- `GuiApp::set_preview_game` **seeds from** the game — `preview_config` becomes a *clone*
  of `paths.load_game(appid)` (empty `GameConfig` if absent), so the preview opens showing
  that game's real settings and every edit mutates only the clone. It is never written
  back. The game's preset (`config.modules.preset`, else `default_preset`) is loaded and
  merged with its parent chain via `collect_parent_chain` + `merge_modules` and cached in
  `preview_preset` — presets are **not** re-read from disk inside the render path, which
  runs every frame. The **unmerged** chain is cached alongside it in
  `preview_preset_chain` (index 0 = the game's own profile, 1 = its parent, …) for the
  inheritance depth badge, which cannot use the merged form — see "Inheritance shading in
  the preview" above.
- **It does not call `switch_game`.** That overwrites `appid`, `game_config`, `preset` and
  rebuilds `cur_specs` — it would change what Config mode edits and what would actually
  launch, as a side effect of moving a preview dropdown. The two jobs share shape, not
  state.
- Either way it drops the `"preview"`-tagged `text_buffers` / `multi_edit` entries, since
  the resolution base changed underneath them.

### Resolution and the launch band

- **`GuiApp::resolve_specs_for_preview`** = `resolve::resolve(specs, Some(&preview_config),
  preview_preset.as_ref(), Some(&global_config))`. Both halves of the column — the form
  body and the launch band — resolve through it, so they can never disagree. The real
  `global_config` is included because it is a genuine layer of the launch being predicted;
  only the game/profile layers are scratch. `resolve_specs_for_editing` is left untouched
  (a `nav_sel` arm there could not have distinguished the two columns anyway).
- **One spec list for both halves.** `ide_specs` is `all_specs` filtered by
  `applies_to(preview_appid)` when a preview game is selected, and `game_specs()` (the
  ambient game's set) otherwise,
  with the draft snapshot spliced over its entry or **appended** when the module isn't in
  the list at all. *Why the preview-game filter:* with a game selected, filtering by the
  *ambient* `appid` would silently omit modules gated to the previewed game and include
  modules gated to the ambient one. *Why the append:* the IDE tree browses the unfiltered
  `all_specs`, so opening a module that doesn't apply would otherwise show a preview that
  silently ignores everything you type.
- **`resolve_specs_for_editing`** (S3b) is `resolve_for_editing` with the spec list as a
  parameter; `resolve_for_editing` delegates to it with `cur_specs`. *Why it was needed:*
  every arm previously said `&self.cur_specs` literally, ignoring any list the caller had
  in hand — inert while the only consumers used `cur_specs`, but it silently blanks badges
  and whole preview bodies for any module outside it. `render_ext_tree` now resolves
  against its own `specs` parameter for the same reason.
- **No "Previewing against: {game}" line** inside the band: the nav footer's selector both
  shows *and* chooses the binding, so a second, non-interactive statement of the same fact
  three columns away would only be a thing that can go stale. The Config-mode footer's copy
  of that line was deleted in S4a (2026-07-19) — it named the *ambient* game while the
  preview resolves against the scratch layer, so it contradicted a settled decision.

### Widget ids

`ui.push_id("ide_preview", ..)` wraps the entire body. It must wrap the whole body, not
just the combo: the multi_string and env-pair renderers auto-id their `TextEdit`s,
`+ Add` buttons and `icon_button`s too. This is the only `push_id` in `gui.rs`; S4
replaces the per-widget salts properly. Note that in IDE mode
`render_module_settings_body` runs **once per frame** — the editor column renders
`render_module_editor` (the manifest editor), not a second settings body — so `push_id`
guards against Config-mode id reuse and future S4 layouts, not a same-frame collision.
The one id that `push_id` cannot cover is the absolute `("text_edit", key)`, handled by
`buffer_scope_tag` above.

If the module resolves to no `ExtResolution`, the column renders an **explicit notice**
rather than staying blank — a blank column reads as "this module has no settings", which
is a very different (and wrong) message.

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
config cleanup, discarding unsaved module edits) is staged into `GuiApp::confirm: Option<ConfirmAction>`
(`crates/ritz-app/src/gui.rs:ConfirmAction`) instead of executing inline, and carried
out by `crate::gui::GuiApp::render_confirm_dialog` only after the user clicks Confirm in
a modal `egui::Window` (a full-screen `egui::Area` backdrop swallows clicks so only the
dialog is interactive). *Why route everything through one enum instead of five ad-hoc
confirm flags:* one dialog renderer handles the modal chrome/backdrop once; each variant
just supplies its title/message and its own commit logic.

Not every variant need be destructive: the renderer lets an arm override two things —
`confirm_label` (default `"Confirm"`) and `destructive` (default `true`, selecting
`theme::danger_button` over `theme::primary_button`). *Why an override and not a second
dialog renderer:* the modal chrome, backdrop and Cancel wiring are byte-identical — such
an arm differs in one word on one button and in whether that button should read as red.
The one arm that uses it is `ConfirmAction::SaveWithPendingRename`, whose "Apply
Rename" / "Save Changes" affirmatives destroy nothing (see "Save and Rename are
independent operations"); every other variant takes the destructive defaults.

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
