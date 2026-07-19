# Brainstorm: IDE Mode for the Settings GUI

**Date:** 2026-07-19 · **Branch:** `feat/custom-module-editor` (follows the custom-module-editor
work)
**Session:** teamlead brainstorm, 3 agents × 2 rounds + 1 final verification round
(model: Opus, high effort) · **Lenses:** (1) Navigation / Information Architecture / UX,
(2) Layout & egui panel architecture, (3) State model, editing lifecycle & maintainability —
overlapping, not siloed.

## The user's original proposal (verbatim)

> Im really thinking about doing an IDE mode. So we split "PROFILE/GAMES" category into two
> seperate things: "GENERAL" (General Settings, IDE Mode — new, should turn the "GAMES/PROFILES"
> selection below into a list of Modules) and "GAMES/PROFILES" (pretty much like it was before,
> including the tree view: Global Profile (prev. Global Settings), Profiles + list, Games +
> list). In this IDE Mode, i should be able to configure the modules and see a live preview to
> the right of it. Since the second collumn (where the modules usually sit) is gone, because the
> modules are displayed in the first collumn in IDE mode, we can split the remaining width into
> the editor and the preview. [ModuleList - fixed width][ModuleEditor - 50% of remaining]
> [ModulePreview - 50% of remaining]. This would make the UI look a lot cleaner again, because i
> really dont like it with the button clutter, that we have added.

## Goal

Add an **IDE Mode** to the settings GUI: a third top-level nav destination where the module
list (currently the `nav` tree) takes over the first column, the module editor sits in the
middle, and a **live WYSIWYG preview** — the module rendered as it would normally look, driving
the actual launch-command preview — sits on the right. Purpose is dual: a cleaner default UI
(clutter fix is independent, see below) and, more importantly, a place to interactively verify
a module's behaviour — including its interaction with *other* modules — before saving anything.

---

## Agreed design

### Nav structure (final, after two clarification rounds)

A boxed category (working name `GENERAL`) containing three buttons, with the list *below* the
box swapping per selection — structurally this reuses the nav's existing "fixed header box
above a scrolling tree" shape: the box replaces the current `nav_header`, only the tree body
swaps.

```
┌─ GENERAL ────────┐
│ General Settings │
│ IDE Mode         │
│ Games / Profiles │
└──────────────────┘
```

- **General Settings** selected → list below is empty.
- **IDE Mode** selected → list below is *all modules* (this is the "GAMES/PROFILES becomes a
  module list" from the proposal).
- **Games / Profiles** selected → list below is exactly today's tree: Global Profile
  (renamed from Global Settings, pure label change — see Decisions), Profiles ▸, Games ▸.

*Why this shape and not two top-level categories:* it's the cheapest change that satisfies the
proposal's "split PROFILE/GAMES into two separate things" — the existing nav panel and its
header-box-over-tree layout are reused verbatim; only the tree body's data source swaps on
selection.

### Layout (IDE Mode only)

```
[ nav: module list ]  [ ModuleEditor ]  [ ModulePreview ]
   fixed width           Central          SidePanel::right
                                           (resizable, default ~50%)
                                        ┌───────────────────────┐
                                        │ (reserved: future      │
                                        │  "IDE settings" band,  │
                                        │  under the editor)      │
                                        ├───────────────────────┤
                                        │ Launch Command Preview │
                                        │ (nested bottom panel,  │
                                        │  under the PREVIEW      │
                                        │  column only, not full  │
                                        │  width)                 │
                                        └───────────────────────┘
```

- Keep the existing `nav` `SidePanel` and swap its body to the module tree; **drop** `ext_list`
  (the panel that normally hosts the module list in Config mode).
  *Why keep `nav` and drop `ext_list`, not the reverse:* `ext_list` is built inline in `ui()`
  with local flow state, while `nav` is already a standalone method — and keeping the surviving
  panel leftmost preserves panel declaration order for egui's available-rect math.
- Editor = Central panel. Preview = resizable right `SidePanel`, default favouring the editor
  (not a hard-coded 50/50 — the proposal's "50% of remaining" is a starting point, not a
  constraint, so the split stays user-adjustable).
- The 743px editor-width clamp (three call sites, see Code findings) must **not** apply in IDE
  mode — it exists for Config mode's narrower column and would leave a dead gutter at wide
  window sizes (e.g. 2560px) if carried over unmodified.
- The launch-command band sits **below the preview column only**, not full width, nested as a
  `TopBottomPanel::bottom(...).show_inside(...)` inside the preview `SidePanel` — the codebase
  already uses exactly this nesting pattern twice (see Code findings: refuted claims). The space
  below the *editor* column is reserved for a future "IDE settings" pane, mirroring the existing
  split-footer row shape.
- **"IDE Mode fixes button clutter" is the wrong justification and is deliberately NOT the
  design driver.** Clutter is an independent ~20-line fix (Fork/Delete → an overflow menu;
  Rename belongs inline beside the identity fields it commits, not in the header) that doesn't
  require a mode switch at all. IDE Mode is justified on its own merits — the live preview —
  and the button cleanup (final header: `[Fork] [Delete] [Discard] [Save]`) is bundled into the
  staging (S4) because that's where the header layout is being touched anyway, not because IDE
  Mode causes the clutter.

### State model

**The real prize of this feature is deleting a lie in the state model.** Today,
`NavSel::ModuleEditor(String)` is a *mode masquerading as a selection* — commented in-code as
"not a config-edit scope — treat it like the ambient game" — and every exhaustive
`match self.nav_sel` pays the cost of that fiction. IDE Mode's state model replaces it with:

- `nav_sel` — the four *real* config scopes (General / Global / Profile / Game), nothing else.
- `mode: Mode { Config, Ide }` — which top-level area is showing.
- `drafts: IndexMap<ModuleId, ModuleDraft>` — the open, unsaved edits, keyed by module identity
  (see Multi-draft below), replacing the single `Option<ModuleDraft>`.

Deleting `NavSel::ModuleEditor` kills, in one stroke: `editor_return`, `editor_exit_target`,
the nav-away draft-drop guard, the Close button, `pending_config_write`,
`config_autosave_held`, `flush_config_writes_if_clean`, and four now-meaningless unit tests
(see Code findings). *Why this is worth doing rather than bolting IDE mode on top of the
existing lie:* leaving `NavSel::ModuleEditor` alive alongside a draft map would produce two
competing sources of truth for "which module is open" — exactly the bug class multi-draft
support needs to avoid.

### Multi-draft editing

Editing one module keeps other modules' unsaved edits alive when you navigate away to look at
or copy from them. *Why:* the user's own words — "So I can switch to another module, copy
something, go back and paste it." A snippet-clipboard alternative was proposed by two agents
(cheaper, ~30 lines vs re-costed as also ~30 lines but with three bookkeeping hazards) and
**rejected by the user** — multi-draft was the original ask and stands as specified.

- The module tree **never locks** in IDE mode. *Why this matters:* the current Config-mode
  behaviour locks the tree whenever ANY draft exists, which was flagged as a dead end in round 1
  — if IDE Mode auto-opens a draft on click, a permanently-existing draft means the tree locks
  forever and you can never switch modules. Resolved: in IDE mode the module tree is a *file
  browser*, and file browsers don't lock; a **per-row dirty dot** (small glyph, own column —
  see Code findings on why it can't be appended to the label string) shows unsaved state
  instead of gating navigation.
- **Cross-draft name collisions are a real corruption path** (open item, not yet resolved — see
  Open items): two drafts staging the same Author/Name both pass individual validation, and the
  second Save silently clobbers the first.
- On exiting IDE mode with unsaved drafts: **no draft survives** — everything reverts — with one
  combined notification listing every dirty module by name ("These modules have unsaved
  changes: Module-1, Module-2…"). *Why a combined notice and not per-module prompts:* keeps the
  exit path a single decision rather than N confirmations.

### Scratch layer — how the preview resolves

All three agents independently converged on the same mechanism, because the codebase already
has the exact precedent: `preset_as_fake_game` (gui.rs:4778) builds an in-memory `GameConfig`
and hands it to the resolver as the game layer. The preview reuses this verbatim:

- `preview_config: GameConfig`, seeded **empty**, is *not* a new scope in the four-layer
  precedence chain — it's an ordinary `GameConfig` passed to
  `resolve::resolve(specs, Some(&preview_config), None, None)`. Extension defaults remain the
  bottom layer for free; nothing new enters the config model.
- **"Resolves against nothing" by default** (round 1, answer 4): the preview starts blank, and
  only values the user sets *inside the preview itself* affect the Launch Command Preview.
  *Why:* "remove clutter and make it easy to test" — the user does not want IDE Mode's preview
  to silently inherit whatever game/profile happened to be selected elsewhere. A future
  game-resolution dropdown may be added, defaulting to (and topmost as) "None" — this is a
  deferred nicety, not part of the initial scratch layer.
- Because it's a real `GameConfig`, `render_field`, `current_scope_value`, and `apply_tri` work
  **verbatim** with one added match arm each — no parallel resolution path to maintain.
- A future "resolve against real game X" dropdown is *the same code path* with a live
  `GameConfig` swapped in for the empty scratch one — this is why the scratch-layer choice is
  cheap for both today's requirement and tomorrow's extension.
- **Preview values are combined across modules in one shared map** (round 2, answer 6),
  explicitly so cross-module `append`-type env-var interactions can be tested, and **all dirty
  drafts are spliced into the launch preview simultaneously** (round 2, answer 2), not just the
  currently-open one. The user must also be able to set preview values on *other, unedited*
  modules (built-in or bundled) and see those reflected too.

#### The RADV_PERFTEST worked example

This is the clearest statement of what the preview is *for*, given by the user directly:
author a new module that appends to `RADV_PERFTEST`, then open the built-in AMD module's
preview, set its own `RADV_PERFTEST` value, and verify the new module's append actually lands
on top of it in the assembled Launch Command Preview. *Why this shapes the design:* it's why
preview values are a **shared, cross-module map** rather than scoped to one draft at a time —
the whole point is checking interaction between modules, not just one module in isolation. The
user's stated purpose in full: "The module preview is only for testing and live visualization.
Not saved, only for checking how it behaves, since the user can check the Launch Command
Preview and see what it actually does."

Editor and preview always show the **same** module — there is no independent preview selector.
"Other modules" means values set earlier (while that module's preview was open) persist in the
shared scratch map and keep feeding the launch string after you've navigated away; sequential
navigation is sufficient to exercise the worked example above, nothing needs simultaneous
on-screen visibility of two modules.

### Bundled-module handling

- **Always edit mode by default**, even for bundled/read-only modules — needed to support
  forking, and the user is notified when a module is read-only. Bundled modules still get the
  full preview and launch-string behaviour.
- `editable` is split so it gates **Save / Rename / Delete only**; in-memory editing (and the
  interactive preview) is universal, including for bundled modules.
- **Consequence:** a bundled draft can go dirty with Save permanently greyed out, so **Fork
  becomes the primary CTA** for that state, and it must carry the unsaved edits — `perform_fork`
  is required to source from the **draft snapshot**, not disk, or it silently drops the edits
  (today it copies from disk — see Code findings, confirmed bugs). With that fix, "edit a
  bundled module, then Fork" becomes a strictly better flow than today's "fork first, then
  edit."
- Round 2 confirmed all three of: Fork is the intended escape hatch for a dirty bundled draft;
  Fork carries the unsaved edits into the new file; the preview stays fully interactive for
  bundled modules — i.e. `editable` gates the editor column's write actions only, never the
  preview.

### Manifest scope only — nothing persists

Preview values are editable **only for the preview**, never saved, held in-memory, and persist
across profile switches (round 1, answer 7) since they live in the scratch `preview_config`,
not in any on-disk scope.

### Rename interaction with the scratch layer

`config.rs:remap_one_scope` is already a pure function over a module map (private today); made
`pub` and called on `preview_config` after the manifest write, giving identical rename semantics
to the on-disk scope sweep with no risk of divergence between "what renames on disk" and "what
renames in the scratch preview."

---

## Decisions and rationale (condensed)

| # | Decision | Why |
|---|---|---|
| 1 | Preview is WYSIWYG (module rendered as its normal settings page) plus a live launch-output readout | "makes it easier to debug/understand" |
| 3 | Keep the whole-UI reshape when entering IDE Mode | user explicitly likes that IDE Mode reshapes the window |
| 4 | Preview resolves against **nothing** by default | "remove clutter and make it easy to test"; a game-resolution dropdown is a later addition, default/topmost "None" |
| 5 | Always edit mode, even for bundled modules, with a read-only notice | needed to support forking |
| 6 | Save/Discard/Cancel, no autosave; switching modules keeps edits alive; only *exiting IDE mode* triggers the discard warning | lets the user copy values between modules without losing work |
| 7 | Preview values are manifest-scope only: editable only for the preview, never saved, in-memory, persist across profile switches | preview is for testing, not for config |
| 9 | "Global Profile" is a pure label rename of "Global Settings"; behaviour unchanged | still the base layer under everything — naming consistency with "Profiles" only |
| 10 | Ship staged (S1–S6, see below) | risk containment — see S4 discussion |
| — | Ctrl+E **removed for now**, not ported | it dies with `selected_ext` anyway, since that index only exists because `ext_list` exists — this is a deletion, not a port; may be revisited later |
| — | Drop the scope-colour legend in IDE mode | round 2, answer 3 — see open item on neutralising scope colours, not just the legend |
| — | Global Profile lives under Games / Profiles (not under General) | round 2 follow-up, final nav placement |

---

## Code findings

### Confirmed (file:line)

- `set_current` (gui.rs:3704-3706) and `unset_current` (gui.rs:3773-3775) both open with
  `self.cur_specs.get(self.selected_ext)` and route `NavSel::ModuleEditor(_)` into the **Game**
  arm → `self.game_config.set_value` (gui.rs:3717, 3787). **Additional finding:**
  `NavSel::GeneralSettings` sits in the same arm, so General Settings edits also write to
  `game_config` — a second latent bug, distinct from the ModuleEditor one.
- **`poll_detect` (gui.rs:3760) writes `self.game_config` directly, bypassing `set_current`
  entirely** — window-class detection performed while in IDE mode would persist straight to the
  real game config.
- `preset_as_fake_game` (gui.rs:4778) — exact precedent for the scratch-layer approach.
- `remap_one_scope` (config.rs:514) — private, pure over `&mut AuthorsMap`, trivially made
  `pub`.
- Draft→preview splice at gui.rs:1376-1381 uses `position(|s| s.id() == draft.id)`.
- `render_field`'s fixed 260px label reserve (gui.rs:3294, twin at 5425).
- `text_buffers.clear()` fires on nav actions at gui.rs:4393, 4400, 4409, 4475, 4772.
  `text_buffers` is keyed `spec.id()::var` with **no scope tag** (gui.rs:3670), unlike
  `multi_edit`, which IS scope-tagged (3343, 3437) — the fix must **add the tag**, not just skip
  the clear.
- `ComboBox::from_id_salt(&field.variable)` (gui.rs:3624) is salted by variable ONLY; explicit
  `egui::Id::new(("text_edit", &key))` at gui.rs:3672. **There is no `ui.push_id` anywhere in
  gui.rs** — nothing namespaces anything today.
- `name_collides` (gui.rs:415-427) compares only against on-disk specs; all three call sites
  pass `self.all_specs` (953, 972, 1782).
- `perform_fork` (gui.rs:1019) sources `self.all_specs` — disk, not draft.
- The 743px clamp is copy-pasted at exactly three sites: gui.rs:1556, 2072, 3810.
- `editor_return` (212), `editor_exit_target` (404), nav-away draft drop (1279-1283),
  `flush_config_writes_if_clean` (1219), `pending_config_write` (199),
  `config_autosave_held` (396) all hang off `NavSel::ModuleEditor` and become dead when it's
  deleted — **plus four unit tests die with them** (gui.rs:5621, 5722-5736, 5809).

### Refuted — recorded so nobody re-derives them

- **`apply_tri` (gui.rs:3542) does NOT contain the `cur_specs[selected_ext]` lookup**; it's a
  pure dispatcher. But it forwards to both writers, so it needed a pass-through `spec`
  parameter of its own: the lookup **deletion** lands in two functions (`set_current`,
  `unset_current`), but the **signature change** touches three (those two plus `apply_tri`).
- **The splice does NOT silently no-op for a renamed draft.** `snapshot()` (gui.rs:271-275)
  clones `ext` and folds in `sections`, while identity edits are deliberately held in
  `PendingIdentity` OUTSIDE the snapshot (gui.rs:245-249) — so `snapshot().id() == draft.id`
  always holds today. **An `origin_id` field is NOT needed** unless identity handling changes
  later.
- **The "~1080px minimum window width" concern is moot.** gui.rs:114 already enforces
  `.with_min_inner_size([1200.0, 650.0])`.
- **The launch-band-nested-in-preview-column option was mis-costed as expensive; it is not.**
  It's the pattern the codebase already uses twice —
  `TopBottomPanel::bottom(…).show_inside(…)` nested in a side panel (gui.rs:1343 `group_toggle`
  inside `ext_list`; gui.rs:4173 `nav_settings` inside `nav`). Cost is **low**. Readability
  mitigation instead: the preview column is resizable up to 900px, and `command_job` +
  `ScrollArea` already handle wrapping (gui.rs:1466-1469); recommend a horizontal-scroll
  fallback below ~450px rather than imposing a width floor.

---

## Open items (ranked, unresolved — do not invent answers)

- **MEDIUM** — `name_collides` needs a concrete cross-draft rule: the check must union on-disk
  identities (minus each draft's own manifest) with every *other* open draft's pending identity.
  Two drafts staging the same Author/Name is a real corruption path (both pass validation
  individually; the second Save clobbers the first).
- **MEDIUM** — with editor and preview both rendering fields in the same frame,
  `from_id_salt(&field.variable)` collides whenever the same variable name appears in both
  columns. Must salt with module id **and** column.
- **MEDIUM** — the scope *colours* on controls must be neutralised in preview, not just the
  legend dropped, or the user sees scope-coloured widgets with nothing left to explain them.
- **MEDIUM** — the reserved-but-empty "IDE settings" band under the editor will look broken in
  the first version; leave it unallocated until there's content, or fill it with the read-only
  notice / validation errors instead of shipping visible dead space.
- **MEDIUM** — four unit tests (gui.rs:5621, 5722-5736, 5809) must be deleted or rewritten when
  `NavSel::ModuleEditor` goes.
- **LOW** — `docs/features/settings-gui.md` "Layout: four panels per frame" (~:43-64) and the
  preview footer note (~:397) go stale the moment IDE mode lands and must be updated in the same
  commit, per project doc discipline.
- **LOW** — the "Previewing against: `{game}`" string (gui.rs:1431) contradicts round-1 answer 4
  ("resolves against nothing by default") and must be removed.
- **RESOLVED (S1).** The interlock-deletion argument (removing `config_autosave_held` etc.)
  holds only if **no** path in IDE mode writes a real scope — and `poll_detect` used to. This was
  first marked resolved citing two things: `poll_detect` routed through `set_scoped` (commit
  `95104df`), and S1 closing the `NavSel::GeneralSettings` arm with a `debug_assert!`-guarded
  no-op. **The second half of that reasoning was wrong** — a QC review found the `debug_assert!`
  arm was in fact *reachable*: `poll_detect` runs unconditionally from `update()` every frame for
  every `NavSel`, using identity snapshotted when the user clicked Detect, and nothing cleared
  `self.detect` on navigation — so navigating to General Settings mid-detection panicked debug
  builds (and in release silently dropped the write while still populating `text_buffers`). Note
  for future agents: the render-path-only proof (assume the writer is only reachable while its
  triggering UI is on screen) doesn't hold once an async completion writer is in play — polling
  runs regardless of what's rendered.

  That failure mode has since been **fixed**, and the item is genuinely resolved, but on
  different, stronger reasoning:
  - `poll_detect` routes through `set_scoped` (commit `95104df`).
  - `Detect` now snapshots the `NavSel` it started under (the `nav: NavSel` field, gui.rs:553);
    `cancel_stale_detect` (gui.rs:3823) clears a pending detection whenever live `nav_sel` no
    longer matches that snapshot. It is called right after the nav panel renders (gui.rs:1319)
    and again at the top of `poll_detect` (gui.rs:3833), before its `Waiting` early-return.
  - `d.nav` is snapshotted inside `start_detect` (gui.rs:3791), whose only caller is
    `render_value_editor` (gui.rs:3672), which is itself only called from `render_field`
    (gui.rs:3312) — and the module-editor rendering path that calls `render_field` is gated off
    under `NavSel::GeneralSettings` (gui.rs:1321). So a surviving detection can never carry
    `nav == GeneralSettings`.
  - Therefore every module write in `gui.rs` provably goes through `set_scoped`/`unset_scoped`,
    and the `GeneralSettings` no-op arm is genuinely unreachable rather than aspirationally so.

---

## Applied

### 2026-07-19 — S5 shipped (interactive preview + preview game selector)

`WriteTarget { Scope, Preview }`, `preview_config` / `preview_preset` / `preview_game`,
`resolve_specs_for_preview`, `with_preview_writes`, `buffer_scope_tag`,
`set_preview_game`, the nav-footer "Preview against" `ComboBox`, and the IDE-mode
Close → **Discard** relabel. Behaviour and rationale live in
[`../features/settings-gui.md`](../features/settings-gui.md) ("IDE-mode preview column",
"IDE-mode header band"); only the corrections to *this* plan are recorded here.

**Corrections to the plan above:**

- **Seed-from-game, not overlay-on-game.** The "Scratch layer" section describes
  `preview_config` as starting empty and only ever holding values the user sets *inside*
  the preview. With the S5b selector, picking a game makes `preview_config` a **clone of
  that game's stored config** — the preview opens showing the game's real settings, and
  edits mutate the clone. *Why:* the worked example below needs the *other* modules'
  existing values present to interact with; an overlay over an empty base would show the
  new module appending to nothing. `None` still gives the empty scratch layer the plan
  describes, and that is still the default.

- **S5 does NOT depend on S4.** The staging table says "**Depends on S1 and S4**". It
  does not. Nothing in the interactive preview or the selector touches the draft model:
  `write_target` is orthogonal to `nav_sel`, and the single `Option<ModuleDraft>` is
  spliced into `ide_specs` exactly as before. The only consequence of shipping S5 first
  is the pre-existing S4 limitation — unsaved *manifest* edits do not survive navigating
  to another module. Preview *values* are unaffected: they live in `preview_config` and
  persist across module navigation already, which is what the worked example needs.

- **The plan's `"ide"` match arm would have been wrong.** The staging entry says to add
  an `"ide"` arm to four `nav_sel` matches. `nav_sel` is `NavSel::ModuleEditor(_)` for
  **both** IDE columns, so such an arm cannot distinguish "editor writing config" from
  "preview writing scratch" — it would have routed them identically. The orthogonal
  `write_target` flag, checked *before* each `nav_sel` match, is what actually separates
  them.

- **The same-frame widget-id collision the plan warns about does not exist.** In IDE mode
  the editor column renders `render_module_editor` (the manifest editor), so
  `render_module_settings_body` runs **once per frame**, not twice. `push_id("ide_preview")`
  is still kept — it guards against Config-mode id reuse and future S4 layouts — but the
  collision it was introduced for was never live. The id that genuinely *did* need fixing
  is the absolute `egui::Id::new(("text_edit", key))`, which `push_id` cannot namespace at
  all; folding the scope tag into `key` handles it.

- **A leak path the plan does not mention:** making the preview editable also makes the
  `window_class` **Detect** button live, and `poll_detect` writes at the frame tail —
  after `with_preview_writes` has restored `WriteTarget::Scope`. `Detect` now captures the
  write target at start and `poll_detect` re-establishes it, and returns `changed` only
  for a real scope.

- **`show_legend` had to be split off `read_only`.** The legend and colour-hint footer
  were suppressed by `!read_only`, so flipping that flag would have resurrected them
  silently. They now ride a separate parameter, and the preview shows them only when a
  preview game is selected — with `None` the scope palette would be decorative.

- **Selector placement:** the IDE **nav footer**, above Open Folder / New Module — not
  below the tab bar. User-specified. Also never persisted, by explicit request.

### 2026-07-19 — S3b shipped (the IDE shell)

`Mode { Config, Ide }`, the GENERAL category box, the mode-swapped nav body/footer, the
`ide_preview` right panel with its nested launch band, and the reserved
`ide_editor_band`. Behaviour and rationale are documented in
[`../features/settings-gui.md`](../features/settings-gui.md) (sections "Two modes",
"Layout: `Mode::Ide`", "Navigation panel", "IDE-mode preview column"); only the
corrections to *this* plan are recorded here.

**Corrections to the plan, found against the code:**

- **`resolve_for_editing` was hardwired to `cur_specs` in every arm** — the plan didn't
  list this among the confirmed findings, but it is a real defect the moment the tree
  renders `all_specs`: a spec outside `cur_specs` gets no `ExtResolution`, so its badges
  and its entire preview body silently come up empty. Fixed by adding
  `resolve_specs_for_editing(&self, specs)` and delegating; `render_ext_tree` now
  resolves against its own `specs` parameter.
- **`NavSel::ModuleEditor` is not merely tolerated in S3b, it is load-bearing.** The plan
  frames it as a lie to be deleted in S4. It is — but until then it is also the only
  carrier for "which module is open", and IDE mode leans on it deliberately: the invariant
  `mode == Ide ⟹ nav_sel == ModuleEditor(_)` is re-established at the top of `GuiApp::ui`
  every frame. That single line is what lets the whole existing draft lifecycle
  (`ensure_draft`, the nav-away guard, Ctrl+S, the autosave interlock) work in IDE mode
  with **zero** changes. S4 must replace the invariant, not just delete the variant.
- **The reserved band under the editor was an open item ("will look broken; leave it
  unallocated").** The user overrode this: it is declared, empty, at `exact_height(198.0)`
  so its bottom edge aligns with the nav footer band.
- **The `ext_list` header's `+` glyph button is the only create-module entry point**, so
  IDE mode's `+ New Module` had to re-raise `open_create_dialog` itself — there was no
  shared handler to call.
- **The launch band needs `cur_specs` + *append*, not just splice.** The plan's splice
  (`position(|s| s.id() == draft.id)`) no-ops when the edited module doesn't apply to the
  ambient game — which is routine once the tree browses `all_specs`. S3b appends in that
  case so the band always reflects what you're typing.
- **The module header had to leave the editor column (2026-07-19).** Not in the plan: with
  the header inside the editor's half, the `[Fork] [Delete] [✕] [Save]` cluster plus the
  name/version/author heading (~500pt of row) was what set the editor column's minimum
  usable width — under roughly a 1300pt window the cluster overflowed and drew over the
  module name. It is now a `TopBottomPanel::top("ide_module_header")` spanning everything
  right of the nav, declared between the nav and the preview panel. This forced the
  header/body split described in `../features/settings-gui.md` ("Header / status / body
  split"), because the header and body now render in different panel closures while the
  body still holds `&mut module_draft`. The status lines deliberately stayed with the
  body — they are conditional and would have resized a full-width band mid-keystroke.
  *Follow-ups from the click-through, same day:* the band took the columns' own
  `theme::PANEL` fill (the `PANEL2` toolbar look was tried and rejected — it read as
  bolted on), grew to two rows / 60pt to stop reading as a cramped strip next to Config
  mode's natural-sized module header, and picked up the module description as a
  fixed-height, elided second row. Full reasoning in `../features/settings-gui.md`
  ("IDE-mode header band").
  *Still open:* the S4 button cleanup (`[Fork] [Delete] [Discard] [Save]`, Rename moved
  inline beside the identity fields) was explicitly **not** bundled in; it is a separate
  design change, and it is now easier to do because the cluster lives in one small free
  function (`render_editor_header_row`).
- **Ctrl+E is gone (2026-07-19).** Staged in the table above as "removed for now, not
  ported" but missed at the time; removed along with `enter_editor_for_selection`, its
  only caller. It was keyed off `selected_ext`, an index that only exists because the
  Config-mode `ext_list` column exists — so it was never portable to IDE mode as written.

**Deliberately left for S4/S5 (not defects):**

- Single `Option<ModuleDraft>` — no multi-draft, no per-row dirty dots. Switching modules
  in the IDE tree discards the open draft (via the existing nav-away guard).
- Scope *colours* are still painted on the preview's controls even though the legend is
  suppressed — the plan's MEDIUM open item on neutralising them is untouched.
- The preview still resolves against the ambient game underneath; the empty scratch
  `preview_config` that makes "resolves against nothing" literally true is S5. Only the
  user-visible "Previewing against: {game}" line was dropped.
- `Mode::Config`'s nav tree still carries its own **General Settings** row, now duplicated
  by the GENERAL box above it. Left in place because removing it would change Config-mode
  rendering, which S3b was required not to do; it is a one-line cleanup for a later stage.
- Pre-existing and out of scope: the Config-mode module footer's **Open Folder** opens
  `paths.games_dir()`, not the extensions dir, despite sitting under the *module* tree.
  IDE mode mirrors it rather than silently diverging.

## Staging (S1–S6)

- **S1 — Writer identity fix.** Remove the `cur_specs[self.selected_ext]` lookup from
  `set_current`/`unset_current`; take the identity as a parameter from the call site, which
  already has `spec` in scope. Two more copies of the same lookup, unmentioned above, turned up
  in the `multi_string` renderers — `render_multi_string_field` (gui.rs:3326) and
  `render_env_pair_field` (gui.rs:3420) — and were removed the same way. Five functions touched
  in total: two lookups deleted from the named writers, two more from these renderers, and a
  pass-through `spec` parameter added to `apply_tri` (gui.rs:3542), the dispatcher both writers
  sit behind. Fix the `GeneralSettings`-writes-to-`game_config` arm. Route `poll_detect` through
  the same writer. Ships alone, zero behaviour change, unblocks everything interactive that
  follows.
- **S2 — Cosmetic.** "Global Settings" → "Global Profile" (`GuiApp::render_nav_panel`'s nav
  tree row, and the edit-context banner in `GuiApp::ui`). Extract a
  `body_max_width` helper replacing the three 743px literals.
- **S3 — IDE shell, read-only preview.** Add `Mode { Config, Ide }` and the boxed three-button
  nav category. In IDE mode: drop `ext_list`, tree into the `nav` column, editor in Central,
  preview as `SidePanel::right` (resizable), launch band as a nested
  `TopBottomPanel::bottom` inside the preview panel reusing the gui.rs:1343 / 4173 pattern.
  Preview renders WYSIWYG but **non-interactive**; legend dropped. Single draft still (no
  multi-draft yet).
- **S4 — Multi-draft.** `drafts: IndexMap`, per-row dirty dots via a separate glyph column
  (never appended to the label string — see state-model note on `icon_cache` poisoning), tree
  never locks. Delete `NavSel::ModuleEditor`, `editor_return`, `editor_exit_target`, the
  nav-away guard, the Close button, `pending_config_write`, `config_autosave_held`,
  `flush_config_writes_if_clean`, and the four dead tests. Combined exit-confirm listing dirty
  module names. Fix `perform_fork` to source the draft snapshot. Fix `name_collides` for the
  cross-draft substitution rule. Salt combo/text ids with module id. **The button cleanup lives
  here and nowhere earlier.**
- **S5 — Interactive preview.** `preview_config: GameConfig` seeded empty, resolved via
  `resolve(specs, Some(&preview_config), None, None)`. `WriteTarget { Scope, Preview }`. Add an
  `"ide"` arm to **four** sites, not two: the two `scope_tag` matches (gui.rs:3334, gui.rs:3427)
  AND `set_scoped` (gui.rs:3706) and `unset_scoped` (gui.rs:3739) — S1 split the old inline
  unset-path match into `unset_scoped` as a twin of `set_scoped`, so both now need the arm. Also
  scope-tag the `text_buffers` keys (3670). Make `remap_one_scope` `pub` and call it in
  `perform_rename`. Remove the "Previewing against:" string. **Depends on S1 and S4.**
- **S6 — Polish.** Game dropdown with "None" default+topmost. IDE settings pane below the
  editor. Read-only banner refinements.

**Riskiest stage: S4.** It is simultaneously (a) a state-model rewrite — single
`Option<ModuleDraft>` to a keyed map, touching every `self.module_draft` site — (b) a large
deletion — six symbols, four tests, an enum variant appearing in five match arms — and (c) a
correctness-critical addition — re-keying drafts on rename/fork/delete, where a missed re-key
silently orphans a draft with no visible symptom until the user loses work. It cannot be
meaningfully split further: leaving `NavSel::ModuleEditor` alive alongside a draft map would
produce two competing sources of truth for "which module is open" — exactly the bug class the
stage exists to prevent.

### Dead end flagged and resolved

Round 1 flagged that if IDE Mode auto-opens a draft on click while the existing Config-mode rule
("lock the tree whenever ANY draft exists") stays in force, the tree locks forever and you can
never switch modules once one draft exists. Resolved in round 2: the tree **never locks** in IDE
mode; per-row dirty dots replace locking. Locking is a config-scope concern; in IDE mode the
tree is a file browser, and file browsers don't lock.

### Considered and not adopted

One agent proposed the preview show the five assembly blocks
(`ENV_VARS`/`WRAPPERS`/`GAME_ENV_VARS`/`%command%`/`GAME_LAUNCH_ARGS`) as stacked cards with the
current module's contributions highlighted. Not adopted — the user chose the WYSIWYG
form-plus-launch-string design instead (decision 1) — but recorded here as a considered
alternative in case the block-card view is useful later (e.g. as an S6+ toggle).
