# Brainstorm: GUI Custom-Module Editor

**Date:** 2026-07-18 · **Branch:** `feat/custom-module-editor`
**Session:** teamlead brainstorm, 5 agents × 3 rounds + 1 assurance round + 1 verify
(model: Opus, high effort) · **Verdict: GO.**

Lenses (each ranging over the whole topic): schema/migration · editor-UX · autosave &
loader-safety · escape-hatch/uncap · sequencing & tripwires.

## Goal

Let users **create** and **fork** custom launch modules entirely from the GUI — editing the
blocks that assemble the launch string (`[ENV_VARS] [WRAPPERS] [GAME_ENV_VARS] %command%
[GAME_LAUNCH_ARGS]`), with a live command preview — while keeping the JSON hand-editable.
Kept deliberately approachable ("doesn't have to be too complicated").

## Ground-truth facts that shaped the design

- **Config identity = `Author + Name`.** Load id is `Author::Name::Version`, but stored config
  keys on `Author→Name→var` only (`config.rs:migrate_authors` drops Version). So a fork's
  identity is Author+Name; Version is cosmetic for config.
- **Output order is not list position.** Cross-module env folds alphabetically by module name;
  wrapper nesting is the `Priority` integer (lower = outermost). So drag-to-reorder across
  modules would mislead — reorder is meaningful *within* a module's block list and via Priority.
- **Loader is fragile.** `extension.rs:discover` uses `?` (~L63) so one bad manifest aborts
  loading the whole extensions dir; every save must pass `validate()` or the module silently
  vanishes. This is why the autosave/save design is load-bearing.
- **`multi_string` resolves to ONE scalar** (`entries.join("\n")`); the builder has no
  list-expansion; `"custom-env"` is a **GUI-only** label with no core handler. Uncapping needs
  a small new backend pre-pass (below).
- **App is a standalone management GUI** already (General/Global/Profiles/Games nav +
  `resolve_for_game` + a per-frame `assemble_launch` preview in the bottom pane).

---

## Build plan — 5 phases (each ships & tests on its own)

**Phase 0 — Safety floor** (precondition for all writes; own commit)
- Harden `extension.rs:discover` → skip-and-warn per file: return
  `(Vec<LoadedExtension>, Vec<ExtensionLoadError>)`; dir-IO stays fatal, per-file parse/validate
  demoted to a collected warning. `ExtensionLoadError` = flat enum
  `{ Parse{path,reason}, Dup{author,name,paths} }` + `Display` → one banner line.
- Extract `write_atomic(path, bytes)` in **ritz-core** (`config.rs`): temp `.tmp` **suffix**
  (append to OsString, not `with_extension`) + `fs::rename`; no fsync (match `resources.rs`
  precedent). Route both `config.rs:write_json` (currently bare `fs::write` ~L397) and the new
  manifest writer through it.
- Duplicate-module detection lives in `load_all` **post-merge**: group the merged set by
  `(author,name)` **Version-blind**; groups > 1 → `Dup` into the warnings vec → UI banner
  ("There are duplicate modules"). NOT `by_id` collisions (Version makes distinct ids that still
  share one config namespace).
- Call-site ripple (`discover`/`discover_into`/`load_all` return types) is mechanical rethread.

**Phase 1 — Read-only manifest inspector**
- New `NavSel::ModuleEditor(id)` render mode in the existing nav/main-panel split (NOT a floating
  window). Renders meta/sections/fields/builders/wrappers read-only + the live-preview skeleton.
  Proves parse→display round-trips with zero write risk. Audit every exhaustive
  `match self.nav_sel` (e.g. `scope_tag` ~L976, `current_scope_value` ~L1054) and add an arm.

**Phase 2 — Edit existing + live preview + explicit Save** (first real release)
- Mutate an already-valid manifest in memory; persist ONLY on an explicit **Save button**
  (enabled only when `dirty && valid && !name_collision`), via `write_atomic`.
  **No manifest autosave.**
- **Config/scope-value edits keep autosaving** as today, BUT are **held while any module editor
  is dirty** (draft ≠ disk); resume when they match (saved or reverted). Prevents autosaving a
  config value that points at a draft-only module field.
- **Live preview** splices the in-memory draft:
  `preview_specs = cur_specs.with(id → draft); resolve(&preview_specs, selected-scope)` — one Vec
  clone/frame, no disk, no second resolver. Resolves against the **ambient game + global**
  (`game_resolution`) with a "Previewing against: `<game>`" label; shows the bare `%command%`
  token when ritz wasn't launched via `ritz %command%`. `set`/`set` same-var data-loss warning =
  **UI-side lint** over the assembled blocks (assembler signature unchanged).
- One reusable `[↑][↓][🗑]` row-action widget for every list (sections=`IndexMap` via
  `swap_indices`; fields/steps/wrappers=`Vec` via `swap`); the widget returns an action, the
  caller applies the container-correct swap. Reorder via ↑/↓, no drag-drop. egui: collect
  add/remove/reorder into **path-addressed** deferred actions drained after the render loop.
- **Requires** = single raw text field, live-validated via `condition::parse` (red border while
  typing, wordy error settles on blur); unparseable can't be saved. Full AND/OR/NOT grammar.
- Wrapper **Priority** = editable int per wrapper ("lower = outermost"); the preview reorders
  live as you change it. (Per-scope Priority GUI is deferred — later.)

**Phase 3 — Create + fork + uniqueness + rename/migration**
- Config primitives in `config.rs` over one core
  `remap_module_config(authors: &mut AuthorsMap, from, to, rename, drop) -> Vec<dropped>`, plus a
  `remap_all_scopes` driver sweeping global + every profile + every game (needs a new
  `list_games()` mirroring `list_presets()`), saving only changed files. Two thin named fns over
  it: `snapshot_config_to_fork` (parent kept) and `migrate_renamed_module`.
- **Fork** = deep-copy manifest + new Author/Name + uniqueness check + config snapshot. On fork
  all vars are present (it's a copy) → no drop. **Create-new** = fork-from-empty-template (one
  flat form, no wizard).
- **Uniqueness** at the editor SAVE path (not `validate()`), Version-blind on **Author+Name**,
  checked against the full loaded set (bundled + user), exclude-self by manifest path; live
  red/green on the Name field, Save disabled while colliding.
- **Rename** (kept — Name editable is a hard requirement): `(Author, Name)` is ONE identity unit,
  both read-only to autosave; a **Rename** button runs `migrate_renamed_module` on explicit
  commit. "Rename" covers **both** key levels — module identity `Author→Name` (moves the whole
  subtree) and a field `Variable` `Author→Name→var` (+ in-module `{var}`/Requires reference
  rewrite). The **manifest is rewritten in place** (file never renamed; `id()` comes from JSON
  meta) and written **LAST**, after the scope sweep, so any crash leaves manifest+config on the
  OLD name = recoverable by re-pressing Rename. Clobber-guard: skip-if-target-nonempty + report.

**Phase 4 — Uncap Custom-Env/Args (+ new Custom Game Env)**
- `builder.rs::build()` gains a `match input.spec.backend.as_deref()` pre-pass, run AFTER the two
  `build_env_block` calls, reading the module's list var by a FIXED per-backend Variable name:
  - `custom-env` → split each resolved line on **first `=`** → `EnvAction::Set` into `env_vars`.
    List var name `env`.
  - `custom-game-env` (NEW, symmetric module) → same, into `game_env_vars`. List var name
    `game_env`.
  - `custom-args` → one argv entry per line, **literal, no shlex**, into `game_args`. List var
    name `args`.
- UI: Env/GameEnv rows are **two fields (Name | Value)** stored `name=value`; Name validated to
  POSIX env charset `^[A-Za-z_][A-Za-z0-9_]*$` so first-`=` split is lossless; no-`=`/empty line
  → skip; dup name in one list → last-wins silently. Custom-Args stays the literal single-field
  `render_multi_string_field`. (~40–60 line two-field widget; no per-row Requires/Type.)
- `backend` is a bare string — **no enum/registry** to touch. Full-switch manifests must
  **delete** the old `ENV_VARS`/`GAME_ENV_VARS`/`GAME_LAUNCH_ARGS` arrays (else double-emit).
  Net effect: deletes ~250 lines of numbered-slot GUI + the static entries → **net-negative LOC**.
- **Two-game migration** (hand-edit, own config): `games/244210.json` → PulseAudio
  `latency_msec: 100`, `games/2483190.json` → `latency_msec: 80`; delete the orphaned
  `env_1_*` / `Custom Env` blocks in the same edit. (Grep-confirmed: only these two hold any
  custom-env config — no global/profile.) Configs already backed up to
  `~/.config/ritz.backup-2026-07-18`.

The `builder.rs` pre-pass is launch-critical with zero editor dependency — may land as its own
commit right after Phase 0; keeping it in Phase 4 is also fine.

---

## Schema change (exactly one field)

```rust
// ExtensionMeta
#[serde(rename = "ForkedFrom", default, skip_serializing_if = "Option::is_none")]
pub forked_from: Option<String>,   // "Author::Name" lineage; cosmetic
```
Plus (Phase 0, correctness for the first-ever `Extension` *serialize* path): add
`skip_serializing_if` to ALL optional/Vec fields on `Extension` + `UiField` (`app_ids`,
`backend`, `requires_desktop`, `hooks`, and the block Vecs) so editor-written manifests don't emit
`"Backend": null` / `"WRAPPERS": []` noise and bundled manifests round-trip byte-identical.
`<MODULE>_<SECTION>` field naming is pure-UI (auto-derived into the existing `Variable`, editable).

## Must-have tests (one runnable check each, no framework)

1. Manifest round-trip (build/parse → editor-write → re-parse → structural eq) **+ no null / no
   empty-array noise** in the serialized output.
2. `remap_module_config` — values move, unmapped preserved/dropped-per-flag, **idempotent**,
   clobber-guard skips when target non-empty.
3. `discover` skip-and-warn — 1 good + 1 broken file → good loads, bad reported, not fatal.
4. Uniqueness/shadowing — same Author+Name rejected (incl. bundled-vs-user, Version-blind);
   distinct Name accepted; exclude-self by manifest path.
5. Save-gate + interlock — Save disabled while invalid/colliding; **config-autosave held while any
   editor dirty** (the new #1 tripwire).
6. `builder.rs` uncap pre-pass — list → N `EnvAction::Set`, split on FIRST `=`, args literal
   one-per-line, no newline-in-varname.

## Residual tripwires

Author+Name silent shadowing (check full loaded set post-merge) · disk slug collision
(`sanitize(Author)__sanitize(Name).json`, `-2` on clash) · duplicate Variable within a module
(add to `validate()`) · config-autosave-hold interlock correctness · egui mid-iteration mutation
(path-addressed deferred actions) · unresolved `{placeholder}` shown literally in preview ·
rename crash-consistency (manifest written last, in place) · atomic-write must precede any
migration wiring.

## Deferred (fast-follow, not MVP)

Per-scope wrapper Priority GUI · drag-and-drop reorder · "simplify command preview" toggle
(hide Steam wrapper / `%command%` expansion) · manifest autosave (explicit Save is the v1 gate).

---

## Decision log (user answers, condensed)

- **Scope:** full create + fork, kept approachable.
- **Fork:** coexists; user sets Author + Name; uniqueness on Author+Name; parent config snapshotted
  into the fork.
- **Rename:** KEPT — Name (and Author) editable via a Rename button + full-scope migration
  (not read-only/fork-only).
- **Requires:** full AND/OR/NOT, live-validated raw field.
- **Same-var collisions:** append+separator when cooperating; warn on set/set data loss (no block).
- **Uncap:** make Custom-Env/GameEnv/Args virtually infinite (list-backed), not capped.
- **Autosave:** manifest edits = explicit Save; normal config keeps autosave; **hold config
  autosave while a module editor is dirty** until draft == disk.
- **Author:** free editable string ("Ritze" for now).
- **Editor location:** standalone; preview resolves against the currently-selected game/profile;
  bare `%command%` when not launched via ritz.
- **Bundled modules:** unchanged; just configured off like any other.
- **Git:** everything on `feat/custom-module-editor`; unrelated in-flight work committed to master
  first (2 commits) — done.
