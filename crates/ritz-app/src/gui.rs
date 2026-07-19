//! The settings manager GUI: renders each extension's UI tree dynamically,
//! honors `Requires` visibility, shows tri-state inheritance (default / preset /
//! game override) with reset-to-inherit, and previews the live launch command.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use egui::text::{LayoutJob, TextFormat};
use egui::Color32;
use ritz_core::condition;
use ritz_core::builder::EnvAction;
use ritz_core::config::{self as core_config, AuthorsMap, GameConfig, GeneralConfig, InheritanceDisplayMode, Paths, Preset};
use ritz_core::extension::{self as core_extension, ExtensionLoadError};
use ritz_core::resolve::{self, Provenance, Resolution};
use ritz_core::schema::{
    apply_var_renames, ArgSpec, EnvBuilderEntry, EnvOp, EnvVarSpec, Extension, FieldType,
    OptionsSpec, UiField, WrapperBuilderEntry, WrapperSpec,
};
use indexmap::IndexMap;
use serde_json::{json, Value};

use crate::context::{self, chain_would_have_cycle, collect_parent_chain, collect_parent_presets, merge_modules, AppContext};
use crate::icon_center::IconCenterCache;

use crate::theme::{self, COL_GAME, COL_GLOBAL, COL_PROFILE, ICON_EDIT, ICON_INHERIT};

/// Width of the left navigator column, in points.
///
/// Named because two places have to agree on it: the `SidePanel::left("nav")`
/// that *is* the column, and IDE Mode's 50/50 split, which sizes itself from
/// "everything to the right of the nav". A literal in both would let them drift.
/// (The Config-mode `ext_list` column happens to be the same width today; that's
/// a separate column with its own reasons, so it keeps its own literal.)
const NAV_W: f32 = 280.0;

/// Height of IDE Mode's module-header band, in points.
///
/// Derived, not eyeballed — term by term, top to bottom:
///
/// | term   | what it is                                                       |
/// |--------|------------------------------------------------------------------|
/// | `6.0`  | the frame's top inner margin                                     |
/// | `23.0` | the name/version/author + button row: `interact_size.y` (`theme.rs`), which is at or above the 19pt heading's galley |
/// | `7.0`  | `item_spacing.y` (`theme.rs`) — egui's gap between those two rows |
/// | `16.0` | [`IDE_HEADER_DESC_H`], the one-line description slot             |
/// | `8.0`  | the frame's bottom inner margin                                  |
///
/// *Why two rows and not one* (2026-07-19): the one-row band (`6 + 23 + 8 = 37`)
/// read as a cramped strip next to Config mode's module header. That header
/// (`GuiApp::render_module_detail_header`) has **no** fixed height — it is
/// natural-sized — but its structure is fixed: `6` space, the same 23pt heading
/// row, an edit-context line, the description, `10` space, a separator. This band
/// reproduces it minus the two parts it does not own: the edit-context line (IDE
/// Mode has no edit scope to name — the tree shows the selection) and the
/// trailing separator (the panel's own `show_separator_line` draws that). What is
/// left is heading row + description, which is the 60pt here.
///
/// *Why `exact_height` and not auto-sizing:* the band spans the editor **and**
/// preview columns, so any height change there reflows half the window. Pinning
/// it also keeps the layout still on the frames where
/// [`GuiApp::editor_header_info`] returns `None` (a module switch, before
/// `ensure_draft` catches up) — an auto-sized band would collapse to nothing and
/// snap back, which reads as a flicker.
const IDE_HEADER_H: f32 = 6.0 + 23.0 + 7.0 + IDE_HEADER_DESC_H + 8.0;

/// Height of the header band's description slot, in points.
///
/// One `Body` (13pt) text row, rounded to a whole point. The slot is **always**
/// allocated at exactly this height — see [`render_editor_header_description`]
/// for why that is the whole point of it.
const IDE_HEADER_DESC_H: f32 = 16.0;

#[derive(Debug, Clone, PartialEq)]
enum NavSel {
    GeneralSettings, // splash timeout, default preset — shown in central panel
    GlobalSettings,  // extension-variable global overrides — shown in central panel (ext editor)
    Profile(String),
    Game(String), // appid
    /// Read-only manifest inspector for one module, keyed by its `Extension::id()`
    /// (`Author::Name::Version`). Not a config-edit scope — non-preview scope
    /// helpers (persist/current_scope_value/etc.) treat it like the ambient game.
    ModuleEditor(String),
}

/// Which top-level area of the window is showing.
///
/// *Why this is a separate axis and not another [`NavSel`] variant:* `NavSel`
/// answers "which config scope am I editing", `Mode` answers "which shape is the
/// window in". Folding IDE Mode into `NavSel` would force every exhaustive
/// `match self.nav_sel` in this file to grow an arm that has nothing to say
/// about scopes — and would collide with `NavSel::ModuleEditor`, which IDE Mode
/// *uses* as its "which module is open" carrier. The two are orthogonal:
/// `Mode::Ide` always runs with `nav_sel == NavSel::ModuleEditor(_)`.
///
/// Deliberately **not persisted** — IDE Mode is a workbench you enter for a
/// session, not a preference. Reopening the settings window always lands in
/// `Config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The classic three-column config editor (nav | modules | fields).
    Config,
    /// The module-authoring workbench (module tree | manifest editor | preview).
    Ide,
}

/// A click on one of the three rows in the nav's GENERAL category box. Collected
/// during render and applied afterwards (the render closure already holds
/// `&mut self`).
#[derive(Debug, Clone, Copy)]
enum NavCategory {
    GeneralSettings,
    Ide,
    GamesProfiles,
}

/// A destructive action awaiting confirmation in a small dialog.
#[derive(Debug, Clone)]
enum ConfirmAction {
    DeleteGame(String),    // appid
    DeleteProfile(String), // preset name
    ClearSettings,
    ReExportResources,
    ConfigCleanup,
    /// Delete a user-authored module manifest. `label` is `Author::Name` for the
    /// prompt; `manifest` is the file to remove.
    DeleteModule { manifest: PathBuf, label: String },
    /// Leave the module editor with unsaved edits (Close acting as Discard).
    DiscardEdits,
}

/// The Fork / Create-new module dialog. Both share the same Author/Name form;
/// Fork additionally copies the parent's saved settings and carries its lineage.
struct ModuleDialog {
    kind: DialogKind,
    author: String,
    name: String,
    /// Fork only: snapshot the parent's stored config into the fork's namespace.
    copy_settings: bool,
}

enum DialogKind {
    /// Fork the module with this `Extension::id()`.
    Fork { parent_id: String },
    /// Create a brand-new module from an empty template.
    Create,
}

/// What the user chose to do with the launch when the editor was opened from the
/// splash (launch mode). In standalone mode this is always `Continue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    Continue,
    Cancel,
}

/// Open the manager with no specific game (picks the first saved game, or a
/// scratch config if none exist).
pub fn run_manager(ctx: &AppContext) -> Result<()> {
    let games = ctx.list_games();
    let (appid, name) = games
        .first()
        .cloned()
        .unwrap_or_else(|| ("0".to_string(), "Scratch".to_string()));
    run(ctx, &appid, &name, vec!["%command%".to_string()], false)?;
    Ok(())
}

/// Open the editor focused on a specific game.
///
/// When `launch_mode` is true (opened from the splash) the window shows a top bar
/// with "Continue" / "Cancel launch"; the returned [`EditOutcome`] tells the
/// caller whether to proceed with the launch.
pub fn run(
    ctx: &AppContext,
    appid: &str,
    name: &str,
    game_command: Vec<String>,
    launch_mode: bool,
) -> Result<EditOutcome> {
    let mut app = GuiApp::new(ctx, appid, name, game_command);
    app.launch_mode = launch_mode;
    let outcome = app.outcome.clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([1200.0, 650.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Ritz — Settings",
        options,
        Box::new(|cc| {
            crate::fonts::install(&cc.egui_ctx, app.general_config.mono_ui);
            crate::theme::apply(&cc.egui_ctx);
            app.logo = load_logo(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("egui error: {e}"))?;

    let outcome = *outcome.lock().unwrap();
    Ok(outcome)
}

/// The settings editor.
pub struct GuiApp {
    paths: Paths,
    default_preset: Option<String>,
    all_specs: Vec<Extension>,
    /// Tree folder for each entry in `all_specs` (relative to the extensions root).
    all_dirs: Vec<PathBuf>,
    /// Absolute manifest path for each entry in `all_specs`, index-aligned. Used
    /// by the module editor to write edits back and decide editability.
    all_manifests: Vec<PathBuf>,
    all_is_folder_ext: Vec<bool>,
    /// Non-fatal load problems (bad manifests / duplicate identities) shown as a
    /// banner atop the module tree. Refreshed on every hot-reload.
    extension_errors: Vec<ExtensionLoadError>,
    cur_specs: Vec<Extension>,
    /// Tree folder for each entry in `cur_specs`, index-aligned.
    cur_dirs: Vec<PathBuf>,
    cur_is_folder_ext: Vec<bool>,
    /// Group the module tree by extension Author instead of folder.
    group_by_author: bool,
    /// Show the per-module inheritance/edit icons in the tree.
    show_inheritance: bool,
    games: Vec<(String, String)>,
    appid: String,
    game_command: Vec<String>,
    selected_ext: usize,
    game_config: GameConfig,
    preset: Option<Preset>,
    text_buffers: HashMap<String, String>,
    /// In-progress edit lists for `multi_string` fields, keyed by scope+ext+var.
    multi_edit: HashMap<String, Vec<String>>,
    /// True when opened from the splash to gate a launch.
    launch_mode: bool,
    /// The launch decision (shared so [`run`] can read it after the window closes).
    outcome: Arc<Mutex<EditOutcome>>,
    /// In-progress window-class detection, if any.
    detect: Option<Detect>,
    // Navigator panel state
    nav_sel: NavSel,
    /// Which top-level area is showing (see [`Mode`]). Orthogonal to `nav_sel`;
    /// in-memory only, never written to any config file.
    mode: Mode,
    /// Index into `all_specs` of the module the IDE column has selected. Kept
    /// separate from `selected_ext` (which indexes `cur_specs`) because the IDE
    /// tree browses the *unfiltered* module set — the two lists have different
    /// lengths and orderings, so one index cannot serve both.
    ide_selected: usize,
    all_presets: Vec<String>,
    /// Pinned profiles: name → slot id (1–10), cached from the preset files.
    preset_pins: HashMap<String, u8>,
    general_config: GeneralConfig,
    /// Global extension-variable overrides (stored as a preset in global.json).
    global_config: Preset,
    /// The preset currently loaded for editing when a Profile is selected in nav.
    editing_preset_buf: Option<Preset>,
    /// Shared rename/create name buffer for the nav settings panel.
    nav_name_buf: String,
    /// AppID buffer used only when creating a new game.
    nav_appid_buf: String,
    creating_profile: bool,
    duplicating_preset: Option<Preset>,
    creating_game: bool,
    /// Focus the create-profile/game name box on the next frame.
    focus_nav_name: bool,
    /// A destructive action awaiting confirmation, if any.
    confirm: Option<ConfirmAction>,
    /// App logo texture (loaded once the egui context exists).
    logo: Option<egui::TextureHandle>,
    /// In-memory draft for the module currently open in the editor, if any. Held
    /// independent of `nav_sel` so a dirty draft keeps the config-autosave
    /// interlock engaged even after the user navigates elsewhere.
    module_draft: Option<ModuleDraft>,
    /// True when an in-memory config edit was withheld from disk because a module
    /// editor draft was dirty; flushed once the draft becomes clean.
    pending_config_write: bool,
    /// The open Fork / Create-new module dialog, if any.
    module_dialog: Option<ModuleDialog>,
    /// "Also purge stored config" checkbox state for the pending Delete-module
    /// confirmation.
    delete_module_purge: bool,
    /// Transient message summarising which stored settings a fork snapshot (or a
    /// future rename) couldn't carry over. Shown near the module editor until
    /// dismissed.
    carryover_report: Option<String>,
    /// The nav selection to restore when the module editor is closed (the view the
    /// user was in when they opened it). Cleared on exit; `None` falls back to the
    /// ambient game.
    editor_return: Option<NavSel>,
    /// Measured ink centers for icon glyphs, so every icon sits visually centered
    /// in its cell (see `crate::icon_center`). Lives on the app so the
    /// measurement survives across frames.
    icon_cache: IconCenterCache,
}

/// In-memory editor state for one module manifest. `ext` carries every editable
/// block *except* the UI sections, which live in `sections` as an ordered
/// `Vec` (trivial reorder/rename/remove) and are folded back into an
/// [`Extension`] via [`ModuleDraft::snapshot`] for serialize / validate / preview.
struct ModuleDraft {
    /// The `Extension::id()` this draft edits — matches the on-disk manifest.
    id: String,
    /// Absolute manifest path (write target).
    manifest: PathBuf,
    /// True when the manifest lives in a user-writable (non-bundled) location.
    editable: bool,
    /// Everything but the UI sections (meta, backend, env/wrapper/arg blocks).
    ext: Extension,
    /// UI sections as an ordered list of `(section name, fields)`.
    sections: Vec<(String, Vec<UiField>)>,
    /// The on-disk manifest as a JSON value — the baseline for the dirty check.
    baseline: Value,
    /// UI-field variables present on disk. A field whose `Variable` is in this set
    /// is an *existing* field (rename would orphan config → read-only); a field
    /// whose variable is absent is *newly added* → its `Variable` is editable.
    baseline_vars: std::collections::HashSet<String>,
    /// Name-collision message for the module's **on-disk** identity, fed into the
    /// Save gate. For an existing editable module the on-disk Author/Name never
    /// collide with themselves (excluded by path), so this stays `None`; identity
    /// edits are validated separately via [`ModuleDraft::identity_error`].
    name_error: Option<String>,
    /// Staged identity change (Author / Name / existing-field `Variable`). Held
    /// **out of** [`ModuleDraft::snapshot`] so editing it never marks the draft
    /// dirty and never travels the normal Save path — it is committed only through
    /// the explicit Rename action.
    identity: PendingIdentity,
    /// Validity of the pending identity change, recomputed each frame by
    /// [`GuiApp::refresh_identity_state`]. `Some(msg)` = there IS a pending change
    /// but it cannot be committed (empty/colliding name, or a bad var rename);
    /// `None` = either no pending change or a committable one.
    identity_error: Option<String>,
}

/// Staged (Author, Name, per-variable) identity edits for the open module,
/// committed only by the Rename action. Seeded from the on-disk identity when the
/// draft is (re)built; `var_edits` is keyed by each **existing** field's current
/// (on-disk) `Variable` — the stable rename key — with the value being the edited
/// name buffer.
#[derive(Clone, Default)]
struct PendingIdentity {
    author: String,
    name: String,
    var_edits: IndexMap<String, String>,
}

impl ModuleDraft {
    /// Fold `sections` back into `ext.ui` to produce the full [`Extension`].
    fn snapshot(&self) -> Extension {
        let mut e = self.ext.clone();
        e.ui = self.sections.iter().cloned().collect();
        e
    }
    fn dirty(&self) -> bool {
        serde_json::to_value(self.snapshot()).ok().as_ref() != Some(&self.baseline)
    }
    /// Save is enabled only when the draft is on a writable module, differs from
    /// disk, validates, every `Requires` parses, and there is no name collision.
    /// Duplicate UI-section names collide when folded into `ext.ui` (an
    /// `IndexMap`), silently dropping a section on Save — block Save instead.
    fn sections_unique(&self) -> bool {
        let mut seen = std::collections::HashSet::new();
        self.sections.iter().all(|(k, _)| seen.insert(k))
    }
    fn save_enabled(&self) -> bool {
        let snap = self.snapshot();
        self.editable
            && self.sections_unique()
            && save_gate(
                self.dirty(),
                core_extension::validate(&snap).is_ok(),
                all_requires_parse(&snap),
                self.name_error.is_some(),
            )
    }

    /// Every current field `Variable` in declaration order (baseline + newly
    /// added). Used to detect chained/colliding var renames.
    fn current_field_vars(&self) -> Vec<String> {
        self.sections
            .iter()
            .flat_map(|(_, fields)| fields.iter().map(|f| f.variable.clone()))
            .collect()
    }

    /// The subset of `identity.var_edits` that actually renames an **existing**
    /// (still-present) field's variable, as `old → new`. Removed fields and
    /// unchanged names are excluded; an edited-to-empty name is kept (so the
    /// validator can flag it).
    fn changed_var_renames(&self) -> IndexMap<String, String> {
        let mut out = IndexMap::new();
        for (_, fields) in &self.sections {
            for f in fields {
                if !self.baseline_vars.contains(&f.variable) {
                    continue;
                }
                if let Some(new) = self.identity.var_edits.get(&f.variable) {
                    let new = new.trim();
                    if new != f.variable {
                        out.insert(f.variable.clone(), new.to_string());
                    }
                }
            }
        }
        out
    }

    /// True when the staged identity differs from the on-disk identity.
    fn has_pending_identity(&self) -> bool {
        self.identity.author != self.ext.meta.author
            || self.identity.name != self.ext.meta.name
            || !self.changed_var_renames().is_empty()
    }

    /// Reason the pending identity change cannot be committed, or `None` when
    /// there is no pending change or it is committable. `name_collides` is passed
    /// in because the collision check needs the full loaded set (owned by
    /// [`GuiApp`]).
    fn compute_identity_error(&self, name_collides: bool) -> Option<String> {
        if !self.has_pending_identity() {
            return None;
        }
        if self.identity.author.trim().is_empty() || self.identity.name.trim().is_empty() {
            return Some("Author and Name must not be empty".to_string());
        }
        if name_collides {
            return Some("Author + Name already in use".to_string());
        }
        validate_var_renames(&self.current_field_vars(), &self.changed_var_renames()).err()
    }
}

/// Reject an invalid set of `old → new` variable renames (pure, unit-tested):
///
/// - an empty target name;
/// - a **chained** rename — a new name equal to some *other* field's current
///   variable — which would strand config (do the renames one at a time);
/// - two renames producing the *same* new name.
///
/// `current_vars` is every field's pre-rename variable. `Ok(())` = safe to apply.
fn validate_var_renames(
    current_vars: &[String],
    renames: &IndexMap<String, String>,
) -> std::result::Result<(), String> {
    let mut seen_new: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (old, new) in renames {
        let new = new.trim();
        if new.is_empty() {
            return Err(format!("Variable \"{old}\" cannot be renamed to an empty name"));
        }
        // Chained rename / collision: the new name is already some other field's
        // live variable (a field renamed A→B while B still exists, or a swap).
        if current_vars.iter().any(|v| v == new && v != old) {
            return Err(format!(
                "Renaming \"{old}\" to \"{new}\" collides with an existing variable \u{2014} rename that one separately"
            ));
        }
        if !seen_new.insert(new) {
            return Err(format!("Two variables renamed to the same name \"{new}\""));
        }
    }
    Ok(())
}

/// Pure Save-gate predicate (factored out for testing): Save is allowed only when
/// the draft differs from disk, validates, every `Requires` parses, and no name
/// collision is flagged.
fn save_gate(dirty: bool, valid: bool, requires_all_parse: bool, name_error: bool) -> bool {
    dirty && valid && requires_all_parse && !name_error
}

/// Pure interlock predicate (factored out for testing): the normal config/scope
/// autosave is held while a module editor draft is dirty (draft ≠ disk).
fn config_autosave_held(editor_dirty: bool) -> bool {
    editor_dirty
}

/// Resolve where closing the module editor should land (factored out for
/// testing). Returns the stored `return_to` view when it is a real (non-editor)
/// selection; otherwise falls back to the ambient game so no dangling editor
/// selection remains.
fn editor_exit_target(return_to: Option<NavSel>, appid: &str) -> NavSel {
    match return_to {
        Some(NavSel::ModuleEditor(_)) | None => NavSel::Game(appid.to_string()),
        Some(other) => other,
    }
}

/// True when `(author, name)` is already taken by a loaded module — **Version-blind**
/// (compares Author+Name only, since config keys on those two). `exclude` skips the
/// module being edited/forked, matched by manifest path so a module never collides
/// with itself. `specs` and `manifests` are index-aligned.
fn name_collides(
    specs: &[Extension],
    manifests: &[PathBuf],
    author: &str,
    name: &str,
    exclude: Option<&Path>,
) -> bool {
    specs.iter().zip(manifests.iter()).any(|(s, m)| {
        exclude != Some(m.as_path())
            && s.meta.author == author
            && s.meta.name == name
    })
}

/// Turn a free-text Author/Name into a filesystem-safe slug component: ASCII
/// alphanumerics kept (lowercased), everything else → `_`, collapsed edges
/// trimmed. Empty input falls back to `module` so a filename always has a stem.
fn sanitize_slug(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let trimmed = mapped.trim_matches('_');
    if trimmed.is_empty() {
        "module".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Given a slug `base` (no extension) and an `exists` predicate over candidate
/// filenames, return the first free `"<base>.json"`, `"<base>-2.json"`,
/// `"<base>-3.json"`, … Factored out (predicate injected) so it's unit-testable
/// without touching the filesystem.
fn uniquify_slug(base: &str, exists: impl Fn(&str) -> bool) -> String {
    let first = format!("{base}.json");
    if !exists(&first) {
        return first;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}.json");
        if !exists(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Build the write path for a new module in `dir`: `sanitize(author)__sanitize(name).json`,
/// suffixed `-2`, `-3`, … if the file already exists (so a fork never clobbers an
/// unrelated manifest that happens to slug-collide).
fn unique_slug_path(dir: &Path, author: &str, name: &str) -> PathBuf {
    let base = format!("{}__{}", sanitize_slug(author), sanitize_slug(name));
    let fname = uniquify_slug(&base, |c| dir.join(c).exists());
    dir.join(fname)
}

/// True if every `Requires` expression in the manifest parses (empty = ok).
fn all_requires_parse(ext: &Extension) -> bool {
    let ok = |r: &Option<String>| {
        r.as_deref()
            .map_or(true, |s| condition::parse(s).is_ok())
    };
    ext.ui.values().flatten().all(|f| ok(&f.requires))
        && ext
            .env_vars
            .iter()
            .chain(ext.game_env_vars.iter())
            .all(|e| ok(&e.requires) && e.builder.iter().all(|b| ok(&b.requires)))
        && ext
            .wrappers
            .iter()
            .all(|w| ok(&w.requires) && w.builder.iter().all(|b| ok(&b.requires)))
        && ext.game_launch_args.iter().all(|a| ok(&a.requires))
}

/// Row-action returned by the reusable `[↑][↓][🗑]` widget; the caller applies the
/// container-correct op.
#[derive(Clone, Copy, PartialEq)]
enum RowAction {
    None,
    Up,
    Down,
    Remove,
}

/// A path-addressed structural edit (add / remove / reorder), collected during
/// render and applied to the draft *after* the render loop so no `Vec`/`IndexMap`
/// is mutated mid-iteration.
enum Deferred {
    Section(usize, RowAction),
    SectionAdd,
    Field(usize, usize, RowAction),
    FieldAdd(usize),
    FieldOpt(usize, usize, usize, RowAction),
    FieldOptAdd(usize, usize),
    Env(bool, usize, RowAction),
    EnvAdd(bool),
    EnvStep(bool, usize, usize, RowAction),
    EnvStepAdd(bool, usize),
    Wrapper(usize, RowAction),
    WrapperAdd,
    WrapperStep(usize, usize, RowAction),
    WrapperStepAdd(usize),
    Arg(usize, RowAction),
    ArgAdd,
}

/// What the editor header requested this frame (Save / Discard / Fork / Delete /
/// Rename).
enum TopAction {
    None,
    Save,
    Fork,
    Delete,
    Rename,
    Close,
}

#[derive(Clone)]
enum DetectStatus {
    Waiting,
    Done(Option<String>),
}

/// A pending "Detect window class" operation targeting one field.
struct Detect {
    result: Arc<Mutex<DetectStatus>>,
    start: Instant,
    /// Extension id (`author::name::version`), for the text-buffer key.
    ext_id: String,
    author: String,
    name: String,
    var: String,
    /// The navigation selection this detection was started under. If `nav_sel`
    /// has moved on by the time the 3s timer fires, the detection is cancelled
    /// rather than applied — see [`GuiApp::cancel_stale_detect`].
    nav: NavSel,
}

impl GuiApp {
    /// Build an editor for a game (does not open a window).
    pub fn new(ctx: &AppContext, appid: &str, name: &str, game_command: Vec<String>) -> GuiApp {
        let all_specs: Vec<Extension> = ctx.extensions.iter().map(|e| e.spec.clone()).collect();
        let all_dirs: Vec<PathBuf> = ctx.extensions.iter().map(|e| e.rel_dir.clone()).collect();
        let all_manifests: Vec<PathBuf> = ctx.extensions.iter().map(|e| e.manifest.clone()).collect();
        let all_is_folder_ext: Vec<bool> = ctx.extensions.iter().map(|e| e.is_folder_ext).collect();
        let extension_errors = ctx.extension_errors.clone();
        let general_config = ctx.paths.load_general().unwrap_or_default();
        let global_config = ctx.paths.load_global_config().unwrap_or_default();
        let all_presets = ctx.paths.list_presets();
        // Closing the editor window (X) defaults to the configured action.
        let close_outcome = if general_config.editor_close_launches {
            EditOutcome::Continue
        } else {
            EditOutcome::Cancel
        };
        let mut app = GuiApp {
            paths: ctx.paths.clone(),
            default_preset: ctx.general.default_preset.clone(),
            all_specs,
            all_dirs,
            all_manifests,
            all_is_folder_ext,
            extension_errors,
            cur_specs: Vec::new(),
            cur_dirs: Vec::new(),
            cur_is_folder_ext: Vec::new(),
            group_by_author: true,
            show_inheritance: true,
            games: ctx.list_games(),
            appid: String::new(),
            game_command,
            selected_ext: 0,
            game_config: GameConfig::new(appid, name),
            preset: None,
            text_buffers: HashMap::new(),
            multi_edit: HashMap::new(),
            launch_mode: false,
            outcome: Arc::new(Mutex::new(close_outcome)),
            detect: None,
            nav_sel: NavSel::Game(appid.to_string()),
            mode: Mode::Config,
            ide_selected: 0,
            all_presets,
            preset_pins: HashMap::new(),
            general_config,
            global_config,
            editing_preset_buf: None,
            nav_name_buf: name.to_string(),
            nav_appid_buf: String::new(),
            creating_profile: false,
            duplicating_preset: None,
            creating_game: false,
            focus_nav_name: false,
            confirm: None,
            logo: None,
            module_draft: None,
            pending_config_write: false,
            module_dialog: None,
            delete_module_purge: false,
            carryover_report: None,
            editor_return: None,
            icon_cache: IconCenterCache::new(),
        };
        app.switch_game(appid, name);
        app.nav_name_buf = app.game_config.game.name.clone();
        app.refresh_presets();
        app
    }

    fn switch_game(&mut self, appid: &str, name: &str) {
        self.appid = appid.to_string();
        self.nav_appid_buf = appid.to_string();
        self.game_config = self
            .paths
            .load_game(appid)
            .ok()
            .flatten()
            .unwrap_or_else(|| GameConfig::new(appid, name));
        let preset_name = self
            .game_config
            .config
            .modules
            .preset
            .clone()
            .or_else(|| self.default_preset.clone());
        self.preset = preset_name.and_then(|n| self.paths.load_preset(&n).ok().flatten());
        self.rebuild_cur_specs();
        self.text_buffers.clear();
        self.multi_edit.clear();
    }

    /// Rebuild `cur_specs` (the modules applicable to the current game) from
    /// `all_specs`, preserving the selected module by id where possible.
    fn rebuild_cur_specs(&mut self) {
        let prev_id = self.cur_specs.get(self.selected_ext).map(|s| s.id());
        self.cur_specs = Vec::new();
        self.cur_dirs = Vec::new();
        self.cur_is_folder_ext = Vec::new();
        for ((spec, dir), &is_fe) in self
            .all_specs
            .iter()
            .zip(self.all_dirs.iter())
            .zip(self.all_is_folder_ext.iter())
        {
            if spec.applies_to(&self.appid) {
                self.cur_specs.push(spec.clone());
                self.cur_dirs.push(dir.clone());
                self.cur_is_folder_ext.push(is_fe);
            }
        }
        self.selected_ext = prev_id
            .and_then(|id| self.cur_specs.iter().position(|s| s.id() == id))
            .unwrap_or(0);
    }

    /// Remove stored values for variables no longer declared by any loaded
    /// module — across the global config, every profile, and every game. Cleans
    /// up after extension updates (removed or renamed variables).
    fn config_cleanup(&mut self) {
        // (author, name, variable) tuples any loaded module still declares.
        let mut valid: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        for spec in &self.all_specs {
            for field in spec.ui.values().flatten() {
                valid.insert((
                    spec.meta.author.clone(),
                    spec.meta.name.clone(),
                    field.variable.clone(),
                ));
            }
        }

        // Global config.
        cleanup_modules(&mut self.global_config.modules, &valid);
        let _ = self.paths.save_global_config(&self.global_config);

        // Every profile on disk.
        for name in self.all_presets.clone() {
            if let Ok(Some(mut p)) = self.paths.load_preset(&name) {
                cleanup_modules(&mut p.modules, &valid);
                let _ = self.paths.save_preset(&p);
            }
        }

        // Every game on disk.
        for (appid, _) in self.games.clone() {
            if let Ok(Some(mut g)) = self.paths.load_game(&appid) {
                cleanup_modules(&mut g.config.modules.authors, &valid);
                let _ = self.paths.save_game(&g);
            }
        }

        // Mirror into the in-memory state so the tree updates immediately.
        cleanup_modules(&mut self.game_config.config.modules.authors, &valid);
        if let Some(p) = self.preset.as_mut() {
            cleanup_modules(&mut p.modules, &valid);
        }
        if let Some(p) = self.editing_preset_buf.as_mut() {
            cleanup_modules(&mut p.modules, &valid);
        }
    }

    /// Reload all extensions from disk into the running editor (hot-reload).
    fn reload_extensions(&mut self) {
        let Ok((exts, errors)) = context::load_extensions(&self.paths) else {
            return;
        };
        self.all_specs = exts.iter().map(|e| e.spec.clone()).collect();
        self.all_dirs = exts.iter().map(|e| e.rel_dir.clone()).collect();
        self.all_manifests = exts.iter().map(|e| e.manifest.clone()).collect();
        self.all_is_folder_ext = exts.iter().map(|e| e.is_folder_ext).collect();
        self.extension_errors = errors;
        self.rebuild_cur_specs();
    }

    /// Reload the on-disk configuration into the running editor: global config,
    /// general settings, the profile/game lists, the current game's config +
    /// preset, and the profile being edited. Discards uncommitted text edits.
    fn reload_configs(&mut self) {
        self.global_config = self.paths.load_global_config().unwrap_or_default();
        self.general_config = self.paths.load_general().unwrap_or_default();
        self.refresh_presets();
        self.refresh_games();
        let appid = self.appid.clone();
        let name = self.game_config.game.name.clone();
        self.switch_game(&appid, &name);
        if let NavSel::Profile(pname) = self.nav_sel.clone() {
            self.editing_preset_buf = self.paths.load_preset(&pname).ok().flatten();
        }
    }

    /// Scope tint for the layer currently being edited (Game blue / Profile green
    /// / Global red). Used to color values set at that layer.
    fn editing_scope_color(&self) -> Color32 {
        match &self.nav_sel {
            NavSel::GlobalSettings => COL_GLOBAL,
            NavSel::Profile(_) => COL_PROFILE,
            _ => COL_GAME,
        }
    }

    /// Resolution used for the extension editor (reflects whichever layer is being edited).
    fn resolve_for_editing(&self) -> Resolution {
        self.resolve_specs_for_editing(&self.cur_specs)
    }

    /// Like [`resolve_for_editing`] but over an explicit spec list.
    ///
    /// *Why this exists (S3b):* every arm below used to say `&self.cur_specs`
    /// literally. That was inert while the only caller resolved `cur_specs`
    /// anyway, but the IDE column's tree and preview browse the **unfiltered**
    /// `all_specs` — and a spec absent from `cur_specs` resolves to no
    /// `ExtResolution` at all, so its badges and its whole preview body would
    /// silently come up empty. Taking the list as a parameter makes the
    /// resolution match the data actually on screen. `resolve_for_editing`
    /// delegates here with `cur_specs`, so Config-mode behaviour is byte-identical.
    fn resolve_specs_for_editing(&self, specs: &[Extension]) -> Resolution {
        let global = Some(&self.global_config);
        match &self.nav_sel {
            NavSel::GeneralSettings => {
                resolve::resolve(specs, Some(&self.game_config), self.preset.as_ref(), global)
            }
            // ModuleEditor is a read-only view over one module, not a config-edit
            // scope — resolve as if the ambient game were selected.
            NavSel::Game(_) | NavSel::ModuleEditor(_) => {
                let merged: Option<Preset> = self.preset.as_ref().and_then(|p| {
                    let pname = p.parent.as_ref()?;
                    let mut base = collect_parent_chain(&self.paths, pname);
                    merge_modules(&mut base.modules, &p.modules);
                    Some(base)
                });
                let effective = merged.as_ref().map(|p| p as &Preset).or(self.preset.as_ref());
                resolve::resolve(specs, Some(&self.game_config), effective, global)
            }
            NavSel::GlobalSettings => {
                let fake = preset_as_fake_game(&self.global_config);
                resolve::resolve(specs, Some(&fake), None, None)
            }
            NavSel::Profile(_) => match &self.editing_preset_buf {
                Some(p) => {
                    let parent = p.parent.as_ref()
                        .map(|n| collect_parent_chain(&self.paths, n));
                    let fake = preset_as_fake_game(p);
                    resolve::resolve(specs, Some(&fake), parent.as_ref(), global)
                }
                None => resolve::resolve(specs, None, None, global),
            },
        }
    }

    /// Resolution always for the current game — used for the launch command preview.
    fn resolve_for_game(&self) -> Resolution {
        self.resolve_specs_for_game(&self.cur_specs)
    }

    /// Like [`resolve_for_game`] but over an explicit spec list — lets the module
    /// editor resolve a draft-spliced spec set for its live preview without
    /// touching disk or the launch assembler's signature.
    fn resolve_specs_for_game(&self, specs: &[Extension]) -> Resolution {
        // If the profile being edited is the one applied to this game, use the
        // live (unsaved) edit buffer so the preview reflects in-progress edits.
        let direct = match (&self.editing_preset_buf, &self.preset) {
            (Some(buf), Some(applied)) if buf.name == applied.name => Some(buf as &Preset),
            _ => self.preset.as_ref(),
        };
        // Merge parent chain under the direct preset (parent = lower priority).
        let merged: Option<Preset> = direct.and_then(|p| {
            let pname = p.parent.as_ref()?;
            let mut base = collect_parent_chain(&self.paths, pname);
            merge_modules(&mut base.modules, &p.modules);
            Some(base)
        });
        let effective = merged.as_ref().map(|p| p as &Preset).or(direct);
        resolve::resolve(
            specs,
            Some(&self.game_config),
            effective,
            Some(&self.global_config),
        )
    }

    /// Manifest path + editability for a module id. A module is editable only when
    /// its manifest is *not* one of the bundled sets (`default/…`, `built-in/…`),
    /// which — although bootstrapped into the user config dir — remain
    /// inspect-only until forked (Phase 3).
    fn module_editability(&self, id: &str) -> Option<(PathBuf, bool)> {
        let idx = self.all_specs.iter().position(|s| s.id() == id)?;
        let rel = &self.all_dirs[idx];
        let top = rel.components().next().and_then(|c| c.as_os_str().to_str());
        let bundled = matches!(top, Some("default") | Some("built-in"));
        Some((self.all_manifests[idx].clone(), !bundled))
    }

    /// Open the module editor for `id`, remembering the current view so Close can
    /// restore it. No-op if already editing that module.
    fn open_module_editor(&mut self, id: String) {
        if self.nav_sel == NavSel::ModuleEditor(id.clone()) {
            return;
        }
        if !matches!(self.nav_sel, NavSel::ModuleEditor(_)) {
            self.editor_return = Some(self.nav_sel.clone());
        }
        self.nav_sel = NavSel::ModuleEditor(id);
    }

    /// Leave the module editor for the remembered (or ambient) view, dropping the
    /// in-memory draft. Releasing the interlock flushes any held config writes.
    fn exit_module_editor(&mut self) {
        let target = editor_exit_target(self.editor_return.take(), &self.appid);
        self.module_draft = None;
        self.nav_sel = target;
        self.flush_config_writes_if_clean();
    }

    /// Load a fresh draft for `id` unless one is already open for it (keeping any
    /// in-progress edits). Opening a *different* module replaces the draft.
    fn ensure_draft(&mut self, id: &str) {
        if self.module_draft.as_ref().map(|d| d.id.as_str()) == Some(id) {
            return;
        }
        let Some(spec) = self.all_specs.iter().find(|s| s.id() == id).cloned() else {
            self.module_draft = None;
            return;
        };
        let (manifest, editable) = self
            .module_editability(id)
            .unwrap_or_else(|| (PathBuf::new(), false));
        let baseline = serde_json::to_value(&spec).unwrap_or(Value::Null);
        let baseline_vars: std::collections::HashSet<String> = spec
            .ui
            .values()
            .flatten()
            .map(|f| f.variable.clone())
            .collect();
        let sections: Vec<(String, Vec<UiField>)> =
            spec.ui.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut ext = spec;
        ext.ui.clear(); // authoritative UI lives in `sections`
        // Seed the staged identity from the on-disk identity: Author/Name buffers
        // and one var-edit buffer per existing field variable (keyed by its
        // current name), so an unedited draft has no pending change.
        let identity = PendingIdentity {
            author: ext.meta.author.clone(),
            name: ext.meta.name.clone(),
            var_edits: baseline_vars
                .iter()
                .map(|v: &String| (v.clone(), v.clone()))
                .collect(),
        };
        self.module_draft = Some(ModuleDraft {
            id: id.to_string(),
            manifest,
            editable,
            ext,
            sections,
            baseline,
            baseline_vars,
            name_error: None,
            identity,
            identity_error: None,
        });
    }

    /// Serialize the draft and write it to its manifest via `write_atomic`, then
    /// reload extensions so `cur_specs`/dirty reset (draft == disk). Refuses to
    /// write unless the Save gate is satisfied.
    fn save_module(&mut self) {
        let Some(draft) = self.module_draft.as_ref() else {
            return;
        };
        if !draft.save_enabled() {
            return;
        }
        let manifest = draft.manifest.clone();
        let json = match serde_json::to_string_pretty(&draft.snapshot()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ritz: failed to serialize module: {e:#}");
                return;
            }
        };
        if let Err(e) = core_config::write_atomic(&manifest, json.as_bytes()) {
            eprintln!("ritz: failed to save module: {e:#}");
            return;
        }
        // Drop the draft and reload from disk, then leave the editor so the user
        // sees the saved ("real") module in the normal view.
        self.module_draft = None;
        self.reload_extensions();
        self.exit_module_editor();
    }

    /// Discard the in-memory draft (Revert) and leave the editor. `exit_module_editor`
    /// clears the draft and releasing the interlock flushes any held config writes.
    fn discard_module(&mut self) {
        self.exit_module_editor();
    }

    /// Recompute the open draft's live name-collision flag (Version-blind, excluding
    /// itself by manifest path). For an existing editable module Author/Name are
    /// locked, so this stays `None`; the field is wired now for stage 2b's rename.
    fn refresh_draft_name_error(&mut self) {
        let flag = self.module_draft.as_ref().map(|d| {
            name_collides(
                &self.all_specs,
                &self.all_manifests,
                &d.ext.meta.author,
                &d.ext.meta.name,
                Some(d.manifest.as_path()),
            )
        });
        if let (Some(collides), Some(draft)) = (flag, self.module_draft.as_mut()) {
            draft.name_error = collides.then(|| "Author + Name already in use".to_string());
        }
    }

    /// Recompute the open draft's pending-identity validity. The (Author, Name)
    /// collision check is Version-blind and excludes the module itself by manifest
    /// path — same rule as [`refresh_draft_name_error`], but run against the
    /// *pending* (edited) Author/Name rather than the on-disk pair.
    fn refresh_identity_state(&mut self) {
        let collides = self.module_draft.as_ref().map(|d| {
            name_collides(
                &self.all_specs,
                &self.all_manifests,
                d.identity.author.trim(),
                d.identity.name.trim(),
                Some(d.manifest.as_path()),
            )
        });
        if let (Some(collides), Some(draft)) = (collides, self.module_draft.as_mut()) {
            draft.identity_error = draft.compute_identity_error(collides);
        }
    }

    /// Open the Fork dialog seeded from the module `id` (its Author, and
    /// "`<name>` (copy)" as the default fork name).
    fn open_fork_dialog(&mut self, id: &str) {
        let Some(spec) = self.all_specs.iter().find(|s| s.id() == id) else {
            return;
        };
        let author = if spec.meta.author.is_empty() {
            "Ritze".to_string()
        } else {
            spec.meta.author.clone()
        };
        let name = format!("{} (copy)", spec.meta.name);
        self.module_dialog = Some(ModuleDialog {
            kind: DialogKind::Fork { parent_id: id.to_string() },
            author,
            name,
            copy_settings: true,
        });
    }

    /// Open the Create-new-module dialog (empty template; no copy-settings option).
    fn open_create_dialog(&mut self) {
        self.module_dialog = Some(ModuleDialog {
            kind: DialogKind::Create,
            author: "Ritze".to_string(),
            name: "New Module".to_string(),
            copy_settings: false,
        });
    }

    /// Deep-copy the parent module, retag it with the new Author/Name + `ForkedFrom`
    /// lineage, write it to the user extensions dir under a unique slug, optionally
    /// snapshot the parent's stored config into the fork's namespace, then reload
    /// and open the fork in the editor.
    fn perform_fork(&mut self, parent_id: &str, author: String, name: String, copy_settings: bool) {
        self.carryover_report = None;
        let Some(parent) = self.all_specs.iter().find(|s| s.id() == parent_id).cloned() else {
            return;
        };
        let parent_author = parent.meta.author.clone();
        let parent_name = parent.meta.name.clone();
        let mut ext = parent;
        ext.meta.author = author.clone();
        ext.meta.name = name.clone();
        ext.meta.forked_from = Some(format!("{parent_author}::{parent_name}"));

        let path = unique_slug_path(&self.paths.user_extensions(), &author, &name);
        let json = match serde_json::to_string_pretty(&ext) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ritz: failed to serialize fork: {e:#}");
                return;
            }
        };
        if let Err(e) = core_config::write_atomic(&path, json.as_bytes()) {
            eprintln!("ritz: failed to write fork: {e:#}");
            return;
        }

        if copy_settings {
            match core_config::snapshot_config_to_fork(
                &self.paths,
                (&parent_author, &parent_name),
                (&author, &name),
            ) {
                Ok(report) => self.set_carryover_report(report),
                Err(e) => eprintln!("ritz: fork config snapshot failed: {e:#}"),
            }
        }

        let new_id = ext.id();
        self.module_draft = None;
        self.reload_extensions();
        self.reload_configs();
        self.nav_sel = NavSel::ModuleEditor(new_id);
    }

    /// Commit the staged identity change (Author / Name / existing-field
    /// `Variable`) for the open editable module. Safety-critical **ordering**:
    ///
    /// 1. compute `from` = current on-disk identity, `to` = new identity, and the
    ///    `old→new` var renames from the changed existing fields;
    /// 2. **scope sweep FIRST** — [`migrate_renamed_module`] moves stored config
    ///    across every scope; on error, abort **without** touching the manifest;
    /// 3. build the new manifest from the draft snapshot: apply the new
    ///    Author/Name and rewrite in-module `{var}`/`Requires` references + field
    ///    `Variable`s via [`apply_var_renames`];
    /// 4. write the manifest **LAST, in place** (file never renamed; `id()` comes
    ///    from JSON meta) — only after the sweep succeeded.
    ///
    /// Because the sweep is idempotent and the manifest is written last, a crash
    /// between steps 2 and 4 leaves the manifest on the OLD identity, so
    /// re-pressing Rename re-runs cleanly (already-moved scopes no-op).
    fn perform_rename(&mut self) {
        // Snapshot everything we need out of the draft so the immutable borrow is
        // released before we touch `self.paths` / reload.
        let Some((mut ext, manifest, old_author, old_name, new_author, new_name, var_rename)) = self
            .module_draft
            .as_ref()
            .filter(|d| d.editable && d.has_pending_identity() && d.identity_error.is_none())
            .map(|d| {
                (
                    d.snapshot(),
                    d.manifest.clone(),
                    d.ext.meta.author.clone(),
                    d.ext.meta.name.clone(),
                    d.identity.author.trim().to_string(),
                    d.identity.name.trim().to_string(),
                    d.changed_var_renames(),
                )
            })
        else {
            return;
        };

        self.carryover_report = None;
        let from = (old_author.as_str(), old_name.as_str());
        let to = (new_author.as_str(), new_name.as_str());

        // Step 2: scope sweep FIRST. On error, abort before the manifest is touched.
        let report =
            match core_config::migrate_renamed_module(&self.paths, from, to, &var_rename) {
                Ok(r) => r,
                Err(e) => {
                    self.carryover_report =
                        Some(format!("Rename aborted: config migration failed: {e}"));
                    return;
                }
            };

        // Step 3: build the new manifest (identity + reference rewrite).
        ext.meta.author = new_author;
        ext.meta.name = new_name;
        apply_var_renames(&mut ext, &var_rename);

        // Step 4: write the manifest LAST, in place, only after the sweep succeeded.
        let json = match serde_json::to_string_pretty(&ext) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ritz: failed to serialize renamed module: {e:#}");
                return;
            }
        };
        if let Err(e) = core_config::write_atomic(&manifest, json.as_bytes()) {
            eprintln!("ritz: failed to write renamed module: {e:#}");
            return;
        }

        // Step 5: report dropped vars, reload, keep the editor open on the new id.
        let new_id = ext.id();
        self.set_carryover_report(report);
        self.module_draft = None;
        self.reload_extensions();
        self.reload_configs();
        self.nav_sel = NavSel::ModuleEditor(new_id);
    }

    /// Write a minimal valid template module (meta + one empty UI section) to the
    /// user extensions dir, then reload and open it in the editor.
    fn perform_create(&mut self, author: String, name: String) {
        self.carryover_report = None;
        let template = json!({
            "Extension": { "Name": name, "Author": author, "Version": "1.0" },
            "UI": { "General": [] }
        });
        let ext: Extension = match serde_json::from_value(template) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("ritz: failed to build template module: {e:#}");
                return;
            }
        };
        if let Err(e) = core_extension::validate(&ext) {
            eprintln!("ritz: template module failed validation: {e:#}");
            return;
        }
        let path = unique_slug_path(&self.paths.user_extensions(), &author, &name);
        let json = match serde_json::to_string_pretty(&ext) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ritz: failed to serialize template module: {e:#}");
                return;
            }
        };
        if let Err(e) = core_config::write_atomic(&path, json.as_bytes()) {
            eprintln!("ritz: failed to write template module: {e:#}");
            return;
        }
        let new_id = ext.id();
        self.module_draft = None;
        self.reload_extensions();
        self.nav_sel = NavSel::ModuleEditor(new_id);
    }

    /// Delete a user-authored module's manifest file. When `purge` is set, also
    /// sweep the now-undeclared config across every scope via [`config_cleanup`].
    /// Leaves the module editor for the ambient game.
    fn delete_module(&mut self, manifest: &Path, purge: bool) {
        if let Err(e) = std::fs::remove_file(manifest) {
            eprintln!("ritz: failed to delete module: {e:#}");
            return;
        }
        self.module_draft = None;
        self.reload_extensions();
        if purge {
            // The deleted module's vars are now undeclared, so cleanup removes
            // exactly them (alongside any other stale values it normally prunes).
            self.config_cleanup();
        }
        self.nav_sel = NavSel::Game(self.appid.clone());
    }

    /// Format a fork/rename dropped-var report into the transient carryover
    /// message (cleared when empty — a clean copy carries everything over).
    fn set_carryover_report(&mut self, report: Vec<(String, Vec<String>)>) {
        if report.is_empty() {
            return;
        }
        let count: usize = report.iter().map(|(_, vars)| vars.len()).sum();
        let mut parts = Vec::new();
        for (scope, vars) in &report {
            for v in vars {
                parts.push(format!("{v} ({scope})"));
            }
        }
        self.carryover_report = Some(format!(
            "{count} setting(s) couldn't be carried over: {}",
            parts.join(", ")
        ));
    }

    /// Flush config edits that were withheld by the interlock, but only once no
    /// module draft is dirty. Saves every config layer so a held edit at any
    /// scope (global / profile / game) reaches disk.
    fn flush_config_writes_if_clean(&mut self) {
        if !self.pending_config_write {
            return;
        }
        if self.module_draft.as_ref().map_or(false, |d| d.dirty()) {
            return;
        }
        let _ = self.paths.save_global_config(&self.global_config);
        let _ = self.paths.save_game(&self.game_config);
        if let Some(p) = &self.editing_preset_buf {
            let _ = self.paths.save_preset(p);
        }
        self.pending_config_write = false;
    }

    fn persist(&self) {
        let result = match &self.nav_sel {
            NavSel::GlobalSettings => self.paths.save_global_config(&self.global_config),
            NavSel::Profile(_) => match &self.editing_preset_buf {
                Some(p) => self.paths.save_preset(p),
                None => return,
            },
            NavSel::GeneralSettings | NavSel::Game(_) | NavSel::ModuleEditor(_) => {
                self.paths.save_game(&self.game_config)
            }
        };
        if let Err(e) = result {
            eprintln!("ritz: failed to save: {e:#}");
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }
}

impl GuiApp {
    /// Render the editor into the given context (panels). Usable both as the whole
    /// window and embedded under another app's top bar.
    pub fn ui(&mut self, ctx: &egui::Context) {
        // Ctrl+R: hot-reload extensions, then the configs, from disk.
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::R)) {
            self.reload_extensions();
            self.reload_configs();
        }

        // A draft may only exist while the editor view is active. If the selection
        // moved off the editor by ANY route (a left-nav click, the IDE tree, …)
        // while a draft is still open, treat it as an implicit Close/discard — drop
        // the draft so a dirty draft can't keep the config-autosave interlock
        // engaged and strand later config writes. The frame-end
        // `flush_config_writes_if_clean` then releases the interlock.
        if !matches!(self.nav_sel, NavSel::ModuleEditor(_)) && self.module_draft.is_some() {
            self.module_draft = None;
            self.editor_return = None;
            self.flush_config_writes_if_clean();
        }

        // IDE-mode invariant: a module is ALWAYS open. `nav_sel` is IDE mode's
        // "which module" carrier, so anything that clears it (the editor's Close
        // button, a Save, a Delete) would otherwise drop the central column into
        // Config mode's module-detail view inside an IDE-shaped window. Re-open the
        // tree's current selection instead, which makes Close read as
        // "discard and reload" — the right meaning when there is nowhere to close to.
        // Runs AFTER the draft-drop guard above so Close's discard still happens.
        if self.mode == Mode::Ide && !matches!(self.nav_sel, NavSel::ModuleEditor(_)) {
            if let Some(spec) = self.all_specs.get(self.ide_selected) {
                let id = spec.id();
                self.open_module_editor(id);
            }
        }

        // Keep the module-editor draft in sync with the selected module.
        if let NavSel::ModuleEditor(id) = self.nav_sel.clone() {
            self.ensure_draft(&id);
            self.refresh_draft_name_error();
            self.refresh_identity_state();
            // Ctrl+S: Save when the editor is open and the Save gate is satisfied
            // (same condition as the button); a no-op otherwise. Save exits the
            // editor on success.
            let save_ok = self.module_draft.as_ref().map_or(false, |d| d.save_enabled());
            if save_ok && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
                self.save_module();
            }
        }

        let edit_resolution = self.resolve_for_editing();
        let game_resolution = self.resolve_for_game();
        let mut changed = false;

        self.render_title_bar(ctx);

        egui::SidePanel::left("nav")
            .exact_width(NAV_W)
            .resizable(false)
            .frame(egui::Frame::none().fill(theme::PANEL))
            .show(ctx, |ui| {
                self.render_nav_panel(ui);
            });
        // The nav panel may have just changed `nav_sel`; drop any detection belonging
        // to the scope we left before anything renders its "Detecting… Ns" countdown.
        // `poll_detect` re-checks at the end of the frame to catch the other nav sites.
        self.cancel_stale_detect();

        // The module-list column exists only in Config mode: IDE mode moves the
        // module tree into the nav column, so a second copy here would be redundant
        // (and would eat the width the editor/preview split needs).
        if self.mode == Mode::Config && !matches!(self.nav_sel, NavSel::GeneralSettings) {
        egui::SidePanel::left("ext_list")
            .exact_width(280.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(theme::PANEL))
            .show(ctx, |ui| {
            egui::TopBottomPanel::top("ext_header")
                .show_separator_line(false)
                .frame(egui::Frame::none()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin { left: 16.0, right: 16.0, top: 14.0, bottom: 8.0 }))
                .show_inside(ui, |ui| {
                    // Create-module now lives only in IDE Mode's nav footer
                    // (`render_ide_nav_footer`'s "+ New Module" button, which shares
                    // `open_create_dialog` with what used to be here) — this header
                    // used to carry its own "+" affordance too, but IDE Mode has
                    // taken over module creation/editing, so a second entry point
                    // here was redundant. Plain label now, no trailing button.
                    ui.label(theme::header_label("Modules"));
                });
            // Toggles + Open Folder pinned to the bottom footer band.
            egui::TopBottomPanel::bottom("group_toggle")
                .exact_height(198.0)
                .show_separator_line(true)
                .frame(egui::Frame::none()
                    .fill(theme::PANEL2)
                    .inner_margin(egui::Margin::same(14.0)))
                .show_inside(ui, |ui| {
                    self.render_modules_footer(ui);
                });
            egui::CentralPanel::default()
                .frame(egui::Frame::none()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin { left: 8.0, right: 18.0, top: 0.0, bottom: 0.0 }))
                .show_inside(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .drag_to_scroll(self.general_config.touch_mode)
                        .show(ui, |ui| {
                            ui.add_space(4.0);
                            self.render_ext_errors_banner(ui);
                            // The MODULES tree is inert (and greyed) while the
                            // module editor is open, so a stray click can't swap
                            // the module out from under an in-progress edit. Exit
                            // is via Close / Save (or Ctrl+S) only.
                            let tree_live = self.module_draft.is_none();
                            ui.add_enabled_ui(tree_live, |ui| {
                                // `render_ext_tree` now takes its data source as
                                // parameters (S3a) so S3b can reuse it against
                                // `all_specs`/`all_dirs`/`all_is_folder_ext` for a
                                // read-only preview column. Clone/copy out of `self`
                                // first — passing `&self.cur_specs`/`&mut
                                // self.selected_ext` directly would borrow `self`
                                // while the method call also needs `&mut self`.
                                let tree_specs = self.cur_specs.clone();
                                let tree_dirs = self.cur_dirs.clone();
                                let tree_is_folder_ext = self.cur_is_folder_ext.clone();
                                let mut tree_selected = self.selected_ext;
                                let show_inheritance = self.show_inheritance;
                                self.render_ext_tree(
                                    ui,
                                    &tree_specs,
                                    &tree_dirs,
                                    &tree_is_folder_ext,
                                    &mut tree_selected,
                                    show_inheritance,
                                );
                                self.selected_ext = tree_selected;
                            });
                        });
                });
        });
        } // end if !GeneralSettings

        // The spec list both IDE columns (preview body + launch band) resolve and
        // assemble against: the ambient game's applicable modules, with the module
        // under edit spliced over its entry — or *appended* when it isn't in
        // `cur_specs` at all. The append matters because the IDE tree browses the
        // unfiltered `all_specs`: without it, opening a module that doesn't apply to
        // this game would show a preview that silently ignores everything you type.
        // Empty (and never used) outside IDE mode, so Config mode is untouched.
        let ide_specs: Vec<Extension> = if self.mode == Mode::Ide {
            let mut specs = self.cur_specs.clone();
            if let NavSel::ModuleEditor(id) = self.nav_sel.clone() {
                let snap = self
                    .module_draft
                    .as_ref()
                    .filter(|d| d.id == id)
                    .map(|d| d.snapshot())
                    .or_else(|| self.all_specs.iter().find(|s| s.id() == id).cloned());
                if let Some(snap) = snap {
                    match specs.iter().position(|s| s.id() == id) {
                        Some(pos) => specs[pos] = snap,
                        None => specs.push(snap),
                    }
                }
            }
            specs
        } else {
            Vec::new()
        };

        // Assemble the launch preview. In the module editor, splice the in-memory
        // draft over its on-disk entry so the preview reflects unsaved edits, and
        // lint for set/set env collisions across modules.
        let (preview, collisions): (String, Vec<String>) = if self.mode == Mode::Ide {
            let res = self.resolve_specs_for_game(&ide_specs);
            let text = context::assemble_launch(&ide_specs, &res, &self.game_command)
                .map(|lc| lc.to_string())
                .unwrap_or_else(|e| format!("<error: {e}>"));
            let coll = set_set_collisions(&ide_specs, &res);
            (text, coll)
        } else if matches!(self.nav_sel, NavSel::ModuleEditor(_)) {
                match &self.module_draft {
                    Some(draft) => {
                        let mut preview_specs = self.cur_specs.clone();
                        if let Some(pos) =
                            preview_specs.iter().position(|s| s.id() == draft.id)
                        {
                            preview_specs[pos] = draft.snapshot();
                        }
                        let res = self.resolve_specs_for_game(&preview_specs);
                        let text =
                            context::assemble_launch(&preview_specs, &res, &self.game_command)
                                .map(|lc| lc.to_string())
                                .unwrap_or_else(|e| format!("<error: {e}>"));
                        let coll = set_set_collisions(&preview_specs, &res);
                        (text, coll)
                    }
                    None => (String::new(), Vec::new()),
                }
            } else {
                let preview_res = if matches!(self.nav_sel, NavSel::Profile(_)) {
                    &edit_resolution
                } else {
                    &game_resolution
                };
                let text =
                    context::assemble_launch(&self.cur_specs, preview_res, &self.game_command)
                        .map(|lc| lc.to_string())
                        .unwrap_or_else(|e| format!("<error: {e}>"));
                (text, Vec::new())
            };

        let dynamic_preview = self.general_config.dynamic_preview;

        // ── Panel declaration order is load-bearing ──────────────────────────
        // egui hands each panel the rect left over by the panels declared before
        // it (`TopBottomPanel`/`SidePanel::show` both start from
        // `ctx.available_rect()`). Order here is: nav (above, left) →
        // ide_module_header (top, spans what's right of the nav) → ide_preview
        // (right, full remaining height) → ide_editor_band (bottom, in what's
        // left) → CentralPanel (the editor).
        //
        // The header's slot is the load-bearing part: declared AFTER the nav it
        // starts at the nav's right edge instead of the window's, and declared
        // BEFORE the preview it spans the editor AND preview columns rather than
        // just the editor's half. Move it after `render_ide_preview_panel` and it
        // silently shrinks to the editor column — no compile error, just the old
        // cramped layout back.
        let mut ide_action = TopAction::None;
        let mut ide_header: Option<EditorHeaderInfo> = None;
        if self.mode == Mode::Ide {
            // Full-width module header, above both columns.
            //
            // *Why it spans both columns* (2026-07-19): the action cluster
            // (`[Fork] [Delete] [✕] [Save]` + `Rename`) plus the name/version/author
            // heading needs ~500pt, and inside the editor's half that set the
            // column's effective minimum window width — the buttons collided with
            // the module name on anything under ~1300pt wide. Spanning the full
            // width right of the nav roughly halves that floor and decouples the
            // editor column's width from the button row entirely.
            if let NavSel::ModuleEditor(id) = self.nav_sel.clone() {
                ide_header = self.editor_header_info(&id);
                let cache = &mut self.icon_cache;
                let info = ide_header.as_ref();
                egui::TopBottomPanel::top("ide_module_header")
                    .exact_height(IDE_HEADER_H)
                    // Kept: with the band now filled the SAME colour as the
                    // columns under it, this hairline (`noninteractive.bg_stroke`
                    // = `theme::BORDER`) is the only thing marking the edge. A
                    // second, hand-drawn bottom border would just double it.
                    .show_separator_line(true)
                    // *Why `PANEL` and not `PANEL2`* (2026-07-19): `PANEL` is the
                    // fill of the nav column, the editor `CentralPanel` and the
                    // preview panel, so the header reads as the top of one
                    // continuous surface. `PANEL2` (the 198px bands' fill) made it
                    // read as a separate toolbar sitting on top of the editor —
                    // tried, seen, rejected.
                    .frame(egui::Frame::none()
                        .fill(theme::PANEL)
                        .inner_margin(egui::Margin {
                            left: 8.0,
                            right: 16.0,
                            top: 6.0,
                            bottom: 8.0,
                        }))
                    .show(ctx, |ui| {
                        // `None` renders an empty band rather than skipping the
                        // panel: `exact_height` holds the layout still while
                        // `ensure_draft` catches up with a module switch.
                        if let Some(info) = info {
                            ide_action = render_editor_header_row(ui, cache, info);
                            // Second row, full band width. *Why below the name row
                            // and not beside it:* the action cluster owns the right
                            // end of row one, so anything sharing that row competes
                            // for the same space and re-creates the collision the
                            // band was built to fix. A row of its own cannot collide.
                            render_editor_header_description(ui, info.description.as_deref());
                        }
                    });
            }
            self.render_ide_preview_panel(ctx, &ide_specs, &preview, &collisions, dynamic_preview);
            // Reserved, declared-but-empty: the future diagnostics pane under the
            // editor column. `exact_height(198.0)` is the same band height as the
            // nav footer, so the two bottom edges align across the window.
            egui::TopBottomPanel::bottom("ide_editor_band")
                .exact_height(198.0)
                .show_separator_line(true)
                .frame(egui::Frame::none()
                    .fill(theme::PANEL2)
                    .inner_margin(egui::Margin::same(16.0)))
                .show(ctx, |_ui| {});
        }

        // Config mode only. *Why skipped entirely rather than emptied:* a
        // zero-content bottom panel still reserves its height across the FULL window
        // width, which would push a dead strip under the IDE editor column on top of
        // the band declared above.
        if self.mode == Mode::Config {
            let mut panel = egui::TopBottomPanel::bottom("preview")
                .show_separator_line(true)
                .frame(egui::Frame::none()
                    .fill(theme::PANEL2)
                    .inner_margin(egui::Margin::same(16.0)));
            if !dynamic_preview {
                panel = panel.exact_height(198.0);
            }
            panel.show(ctx, |ui| {
                // General Settings has no launch command — show repo link + credits
                // in the same box instead of the preview.
                if matches!(self.nav_sel, NavSel::GeneralSettings) {
                    self.render_about(ui);
                    return;
                }
                ui.label(theme::header_label("Launch command preview"));
                if matches!(self.nav_sel, NavSel::ModuleEditor(_)) {
                    ui.label(
                        egui::RichText::new(format!(
                            "Previewing against: {}",
                            self.game_config.game.name
                        ))
                        .color(theme::FAINT)
                        .small(),
                    );
                }
                for var in &collisions {
                    ui.label(
                        egui::RichText::new(format!(
                            "\u{f071} Two modules both Set {var} — one value will be lost.",
                        ))
                        .color(theme::COL_GLOBAL)
                        .small(),
                    );
                }
                ui.add_space(8.0);
                egui::Frame::none()
                    .fill(theme::FIELD)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(egui::Margin::symmetric(13.0, 11.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if !dynamic_preview {
                            // Fill the remaining footer height; the panel's 16px bottom
                            // margin becomes the gap below, matching the left/right insets.
                            let box_h = ui.available_height();
                            ui.set_min_height((box_h - 22.0).max(0.0));
                        }
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, dynamic_preview])
                            .show(ui, |ui| {
                                ui.add(egui::Label::new(command_job(&preview)).wrap());
                            });
                    });
            });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if matches!(self.nav_sel, NavSel::GeneralSettings) {
                if self.render_general_settings_panel(ui) {
                    changed = true;
                }
                return;
            }

            if let NavSel::ModuleEditor(id) = self.nav_sel.clone() {
                // IDE mode drew the header row in its own full-width band above
                // and dispatches that click itself, below.
                let header_inline = self.mode == Mode::Config;
                self.render_module_editor(ui, &id, header_inline);
                return;
            }

            let Some(spec) = self.cur_specs.get(self.selected_ext).cloned() else {
                ui.label("No extensions apply to this game.");
                return;
            };

            // S3a: the detail header (name/version/author/description/separator)
            // and the settings body (ScrollArea + section/field loop + legend/hint)
            // are extracted methods so S3b can call the body a second time for a
            // read-only preview column without duplicating this layout.
            self.render_module_detail_header(ui, &spec);

            let ext_id = spec.id();
            let ext_res = edit_resolution.exts.get(&ext_id);
            let full_width = self.general_config.full_width;
            if self.render_module_settings_body(ui, &spec, ext_res, full_width, false) {
                changed = true;
            }
        });

        // IDE mode's header band produced its click before the preview column and
        // the editor body rendered; carry it out only now that both have. Acting on
        // it at the point of the click would drop the draft mid-frame and leave
        // everything downstream rendering a "no longer available" flash against
        // state the header had already invalidated. This lands at the same point in
        // the frame as Config mode's inline dispatch (the tail of
        // `render_module_editor`, one closure up), so `ensure_draft`, the draft-drop
        // guard, the IDE reopen invariant and the autosave interlock below all keep
        // their existing order.
        if let (Some(info), NavSel::ModuleEditor(id)) = (ide_header.as_ref(), self.nav_sel.clone()) {
            self.dispatch_top_action(ide_action, &id, info.dirty, info.editable);
        }

        if self.render_confirm_dialog(ctx) {
            changed = true;
        }

        self.render_module_dialog(ctx);

        if self.poll_detect(ctx) {
            changed = true;
        }

        if changed {
            // Interlock: hold the config write to disk while a module editor draft
            // is dirty (the in-memory config already reflects the edit). The held
            // write is flushed once the draft becomes clean (Save / Discard / manual
            // revert), so no config change is lost.
            let editor_dirty = self.module_draft.as_ref().map_or(false, |d| d.dirty());
            if config_autosave_held(editor_dirty) {
                self.pending_config_write = true;
            } else {
                self.persist();
            }
        }
        // Release the interlock if the draft became clean without a Save/Discard
        // click (e.g. the user hand-reverted the edit back to disk).
        self.flush_config_writes_if_clean();
    }

    /// Render the pending-confirmation modal, if any. Returns true if a
    /// destructive action was carried out.
    fn render_confirm_dialog(&mut self, ctx: &egui::Context) -> bool {
        let Some(action) = self.confirm.clone() else {
            return false;
        };
        let (title, msg) = match &action {
            ConfirmAction::DeleteGame(_) => (
                "Delete Game",
                format!("Delete \"{}\"? This cannot be undone.", self.game_config.game.name),
            ),
            ConfirmAction::DeleteProfile(name) => (
                "Delete Profile",
                format!("Delete profile \"{name}\"? This cannot be undone."),
            ),
            ConfirmAction::ClearSettings => (
                "Clear Settings",
                "Reset every override in the current scope to inherited? This cannot be undone."
                    .to_string(),
            ),
            ConfirmAction::ReExportResources => (
                "Re-Export Modules and Plugins",
                "Do you really want to re-export the bundled modules and plugins? \
                 Any local edits to them will be lost."
                    .to_string(),
            ),
            ConfirmAction::ConfigCleanup => (
                "Config Cleanup",
                "Remove stored values for variables that no longer exist in any \
                 module? This cleans every game, profile, and the global config."
                    .to_string(),
            ),
            ConfirmAction::DeleteModule { label, .. } => (
                "Delete Module",
                format!("Delete the module \"{label}\"? This removes its manifest file."),
            ),
            ConfirmAction::DiscardEdits => (
                "Discard Changes",
                "This module has unsaved changes. Close the editor and discard them?"
                    .to_string(),
            ),
        };

        // Modal backdrop: dim everything and swallow clicks meant for the panels
        // (which live on the Background layer) so only the dialog is interactive.
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("modal_backdrop"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::Pos2::ZERO)
            .interactable(true)
            .show(ctx, |ui| {
                ui.painter().rect_filled(screen, 0.0, egui::Color32::from_black_alpha(160));
                ui.allocate_response(screen.size(), egui::Sense::click_and_drag());
            });

        let mut confirmed = false;
        let mut cancelled = false;
        egui::Window::new(title)
            .order(egui::Order::Foreground)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(320.0);
                ui.add_space(4.0);
                ui.label(msg);
                if matches!(action, ConfirmAction::DeleteModule { .. }) {
                    ui.add_space(8.0);
                    styled_checkbox(
                        ui,
                        &mut self.delete_module_purge,
                        "Also purge this module's saved settings",
                    );
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.add(theme::danger_button("Confirm")).clicked() {
                        confirmed = true;
                    }
                    if ui.add(theme::secondary_button("Cancel")).clicked() {
                        cancelled = true;
                    }
                });
            });

        if confirmed {
            match action {
                ConfirmAction::DeleteGame(appid) => self.delete_game(&appid),
                ConfirmAction::DeleteProfile(name) => self.delete_profile(&name),
                ConfirmAction::ClearSettings => self.clear_current_settings(),
                ConfirmAction::ReExportResources => {
                    let _ = crate::resources::reexport(&self.paths);
                    self.reload_extensions();
                }
                ConfirmAction::ConfigCleanup => self.config_cleanup(),
                ConfirmAction::DeleteModule { manifest, .. } => {
                    self.delete_module(&manifest, self.delete_module_purge);
                    self.delete_module_purge = false;
                }
                ConfirmAction::DiscardEdits => self.discard_module(),
            }
            self.confirm = None;
            return true;
        }
        if cancelled {
            self.confirm = None;
        }
        false
    }

    /// Render the Fork / Create-new module dialog, if open. Live-validates the
    /// (Author, Name) pair for uniqueness (Version-blind, whole loaded set) and
    /// disables the confirm button while the pair collides or Name is empty.
    fn render_module_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.module_dialog.take() else {
            return;
        };

        let author = dlg.author.trim().to_string();
        let name = dlg.name.trim().to_string();
        let name_empty = name.is_empty();
        let author_empty = author.is_empty();
        // Uniqueness is checked over the full loaded set; neither Fork nor Create
        // is editing an existing entry, so nothing is excluded.
        let collides = !name_empty
            && name_collides(&self.all_specs, &self.all_manifests, &author, &name, None);
        let can_confirm = !name_empty && !author_empty && !collides;

        let (title, verb) = match &dlg.kind {
            DialogKind::Fork { .. } => ("Fork Module", "Fork"),
            DialogKind::Create => ("Create Module", "Create"),
        };

        // Modal backdrop (mirrors the confirm dialog).
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("module_dialog_backdrop"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::Pos2::ZERO)
            .interactable(true)
            .show(ctx, |ui| {
                ui.painter().rect_filled(screen, 0.0, egui::Color32::from_black_alpha(160));
                ui.allocate_response(screen.size(), egui::Sense::click_and_drag());
            });

        let mut confirmed = false;
        let mut cancelled = false;
        egui::Window::new(title)
            .order(egui::Order::Foreground)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(360.0);
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Author").color(theme::DIM).small());
                ui.add(egui::TextEdit::singleline(&mut dlg.author).desired_width(f32::INFINITY));
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Name").color(theme::DIM).small());
                ui.add(egui::TextEdit::singleline(&mut dlg.name).desired_width(f32::INFINITY));
                if matches!(dlg.kind, DialogKind::Fork { .. }) {
                    ui.add_space(8.0);
                    styled_checkbox(ui, &mut dlg.copy_settings, "Copy saved settings");
                }

                ui.add_space(8.0);
                let (fb, col) = if name_empty {
                    ("Name is required".to_string(), theme::COL_GLOBAL)
                } else if author_empty {
                    ("Author is required".to_string(), theme::COL_GLOBAL)
                } else if collides {
                    (format!("\u{f00d} {author} :: {name} already exists"), theme::COL_GLOBAL)
                } else {
                    (format!("\u{f00c} {author} :: {name} is available"), theme::COL_PROFILE)
                };
                ui.label(egui::RichText::new(fb).color(col).small());

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(can_confirm, theme::primary_button(verb)).clicked() {
                        confirmed = true;
                    }
                    if ui.add(theme::secondary_button("Cancel")).clicked() {
                        cancelled = true;
                    }
                });
            });

        if confirmed {
            match dlg.kind {
                DialogKind::Fork { parent_id } => {
                    self.perform_fork(&parent_id, author, name, dlg.copy_settings)
                }
                DialogKind::Create => self.perform_create(author, name),
            }
            return; // dialog consumed
        }
        if !cancelled {
            // Not dismissed — keep it open (with the edited buffers) next frame.
            self.module_dialog = Some(dlg);
        }
    }
}

/// Everything the editor's header row and status lines need, snapshotted **out
/// of** [`ModuleDraft`] once per frame.
///
/// *Why a snapshot struct and not a `&ModuleDraft`:* in IDE mode the header row
/// and the editor body render inside **different panel closures**, and the body
/// holds `&mut self.module_draft` for its whole scope. Passing owned, plain data
/// to the header means neither closure has to borrow the draft while the other
/// runs, which is what lets the header move to its own full-width panel at all.
/// Every field is cheap (a few `String`s + `bool`s); the two `Option<String>`
/// diagnostics are the only allocations that aren't already being made.
struct EditorHeaderInfo {
    name: String,
    version: String,
    author: String,
    /// `Extension::meta.description`, verbatim and un-truncated. Truncation is a
    /// render-time concern (see [`render_editor_header_description`]) — the full
    /// string has to survive to here so the hover tooltip can show it.
    description: Option<String>,
    editable: bool,
    dirty: bool,
    save_on: bool,
    has_identity: bool,
    identity_err: Option<String>,
    sections_unique: bool,
    validate_err: Option<String>,
    req_ok: bool,
}

/// The editor's header row: name / version / author on the left, the action
/// cluster on the right. Returns the [`TopAction`] the user clicked, if any —
/// the caller dispatches it (never this function, see
/// [`GuiApp::dispatch_top_action`]).
///
/// Free function taking `cache` directly rather than a `&mut self` method: the
/// IDE-mode call site renders this inside a panel closure that already holds
/// `&mut self`, and the only thing it needs from `self` is the icon cache.
///
/// **Invariant: this row must stay constant-height.** IDE mode renders it in a
/// fixed-height panel spanning both columns, so anything that changed height
/// here would reflow the editor *and* the preview column. The `editable`-gated
/// Delete / Rename buttons are safe: `editable` is fixed per module, and they
/// grow the row horizontally, never vertically.
fn render_editor_header_row(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    info: &EditorHeaderInfo,
) -> TopAction {
    let mut action = TopAction::None;
    ui.horizontal(|ui| {
        ui.heading(&info.name);
        ui.add_space(6.0);
        ui.label(egui::RichText::new(format!("v{}", info.version)).color(theme::FAINT));
        ui.label(egui::RichText::new(format!("by {}", info.author)).color(theme::FAINT));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Right-to-left layout: added first == drawn rightmost, so this
            // reads [Fork] [Delete] [Close] [Save] on screen.
            let mut save = ui.add_enabled(info.save_on, theme::primary_button("Save"));
            if !info.editable {
                save = save.on_hover_text("Fork to edit");
            }
            if save.clicked() {
                action = TopAction::Save;
            }
            // Always-present exit. Doubles as Discard: with unsaved edits it
            // asks for confirmation first, otherwise it just leaves.
            if icon_button(ui, cache, "\u{f00d}", "Close", IconBtn::Secondary, true)
                .on_hover_text("Leave the editor (discards unsaved edits)")
                .clicked()
            {
                action = TopAction::Close;
            }
            if info.editable
                && ui
                    .add(theme::danger_button("Delete"))
                    .on_hover_text("Delete this user module")
                    .clicked()
            {
                action = TopAction::Delete;
            }
            // Fork works on any module (bundled or user); Delete only on
            // user-authored (editable) modules.
            if ui
                .add(theme::secondary_button("Fork"))
                .on_hover_text("Create an editable copy of this module")
                .clicked()
            {
                action = TopAction::Fork;
            }
            // Rename / Apply identity changes: commits the staged Author /
            // Name / Variable edits via the scope sweep + manifest rewrite.
            // Enabled only for an editable module with a *valid* pending
            // identity change (never the normal Save/autosave path).
            if info.editable {
                let rename = ui
                    .add_enabled(
                        info.has_identity && info.identity_err.is_none(),
                        theme::secondary_button("Rename"),
                    )
                    .on_hover_text(
                        "Apply the staged Author / Name / Variable changes \u{2014} migrates saved settings across all scopes",
                    );
                if rename.clicked() {
                    action = TopAction::Rename;
                }
            }
        });
    });
    action
}

/// The header band's second row: the module's description, on exactly one line.
///
/// **Invariant: this occupies [`IDE_HEADER_DESC_H`] points, unconditionally.**
/// [`IDE_HEADER_H`] budgets for it, and the band is `exact_height`, so a slot
/// that grew would paint over the columns beneath it. Three things could have
/// made it vary, and each is closed off here:
///
/// 1. **A module with no description** (`meta.description` is `Option`) — the
///    rect is allocated *before* the text is even looked at, so the empty case
///    reserves the identical space and the band does not twitch as you walk the
///    module tree.
/// 2. **A long description wrapping to two lines** — the layout job is capped at
///    `max_rows = 1` with an ellipsis overflow character, so it elides instead of
///    wrapping. This is why the text is painted from a hand-built `LayoutJob`
///    rather than handed to `ui.label`: a `Label` sizes the `Ui` from its galley,
///    and the galley is exactly the thing that must not be allowed to.
/// 3. **A narrow window** — `max_width` follows the rect, so narrowing the window
///    moves the ellipsis leftward and changes nothing else. Wrapping is the only
///    way width could have become height, and (2) removed it.
///
/// The galley is *painted*, not allocated, and centred in the slot: if the 13pt
/// body row ever measures a shade over 16pt it bleeds into the frame's 8pt bottom
/// margin (invisible, and clipped by the panel) instead of pushing anything.
///
/// Elided text keeps the full string on hover, so nothing is actually lost. The
/// tooltip is attached only when the text *was* elided — a tooltip that merely
/// repeats what you can already read is noise.
///
/// Colour is [`theme::DIM`], matching how `GuiApp::render_module_detail_header`
/// renders the same field in Config mode.
fn render_editor_header_description(ui: &mut egui::Ui, desc: Option<&str>) {
    // Reserve the slot FIRST — before any early return — so every path through
    // this function costs the same height. See invariant (1) above.
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), IDE_HEADER_DESC_H),
        egui::Sense::hover(),
    );
    let Some(text) = desc.map(str::trim).filter(|t| !t.is_empty()) else {
        return;
    };
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.to_owned(),
        egui::TextStyle::Body.resolve(ui.style()),
        theme::DIM,
    );
    job.wrap.max_width = rect.width();
    job.wrap.max_rows = 1;
    job.wrap.overflow_character = Some('\u{2026}');
    let galley = ui.fonts(|f| f.layout_job(job));
    let elided = galley.elided;
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter()
        .galley(egui::pos2(rect.left(), y), galley, theme::DIM);
    if elided {
        resp.on_hover_text(text);
    }
}

/// The status lines under the header: dirty state, then any schema / `Requires`
/// / pending-identity diagnostics.
///
/// *Why these stay with the editor **body** and not with the header row* (which
/// IDE mode hoists into a full-width panel): only the first line is
/// unconditional — the validation, `Requires` and identity lines appear and
/// disappear **as you type**. In a full-width header they would resize the band
/// mid-keystroke and shove both the editor and the preview column down. Kept
/// here, any height change stays confined to the editor column, exactly as it
/// was before the header moved.
fn render_editor_status_lines(ui: &mut egui::Ui, info: &EditorHeaderInfo) {
    // Status line under the header. ALWAYS rendered (greyed when clean) so
    // the clean→dirty transition on the first keystroke never inserts a new
    // line above the fields — which would reflow the edit area. Combined
    // with the explicit widget IDs below, typing is never interrupted.
    if !info.editable {
        ui.label(
            egui::RichText::new("Bundled module \u{2014} read-only. Fork to edit (coming soon).")
                .color(theme::COL_GLOBAL)
                .small(),
        );
    } else if info.dirty {
        ui.label(
            egui::RichText::new(
                "Unsaved changes \u{2014} config autosave paused until you Save or Discard.",
            )
            .color(theme::COL_PROFILE)
            .small(),
        );
    } else {
        ui.label(
            egui::RichText::new("All changes saved.")
                .color(theme::FAINT)
                .small(),
        );
    }
    // Explain *why* Save is greyed for a schema problem. Duplicate section
    // names collapse when folded into the `IndexMap` before `validate` sees
    // them, so that case is caught by `sections_unique` separately.
    if info.editable && !info.sections_unique {
        ui.label(
            egui::RichText::new("Cannot save: two UI sections share a name \u{2014} rename one.")
                .color(theme::COL_GLOBAL)
                .small(),
        );
    } else if info.editable {
        if let Some(reason) = &info.validate_err {
            ui.label(
                egui::RichText::new(format!("Cannot save: {reason}"))
                    .color(theme::COL_GLOBAL)
                    .small(),
            );
        }
    }
    if info.editable && !info.req_ok {
        ui.label(
            egui::RichText::new("A Requires expression does not parse.")
                .color(theme::COL_GLOBAL)
                .small(),
        );
    }
    // Pending-identity feedback: why Rename is blocked, or a ready prompt.
    if info.editable && info.has_identity {
        match &info.identity_err {
            Some(reason) => {
                ui.label(
                    egui::RichText::new(format!("Cannot rename: {reason}"))
                        .color(theme::COL_GLOBAL)
                        .small(),
                );
            }
            None => {
                ui.label(
                    egui::RichText::new(
                        "Pending identity change \u{2014} press Rename to migrate saved settings and apply it.",
                    )
                    .color(theme::COL_PROFILE)
                    .small(),
                );
            }
        }
    }
}

impl GuiApp {
    /// Snapshot the open draft's header state. `&self` only, so it is safe to
    /// call **before any panel is declared** — which is what IDE mode does, to
    /// have the data ready for a header panel that renders ahead of the editor.
    ///
    /// `None` means "nothing to head": either no draft, or a draft for a
    /// different module (`ensure_draft` has not caught up yet this frame).
    fn editor_header_info(&self, id: &str) -> Option<EditorHeaderInfo> {
        let d = self.module_draft.as_ref().filter(|d| d.id == id)?;
        let snap = d.snapshot();
        Some(EditorHeaderInfo {
            name: d.ext.meta.name.clone(),
            version: d.ext.meta.version.clone(),
            author: d.ext.meta.author.clone(),
            description: d.ext.meta.description.clone(),
            editable: d.editable,
            dirty: d.dirty(),
            save_on: d.save_enabled(),
            has_identity: d.has_pending_identity(),
            identity_err: d.identity_error.clone(),
            sections_unique: d.sections_unique(),
            validate_err: core_extension::validate(&snap).err().map(|e| e.to_string()),
            req_ok: all_requires_parse(&snap),
        })
    }

    /// Carry out the action the header row produced.
    ///
    /// *Why this is its own method:* the header row renders in the editor's own
    /// `Ui` in Config mode but in a separate full-width panel in IDE mode, so
    /// the two paths reach the dispatch from different places. Both must run it
    /// at the same point in the frame — **after** the editor body and the
    /// preview column have rendered. Saving or closing mid-frame drops the
    /// draft, and everything downstream would then render a "no longer
    /// available" flash against state the header already invalidated.
    fn dispatch_top_action(&mut self, action: TopAction, id: &str, dirty: bool, editable: bool) {
        match action {
            TopAction::Save => self.save_module(),
            // Close doubles as Discard: warn before dropping unsaved edits.
            TopAction::Close => {
                if dirty && editable {
                    self.confirm = Some(ConfirmAction::DiscardEdits);
                } else {
                    self.exit_module_editor();
                }
            }
            TopAction::Fork => self.open_fork_dialog(id),
            TopAction::Rename => self.perform_rename(),
            TopAction::Delete => {
                if let Some(d) = &self.module_draft {
                    let label = format!("{}::{}", d.ext.meta.author, d.ext.meta.name);
                    let manifest = d.manifest.clone();
                    self.delete_module_purge = false;
                    self.confirm = Some(ConfirmAction::DeleteModule { manifest, label });
                }
            }
            TopAction::None => {}
        }
    }

    /// Phase 2: editable manifest editor. Edits an in-memory [`ModuleDraft`] with a
    /// live preview and an explicit Save. Bundled modules render the same widgets
    /// but disabled (Save shows a "Fork to edit" tooltip). `id` is the module's
    /// `Extension::id()`; the draft is (re)loaded by [`ensure_draft`] each frame.
    ///
    /// `header_inline` controls whether the name/version/author + action row is
    /// drawn here. Config mode passes `true` (the header belongs to this column).
    /// IDE mode passes `false`: it renders the same row via
    /// [`render_editor_header_row`] in a full-width `TopBottomPanel` spanning the
    /// editor *and* preview columns, and dispatches the action itself. The status
    /// lines stay here in both modes — see [`render_editor_status_lines`].
    fn render_module_editor(&mut self, ui: &mut egui::Ui, id: &str, header_inline: bool) {
        let touch = self.general_config.touch_mode;
        // IDE mode always runs full-width: its editor column is already sized by the
        // nav/preview split, so the 743px clamp (built for Config mode's wider,
        // unsplit central panel) would only add a dead gutter.
        let full_width = self.general_config.full_width || self.mode == Mode::Ide;
        let mut action = TopAction::None;

        // The editor body holds a `&mut` borrow of `self.module_draft` for its
        // whole scope, so a second `&mut self.icon_cache` can't be taken
        // alongside it. Move the (map-only, cheap) cache out for the duration and
        // put it back afterwards; the measurements survive across frames.
        let mut cache = std::mem::take(&mut self.icon_cache);

        // Transient dropped-var report from the last fork snapshot, dismissible.
        if let Some(msg) = self.carryover_report.clone() {
            let mut dismiss = false;
            editor_card(ui, &mut cache, theme::EDIT_L1, "Carry-over report", None, |ui, _cache| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("\u{f071} {msg}"))
                            .color(theme::COL_GLOBAL)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(theme::secondary_button("Dismiss")).clicked() {
                            dismiss = true;
                        }
                    });
                });
            });
            if dismiss {
                self.carryover_report = None;
            }
        }
        // Header state is snapshotted out of the draft BEFORE the mutable borrow
        // below, so the header row (which in IDE mode renders in a different panel
        // entirely) never has to hold the draft.
        let Some(info) = self.editor_header_info(id) else {
            // Two distinct "nothing to draw" cases, kept apart because they read
            // very differently to the user: the draft is gone for good, or
            // `ensure_draft` simply hasn't caught up with a fresh selection yet.
            let msg = if self.module_draft.is_none() {
                "This module is no longer available."
            } else {
                "Loading\u{2026}"
            };
            ui.label(egui::RichText::new(msg).color(theme::DIM));
            self.icon_cache = cache;
            return;
        };
        let (dirty, editable) = (info.dirty, info.editable);

        ui.add_space(6.0);
        if header_inline {
            action = render_editor_header_row(ui, &mut cache, &info);
        }
        render_editor_status_lines(ui, &info);
        ui.add_space(10.0);
        ui.separator();

        {
            // Re-borrow for the body only. `editor_header_info` above already
            // established that a draft for `id` exists, so this cannot fail — but
            // handle it rather than unwrap, since a panic here would take the app
            // down mid-frame.
            let Some(draft) = self.module_draft.as_mut() else {
                self.icon_cache = cache;
                return;
            };

            let mut deferred: Vec<Deferred> = Vec::new();
            // Explicit, stable scroll id: keeps the body's content Ui id constant
            // regardless of how many status lines render above it, so the focused
            // TextEdit's id never shifts when the dirty/identity banners appear.
            egui::ScrollArea::vertical()
                .id_salt("module_editor_body")
                .auto_shrink([false, false])
                .drag_to_scroll(touch)
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        let max_w = body_max_width(ui, full_width);
                        ui.set_max_width(max_w);
                        ui.add_enabled_ui(editable, |ui| {
                            render_editor_body(ui, &mut cache, draft, &mut deferred);
                        });
                    });
                });
            // Apply structural edits AFTER the render loop (never mid-iteration).
            for d in deferred {
                apply_deferred(draft, d);
            }
        }
        self.icon_cache = cache;
        // Only the inline (Config-mode) header produces an action here; IDE mode's
        // header panel dispatches its own, after the preview column has rendered.
        if header_inline {
            self.dispatch_top_action(action, id, dirty, editable);
        }
    }
}

/// Width to clamp a central-panel body to: fills the pane less an 18px
/// scrollbar gutter, then caps at ~743px unless "Use full UI width" is on.
/// `min(..)` lets it scale DOWN on narrow windows instead of overflowing.
/// Shared by the module list, the manifest editor, and General Settings —
/// was copy-pasted three times before this extraction.
///
/// Free function, not a `&self` method: `render_module_editor` computes this
/// while `self.module_draft` is borrowed mutably (as `draft`) for the whole
/// surrounding block, so a `&self` call there would conflict with that
/// borrow. Taking `full_width` as a plain `bool` sidesteps it everywhere,
/// not just at that one call site.
///
/// *Why this exists as its own helper:* IDE mode (see
/// `docs/brainstorm/ide-mode.md`, S3) will need the clamp to NOT bind in its
/// wide preview layout — one helper means one place to add that condition
/// later instead of three. `Mode` doesn't exist yet, so that condition is
/// deliberately not added here.
fn body_max_width(ui: &egui::Ui, full_width: bool) -> f32 {
    let avail = (ui.available_width() - 18.0).max(300.0);
    if full_width { avail } else { avail.min(743.0) }
}

// ── Editor column geometry ────────────────────────────────────────────────
// The editor reuses the fixed-column idiom the normal module field rows already
// use (`AppState::render_field`: reserve a constant label column by padding the
// cursor out to a fixed x, then let the control take the remaining width), so
// every row's control starts and ends at the same x. `render_field` reserves
// 260px for its checkbox+label; editor labels are single words, so the editor
// applies the same mechanism with its own narrower constant.

/// Fixed label-column width for an editor row — `render_field`'s 260px reserve
/// at editor scale.
const EDITOR_LABEL_W: f32 = 96.0;
/// Side of the square a glyph-only [`icon_button`] allocates: `interact_size.y`
/// (== `font.size + button_padding.y * 2` at the editor's theme values —
/// `13.0 + 5.0*2 == 23.0`, set in `theme.rs`). Kept as its own constant, rather
/// than re-derived at each call site, so [`ACTION_COL_W`] can reference the same
/// number `icon_button` actually draws at. If `theme.rs` ever changes
/// `interact_size.y`, `button_padding.y`, or the Button font size, update this
/// literal to match (`icon_button_width` computes the live value from
/// `ui.spacing()`, so a mismatch here only affects the const-derived
/// [`ACTION_COL_W`], not the buttons themselves).
const ICON_BTN_SIDE: f32 = 23.0;
/// Fixed width reserved for a row's `[↑][↓][🗑]` cluster. Constant so every
/// cluster in a card pins to the same right edge whatever precedes it. Must
/// cover what the cluster actually draws, or a control sized with this as its
/// `reserve` slides under the leftmost button: three square, glyph-only
/// `icon_button`s ([`ICON_BTN_SIDE`] each) plus two `item_spacing.x` gaps
/// (7.0, `theme.rs`) = `3 * 23.0 + 2 * 7.0` = 83.
const ACTION_COL_W: f32 = 3.0 * ICON_BTN_SIDE + 2.0 * 7.0;
/// Floor for a row control, so a narrow window shrinks but never collapses it.
const MIN_CONTROL_W: f32 = 80.0;
/// Side of the square cell an [`icon_button`] reserves for its glyph.
const ICON_CELL: f32 = 18.0;

/// Width for an editor row's control: what remains of the row after the fixed
/// label column, minus any right-pinned `reserve` (e.g. an action cluster),
/// floored at [`MIN_CONTROL_W`]. Pure — unit-tested.
fn editor_control_width(remaining: f32, reserve: f32) -> f32 {
    (remaining - reserve).max(MIN_CONTROL_W)
}

/// Start an editor row: draw `label`, pad out to the fixed label column, and
/// return the width the row's control should use so it ends flush at the card's
/// right edge (less `reserve`). Every editor row goes through this, which is
/// what makes the textboxes share one left and one right edge.
fn editor_row_label(ui: &mut egui::Ui, label: &str, reserve: f32) -> f32 {
    let left = ui.cursor().min.x;
    field_label(ui, label);
    let used = ui.cursor().min.x - left;
    ui.add_space((EDITOR_LABEL_W - used).max(0.0));
    editor_control_width(ui.available_width(), reserve)
}

/// Which theme role an [`icon_button`] draws its fill / stroke / text from.
#[derive(Clone, Copy, PartialEq)]
enum IconBtn {
    Secondary,
    Danger,
}

/// The single entry point for every editor icon affordance (↑ ↓ 🗑 ✕ ✎ ＋).
///
/// Reserves a fixed square cell for the glyph and paints it through the
/// [`IconCenterCache`] so its *ink* — not its layout box, whose height is the
/// same for every glyph and whose width includes side bearings — sits centered
/// in the cell. A row of different glyphs then reads as one aligned column.
/// `label` may be empty for a glyph-only button.
///
/// Anchors `Align2::LEFT_TOP`: the cache already returns a corrected top-left,
/// so anchoring `CENTER_CENTER` here would double-correct.
fn icon_button(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    icon: &str,
    label: &str,
    style: IconBtn,
    enabled: bool,
) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let font = egui::TextStyle::Button.resolve(ui.style());
        let pad = ui.spacing().button_padding;
        let gap = if label.is_empty() { 0.0 } else { 6.0 };
        // Laid out with PLACEHOLDER so `Painter::galley` can substitute the
        // state-dependent color below without a second layout pass.
        let text = (!label.is_empty()).then(|| {
            ui.painter()
                .layout_no_wrap(label.to_owned(), font.clone(), Color32::PLACEHOLDER)
        });
        let text_w = text.as_ref().map_or(0.0, |g| g.size().x);
        // Height must match plain `egui::Button`'s own formula
        // (`interact_size.y.max(galley.size().y + pad.y*2)`, see
        // `egui::widgets::Button::ui`), or a labelled `icon_button` (e.g. "Edit")
        // ends up a different height than a same-row `theme::secondary_button`
        // (e.g. "Fork") — `font.size` is the font's nominal point size, not the
        // laid-out glyph height a real button measures, so using it here for
        // labelled buttons under-measured and let interact_size.y win when the
        // sibling button's actual galley was taller. Icon-only buttons keep the
        // old `font.size` fallback (`text` is `None`), so `ICON_BTN_SIDE`/
        // `ACTION_COL_W` are untouched.
        let text_h = text.as_ref().map_or(font.size, |g| g.size().y);
        let h = ui.spacing().interact_size.y.max(text_h + pad.y * 2.0);
        // Icon-only buttons drop the horizontal padding and allocate a square
        // cell (width == height) instead of `pad.x * 2 + ICON_CELL`, which used
        // to leave them wider than tall. Labelled buttons are unchanged.
        let w = if label.is_empty() { h } else { pad.x * 2.0 + ICON_CELL + gap + text_w };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
        if ui.is_rect_visible(rect) {
            let vis = ui.style().interact(&resp).clone();
            let (fill, stroke, fg) = match style {
                IconBtn::Secondary => (vis.weak_bg_fill, vis.bg_stroke, theme::TEXT),
                IconBtn::Danger => (
                    if resp.hovered() { theme::HOV } else { Color32::TRANSPARENT },
                    egui::Stroke::new(
                        1.0,
                        Color32::from_rgba_unmultiplied(
                            theme::COL_GLOBAL.r(),
                            theme::COL_GLOBAL.g(),
                            theme::COL_GLOBAL.b(),
                            82,
                        ),
                    ),
                    theme::COL_GLOBAL,
                ),
            };
            let fg = if enabled { fg } else { theme::FAINT };
            ui.painter().rect(rect, egui::Rounding::same(8.0), fill, stroke);
            let cell_x = if label.is_empty() { (rect.width() - ICON_CELL) / 2.0 } else { pad.x };
            let cell = egui::Rect::from_min_size(
                egui::pos2(rect.min.x + cell_x, rect.min.y),
                egui::vec2(ICON_CELL, rect.height()),
            );
            let pos = cache.centered_pos(ui, icon, &font, cell.center());
            ui.painter().text(pos, egui::Align2::LEFT_TOP, icon, font, fg);
            if let Some(g) = text {
                let ty = rect.center().y - g.size().y / 2.0;
                ui.painter().galley(egui::pos2(cell.max.x + gap, ty), g, fg);
            }
        }
        resp
    })
    .inner
}

/// A card matching the rounding used elsewhere in the app, holding the editable
/// widgets of one field / block entry.
///
/// The `title` is the FIRST widget inside the frame's inner margin, styled with
/// `theme::section_label` — the border stays a plain continuous 1px stroke, this
/// is deliberately NOT a fieldset/legend notch. An empty `title` skips the
/// header line. `fill` selects the nesting shade (the `theme::EDIT_L*` ramp) so
/// deeper levels read as a visible hierarchy — ritz nests module → block →
/// section/field → builder row, so the shades carry information the border
/// alone can't.
///
/// `actions` (index, length) adds the `[↑][↓][🗑]` cluster to the header row,
/// right-pinned at a fixed width, and returns what the user clicked. `add`
/// receives the icon cache back so nested cards and buttons can use it (one
/// `&mut` borrow, handed down in sequence).
fn editor_card(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    fill: egui::Color32,
    title: &str,
    actions: Option<(usize, usize)>,
    add: impl FnOnce(&mut egui::Ui, &mut IconCenterCache),
) -> RowAction {
    let mut act = RowAction::None;
    egui::Frame::none()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !title.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(theme::section_label(title));
                    if let Some((idx, len)) = actions {
                        act = row_actions(ui, cache, idx, len);
                    }
                });
                ui.add_space(4.0);
            }
            add(ui, cache);
        });
    ui.add_space(6.0);
    act
}

/// Width a glyph-only [`icon_button`] occupies: it's square, so this is the
/// same cell height the button allocates. Callers that reserve room for the
/// button before laying out the flexible part of a row need this up front.
fn icon_button_width(ui: &egui::Ui) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let pad = ui.spacing().button_padding;
    ui.spacing().interact_size.y.max(font.size + pad.y * 2.0)
}

/// A short leading label placed before an editor textbox so the user knows what
/// the box is for.
fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(theme::DIM).small());
}

/// Gray placeholder/hint text for an editor textbox (entered text stays the
/// normal foreground; only the hint is dimmed).
fn gray_hint(text: &str) -> egui::RichText {
    egui::RichText::new(text).color(theme::FAINT)
}

/// A `+`-prefixed secondary button used for "add" affordances.
fn add_button(ui: &mut egui::Ui, cache: &mut IconCenterCache, label: &str) -> egui::Response {
    icon_button(ui, cache, "\u{f067}", label, IconBtn::Secondary, true)
}

/// The reusable `[↑][↓][🗑]` row-action widget. Returns the action the user
/// clicked; the caller records a path-addressed [`Deferred`] and applies the
/// container-correct op after the render loop.
///
/// The cluster pins itself to the row's right edge and then allocates exactly
/// [`ACTION_COL_W`], so section rows, field rows and builder rows all put their
/// buttons at the same x and share one vertical center — instead of each row
/// ending wherever its own content happened to run out.
fn row_actions(ui: &mut egui::Ui, cache: &mut IconCenterCache, idx: usize, len: usize) -> RowAction {
    let mut a = RowAction::None;
    ui.add_space((ui.available_width() - ACTION_COL_W).max(0.0));
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(
        egui::vec2(ACTION_COL_W, h),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            if icon_button(ui, cache, "\u{f1f8}", "", IconBtn::Danger, true)
                .on_hover_text("Remove")
                .clicked()
            {
                a = RowAction::Remove;
            }
            if icon_button(ui, cache, "\u{f063}", "", IconBtn::Secondary, idx + 1 < len)
                .on_hover_text("Move down")
                .clicked()
            {
                a = RowAction::Down;
            }
            if icon_button(ui, cache, "\u{f062}", "", IconBtn::Secondary, idx > 0)
                .on_hover_text("Move up")
                .clicked()
            {
                a = RowAction::Up;
            }
        },
    );
    a
}

/// Edit an `Option<String>` as a single line (empty text → `None`) with a leading
/// `label` and a gray `hint` placeholder. `id_salt` gives the box a stable egui id
/// so it never loses focus when banners above it appear/disappear.
fn opt_text_edit(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    label: &str,
    val: &mut Option<String>,
    hint: &str,
) {
    let mut s = val.clone().unwrap_or_default();
    ui.horizontal(|ui| {
        let w = editor_row_label(ui, label, 0.0);
        ui.add(
            egui::TextEdit::singleline(&mut s)
                .id_salt(id_salt)
                .hint_text(gray_hint(hint))
                .desired_width(w),
        );
    });
    *val = if s.is_empty() { None } else { Some(s) };
}

/// A single-line `Requires` editor with a leading label, live-validated via
/// `condition::parse`. Entered text is the normal foreground and turns red only on
/// a parse error; the hint stays gray. The wordy error is shown once the field is
/// no longer being edited. `id_salt` keeps focus stable across banner reflows.
fn requires_edit(ui: &mut egui::Ui, id_salt: impl std::hash::Hash, val: &mut Option<String>) {
    let mut s = val.clone().unwrap_or_default();
    let parsed = condition::parse(&s);
    let ok = s.trim().is_empty() || parsed.is_ok();
    let color = if ok { theme::TEXT } else { theme::COL_GLOBAL };
    let resp = ui
        .horizontal(|ui| {
            let w = editor_row_label(ui, "Requires", 0.0);
            ui.add(
                egui::TextEdit::singleline(&mut s)
                    .id_salt(id_salt)
                    .hint_text(gray_hint("e.g. enabled AND !clear"))
                    .text_color(color)
                    .desired_width(w),
            )
        })
        .inner;
    *val = if s.trim().is_empty() { None } else { Some(s) };
    if !ok && (!resp.has_focus() || resp.lost_focus()) {
        if let Err(e) = parsed {
            ui.label(egui::RichText::new(e.to_string()).color(theme::COL_GLOBAL).small());
        }
    }
}

/// Split a stored custom-env row on its FIRST `=` into (name, value); a row with
/// no `=` becomes (whole string, empty value). Pure — the inverse of
/// `format!("{name}={value}")`.
fn parse_env_row(entry: &str) -> (String, String) {
    match entry.split_once('=') {
        Some((n, v)) => (n.to_string(), v.to_string()),
        None => (entry.to_string(), String::new()),
    }
}

/// POSIX env-var name charset: `^[A-Za-z_][A-Za-z0-9_]*$`.
fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Title text for a list-entry card: the entry's own identifying string, or a
/// numbered `kind` placeholder while it's still blank (a card must never show an
/// empty header — the title is what anchors the row-action cluster). Pure.
fn entry_title(value: &str, kind: &str, idx: usize) -> String {
    let v = value.trim();
    if v.is_empty() {
        format!("{kind} {}", idx + 1)
    } else {
        v.to_string()
    }
}

fn field_type_label(t: FieldType) -> &'static str {
    match t {
        FieldType::Toggle => "toggle",
        FieldType::Selection => "selection",
        FieldType::Integer => "integer",
        FieldType::Float => "float",
        FieldType::String => "string",
        FieldType::MultiString => "multi_string",
    }
}

fn env_op_label(op: EnvOp) -> &'static str {
    match op {
        EnvOp::Set => "set",
        EnvOp::Append => "append",
        EnvOp::Unset => "unset",
    }
}

/// Ensure a field's `Options` is a `List`, returning it for editing. Called only
/// for Selection fields (a fresh Selection with no list starts empty).
fn ensure_list(options: &mut Option<OptionsSpec>) -> &mut Vec<String> {
    if !matches!(options, Some(OptionsSpec::List(_))) {
        *options = Some(OptionsSpec::List(Vec::new()));
    }
    match options {
        Some(OptionsSpec::List(v)) => v,
        _ => unreachable!(),
    }
}

fn new_field(variable: String) -> UiField {
    UiField {
        name: None,
        description: None,
        field_type: FieldType::Toggle,
        variable,
        default: None,
        options: None,
        display_options: None,
        requires: None,
    }
}

/// The whole editable body: meta, UI sections (with fields), and the four output
/// blocks. Structural edits are pushed to `deferred`; scalar edits mutate in place.
fn render_editor_body(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    draft: &mut ModuleDraft,
    deferred: &mut Vec<Deferred>,
) {
    let ModuleDraft {
        ext,
        sections,
        baseline_vars,
        identity,
        ..
    } = draft;

    // ── Meta ────────────────────────────────────────────────────────────────
    ui.add_space(6.0);
    editor_card(ui, cache, theme::EDIT_L0, "Module", None, |ui, _cache| {
        // Author + Name are STAGED identity edits: they mutate `identity`, never
        // `ext.meta`, so they don't touch the draft snapshot / Save gate and are
        // committed only through the Rename button.
        ui.horizontal(|ui| {
            let w = editor_row_label(ui, "Author", 0.0);
            ui.add(
                egui::TextEdit::singleline(&mut identity.author)
                    .id_salt("meta_author")
                    .desired_width(w),
            );
        });
        ui.horizontal(|ui| {
            let w = editor_row_label(ui, "Name", 0.0);
            ui.add(
                egui::TextEdit::singleline(&mut identity.name)
                    .id_salt("meta_name")
                    .desired_width(w),
            );
        });
        ui.label(
            egui::RichText::new(
                "Author & Name are applied via Rename (migrates saved settings). Version is fixed.",
            )
            .color(theme::FAINT)
            .small(),
        );
        ui.add_space(6.0);
        opt_text_edit(ui, "meta_desc", "Description", &mut ext.meta.description, "What this module does");
        ui.add_space(4.0);
        opt_text_edit(ui, "meta_backend", "Backend", &mut ext.backend, "advanced, optional");
    });

    // ── UI sections ─────────────────────────────────────────────────────────
    // The block header lives INSIDE the block's own card (it used to sit above
    // the boxes), so every title is enclosed by the border it names.
    ui.add_space(12.0);
    editor_card(ui, cache, theme::EDIT_L0, "UI Sections", None, |ui, cache| {
        let sec_len = sections.len();
        for (si, (name, fields)) in sections.iter_mut().enumerate() {
            let a = editor_card(
                ui,
                cache,
                theme::EDIT_L1,
                &entry_title(name, "Section", si),
                Some((si, sec_len)),
                |ui, cache| {
                    ui.horizontal(|ui| {
                        let w = editor_row_label(ui, "Name", 0.0);
                        ui.add(
                            egui::TextEdit::singleline(name)
                                .id_salt(("sec_name", si))
                                .hint_text(gray_hint("Section name"))
                                .desired_width(w),
                        );
                    });
                    ui.add_space(4.0);
                    let f_len = fields.len();
                    for (fi, field) in fields.iter_mut().enumerate() {
                        render_field_editor(
                            ui,
                            cache,
                            field,
                            si,
                            fi,
                            f_len,
                            baseline_vars,
                            &mut identity.var_edits,
                            deferred,
                        );
                    }
                    if add_button(ui, cache, "Add field").clicked() {
                        deferred.push(Deferred::FieldAdd(si));
                    }
                },
            );
            if a != RowAction::None {
                deferred.push(Deferred::Section(si, a));
            }
        }
        if add_button(ui, cache, "Add section").clicked() {
            deferred.push(Deferred::SectionAdd);
        }
    });

    // ── Output blocks ─────────────────────────────────────────────────────────
    render_env_block_editor(ui, cache, "ENV_VARS", false, &mut ext.env_vars, deferred);
    render_env_block_editor(ui, cache, "GAME_ENV_VARS", true, &mut ext.game_env_vars, deferred);
    render_wrapper_block_editor(ui, cache, &mut ext.wrappers, deferred);
    render_arg_block_editor(ui, cache, &mut ext.game_launch_args, deferred);
}

/// One editable UI-field card. An *existing* field's `Variable` (present on disk)
/// is renamed as a **staged identity edit** — the buffer lives in
/// `var_edits[current-name]`, so `field.variable` itself is untouched (no draft
/// dirtiness) until the Rename action migrates config and rewrites the manifest.
/// A newly added field's `Variable` is edited directly (no config to migrate).
fn render_field_editor(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    field: &mut UiField,
    si: usize,
    fi: usize,
    f_len: usize,
    baseline_vars: &std::collections::HashSet<String>,
    var_edits: &mut IndexMap<String, String>,
    deferred: &mut Vec<Deferred>,
) {
    let title = entry_title(
        field.name.as_deref().unwrap_or(&field.variable),
        "Field",
        fi,
    );
    let a = editor_card(ui, cache, theme::EDIT_L2, &title, Some((fi, f_len)), |ui, cache| {
        ui.horizontal(|ui| {
            let w = editor_row_label(ui, "Name", 0.0);
            let mut name = field.name.clone().unwrap_or_default();
            ui.add(
                egui::TextEdit::singleline(&mut name)
                    .id_salt(("field_name", si, fi))
                    .hint_text(gray_hint("Field label"))
                    .desired_width(w),
            );
            field.name = if name.is_empty() { None } else { Some(name) };
        });

        // Variable — for an existing (on-disk) field this edits the STAGED rename
        // buffer (committed via Rename); a newly added field edits it directly.
        let mut pending_rename = false;
        ui.horizontal(|ui| {
            let w = editor_row_label(ui, "Variable", 0.0);
            if baseline_vars.contains(&field.variable) {
                let key = field.variable.clone();
                let buf = var_edits.entry(key.clone()).or_insert_with(|| key.clone());
                ui.add(
                    egui::TextEdit::singleline(buf)
                        .id_salt(("field_var", si, fi))
                        .desired_width(w),
                );
                pending_rename = buf.trim() != key;
            } else {
                ui.add(
                    egui::TextEdit::singleline(&mut field.variable)
                        .id_salt(("field_var", si, fi))
                        .hint_text(gray_hint("variable_name"))
                        .desired_width(w),
                );
            }
        });
        // Rendered on its own line so the note can never shorten the Variable
        // box and break the shared control column.
        if pending_rename {
            ui.horizontal(|ui| {
                editor_row_label(ui, "", 0.0);
                ui.label(
                    egui::RichText::new("(rename pending)")
                        .color(theme::COL_PROFILE)
                        .small(),
                );
            });
        }

        // Type.
        ui.horizontal(|ui| {
            let w = editor_row_label(ui, "Type", 0.0);
            egui::ComboBox::from_id_salt((si, fi, "type"))
                .width(w)
                .selected_text(field_type_label(field.field_type))
                .show_ui(ui, |ui| {
                    for t in [
                        FieldType::Toggle,
                        FieldType::Selection,
                        FieldType::Integer,
                        FieldType::Float,
                        FieldType::String,
                        FieldType::MultiString,
                    ] {
                        ui.selectable_value(&mut field.field_type, t, field_type_label(t));
                    }
                });
        });

        opt_text_edit(ui, ("field_desc", si, fi), "Description", &mut field.description, "Shown under the field");
        requires_edit(ui, ("field_req", si, fi), &mut field.requires);

        // Type-specific detail.
        match field.field_type {
            FieldType::Selection => {
                let opts = ensure_list(&mut field.options);
                let ol = opts.len();
                for (oi, opt) in opts.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        // Each option row keeps the same label column and the
                        // same right-pinned action cluster as every other row.
                        let w = editor_row_label(ui, "Option", ACTION_COL_W);
                        ui.add(
                            egui::TextEdit::singleline(opt)
                                .id_salt(("field_opt", si, fi, oi))
                                .hint_text(gray_hint("option"))
                                .desired_width(w),
                        );
                        let a = row_actions(ui, cache, oi, ol);
                        if a != RowAction::None {
                            deferred.push(Deferred::FieldOpt(si, fi, oi, a));
                        }
                    });
                }
                if add_button(ui, cache, "Add option").clicked() {
                    deferred.push(Deferred::FieldOptAdd(si, fi));
                }
            }
            FieldType::Integer | FieldType::Float => {
                let (mut min, mut max, mut step) = number_range(field);
                let mut ch = false;
                ui.horizontal(|ui| {
                    editor_row_label(ui, "Range", 0.0);
                    field_label(ui, "Min");
                    ch |= ui.add(egui::DragValue::new(&mut min)).changed();
                    field_label(ui, "Max");
                    ch |= ui.add(egui::DragValue::new(&mut max)).changed();
                    field_label(ui, "Step");
                    ch |= ui.add(egui::DragValue::new(&mut step)).changed();
                });
                if ch {
                    field.options = Some(OptionsSpec::Range { min, max, step: Some(step) });
                }
            }
            FieldType::Toggle => {
                ui.horizontal(|ui| {
                    editor_row_label(ui, "Default", 0.0);
                    let mut def =
                        field.default.as_ref().and_then(|v| v.as_bool()).unwrap_or(false);
                    if ui.checkbox(&mut def, "On").changed() {
                        field.default = Some(json!(def));
                    }
                });
            }
            FieldType::String | FieldType::MultiString => {}
        }
    });
    if a != RowAction::None {
        deferred.push(Deferred::Field(si, fi, a));
    }
}

/// ENV_VARS / GAME_ENV_VARS editor. `game` selects which block (for deferral).
fn render_env_block_editor(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    title: &str,
    game: bool,
    specs: &mut [EnvVarSpec],
    deferred: &mut Vec<Deferred>,
) {
    ui.add_space(12.0);
    editor_card(ui, cache, theme::EDIT_L0, title, None, |ui, cache| {
        let len = specs.len();
        for (ei, spec) in specs.iter_mut().enumerate() {
            let a = editor_card(
                ui,
                cache,
                theme::EDIT_L1,
                &entry_title(&spec.name, "Variable", ei),
                Some((ei, len)),
                |ui, cache| {
                    ui.horizontal(|ui| {
                        let w = editor_row_label(ui, "Name", 0.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut spec.name)
                                .id_salt(("env_name", game, ei))
                                .hint_text(gray_hint("VAR_NAME"))
                                .desired_width(w),
                        );
                    });
                    requires_edit(ui, ("env_req", game, ei), &mut spec.requires);
                    let sl = spec.builder.len();
                    for (bi, step) in spec.builder.iter_mut().enumerate() {
                        let sa = editor_card(
                            ui,
                            cache,
                            theme::EDIT_L3,
                            &format!("Step {}", bi + 1),
                            Some((bi, sl)),
                            |ui, _cache| {
                                ui.horizontal(|ui| {
                                    let w = editor_row_label(ui, "Op", 0.0);
                                    egui::ComboBox::from_id_salt((game, ei, bi, "op"))
                                        .selected_text(env_op_label(step.op))
                                        .width(w)
                                        .show_ui(ui, |ui| {
                                            for op in [EnvOp::Set, EnvOp::Append, EnvOp::Unset] {
                                                ui.selectable_value(
                                                    &mut step.op,
                                                    op,
                                                    env_op_label(op),
                                                );
                                            }
                                        });
                                });
                                ui.horizontal(|ui| {
                                    let w = editor_row_label(ui, "Value", 0.0);
                                    let mut v = step.value.clone().unwrap_or_default();
                                    ui.add(
                                        egui::TextEdit::singleline(&mut v)
                                            .id_salt(("env_step_val", game, ei, bi))
                                            .hint_text(gray_hint("value"))
                                            .desired_width(w),
                                    );
                                    step.value = if v.is_empty() { None } else { Some(v) };
                                });
                                // The separator only means anything for `append`
                                // (set/unset replace or clear), so it only shows
                                // there — one less always-empty row per step.
                                if step.op == EnvOp::Append {
                                    ui.horizontal(|ui| {
                                        let w = editor_row_label(ui, "Separator", 0.0);
                                        let mut sep = step.separator.clone().unwrap_or_default();
                                        ui.add(
                                            egui::TextEdit::singleline(&mut sep)
                                                .id_salt(("env_step_sep", game, ei, bi))
                                                .hint_text(gray_hint("e.g. :"))
                                                .desired_width(w),
                                        );
                                        step.separator =
                                            if sep.is_empty() { None } else { Some(sep) };
                                    });
                                }
                                requires_edit(
                                    ui,
                                    ("env_step_req", game, ei, bi),
                                    &mut step.requires,
                                );
                            },
                        );
                        if sa != RowAction::None {
                            deferred.push(Deferred::EnvStep(game, ei, bi, sa));
                        }
                    }
                    if add_button(ui, cache, "Add step").clicked() {
                        deferred.push(Deferred::EnvStepAdd(game, ei));
                    }
                },
            );
            if a != RowAction::None {
                deferred.push(Deferred::Env(game, ei, a));
            }
        }
        if add_button(ui, cache, "Add variable").clicked() {
            deferred.push(Deferred::EnvAdd(game));
        }
    });
}

/// WRAPPERS editor: CommandSyntax, an editable Priority (lower = outermost), and
/// per-step option values.
fn render_wrapper_block_editor(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    wrappers: &mut [WrapperSpec],
    deferred: &mut Vec<Deferred>,
) {
    ui.add_space(12.0);
    editor_card(ui, cache, theme::EDIT_L0, "WRAPPERS", None, |ui, cache| {
        let len = wrappers.len();
        for (wi, w) in wrappers.iter_mut().enumerate() {
            let a = editor_card(
                ui,
                cache,
                theme::EDIT_L1,
                &entry_title(&w.command_syntax, "Wrapper", wi),
                Some((wi, len)),
                |ui, cache| {
                    ui.horizontal(|ui| {
                        let cw = editor_row_label(ui, "Command", 0.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut w.command_syntax)
                                .id_salt(("wrap_syntax", wi))
                                .hint_text(gray_hint("gamescope {OPTIONS} --"))
                                .desired_width(cw),
                        );
                    });
                    ui.horizontal(|ui| {
                        editor_row_label(ui, "Priority", 0.0);
                        ui.add(egui::DragValue::new(&mut w.priority));
                        ui.label(
                            egui::RichText::new("lower = outermost").color(theme::FAINT).small(),
                        );
                    });
                    requires_edit(ui, ("wrap_req", wi), &mut w.requires);
                    let sl = w.builder.len();
                    for (bi, step) in w.builder.iter_mut().enumerate() {
                        let sa = editor_card(
                            ui,
                            cache,
                            theme::EDIT_L3,
                            &format!("Option {}", bi + 1),
                            Some((bi, sl)),
                            |ui, _cache| {
                                ui.horizontal(|ui| {
                                    let vw = editor_row_label(ui, "Value", 0.0);
                                    ui.add(
                                        egui::TextEdit::singleline(&mut step.value)
                                            .id_salt(("wrap_step_val", wi, bi))
                                            .hint_text(gray_hint("option"))
                                            .desired_width(vw),
                                    );
                                });
                                requires_edit(ui, ("wrap_step_req", wi, bi), &mut step.requires);
                            },
                        );
                        if sa != RowAction::None {
                            deferred.push(Deferred::WrapperStep(wi, bi, sa));
                        }
                    }
                    if add_button(ui, cache, "Add option").clicked() {
                        deferred.push(Deferred::WrapperStepAdd(wi));
                    }
                },
            );
            if a != RowAction::None {
                deferred.push(Deferred::Wrapper(wi, a));
            }
        }
        if add_button(ui, cache, "Add wrapper").clicked() {
            deferred.push(Deferred::WrapperAdd);
        }
    });
}

/// GAME_LAUNCH_ARGS editor: one Value + Requires per arg.
fn render_arg_block_editor(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    args: &mut [ArgSpec],
    deferred: &mut Vec<Deferred>,
) {
    ui.add_space(12.0);
    editor_card(ui, cache, theme::EDIT_L0, "GAME_LAUNCH_ARGS", None, |ui, cache| {
        let len = args.len();
        for (ai, arg) in args.iter_mut().enumerate() {
            let a = editor_card(
                ui,
                cache,
                theme::EDIT_L1,
                &entry_title(&arg.value, "Argument", ai),
                Some((ai, len)),
                |ui, _cache| {
                    ui.horizontal(|ui| {
                        let w = editor_row_label(ui, "Value", 0.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut arg.value)
                                .id_salt(("arg_val", ai))
                                .hint_text(gray_hint("--argument"))
                                .desired_width(w),
                        );
                    });
                    requires_edit(ui, ("arg_req", ai), &mut arg.requires);
                },
            );
            if a != RowAction::None {
                deferred.push(Deferred::Arg(ai, a));
            }
        }
        if add_button(ui, cache, "Add argument").clicked() {
            deferred.push(Deferred::ArgAdd);
        }
    });
}

/// Apply a `RowAction` to a `Vec` (reorder via `swap`, remove via `remove`).
fn apply_row<T>(v: &mut Vec<T>, idx: usize, a: RowAction) {
    match a {
        RowAction::Up => {
            if idx > 0 && idx < v.len() {
                v.swap(idx - 1, idx);
            }
        }
        RowAction::Down => {
            if idx + 1 < v.len() {
                v.swap(idx, idx + 1);
            }
        }
        RowAction::Remove => {
            if idx < v.len() {
                v.remove(idx);
            }
        }
        RowAction::None => {}
    }
}

/// A `base` name made unique against `used` by appending `_2`, `_3`, …
fn unique_string(used: &std::collections::HashSet<String>, base: &str) -> String {
    if !used.contains(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let cand = format!("{base}_{n}");
        if !used.contains(&cand) {
            return cand;
        }
        n += 1;
    }
}

fn env_block<'a>(ext: &'a mut Extension, game: bool) -> &'a mut Vec<EnvVarSpec> {
    if game {
        &mut ext.game_env_vars
    } else {
        &mut ext.env_vars
    }
}

/// Apply one path-addressed structural edit to the draft. Applied after the
/// render loop so the containers are never mutated mid-iteration.
fn apply_deferred(draft: &mut ModuleDraft, d: Deferred) {
    match d {
        Deferred::Section(i, a) => apply_row(&mut draft.sections, i, a),
        Deferred::SectionAdd => {
            let used: std::collections::HashSet<String> =
                draft.sections.iter().map(|(k, _)| k.clone()).collect();
            draft
                .sections
                .push((unique_string(&used, "New Section"), Vec::new()));
        }
        Deferred::Field(si, fi, a) => {
            if let Some((_, fs)) = draft.sections.get_mut(si) {
                apply_row(fs, fi, a);
            }
        }
        Deferred::FieldAdd(si) => {
            let used: std::collections::HashSet<String> = draft
                .sections
                .iter()
                .flat_map(|(_, fs)| fs.iter().map(|f| f.variable.clone()))
                .collect();
            let var = unique_string(&used, "new_var");
            if let Some((_, fs)) = draft.sections.get_mut(si) {
                fs.push(new_field(var));
            }
        }
        Deferred::FieldOpt(si, fi, oi, a) => {
            if let Some((_, fs)) = draft.sections.get_mut(si) {
                if let Some(f) = fs.get_mut(fi) {
                    if let Some(OptionsSpec::List(v)) = &mut f.options {
                        apply_row(v, oi, a);
                    }
                }
            }
        }
        Deferred::FieldOptAdd(si, fi) => {
            if let Some((_, fs)) = draft.sections.get_mut(si) {
                if let Some(f) = fs.get_mut(fi) {
                    ensure_list(&mut f.options).push(String::new());
                }
            }
        }
        Deferred::Env(game, i, a) => apply_row(env_block(&mut draft.ext, game), i, a),
        Deferred::EnvAdd(game) => env_block(&mut draft.ext, game).push(EnvVarSpec {
            name: "NEW_VAR".to_string(),
            requires: None,
            builder: Vec::new(),
        }),
        Deferred::EnvStep(game, ei, bi, a) => {
            if let Some(e) = env_block(&mut draft.ext, game).get_mut(ei) {
                apply_row(&mut e.builder, bi, a);
            }
        }
        Deferred::EnvStepAdd(game, ei) => {
            if let Some(e) = env_block(&mut draft.ext, game).get_mut(ei) {
                e.builder.push(EnvBuilderEntry {
                    requires: None,
                    op: EnvOp::Set,
                    value: Some(String::new()),
                    separator: None,
                });
            }
        }
        Deferred::Wrapper(i, a) => apply_row(&mut draft.ext.wrappers, i, a),
        Deferred::WrapperAdd => draft.ext.wrappers.push(WrapperSpec {
            command_syntax: String::new(),
            requires: None,
            priority: 0,
            builder: Vec::new(),
        }),
        Deferred::WrapperStep(wi, bi, a) => {
            if let Some(w) = draft.ext.wrappers.get_mut(wi) {
                apply_row(&mut w.builder, bi, a);
            }
        }
        Deferred::WrapperStepAdd(wi) => {
            if let Some(w) = draft.ext.wrappers.get_mut(wi) {
                w.builder.push(WrapperBuilderEntry {
                    requires: None,
                    value: String::new(),
                });
            }
        }
        Deferred::Arg(i, a) => apply_row(&mut draft.ext.game_launch_args, i, a),
        Deferred::ArgAdd => draft.ext.game_launch_args.push(ArgSpec {
            requires: None,
            value: String::new(),
        }),
    }
}

/// UI-side lint over the preview: env var names that more than one module `Set`s,
/// where the later fold silently discards a value. Returns names sorted.
fn set_set_collisions(specs: &[Extension], res: &Resolution) -> Vec<String> {
    let mut setters: BTreeMap<String, std::collections::HashSet<String>> = BTreeMap::new();
    for spec in specs {
        if let Ok(lc) = context::assemble_launch(std::slice::from_ref(spec), res, &[]) {
            for (name, action) in lc.env_vars.iter().chain(lc.game_env_vars.iter()) {
                if matches!(action, EnvAction::Set(_)) {
                    setters.entry(name.clone()).or_default().insert(spec.id());
                }
            }
        }
    }
    setters
        .into_iter()
        .filter(|(_, m)| m.len() > 1)
        .map(|(k, _)| k)
        .collect()
}

impl GuiApp {
    /// The Graphite title bar: logo + wordmark, breadcrumb chip, and (in launch
    /// mode) the Cancel/Launch action pair.
    fn render_title_bar(&mut self, ctx: &egui::Context) {
        let frame = egui::Frame::none()
            .fill(theme::HEAD)
            .inner_margin(egui::Margin { left: 7.0, right: 18.0, top: 0.0, bottom: 0.0 });
        egui::TopBottomPanel::top("titlebar")
            .exact_height(61.0)
            .frame(frame)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    let (logo, _) =
                        ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
                    if let Some(tex) = &self.logo {
                        ui.painter().image(
                            tex.id(),
                            logo,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    } else {
                        paint_logo(ui.painter(), logo);
                    }
                    ui.add_space(-3.5);
                    ui.label(
                        egui::RichText::new("Ritz").color(theme::ACCENT).font(
                            egui::FontId::new(28.8, egui::FontFamily::Name("bold".into())),
                        ),
                    );
                    ui.add_space(10.0);
                    let (div, _) =
                        ui.allocate_exact_size(egui::vec2(1.0, 18.0), egui::Sense::hover());
                    ui.painter().rect_filled(div, 0.0, theme::BORDER);
                    ui.add_space(10.0);
                    breadcrumb_chip(ui, &self.game_config.game.name);

                    if self.launch_mode {
                        let sep = icon_sep(self.general_config.mono_ui);
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.add(theme::primary_button(format!("\u{f04b}{sep}Launch Game"))).clicked()
                                {
                                    *self.outcome.lock().unwrap() = EditOutcome::Continue;
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                                ui.add_space(8.0);
                                if ui.add(theme::danger_button("Cancel Launch")).clicked() {
                                    *self.outcome.lock().unwrap() = EditOutcome::Cancel;
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            },
                        );
                    }
                });
            });
    }

    /// The modules-column footer band: tree toggles + Open Folder.
    fn render_modules_footer(&mut self, ui: &mut egui::Ui) {
        styled_checkbox(ui, &mut self.group_by_author, "Group by Author");
        ui.add_space(6.0);
        styled_checkbox(ui, &mut self.show_inheritance, "Show Inheritance");

        let sep = icon_sep(self.general_config.mono_ui);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            // bottom_up: first added sits at the bottom, so Clear Settings ends up
            // below Open Folder.
            let w = ui.available_width();
            if ui.add_sized([w, 30.0], theme::danger_button("Clear Settings")).clicked() {
                self.confirm = Some(ConfirmAction::ClearSettings);
            }
            if ui
                .add_sized([w, 30.0], theme::secondary_button(format!("\u{f07b}{sep}Open Folder")))
                .clicked()
            {
                let _ = Command::new("xdg-open").arg(self.paths.games_dir()).spawn();
            }
        });
    }

    /// IDE mode's right-hand column: a read-only WYSIWYG render of the module
    /// under edit, over a nested launch-command band.
    ///
    /// *Why the launch band is nested inside this side panel* rather than being a
    /// full-width bottom panel: it belongs to the preview, not to the editor — the
    /// space under the editor column is the reserved diagnostics band. The nesting
    /// idiom (`TopBottomPanel::bottom(..).show_inside(..)`) is the same one
    /// `group_toggle` and `nav_settings` already use. Nested panels obey
    /// declaration order exactly like top-level ones, so the band is declared
    /// **before** the inner `CentralPanel` or it would be laid out over the body.
    fn render_ide_preview_panel(
        &mut self,
        ctx: &egui::Context,
        ide_specs: &[Extension],
        preview: &str,
        collisions: &[String],
        dynamic_preview: bool,
    ) {
        let NavSel::ModuleEditor(id) = self.nav_sel.clone() else {
            return;
        };
        // Resolve over the SAME spliced list the launch band assembled from, so the
        // two halves of the column can never disagree. Resolving over `cur_specs`
        // (what `resolve_for_editing` hardwired before S3b) would return `None` for
        // any module that doesn't apply to the ambient game and blank the body.
        let resolution = self.resolve_specs_for_editing(ide_specs);
        let spec = ide_specs.iter().find(|s| s.id() == id).cloned();
        let touch = self.general_config.touch_mode;
        // ── Column split ────────────────────────────────────────────────────
        // A STATIC 50/50: editor and preview each get exactly half of everything
        // right of the fixed `NAV_W` nav column. Recomputed from the live
        // `screen_rect` every frame, so the halves track a window resize instead
        // of holding a width captured at first paint.
        //
        // *Why fixed and not a clamped drag range:* the splitter could be dragged
        // far enough left that the preview slid **behind** the nav column and
        // vanished. Clamping would paper over that; removing the handle removes
        // the failure mode outright, and the user asked for a static half-and-half
        // to begin with. The old `PREVIEW_MIN_W`/`EDITOR_MIN_W`/`width_range`
        // machinery only existed to bound that drag, so it is gone rather than
        // left inert — half a window is comfortably above either column's floor
        // on any display this app is usable on.
        //
        // *Not exactly half, by one pixel:* `SidePanel` paints its separator line
        // out of the space **outside** `exact_width`, so the editor's central
        // panel ends up ~1px narrower than the preview. Absorbing that into the
        // computation would make the two columns provably unequal in the other
        // direction; a 1px asymmetry from a hairline is the smaller lie.
        let split_w = ((ctx.screen_rect().width() - NAV_W) * 0.5).max(0.0);

        // Id deliberately bumped from "ide_preview" to "ide_preview_split" back
        // when the width was persisted. It stays: `exact_width` + `resizable(false)`
        // ignores stored width entirely, so the stale-memory hazard that motivated
        // the rename is now moot, but renaming again would only churn ids. (The
        // `push_id("ide_preview")` widget namespace below is unrelated and
        // intentionally left alone.)
        egui::SidePanel::right("ide_preview_split")
            .resizable(false)
            .exact_width(split_w)
            .frame(egui::Frame::none().fill(theme::PANEL))
            .show(ctx, |ui| {
                let mut band = egui::TopBottomPanel::bottom("ide_launch_band")
                    .show_separator_line(true)
                    .frame(egui::Frame::none()
                        .fill(theme::PANEL2)
                        .inner_margin(egui::Margin::same(16.0)));
                if !dynamic_preview {
                    band = band.exact_height(198.0);
                }
                band.show_inside(ui, |ui| {
                    ui.label(theme::header_label("Launch command preview"));
                    // No "Previewing against: {game}" line here, unlike Config mode:
                    // IDE Mode is specified to resolve against nothing by default, so
                    // naming a game would advertise a binding the design doesn't want
                    // (the real scratch-layer resolution lands in S5).
                    for var in collisions {
                        ui.label(
                            egui::RichText::new(format!(
                                "\u{f071} Two modules both Set {var} — one value will be lost.",
                            ))
                            .color(theme::COL_GLOBAL)
                            .small(),
                        );
                    }
                    ui.add_space(8.0);
                    egui::Frame::none()
                        .fill(theme::FIELD)
                        .stroke(egui::Stroke::new(1.0, theme::BORDER))
                        .rounding(egui::Rounding::same(8.0))
                        .inner_margin(egui::Margin::symmetric(13.0, 11.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            if !dynamic_preview {
                                let box_h = ui.available_height();
                                ui.set_min_height((box_h - 22.0).max(0.0));
                            }
                            egui::ScrollArea::vertical()
                                .id_salt("ide_launch_scroll")
                                .auto_shrink([false, dynamic_preview])
                                .drag_to_scroll(touch)
                                .show(ui, |ui| {
                                    ui.add(egui::Label::new(command_job(preview)).wrap());
                                });
                        });
                });
                egui::CentralPanel::default()
                    .frame(egui::Frame::none()
                        .fill(theme::PANEL)
                        .inner_margin(egui::Margin { left: 16.0, right: 8.0, top: 14.0, bottom: 0.0 }))
                    .show_inside(ui, |ui| {
                        // ── Id namespace ────────────────────────────────────────
                        // The editor column and this one render the same module in
                        // the same frame. `render_module_settings_body` mints
                        // position-derived auto-ids (and `ComboBox::from_id_salt`
                        // salted by variable name ALONE) — identical in both columns,
                        // so without a namespace the two would fight over widget
                        // state. `push_id` must wrap the WHOLE body, not just the
                        // combo: the multi_string and env-pair renderers auto-id
                        // their TextEdits, "+ Add" buttons and icon_buttons too.
                        // This is the only `push_id` in the file; S4 replaces the
                        // per-widget salts properly.
                        ui.push_id("ide_preview", |ui| {
                            ui.label(theme::header_label("Preview"));
                            let Some(spec) = spec.as_ref() else {
                                ui.add_space(8.0);
                                ui.label(
                                    egui::RichText::new(
                                        "This module isn't loaded — nothing to preview.",
                                    )
                                    .color(theme::DIM),
                                );
                                return;
                            };
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} v{} by {}",
                                    spec.meta.name, spec.meta.version, spec.meta.author
                                ))
                                .color(theme::FAINT)
                                .small(),
                            );
                            ui.add_space(8.0);
                            ui.separator();
                            let Some(ext_res) = resolution.exts.get(&spec.id()) else {
                                // Explicit notice rather than an empty panel: a blank
                                // column reads as "this module has no settings", which
                                // is a very different (and wrong) message.
                                ui.add_space(8.0);
                                ui.label(
                                    egui::RichText::new(
                                        "\u{f071} Couldn't resolve this module — preview unavailable.",
                                    )
                                    .color(theme::COL_GLOBAL),
                                );
                                return;
                            };
                            // `full_width = true`: the 743px clamp exists for Config
                            // mode's narrow column and would leave a dead gutter here.
                            let changed = self.render_module_settings_body(
                                ui,
                                spec,
                                Some(ext_res),
                                true,
                                true,
                            );
                            // Zero write-path risk is the entire point of this stage:
                            // any write from here would reach `set_scoped`, whose
                            // `ModuleEditor(_)` arm writes the REAL game config.
                            debug_assert!(!changed, "read-only preview reported a change");
                        });
                    });
            });
    }

    /// Detail header for the selected module in the central panel: the
    /// name/version/author heading row, the "Editing Game/Profile/Global: X" context
    /// label, the description, and a trailing separator. Split out of the old inline
    /// `ui()` block (S3a) so `render_module_settings_body` below can eventually be
    /// called twice — this header only ever needs to render once per selected
    /// module, which is why it stays a separate method rather than folding into the
    /// body.
    ///
    /// *Why no "Edit" button here:* this header used to carry an "Edit" icon button
    /// opening the manifest editor. IDE Mode's tab now owns that job, so
    /// a second entry point in the read-only Config-mode detail view was redundant
    /// — removed along with the `open_inspector` deferred flag it existed to serve.
    fn render_module_detail_header(&mut self, ui: &mut egui::Ui, spec: &Extension) {
        let edit_ctx_label: Option<String> = match &self.nav_sel {
            NavSel::GlobalSettings => {
                Some("Editing Global Profile — applies to all games".to_string())
            }
            NavSel::Profile(name) => Some(format!("Editing Profile: {name}")),
            NavSel::ModuleEditor(_) => None, // handled by the caller; unreachable here
            NavSel::Game(_) => {
                Some(format!("Editing Game: {}", self.game_config.game.name))
            }
            NavSel::GeneralSettings => None,
        };
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading(&spec.meta.name);
            ui.add_space(6.0);
            ui.label(egui::RichText::new(format!("v{}", spec.meta.version)).color(theme::FAINT));
            ui.label(egui::RichText::new(format!("by {}", spec.meta.author)).color(theme::FAINT));
        });
        if let Some(label) = edit_ctx_label {
            ui.add_space(2.0);
            ui.label(egui::RichText::new(label).color(theme::ACCENT).small());
        }
        if let Some(desc) = &spec.meta.description {
            ui.add_space(4.0);
            ui.label(egui::RichText::new(desc).color(theme::DIM));
        }
        ui.add_space(10.0);
        ui.separator();
    }

    /// Settings body for the selected module: the field-editor `ScrollArea`
    /// (section header + per-field `render_field` loop), the scope legend, and the
    /// trailing colour-hint footer. Returns whether any field changed (mirrors the
    /// old inline `changed` bool this replaced).
    ///
    /// `read_only` forces every field to render non-editable and suppresses the
    /// scope legend and colour-hint footer — neither makes sense on a read-only
    /// preview. It is threaded through to `render_field`/`render_value_editor`
    /// rather than relying on `ui.add_enabled_ui`; see `render_value_editor`'s doc
    /// comment for why. Every call site in this stage (S3a) passes `false`, so
    /// behaviour is unchanged — a second call with `read_only: true` is S3b's job.
    fn render_module_settings_body(
        &mut self,
        ui: &mut egui::Ui,
        spec: &Extension,
        ext_res: Option<&resolve::ExtResolution>,
        full_width: bool,
        read_only: bool,
    ) -> bool {
        let mut changed = false;
        egui::ScrollArea::vertical()
            // Fill the panel (don't shrink to content) so the scrollbar sits at
            // the far right while the content stays capped/left-aligned.
            .auto_shrink([false, false])
            .drag_to_scroll(self.general_config.touch_mode)
            .show(ui, |ui| {
            ui.vertical(|ui| {
                let max_w = body_max_width(ui, full_width);
                ui.set_max_width(max_w);
                // Custom-env/-game-env/-args modules render through this same
                // generic section/field loop — `render_field` dispatches their
                // single `multi_string` field (Variable `env`/`game_env`/`args`)
                // to the right widget by backend string, see `render_field`.
                {
                    let mut any_section = false;
                    for (section, fields) in &spec.ui {
                        let visible: Vec<&UiField> =
                            fields.iter().filter(|f| field_visible(f, ext_res)).collect();
                        if visible.is_empty() {
                            continue;
                        }
                        any_section = true;
                        ui.add_space(12.0);
                        ui.horizontal(|ui| {
                            ui.label(theme::section_label(section));
                            if !read_only {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    scope_legend(ui);
                                });
                            }
                        });
                        ui.add_space(6.0);
                        let field_chain: Vec<Preset> = match &self.nav_sel {
                            NavSel::Game(_) => {
                                let mut c = Vec::new();
                                if let Some(p) = &self.preset {
                                    c.push(p.clone());
                                    if let Some(pn) = &p.parent {
                                        c.extend(collect_parent_presets(&self.paths, pn));
                                    }
                                }
                                c
                            }
                            NavSel::Profile(_) => self.editing_preset_buf.as_ref()
                                .and_then(|p| p.parent.as_ref())
                                .map(|pn| collect_parent_presets(&self.paths, pn))
                                .unwrap_or_default(),
                            _ => Vec::new(),
                        };
                        for field in visible {
                            let Some(res) = ext_res.and_then(|e| e.fields.get(&field.variable)) else {
                                continue;
                            };
                            let preset_depth: Option<usize> =
                                if res.provenance == resolve::Provenance::Preset
                                    && self.general_config.show_field_inheritance
                                    && !field_chain.is_empty()
                                {
                                    field_chain.iter().enumerate()
                                        .find(|(_, p)| p.modules
                                            .get(&spec.meta.author)
                                            .and_then(|e| e.get(&spec.meta.name))
                                            .and_then(|vars| vars.get(&field.variable))
                                            .is_some())
                                        .map(|(i, _)| i)
                                } else { None };
                            if self.render_field(ui, spec, field, res, preset_depth, read_only) {
                                changed = true;
                            }
                        }
                    }
                    if any_section && !read_only {
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new(
                            "Colours mark where each value resolves from — Global, Profile, or this Game. \
                             Left-click a checkbox to toggle, right-click to reset to inherited.",
                        ).color(theme::FAINT).small());
                    }
                }
            });
        });
        changed
    }

    fn render_field(
        &mut self,
        ui: &mut egui::Ui,
        spec: &Extension,
        field: &UiField,
        res: &resolve::ResolvedField,
        preset_depth: Option<usize>,
        read_only: bool,
    ) -> bool {
        let state = tri_state(res);
        // A value set at the layer being edited resolves as `Provenance::Game`
        // (the layer is loaded as a fake game), so color it by the actual editing
        // scope — Game blue, Profile green, Global red — not always blue.
        let scope = match res.provenance {
            resolve::Provenance::Game => self.editing_scope_color(),
            resolve::Provenance::Preset
                if self.general_config.show_field_inheritance
                    && matches!(self.general_config.inheritance_display, InheritanceDisplayMode::Color)
                    && preset_depth.is_some() =>
            {
                profile_depth_color(preset_depth.unwrap())
            }
            p => theme::scope_color(p),
        };
        const NUMS: [&str; 6] = ["1", "2", "3", "4", "5", "5+"];
        let badge: Option<(&str, Color32)> =
            if self.general_config.show_field_inheritance
                && matches!(self.general_config.inheritance_display, InheritanceDisplayMode::Numbers)
                && res.provenance == resolve::Provenance::Preset
            {
                preset_depth.map(|d| (NUMS[d.min(5)], COL_PROFILE))
            } else {
                None
            };
        let mut changed = false;

        // A list field renders as its own growing slot card, not the one-row layout.
        // The custom-env / custom-game-env modules' one `multi_string` field (keyed
        // by backend + Variable name, not by manifest identity) gets the two-column
        // Name|Value widget instead of the plain single-column one; custom-args
        // keeps the plain widget (one literal argument per row).
        if field.field_type == FieldType::MultiString {
            let pair_var = match spec.backend.as_deref() {
                Some("custom-env") => Some("env"),
                Some("custom-game-env") => Some("game_env"),
                _ => None,
            };
            if pair_var == Some(field.variable.as_str()) {
                return self.render_env_pair_field(ui, spec, field, scope, read_only);
            }
            return self.render_multi_string_field(ui, spec, field, scope, read_only);
        }

        // Row card: scope-tinted background, 8px rounding, with a 3px left bar.
        let tint = Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16);
        let inner = egui::Frame::none()
            .fill(tint)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin { left: 12.0, right: 5.0, top: 0.0, bottom: 0.0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    // Fixed row height; all content vertically centered, so the
                    // backdrop doesn't grow when taller controls appear.
                    ui.set_min_height(39.0);
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let label = field.name.clone().unwrap_or_else(|| field.variable.clone());
                    let hover = field.description.clone().unwrap_or_default();
                    let left = ui.cursor().min.x;
                    // `read_only` goes *into* `scope_checkbox` rather than gating
                    // its result here: the row must still paint (WYSIWYG preview)
                    // but must be allocated non-interactively, so no click can ever
                    // reach `apply_tri` — which writes config and seeds
                    // `text_buffers`. This is the one write path shared by *every*
                    // field type, Toggle included.
                    let value = TriValue { state, inherited_on: res.var.truthy };
                    if let Some(new) =
                        scope_checkbox(ui, &label, &hover, value, scope, badge, read_only)
                    {
                        self.apply_tri(spec, field, res, new);
                        changed = true;
                    }
                    // Reserve a fixed 260px for the checkbox+label; the control fills the rest.
                    let used = ui.cursor().min.x - left;
                    ui.add_space((260.0 - used).max(0.0));

                    // Always draw the editor for non-toggle fields so the value is
                    // visible; when not editable at this scope (Disabled/inherited)
                    // it's shown greyed-out and read-only, displaying the effective
                    // (inherited) value. Right-to-left so controls are right-bound.
                    if field.field_type != FieldType::Toggle {
                        // `read_only` forces this to false regardless of resolution
                        // state — see `render_value_editor` for why that has to be an
                        // explicit bool rather than relying on `ui.is_enabled()`.
                        let editable = !read_only && state == Tri::Enabled;
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_enabled_ui(editable, |ui| {
                                changed |= self.render_value_editor(ui, spec, field, res, scope, editable, read_only);
                            });
                        });
                    }
                });
            });

        // Left scope bar: clip to the left 3px and draw the full rounded card
        // outline, so the bar follows the backdrop's corner curvature exactly.
        let r = inner.response.rect;
        let bar_clip = egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + 3.0, r.max.y));
        ui.painter()
            .with_clip_rect(bar_clip)
            .rect_filled(r, egui::Rounding::same(8.0), scope);
        ui.add_space(8.0);

        changed
    }

    /// Render a `multi_string` field: a scope-tinted card with the label and a
    /// growing list of entry rows (text box + delete) plus a `+ Add` button. The
    /// list is stored as a JSON array at the current scope.
    ///
    /// `read_only` renders the identical card — same label, same rows, same
    /// buttons — but with every widget non-interactive and *no* shared state
    /// touched: the working copy is seeded straight from the stored array instead
    /// of taking (and re-inserting) `self.multi_edit`, and the persist block is
    /// skipped, so neither `set_current` nor `unset_current` (which removes from
    /// `text_buffers`) can run. Returns `false` unconditionally when read-only.
    fn render_multi_string_field(&mut self, ui: &mut egui::Ui, spec: &Extension, field: &UiField, scope: Color32, read_only: bool) -> bool {
        let author = spec.meta.author.clone();
        let name = spec.meta.name.clone();
        let var = field.variable.clone();
        let label = field.name.clone().unwrap_or_else(|| var.clone());
        let hover = field.description.clone().unwrap_or_default();

        // Per scope+field working copy of the list, seeded from the stored array.
        let scope_tag = match &self.nav_sel {
            NavSel::GlobalSettings => "global".to_string(),
            NavSel::Profile(n) => format!("profile:{n}"),
            NavSel::Game(a) => format!("game:{a}"),
            // Not a real edit scope — behave like the ambient game.
            NavSel::ModuleEditor(_) => format!("game:{}", self.appid),
            NavSel::GeneralSettings => "general".to_string(),
        };
        let key = format!("{scope_tag}::{author}::{name}::{var}");
        let stored: Vec<String> = self
            .current_scope_value(&author, &name, &var)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        // Read-only takes the working copy by *reference* (clone, no `remove`) so
        // the live editor's in-progress buffer is still what the preview shows,
        // without the preview owning or consuming it.
        let mut entries = if read_only {
            self.multi_edit.get(&key).cloned().unwrap_or_else(|| stored.clone())
        } else {
            self.multi_edit.remove(&key).unwrap_or(stored.clone())
        };

        let tint = Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16);
        let btn_w = icon_button_width(ui) + ui.spacing().item_spacing.x;
        let cache = &mut self.icon_cache;
        let mut to_delete: Option<usize> = None;

        let inner = egui::Frame::none()
            .fill(tint)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin { left: 12.0, right: 8.0, top: 8.0, bottom: 8.0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let lab = ui.label(egui::RichText::new(&label).color(theme::TEXT).strong());
                if !hover.is_empty() {
                    lab.on_hover_text(&hover);
                }
                ui.add_space(4.0);
                let w = (ui.available_width() - btn_w - 4.0).max(40.0);
                for (i, entry) in entries.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(entry)
                                .desired_width(w)
                                .interactive(!read_only)
                                .hint_text(egui::RichText::new("command").color(theme::FAINT)),
                        );
                        if icon_button(ui, cache, "\u{f467}", "", IconBtn::Danger, !read_only).clicked() {
                            to_delete = Some(i);
                        }
                    });
                }
                if ui.add_enabled(!read_only, egui::Button::new("+ Add")).clicked() {
                    entries.push(String::new());
                }
            });

        if let Some(i) = to_delete {
            entries.remove(i);
        }

        // Persist the non-empty entries when they differ from what's stored.
        let cleaned: Vec<String> = entries.iter().filter(|s| !s.trim().is_empty()).cloned().collect();
        let mut changed = false;
        if !read_only && cleaned != stored {
            if cleaned.is_empty() {
                self.unset_current(spec, field);
            } else {
                self.set_current(spec, field, json!(cleaned));
            }
            changed = true;
        }

        // Left scope bar to match the other field cards.
        let r = inner.response.rect;
        let bar_clip = egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + 3.0, r.max.y));
        ui.painter()
            .with_clip_rect(bar_clip)
            .rect_filled(r, egui::Rounding::same(8.0), scope);
        ui.add_space(8.0);

        if !read_only {
            self.multi_edit.insert(key, entries);
        }
        changed
    }

    /// Render a two-column Name | Value list for the custom-env / custom-game-env
    /// backends' single `multi_string` field. Storage is unchanged — still the
    /// field's `multi_string` list — but each entry is displayed split into a Name
    /// and a Value box and reassembled as `"{name}={value}"` (see [`parse_env_row`]).
    /// The Name half is live-validated to the POSIX env charset
    /// ([`is_valid_env_name`]): an invalid name gets a red outline, and a row whose
    /// name is empty/invalid is dropped when persisting, so a bad pair can never
    /// reach a stored env var. The Value half is unrestricted (may contain `=`).
    ///
    /// `read_only` behaves exactly as in [`Self::render_multi_string_field`]: same
    /// card, non-interactive widgets, `multi_edit` read but never taken or
    /// re-inserted, and the persist block skipped so no `set_current` /
    /// `unset_current` can fire. Returns `false` unconditionally when read-only.
    fn render_env_pair_field(&mut self, ui: &mut egui::Ui, spec: &Extension, field: &UiField, scope: Color32, read_only: bool) -> bool {
        let author = spec.meta.author.clone();
        let name = spec.meta.name.clone();
        let var = field.variable.clone();
        let label = field.name.clone().unwrap_or_else(|| var.clone());
        let hover = field.description.clone().unwrap_or_default();

        let scope_tag = match &self.nav_sel {
            NavSel::GlobalSettings => "global".to_string(),
            NavSel::Profile(n) => format!("profile:{n}"),
            NavSel::Game(a) => format!("game:{a}"),
            // Not a real edit scope — behave like the ambient game.
            NavSel::ModuleEditor(_) => format!("game:{}", self.appid),
            NavSel::GeneralSettings => "general".to_string(),
        };
        let key = format!("{scope_tag}::{author}::{name}::{var}");
        let stored: Vec<String> = self
            .current_scope_value(&author, &name, &var)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        // See `render_multi_string_field`: read-only clones the working copy
        // instead of taking it, so the preview never consumes the live buffer.
        let mut entries = if read_only {
            self.multi_edit.get(&key).cloned().unwrap_or_else(|| stored.clone())
        } else {
            self.multi_edit.remove(&key).unwrap_or(stored.clone())
        };

        let tint = Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16);
        let btn_w = icon_button_width(ui) + ui.spacing().item_spacing.x;
        let cache = &mut self.icon_cache;
        let mut to_delete: Option<usize> = None;

        let inner = egui::Frame::none()
            .fill(tint)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin { left: 12.0, right: 8.0, top: 8.0, bottom: 8.0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let lab = ui.label(egui::RichText::new(&label).color(theme::TEXT).strong());
                if !hover.is_empty() {
                    lab.on_hover_text(&hover);
                }
                ui.add_space(4.0);
                // Name gets ~1/3 of the row, Value the rest.
                let total_w = (ui.available_width() - btn_w - 8.0).max(80.0);
                let name_w = (total_w * 0.32).max(60.0);
                let value_w = (total_w - name_w).max(40.0);
                for (i, entry) in entries.iter_mut().enumerate() {
                    let (mut n, mut v) = parse_env_row(entry);
                    let name_ok = n.is_empty() || is_valid_env_name(&n);
                    ui.horizontal(|ui| {
                        let name_resp = ui.add(
                            egui::TextEdit::singleline(&mut n)
                                .desired_width(name_w)
                                .interactive(!read_only)
                                .hint_text(egui::RichText::new("NAME").color(theme::FAINT)),
                        );
                        if !name_ok {
                            ui.painter().rect_stroke(
                                name_resp.rect,
                                ui.visuals().widgets.inactive.rounding,
                                egui::Stroke::new(1.5, theme::COL_GLOBAL),
                            );
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut v)
                                .desired_width(value_w)
                                .interactive(!read_only)
                                .hint_text(egui::RichText::new("value").color(theme::FAINT)),
                        );
                        if icon_button(ui, cache, "\u{f467}", "", IconBtn::Danger, !read_only).clicked() {
                            to_delete = Some(i);
                        }
                    });
                    *entry = format!("{n}={v}");
                }
                if ui.add_enabled(!read_only, egui::Button::new("+ Add")).clicked() {
                    entries.push(String::new());
                }
            });

        if let Some(i) = to_delete {
            entries.remove(i);
        }

        // Persist only rows with a valid, non-empty name — an invalid/empty name
        // never reaches the stored list, so it can't produce a broken env var.
        let cleaned: Vec<String> = entries
            .iter()
            .filter_map(|e| {
                let (n, v) = parse_env_row(e);
                let n = n.trim();
                (!n.is_empty() && is_valid_env_name(n)).then(|| format!("{n}={v}"))
            })
            .collect();
        let mut changed = false;
        if !read_only && cleaned != stored {
            if cleaned.is_empty() {
                self.unset_current(spec, field);
            } else {
                self.set_current(spec, field, json!(cleaned));
            }
            changed = true;
        }

        // Left scope bar to match the other field cards.
        let r = inner.response.rect;
        let bar_clip = egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + 3.0, r.max.y));
        ui.painter()
            .with_clip_rect(bar_clip)
            .rect_filled(r, egui::Rounding::same(8.0), scope);
        ui.add_space(8.0);

        if !read_only {
            self.multi_edit.insert(key, entries);
        }
        changed
    }

    /// The raw stored value for a variable at the scope currently being edited.
    fn current_scope_value(&self, author: &str, name: &str, var: &str) -> Option<&Value> {
        match &self.nav_sel {
            NavSel::GlobalSettings => self.global_config.get_value(author, name, var),
            NavSel::Profile(_) => self.editing_preset_buf.as_ref()?.get_value(author, name, var),
            _ => self.game_config.get_value(author, name, var),
        }
    }

    /// Apply a new tri-state to `spec`'s field. Pure dispatcher — it holds no
    /// identity of its own, it only forwards the caller's `spec` to the writers.
    fn apply_tri(&mut self, spec: &Extension, field: &UiField, res: &resolve::ResolvedField, new: Tri) {
        match new {
            Tri::Unset => self.unset_current(spec, field),
            Tri::Disabled => {
                let val = if field.field_type == FieldType::Toggle {
                    json!(false)
                } else {
                    Value::Null
                };
                self.set_current(spec, field, val);
            }
            Tri::Enabled => {
                let val = match field.field_type {
                    FieldType::Toggle => json!(true),
                    FieldType::Selection => {
                        let cur = if res.var.value.is_empty() {
                            option_values(field)
                                .first()
                                .map(|(v, _)| v.clone())
                                .unwrap_or_default()
                        } else {
                            res.var.value.clone()
                        };
                        json!(cur)
                    }
                    FieldType::Integer => {
                        let v: f64 = res.var.value.parse().unwrap_or(number_range(field).0);
                        num_value(v, true)
                    }
                    FieldType::Float => {
                        let v: f64 = res.var.value.parse().unwrap_or(number_range(field).0);
                        num_value(v, false)
                    }
                    FieldType::String => json!(res.var.value.clone()),
                    // Edited via its own slot card, never through the tri-state checkbox.
                    FieldType::MultiString => json!([] as [String; 0]),
                };
                self.set_current(spec, field, val);
            }
        }
    }

    /// Value editor for a non-toggle field. When `editable` is false the widgets
    /// are drawn read-only (the caller has disabled the ui) and just display the
    /// effective value, so no persistent edit buffer is touched.
    ///
    /// `read_only` is threaded down separately from `editable` (rather than
    /// trusting the caller's `editable` alone) as a deliberate belt-and-suspenders:
    /// the `String` arm below branches on the `editable` *parameter*, not
    /// `ui.is_enabled()` — wrapping the call in `ui.add_enabled_ui(false, …)` at the
    /// call site greys out the widget but does NOT stop this arm from writing into
    /// `self.text_buffers` or minting the `("text_edit", key)` `egui::Id`. Re-deriving
    /// `editable` from `read_only` here means a future caller can never reintroduce a
    /// read-only leak by passing a stale `editable: true` alongside `read_only: true`.
    fn render_value_editor(
        &mut self,
        ui: &mut egui::Ui,
        spec: &Extension,
        field: &UiField,
        res: &resolve::ResolvedField,
        scope: Color32,
        editable: bool,
        read_only: bool,
    ) -> bool {
        let editable = editable && !read_only;
        let mut changed = false;
        match field.field_type {
            FieldType::Selection => {
                let vals = option_values(field);
                let current = res.var.value.clone();
                let cur_label = vals
                    .iter()
                    .find(|(v, _)| *v == current)
                    .map(|(_, l)| l.clone())
                    .unwrap_or_else(|| current.clone());
                // egui draws the ComboBox button ~4px taller than the height it
                // reserves for layout (downward), so cross-axis centering lands it
                // low. Place it in an explicit rect instead, vertically centered in
                // the row band and right-aligned to the band edge (the row card's
                // 5px right margin gives the 5px gap to the card's visual edge).
                let band = ui.max_rect();
                let h = ui.spacing().interact_size.y + 4.0;
                let left = band.left() + 8.0;
                let right = band.right();
                // -2: the ComboBox button draws ~2px below the ui's top, so nudge
                // the placement up to land it centered in the band.
                let rect = egui::Rect::from_min_size(
                    egui::pos2(left, band.center().y - h / 2.0 - 2.0),
                    egui::vec2(right - left, h),
                );
                let mut picked = None;
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
                    egui::ComboBox::from_id_salt(&field.variable)
                        .width(rect.width())
                        .selected_text(cur_label)
                        .show_ui(ui, |ui| {
                            for (value, label) in &vals {
                                if ui.selectable_label(*value == current, label).clicked() {
                                    picked = Some(value.clone());
                                }
                            }
                        });
                });
                // The ComboBox is deliberately still *drawn* when read-only (it is a
                // WYSIWYG preview); `editable` is what stops the pick from writing.
                // A disabled `Ui` already makes `picked` unreachable, so this guard
                // is a no-op for `read_only == false` — it exists so the parameter,
                // not `ui.is_enabled()`, is what guarantees inertness.
                if let Some(v) = picked.filter(|_| editable) {
                    self.set_current(spec, field, json!(v));
                    changed = true;
                }
            }
            FieldType::Integer | FieldType::Float => {
                let integer = field.field_type == FieldType::Integer;
                let (min, max, step) = number_range(field);
                let v: f64 = res.var.value.parse().unwrap_or(min);
                // Same reasoning as the Selection arm: the slider/spinner is drawn
                // either way, `editable` is the gate on the write.
                if let Some(nv) = scope_slider(ui, v, min, max, step, integer, scope)
                    .filter(|_| editable)
                {
                    self.set_current(spec, field, num_value(nv, integer));
                    changed = true;
                }
            }
            FieldType::String => {
                // Right-to-left: the Detect button (if any) pins to the right, then
                // the text field fills the remaining width to its left.
                if editable && field.variable == "window_class" {
                    let remaining = self.detect.as_ref().and_then(|d| {
                        matches!(*d.result.lock().unwrap(), DetectStatus::Waiting).then(|| {
                            (3.0 - d.start.elapsed().as_secs_f32()).ceil().max(0.0) as u64
                        })
                    });
                    if let Some(secs) = remaining {
                        ui.add_enabled(false, egui::Button::new(format!("Detecting… {secs}")));
                    } else if ui
                        .button("Detect")
                        .on_hover_text("Focus the game window within 3 seconds")
                        .clicked()
                    {
                        self.start_detect(spec, field);
                    }
                }

                let tw = (ui.available_width() - 4.0).max(40.0);
                if editable {
                    let key = format!("{}::{}", spec.id(), field.variable);
                    let id = egui::Id::new(("text_edit", &key));
                    // Sync the buffer from the resolved value whenever the field isn't
                    // actively focused. This prevents stale values after a profile switch
                    // (the nav action and resolve_for_editing run in the wrong order within
                    // one frame, so without this the buffer gets populated from a stale
                    // resolution and persists until the user clicks the profile again).
                    if !ui.ctx().memory(|m| m.has_focus(id)) {
                        self.text_buffers.insert(key.clone(), res.var.value.clone());
                    }
                    let edited = {
                        let buf = self.text_buffers.entry(key).or_insert_with(|| res.var.value.clone());
                        ui.add(egui::TextEdit::singleline(buf).desired_width(tw).id(id))
                            .changed()
                            .then(|| buf.clone())
                    };
                    if let Some(v) = edited {
                        self.set_current(spec, field, json!(v));
                        changed = true;
                    }
                } else {
                    // Read-only: show the effective value without a persistent buffer.
                    let mut shown = res.var.value.clone();
                    ui.add(egui::TextEdit::singleline(&mut shown).desired_width(tw));
                }
            }
            // Rendered by their own dedicated paths, not the inline value editor.
            FieldType::Toggle | FieldType::MultiString => {}
        }
        changed
    }

    /// Write `value` for `author::name::var` into whichever scope is currently being
    /// edited. The single place that maps `nav_sel` onto a config store for writes —
    /// every writer must route through here, or it silently lands in the wrong scope
    /// (see `poll_detect`, which used to write `game_config` unconditionally).
    fn set_scoped(&mut self, a: &str, n: &str, var: &str, value: Value) {
        match &self.nav_sel.clone() {
            NavSel::GlobalSettings => {
                self.global_config.set_value(a, n, var, value);
            }
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf {
                    p.set_value(a, n, var, value);
                }
            }
            NavSel::Game(_) | NavSel::ModuleEditor(_) => {
                self.game_config.set_value(a, n, var, value);
            }
            // Unreachable, on two legs — both are needed, the render one alone is not
            // enough:
            //   1. Render path: no module field is ever rendered while General
            //      Settings is selected (the central panel returns right after
            //      `render_general_settings_panel`, and the module-list panel is
            //      skipped for this selection), so no `set_current` caller runs.
            //   2. Async path: `poll_detect` runs every frame for *every* `nav_sel`,
            //      outside both of those gates, and is the one other caller. It is
            //      barred by `cancel_stale_detect`, which drops a detection whose
            //      `nav_sel` has changed — and a detection can only ever be started
            //      from `render_field`, i.e. never under General Settings to begin
            //      with. Note `NavSel::ModuleEditor` is off the render path for the
            //      same reason yet stays on the game arm below: it is a real edit
            //      scope reachable via that async path.
            // *Why a no-op and not the game arm it used to share:* routing a module
            // write here into `games/<appid>.json` would be a wrong-scope write with
            // no visible symptom, and there is no honest scope to pick — General
            // Settings' own controls write `general_config` (saved inside
            // `render_general_settings_panel`), never module values. `persist()` maps
            // this selection to `save_game`, which looks like evidence to the contrary
            // but is only a harmless re-save of an unchanged game config.
            NavSel::GeneralSettings => {
                debug_assert!(false, "module write with General Settings selected: {a}::{n}::{var}");
            }
        }
    }

    /// Remove a stored value for `author::name::var` from whichever scope is
    /// currently being edited — the unset twin of [`Self::set_scoped`], and the
    /// only place `nav_sel` maps onto a config store for removals.
    fn unset_scoped(&mut self, a: &str, n: &str, var: &str) {
        match &self.nav_sel.clone() {
            NavSel::GlobalSettings => {
                self.global_config.unset_value(a, n, var);
            }
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf {
                    p.unset_value(a, n, var);
                }
            }
            NavSel::Game(_) | NavSel::ModuleEditor(_) => {
                self.game_config.unset_value(a, n, var);
            }
            // Unreachable for the same reason as in `set_scoped` — see the
            // rationale there.
            NavSel::GeneralSettings => {
                debug_assert!(false, "module unset with General Settings selected: {a}::{n}::{var}");
            }
        }
    }

    /// Set a value for `spec`'s field on the *currently active edit target*.
    ///
    /// The module identity comes from the caller's `spec`, never from
    /// `cur_specs[selected_ext]`: a frame that renders more than one module's
    /// fields (e.g. a module next to a preview of another) would otherwise write
    /// every edit into whichever module the global selection index happens to
    /// point at.
    fn set_current(&mut self, spec: &Extension, field: &UiField, value: Value) {
        let (a, n) = (spec.meta.author.clone(), spec.meta.name.clone());
        self.set_scoped(&a, &n, &field.variable, value);
    }

    /// Begin a 3-second window-class detection for the given field.
    fn start_detect(&mut self, spec: &Extension, field: &UiField) {
        let result = Arc::new(Mutex::new(DetectStatus::Waiting));
        let handle = result.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            *handle.lock().unwrap() = DetectStatus::Done(detect_active_window_class());
        });
        self.detect = Some(Detect {
            result,
            start: Instant::now(),
            ext_id: spec.id(),
            author: spec.meta.author.clone(),
            name: spec.meta.name.clone(),
            var: field.variable.clone(),
            nav: self.nav_sel.clone(),
        });
    }

    /// Drop a pending detection whose scope the user has since navigated away from.
    ///
    /// `poll_detect` is an async completion writer decoupled from rendering: it runs
    /// every frame for every `nav_sel`, so without this a detection started under one
    /// scope could land its `set_scoped` write under another. *Why cancel rather than
    /// write to the scope it started in:* the snapshotted target may no longer exist —
    /// `editing_preset_buf` gets swapped on a profile switch and `game_config` gets
    /// replaced on a game switch — so "apply later" would resurrect exactly the
    /// wrong-scope write this guard exists to prevent. Cancelling also matches intent:
    /// the user navigated away from the field they were configuring.
    ///
    /// Comparing against a snapshot here (rather than clearing `detect` at each nav
    /// site) is deliberate: it cannot be defeated by a future `nav_sel` assignment
    /// that forgets to clear, of which there are a dozen-odd across this file.
    fn cancel_stale_detect(&mut self) {
        if self.detect.as_ref().is_some_and(|d| d.nav != self.nav_sel) {
            self.detect = None;
        }
    }

    /// Apply a finished detection (if any). Returns true if the config changed.
    fn poll_detect(&mut self, ctx: &egui::Context) -> bool {
        // Must precede the `Waiting` early return below, or an in-flight detection
        // would never be re-examined for a scope change until it completed.
        self.cancel_stale_detect();
        let done = match &self.detect {
            Some(d) => match d.result.lock().unwrap().clone() {
                DetectStatus::Waiting => {
                    ctx.request_repaint_after(Duration::from_millis(200));
                    return false;
                }
                DetectStatus::Done(class) => Some((
                    d.ext_id.clone(),
                    d.author.clone(),
                    d.name.clone(),
                    d.var.clone(),
                    class,
                )),
            },
            None => None,
        };
        let Some((ext_id, author, name, var, class)) = done else {
            return false;
        };
        self.detect = None;
        // Redraw so the text box reflects the new value immediately.
        ctx.request_repaint();
        if let Some(c) = class {
            // Scope-aware: detecting while editing a Profile must write the profile,
            // not the ambient game. Writing `game_config` here meant the value never
            // reached the edited scope (persist() saves by scope) while the text
            // buffer below still showed it — a silent wrong-scope write.
            self.set_scoped(&author, &name, &var, json!(c.clone()));
            self.text_buffers.insert(format!("{ext_id}::{var}"), c);
            return true;
        }
        false
    }

    /// Remove an override for `spec`'s field on the *currently active edit target*
    /// (reset to inherit). Identity — including the `text_buffers` key — comes from
    /// the caller's `spec`, for the reason given on [`Self::set_current`].
    fn unset_current(&mut self, spec: &Extension, field: &UiField) {
        let (a, n) = (spec.meta.author.clone(), spec.meta.name.clone());
        self.text_buffers.remove(&format!("{}::{}", spec.id(), field.variable));
        self.unset_scoped(&a, &n, &field.variable);
    }

    /// Render general app settings (splash timeout, default profile) in the central panel.
    /// Returns true if anything changed.
    fn render_general_settings_panel(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        ui.heading("General Settings");
        ui.separator();
        ui.add_space(8.0);

        egui::ScrollArea::vertical()
            // Fill the panel (don't shrink to content) so the scrollbar sits at
            // the far right while the content stays capped/left-aligned.
            .auto_shrink([false, false])
            .drag_to_scroll(self.general_config.touch_mode)
            .show(ui, |ui| {
        ui.vertical(|ui| {
        // Cap the row width like the module panel.
        let max_w = body_max_width(ui, self.general_config.full_width);
        ui.set_max_width(max_w);

        // Splash timeout
        changed |= settings_value_row(
            ui,
            "Splash timeout (s)",
            "Seconds the launch splash counts down before launching.",
            |ui| {
                let mut v = self.general_config.splash_timeout_secs as f32;
                ui.spacing_mut().slider_width = 170.0;
                if ui.add(egui::Slider::new(&mut v, 1.0_f32..=30.0).integer()).changed() {
                    self.general_config.splash_timeout_secs = v as u64;
                    true
                } else {
                    false
                }
            },
        );

        // Default profile
        let presets = self.all_presets.clone();
        let cur = self.general_config.default_preset.clone().unwrap_or_default();
        let cur_label = if cur.is_empty() { "None".to_string() } else { cur.clone() };
        changed |= settings_value_row(
            ui,
            "Default profile",
            "Profile applied to games that don't select their own.",
            |ui| {
                let mut new_preset: Option<Option<String>> = None;
                egui::ComboBox::from_id_salt("general_default_preset_main")
                    .selected_text(cur_label)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(cur.is_empty(), "None").clicked() {
                            new_preset = Some(None);
                        }
                        for p in &presets {
                            if ui.selectable_label(cur == *p, p).clicked() {
                                new_preset = Some(Some(p.clone()));
                            }
                        }
                    });
                if let Some(p) = new_preset {
                    self.general_config.default_preset = p.clone();
                    self.default_preset = p;
                    true
                } else {
                    false
                }
            },
        );

        // Editor closing action
        changed |= settings_value_row(
            ui,
            "Editor closing action",
            "What closing the editor window does when it was opened to gate a launch.",
            |ui| {
                let launches = self.general_config.editor_close_launches;
                let cur_label = if launches { "Launch Game" } else { "Cancel Launch" };
                let mut ch = false;
                egui::ComboBox::from_id_salt("general_editor_close_action")
                    .selected_text(cur_label)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(launches, "Launch Game").clicked() && !launches {
                            self.general_config.editor_close_launches = true;
                            // Apply live: the window-close default follows the setting.
                            *self.outcome.lock().unwrap() = EditOutcome::Continue;
                            ch = true;
                        }
                        if ui.selectable_label(!launches, "Cancel Launch").clicked() && launches {
                            self.general_config.editor_close_launches = false;
                            *self.outcome.lock().unwrap() = EditOutcome::Cancel;
                            ch = true;
                        }
                    });
                ch
            },
        );

        // Boolean toggles
        let mut mono = self.general_config.mono_ui;
        if settings_toggle_row(
            ui,
            "Monospace UI font",
            "Render the UI in a monospaced font — if you prefer a more techy look.",
            &mut mono,
        ) {
            self.general_config.mono_ui = mono;
            crate::fonts::install(ui.ctx(), mono);
            changed = true;
        }

        let mut touch = self.general_config.touch_mode;
        if settings_toggle_row(
            ui,
            "Touch Mode",
            "Drag content to scroll (instead of only the scrollbar). Handy on touchscreens.",
            &mut touch,
        ) {
            self.general_config.touch_mode = touch;
            changed = true;
        }

        let mut full = self.general_config.full_width;
        if settings_toggle_row(
            ui,
            "Use full UI width",
            "Let the settings rows stretch across the whole pane instead of capping their width.",
            &mut full,
        ) {
            self.general_config.full_width = full;
            changed = true;
        }

        let mut dyn_prev = self.general_config.dynamic_preview;
        if settings_toggle_row(
            ui,
            "Dynamic Preview Size",
            "Auto-size the launch command preview to its content instead of a fixed height.",
            &mut dyn_prev,
        ) {
            self.general_config.dynamic_preview = dyn_prev;
            changed = true;
        }

        {
            let prev = self.general_config.inheritance_display;
            settings_value_row(ui, "Inheritance Display Mode", "How profile chain depth is shown in the nav and module trees.", |ui| {
                egui::ComboBox::from_id_salt("inheritance_display_mode")
                    .selected_text(match self.general_config.inheritance_display {
                        InheritanceDisplayMode::Color => "Color",
                        InheritanceDisplayMode::Numbers => "Numbers",
                        InheritanceDisplayMode::ArrowsOnly => "Arrows Only",
                    })
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for (label, val) in [
                            ("Color", InheritanceDisplayMode::Color),
                            ("Numbers", InheritanceDisplayMode::Numbers),
                            ("Arrows Only", InheritanceDisplayMode::ArrowsOnly),
                        ] {
                            if ui.selectable_label(self.general_config.inheritance_display == val, label).clicked() {
                                self.general_config.inheritance_display = val;
                            }
                        }
                    });
            });
            if self.general_config.inheritance_display != prev { changed = true; }
        }

        let mut show_fi = self.general_config.show_field_inheritance;
        if settings_toggle_row(
            ui,
            "Show Field Inheritance",
            "Tint or label inherited fields in the module settings panel by chain depth.",
            &mut show_fi,
        ) {
            self.general_config.show_field_inheritance = show_fi;
            changed = true;
        }

        // Reload: [Reload Extensions] [Reload Configs]
        settings_value_row(ui, "Reload Actions", "", |ui| {
            if ui
                .add(theme::secondary_button("Reload Configs"))
                .on_hover_text("Re-read the current configuration from disk (part of Ctrl+R).")
                .clicked()
            {
                self.reload_configs();
            }
            if ui
                .add(theme::secondary_button("Reload Extensions"))
                .on_hover_text("Reload all modules from disk into this editor (part of Ctrl+R).")
                .clicked()
            {
                self.reload_extensions();
            }
        });

        // Maintenance: [Re-Export Modules and Plugins] [Clean Up Configs] — these
        // overwrite/strip files, so they use the red (destructive) button style.
        // right_to_left places the first-added button rightmost, so add in
        // reverse of the desired reading order.
        settings_value_row(ui, "Maintenance Actions", "", |ui| {
            if ui
                .add(theme::danger_button("Clean Up Configs"))
                .on_hover_text(
                    "Remove stored values for variables that no longer exist in any module \
                     (e.g. after an extension update).",
                )
                .clicked()
            {
                self.confirm = Some(ConfirmAction::ConfigCleanup);
            }
            if ui
                .add(theme::danger_button("Re-Export Modules and Plugins"))
                .on_hover_text(
                    "Restore the bundled modules and plugins, overwriting the files on disk.",
                )
                .clicked()
            {
                self.confirm = Some(ConfirmAction::ReExportResources);
            }
        });
        }); // end width-capped column
        }); // end scroll area

        if changed {
            let _ = self.paths.save_general(&self.general_config);
        }
        changed
    }

    /// Repo link + project credits, shown in the bottom box on General Settings.
    fn render_about(&self, ui: &mut egui::Ui) {
        ui.hyperlink_to(
            egui::RichText::new("\u{f09b}  ritz-game-launcher on GitHub")
                .color(theme::ACCENT)
                .size(15.0),
            "https://github.com/Ritze03/ritz-game-launcher",
        );
        ui.add_space(10.0);
        ui.label(theme::header_label("Credits"));
        ui.add_space(2.0);
        // Tighter rows for the credit list.
        ui.spacing_mut().item_spacing.y = 1.0;
        let credit = |ui: &mut egui::Ui, name: &str, url: &str, what: &str| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                ui.hyperlink_to(
                    egui::RichText::new(format!("\u{f09b}  {name} on GitHub"))
                        .color(theme::ACCENT)
                        .size(12.5),
                    url,
                );
                ui.label(egui::RichText::new(what).color(theme::FAINT).size(12.5));
            });
        };
        credit(ui, "geist-font", "https://github.com/vercel/geist-font", "— the UI font");
        credit(ui, "nerd-fonts", "https://github.com/ryanoasis/nerd-fonts", "— the icons");
        credit(ui, "hyprvibr", "https://github.com/devcexx/hyprvibr", "— their work");
    }

    fn refresh_presets(&mut self) {
        self.all_presets = self.paths.list_presets();
        self.preset_pins = self
            .all_presets
            .iter()
            .filter_map(|name| {
                self.paths
                    .load_preset(name)
                    .ok()
                    .flatten()
                    .and_then(|p| p.pin.map(|id| (name.clone(), id)))
            })
            .collect();
    }

    /// Set (or clear) the pin slot of the currently-edited profile, persist, and
    /// refresh the pin cache.
    fn set_parent_preset(&mut self, name: &str, parent: Option<String>) {
        if let Some(buf) = &mut self.editing_preset_buf {
            if buf.name == name {
                buf.parent = parent;
                let _ = self.paths.save_preset(buf);
            }
        }
    }

    fn set_current_preset_pin(&mut self, pin: Option<u8>) {
        if let Some(p) = &mut self.editing_preset_buf {
            p.pin = pin;
        }
        self.persist();
        self.refresh_presets();
    }

    /// Assign slot `new_id` to the currently-edited profile `name`. If another
    /// profile already holds `new_id`, the two swap slots.
    fn assign_pin(&mut self, name: &str, new_id: u8) {
        let old = self.editing_preset_buf.as_ref().and_then(|p| p.pin);
        let holder = self
            .preset_pins
            .iter()
            .find(|(n, id)| **id == new_id && n.as_str() != name)
            .map(|(n, _)| n.clone());
        if let Some(other) = holder {
            self.write_preset_pin(&other, old);
        }
        if let Some(p) = &mut self.editing_preset_buf {
            p.pin = Some(new_id);
        }
        self.persist();
        self.refresh_presets();
    }

    /// Set a preset file's pin slot directly (used for the swap partner).
    fn write_preset_pin(&self, name: &str, pin: Option<u8>) {
        if let Ok(Some(mut p)) = self.paths.load_preset(name) {
            p.pin = pin;
            let _ = self.paths.save_preset(&p);
        }
    }

    fn refresh_games(&mut self) {
        let mut games = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.paths.games_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(s) = std::fs::read_to_string(&path) {
                    if let Ok(gc) = serde_json::from_str::<GameConfig>(&s) {
                        games.push((gc.game.appid, gc.game.name));
                    }
                }
            }
        }
        games.sort();
        self.games = games;
    }

    fn rename_preset(&mut self, old_name: &str, new_name: &str) {
        if old_name == new_name || new_name.is_empty() {
            return;
        }
        if let Ok(Some(mut p)) = self.paths.load_preset(old_name) {
            p.name = new_name.to_string();
            let _ = self.paths.save_preset(&p);
            let _ = self.paths.delete_preset(old_name);
        }
        // Update any saved game config that references the old preset name.
        let game_files: Vec<_> = self.games.iter().map(|(id, _)| id.clone()).collect();
        for appid in game_files {
            if let Ok(Some(mut gc)) = self.paths.load_game(&appid) {
                if gc.config.modules.preset.as_deref() == Some(old_name) {
                    gc.config.modules.preset = Some(new_name.to_string());
                    let _ = self.paths.save_game(&gc);
                    if self.appid == appid {
                        self.game_config.config.modules.preset = Some(new_name.to_string());
                    }
                }
            }
        }
        if self.general_config.default_preset.as_deref() == Some(old_name) {
            self.general_config.default_preset = Some(new_name.to_string());
            let _ = self.paths.save_general(&self.general_config);
            self.default_preset = Some(new_name.to_string());
        }
        self.refresh_presets();
        self.editing_preset_buf = self.paths.load_preset(new_name).ok().flatten();
        self.nav_sel = NavSel::Profile(new_name.to_string());
        self.nav_name_buf = new_name.to_string();
    }
}

// ---- Navigator panel -------------------------------------------------------

impl GuiApp {
    fn render_nav_panel(&mut self, ui: &mut egui::Ui) {
        // Deferred intents collected inside the panel closures, applied after them.
        // *Why deferred:* every closure below already holds `&mut self`, so the
        // handlers (which also need `&mut self`) can't run inline. Same pattern as
        // the `open_create` bool in `ui()`'s `ext_list` block.
        let mut nav_action: Option<NavCategory> = None;
        let mut ide_open: Option<String> = None;
        let mut open_create = false;
        let mode = self.mode;

        // Always show the bottom band (it's empty for General Settings/Global Profile).
        egui::TopBottomPanel::bottom("nav_settings")
            .exact_height(198.0)
            .show_separator_line(true)
            .frame(egui::Frame::none()
                .fill(theme::PANEL2)
                .inner_margin(egui::Margin::same(14.0)))
            .show_inside(ui, |ui| {
                match mode {
                    Mode::Config => self.render_nav_settings(ui),
                    Mode::Ide => open_create = self.render_ide_nav_footer(ui),
                }
            });
        egui::TopBottomPanel::top("nav_header")
            .show_separator_line(false)
            .frame(egui::Frame::none()
                .fill(theme::PANEL)
                .inner_margin(egui::Margin { left: 16.0, right: 16.0, top: 14.0, bottom: 8.0 }))
            .show_inside(ui, |ui| {
                nav_action = self.render_nav_category_box(ui);
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::none()
                .fill(theme::PANEL)
                .inner_margin(egui::Margin { left: 8.0, right: 18.0, top: 0.0, bottom: 0.0 }))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical()
                    .drag_to_scroll(self.general_config.touch_mode)
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        match mode {
                            // General Settings owns the whole nav body: the list
                            // below the GENERAL box is EMPTY. *Why:* the tree
                            // lists config *scopes* (Global Profile / Profiles /
                            // Games) and General Settings is not one — it edits
                            // app-wide preferences. Drawing the scope tree under
                            // it would offer a selection that the panel on the
                            // right isn't showing.
                            Mode::Config
                                if matches!(self.nav_sel, NavSel::GeneralSettings) => {}
                            Mode::Config => {
                                // The left nav retargets the editor's live preview, so it
                                // stays usable with a *clean* draft. Once the draft is
                                // dirty it locks: navigating away drops the draft, and a
                                // stray click must not silently discard unsaved edits.
                                let nav_live =
                                    !self.module_draft.as_ref().map_or(false, |d| d.dirty());
                                ui.add_enabled_ui(nav_live, |ui| {
                                    self.render_nav_tree(ui);
                                });
                            }
                            Mode::Ide => ide_open = self.render_ide_module_tree(ui),
                        }
                    });
            });

        if let Some(cat) = nav_action {
            self.select_nav_category(cat);
        }
        if let Some(id) = ide_open {
            self.open_module_editor(id);
        }
        if open_create {
            self.open_create_dialog();
        }
    }

    /// The bordered category **tab bar** that sits above the nav tree: the three
    /// top-level destinations as three buttons side by side. Replaces the old
    /// single `header_label("Profiles / Games")` — the destinations are now named
    /// on screen, so what the tree below is showing is never a guess.
    ///
    /// *Why a horizontal tab strip and not the earlier stacked rows:* three
    /// full-width rows plus an uppercase `GENERAL` caption spent four lines of
    /// vertical space on navigation that the module tree needs, and read as a
    /// list of *items* rather than as a mode switch. One row of tabs is the
    /// conventional shape for "pick one of N views" and costs one line. The
    /// caption is gone with it (the tabs label themselves); the border stays,
    /// which is what still groups the three as one control.
    ///
    /// Border idiom copied from [`editor_card`] rather than reusing it:
    /// `editor_card` is specialised for the manifest editor (it takes an
    /// `IconCenterCache` and returns a `RowAction`), neither of which applies here.
    fn render_nav_category_box(&mut self, ui: &mut egui::Ui) -> Option<NavCategory> {
        let mut action = None;
        // Selection is a function of BOTH axes: `mode` picks IDE vs. the two config
        // destinations, `nav_sel` disambiguates those two.
        // Both flags are derived from live state every frame, never cached — so
        // ANY code path that moves `mode`/`nav_sel` (Close, the IDE tree, the exit
        // interlock in `ui()`) is followed by the box automatically. There is no
        // second copy of "which category is selected" that could drift.
        let is_ide = self.mode == Mode::Ide;
        let is_general = !is_ide && matches!(self.nav_sel, NavSel::GeneralSettings);
        let is_games = !is_ide && !is_general;
        let mono = self.general_config.mono_ui;
        egui::Frame::none()
            .fill(theme::PANEL2)
            .stroke(egui::Stroke::new(1.0, theme::BORDER))
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::symmetric(8.0, 8.0))
            .show(ui, |ui| {
                let avail = ui.available_width();
                ui.set_width(avail);
                // Equal thirds of whatever the frame leaves, minus the two gutters
                // between them — computed, not a constant, so the tabs stay equal
                // if the nav width or the frame margins ever move.
                let tab_w = ((avail - 2.0 * TAB_GAP) / 3.0).max(0.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = TAB_GAP;
                    // Order: Profiles · IDE Mode · Settings. Profiles first because
                    // it's the app's default/startup destination (see
                    // `GuiApp::new`'s `nav_sel` init) — landing on the leftmost tab
                    // matches "first tab is the default" convention even though the
                    // default is actually chosen by name, not position.
                    if nav_category_tab(ui, tab_w, is_games, "\u{f4ff}", "Profiles", mono).clicked() {
                        action = Some(NavCategory::GamesProfiles);
                    }
                    // `\u{f044}` is `theme::ICON_EDIT`, the same pencil the module
                    // detail header uses for its Edit affordance. Reused on purpose:
                    // both mean "author this thing". It replaced `\u{eef4}`, whose
                    // ink is too fine to read at 11px.
                    if nav_category_tab(ui, tab_w, is_ide, theme::ICON_EDIT, "IDE Mode", mono).clicked() {
                        action = Some(NavCategory::Ide);
                    }
                    // Cog reused verbatim from the old General Settings tree row.
                    if nav_category_tab(ui, tab_w, is_general, "\u{f013}", "Settings", mono).clicked() {
                        action = Some(NavCategory::GeneralSettings);
                    }
                });
            });
        ui.add_space(2.0);
        action
    }

    /// Apply a click on the GENERAL category box.
    fn select_nav_category(&mut self, cat: NavCategory) {
        match cat {
            NavCategory::GeneralSettings => {
                self.mode = Mode::Config;
                self.nav_sel = NavSel::GeneralSettings;
            }
            NavCategory::GamesProfiles => {
                self.mode = Mode::Config;
                // Coming back from IDE mode `nav_sel` is still `ModuleEditor(_)`,
                // which is not a config destination. `exit_module_editor` restores
                // exactly what Close would (the remembered view, else the ambient
                // game) and drops the draft — S3b is still single-draft, so leaving
                // IDE mode discards it; S4 adds the combined "unsaved changes" notice.
                if matches!(self.nav_sel, NavSel::ModuleEditor(_)) {
                    self.exit_module_editor();
                }
                // Coming back from General Settings, `nav_sel` is still
                // `GeneralSettings` — which is the box's *other* category. Left
                // alone, the box would snap straight back to "General Settings"
                // (`is_general` is derived from `nav_sel`) and the tree would
                // stay empty, making the click look dead. Land on the ambient
                // game, the same destination `editor_exit_target` defaults to.
                if matches!(self.nav_sel, NavSel::GeneralSettings) {
                    self.nav_sel = NavSel::Game(self.appid.clone());
                    self.nav_name_buf = self.game_config.game.name.clone();
                }
            }
            NavCategory::Ide => {
                self.mode = Mode::Ide;
                // IDE mode is only coherent with a module open — `nav_sel ==
                // ModuleEditor(_)` is the invariant every IDE column relies on.
                if let Some(spec) = self.all_specs.get(self.ide_selected) {
                    self.open_module_editor(spec.id());
                }
            }
        }
    }

    /// The IDE column's module tree: the load-error banner, then the whole
    /// (unfiltered) module set. Returns the id to open when the selection moved.
    ///
    /// *Why no `add_enabled_ui` gate here, unlike the Config-mode nav:* in IDE mode
    /// this tree IS the primary navigation. Locking it while a draft is dirty —
    /// which is what Config mode does — would mean you can never leave the module
    /// you just typed into. The plan flags this exact shape as a dead end; per-row
    /// dirty markers replace locking in S4.
    fn render_ide_module_tree(&mut self, ui: &mut egui::Ui) -> Option<String> {
        // Rehomed from the `ext_list` block, which IDE mode drops entirely. Losing
        // it in an *authoring* mode would be the worst outcome of this restructure:
        // you'd write broken JSON and get silence.
        self.render_ext_errors_banner(ui);

        // Clone out of `self` first — `render_ext_tree` takes `&mut self`, so it
        // can't also borrow `self.all_specs` / `&mut self.ide_selected`.
        let specs = self.all_specs.clone();
        let dirs = self.all_dirs.clone();
        let is_folder_ext = self.all_is_folder_ext.clone();
        let before = self.ide_selected.min(specs.len().saturating_sub(1));
        let mut selected = before;
        // `show_inheritance = false`: IDE mode edits manifests, not config scopes,
        // so inheritance/edit badges have no scope to describe.
        self.render_ext_tree(ui, &specs, &dirs, &is_folder_ext, &mut selected, false);
        self.ide_selected = selected;
        // `leaf` writes `selected` directly and never calls `open_module_editor`, so
        // the tree click has to be turned into an editor open here, after the render.
        if selected != before {
            specs.get(selected).map(|s| s.id())
        } else {
            None
        }
    }

    /// IDE-mode replacement for [`render_nav_settings`] in the nav's bottom band.
    /// Returns true when "New Module" was clicked.
    ///
    /// Deliberately omits two controls the Config-mode module footer has:
    /// **Show Inheritance** (no scope in IDE mode, so the badges it toggles are
    /// meaningless) and **Clear Settings** (it clears stored *config*; IDE Mode
    /// edits manifests and must never touch config).
    fn render_ide_nav_footer(&mut self, ui: &mut egui::Ui) -> bool {
        let mut open_create = false;
        styled_checkbox(ui, &mut self.group_by_author, "Group by Author");
        let sep = icon_sep(self.general_config.mono_ui);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            // bottom_up: first added sits at the bottom, so Open Folder ends up
            // below New Module.
            let w = ui.available_width();
            if ui
                .add_sized([w, 30.0], theme::secondary_button(format!("\u{f07b}{sep}Open Folder")))
                .on_hover_text("Open the user extensions folder")
                .clicked()
            {
                // `user_extensions()`, **not** `games_dir()` — IDE Mode edits module
                // *manifests*, which live in `~/.config/ritz/extensions/`. The
                // Config-mode module footer's identical-looking button still opens
                // `games_dir()`; that is a pre-existing target bug on a different
                // screen and is deliberately left alone here, so the two buttons
                // now open different folders on purpose.
                let _ = Command::new("xdg-open").arg(self.paths.user_extensions()).spawn();
            }
            if ui
                .add_sized([w, 30.0], theme::primary_button(format!("\u{f067}{sep}New Module")))
                .on_hover_text("Create a new custom module")
                .clicked()
            {
                open_create = true;
            }
        });
        open_create
    }

    fn render_nav_tree(&mut self, ui: &mut egui::Ui) {
        // Collect any nav action to apply after rendering (avoids borrow conflicts).
        enum NavAction {
            SelectGlobal,
            SelectProfile(String),
            SelectGame(String, String), // appid, name
            StartCreateProfile,
            StartCreateGame,
        }
        let mut action: Option<NavAction> = None;

        let sep = icon_sep(self.general_config.mono_ui);
        let gap = if self.general_config.mono_ui { 8.0 } else { 7.0 };
        // No General Settings row here any more — it lives *only* in the GENERAL
        // category box above the tree (`render_nav_category_box`).
        // *Why removed:* with the box present the row was a second, competing
        // control for the same destination — selecting it left the box showing
        // "Games / Profiles" while the panel showed General Settings, and picking
        // any profile/game from the tree silently flipped the box's category. One
        // destination, one control.
        let is_global = self.nav_sel == NavSel::GlobalSettings;
        if full_selectable(ui, is_global, egui::RichText::new(format!("\u{f0ac}{sep}Global Profile")).color(COL_GLOBAL)).clicked() {
            action = Some(NavAction::SelectGlobal);
        }
        ui.add_space(2.0);

        // Profiles — highlight the assigned profile only when a Game is selected.
        let active_preset = match &self.nav_sel {
            NavSel::Game(_) => self.game_config.config.modules.preset.clone(),
            _ => None,
        };
        // Collect the parent chain as depth → arrow count.
        // Profile selected: direct parent=1, grandparent=2, …
        // Game selected:    active profile=1 (via is_active), its parent=2, …
        let profile_parents: std::collections::HashMap<String, usize> = match &self.nav_sel {
            NavSel::Profile(_) => {
                let mut depths = std::collections::HashMap::new();
                let mut cur = self.editing_preset_buf.as_ref().and_then(|p| p.parent.clone());
                let mut seen = std::collections::HashSet::new();
                let mut depth = 1usize;
                while let Some(pname) = cur {
                    if !seen.insert(pname.clone()) { break; }
                    depths.insert(pname.clone(), depth);
                    cur = self.paths.load_preset(&pname).ok().flatten().and_then(|p| p.parent);
                    depth += 1;
                }
                depths
            }
            NavSel::Game(_) => {
                let mut depths = std::collections::HashMap::new();
                if let Some(ref preset_name) = active_preset {
                    let mut cur = self.paths.load_preset(preset_name).ok().flatten().and_then(|p| p.parent);
                    let mut seen = std::collections::HashSet::new();
                    seen.insert(preset_name.clone());
                    let mut depth = 2usize;
                    while let Some(pname) = cur {
                        if !seen.insert(pname.clone()) { break; }
                        depths.insert(pname.clone(), depth);
                        cur = self.paths.load_preset(&pname).ok().flatten().and_then(|p| p.parent);
                        depth += 1;
                    }
                }
                depths
            }
            _ => std::collections::HashMap::new(),
        };
        // Ordered: pinned profiles (by slot id, with " [id]" suffix) first, then
        // the rest in list order.
        let presets = self.all_presets.clone();
        let mut pinned: Vec<(u8, String)> = presets
            .iter()
            .filter_map(|n| self.preset_pins.get(n).map(|id| (*id, n.clone())))
            .collect();
        pinned.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        // (name, display) — pinned show "<id> - <name>" and come first.
        // (name, pin id) — pinned first, shown as "[id] name" with a gray id.
        let ordered: Vec<(String, Option<u8>)> = pinned
            .iter()
            .map(|(id, n)| (n.clone(), Some(*id)))
            .chain(
                presets
                    .iter()
                    .filter(|n| !self.preset_pins.contains_key(*n))
                    .map(|n| (n.clone(), None)),
            )
            .collect();
        let id_col = egui::Color32::from_rgb(0x59, 0x5E, 0x66);
        egui::CollapsingHeader::new(egui::RichText::new("Profiles").color(COL_PROFILE))
            .default_open(true)
            .show(ui, |ui| {
                // Total number of profiles that carry an indicator in this context.
                // Used by Numbers mode to decide arrow vs. numeric labels.
                let chain_total: usize =
                    (if active_preset.is_some() { 1 } else { 0 }) + profile_parents.len();
                for (name, pin) in &ordered {
                    let is_sel = self.nav_sel == NavSel::Profile(name.clone());
                    let is_active = active_preset.as_deref() == Some(name.as_str());
                    let parent_depth = profile_parents.get(name.as_str()).copied();
                    let show_indicator = is_active || parent_depth.is_some();
                    // color_depth: 0 = full COL_PROFILE, +1 per step deeper in the chain.
                    // profile_parents stores 1-based depth; subtract 1 to get color index.
                    let color_depth = parent_depth.map(|d| d - 1).unwrap_or(0);
                    let label: egui::WidgetText = if show_indicator {
                        let font_id = ui.style().text_styles[&egui::TextStyle::Body].clone();
                        let mut job = LayoutJob::default();
                        match self.general_config.inheritance_display {
                            InheritanceDisplayMode::Color => {
                                job.append(ICON_INHERIT, 0.0, TextFormat { font_id: font_id.clone(), color: profile_depth_color(color_depth), ..Default::default() });
                            }
                            InheritanceDisplayMode::Numbers => {
                                const NUMS: [&str; 6] = ["1", "2", "3", "4", "5", "5+"];
                                if chain_total == 1 {
                                    // Single profile in chain → plain arrow, no number needed.
                                    job.append(ICON_INHERIT, 0.0, TextFormat { font_id: font_id.clone(), color: COL_PROFILE, ..Default::default() });
                                } else {
                                    // Depth number: is_active = 1, chain entry at stored depth d = d.
                                    let number = parent_depth.unwrap_or(1);
                                    job.append(NUMS[(number - 1).min(5)], 0.0, TextFormat { font_id: font_id.clone(), color: COL_PROFILE, ..Default::default() });
                                }
                            }
                            InheritanceDisplayMode::ArrowsOnly => {
                                // N plain arrows: is_active = 1, chain entry at depth d = d arrows.
                                let arrow_count = parent_depth.unwrap_or(1);
                                for i in 0..arrow_count {
                                    if i > 0 {
                                        job.append(" ", 0.0, TextFormat { font_id: font_id.clone(), color: theme::TEXT, ..Default::default() });
                                    }
                                    job.append(ICON_INHERIT, 0.0, TextFormat { font_id: font_id.clone(), color: COL_PROFILE, ..Default::default() });
                                }
                            }
                        }
                        job.append(name, gap, TextFormat { font_id, color: theme::TEXT, ..Default::default() });
                        job.into()
                    } else {
                        name.as_str().into()
                    };
                    let resp = full_selectable(ui, is_sel, label);
                    if resp.clicked() {
                        action = Some(NavAction::SelectProfile(name.clone()));
                    }
                    // Right-bound pin id.
                    if let Some(id) = pin {
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        let r = resp.rect;
                        ui.painter().text(
                            egui::pos2(r.right() - 8.0, r.center().y),
                            egui::Align2::RIGHT_CENTER,
                            format!("[{id}]"),
                            font,
                            id_col,
                        );
                    }
                }
                if full_selectable(ui, false, egui::RichText::new(format!("\u{2b}{sep}Add profile")).color(theme::FAINT)).clicked() {
                    action = Some(NavAction::StartCreateProfile);
                }
            });
        ui.add_space(2.0);

        // Games
        let games = self.games.clone();
        egui::CollapsingHeader::new(egui::RichText::new("Games").color(COL_GAME))
            .default_open(true)
            .show(ui, |ui| {
                for (appid, name) in &games {
                    let is_sel = self.nav_sel == NavSel::Game(appid.clone());
                    if full_selectable(ui, is_sel, name.as_str()).clicked() && !is_sel {
                        action = Some(NavAction::SelectGame(appid.clone(), name.clone()));
                    }
                }
                if full_selectable(ui, false, egui::RichText::new(format!("\u{2b}{sep}Add game")).color(theme::FAINT)).clicked() {
                    action = Some(NavAction::StartCreateGame);
                }
            });

        // Apply action
        match action {
            None => {}
            Some(NavAction::SelectGlobal) => {
                self.nav_sel = NavSel::GlobalSettings;
                self.creating_profile = false;
                self.duplicating_preset = None;
                self.creating_game = false;
                self.text_buffers.clear();
            }
            Some(NavAction::SelectProfile(name)) => {
                self.editing_preset_buf = self.paths.load_preset(&name).ok().flatten();
                self.nav_name_buf = name.clone();
                self.nav_sel = NavSel::Profile(name);
                self.creating_profile = false;
                self.duplicating_preset = None;
                self.creating_game = false;
                self.text_buffers.clear();
            }
            Some(NavAction::SelectGame(appid, name)) => {
                self.switch_game(&appid, &name);
                self.nav_sel = NavSel::Game(appid);
                self.nav_name_buf = self.game_config.game.name.clone();
                self.editing_preset_buf = None;
                self.creating_profile = false;
                self.duplicating_preset = None;
                self.creating_game = false;
            }
            Some(NavAction::StartCreateProfile) => {
                self.creating_profile = true;
                self.creating_game = false;
                self.nav_name_buf = String::new();
                self.focus_nav_name = true;
            }
            Some(NavAction::StartCreateGame) => {
                self.creating_game = true;
                self.creating_profile = false;
                self.nav_name_buf = String::new();
                self.nav_appid_buf = String::new();
                self.focus_nav_name = true;
            }
        }
    }

    /// Reload the nav-panel buffers for the currently-selected entry (used after
    /// cancelling a create, when the buffers were cleared for the new entry).
    fn reselect_current(&mut self) {
        match self.nav_sel.clone() {
            NavSel::Profile(name) => {
                self.editing_preset_buf = self.paths.load_preset(&name).ok().flatten();
                self.nav_name_buf = name;
            }
            NavSel::Game(appid) => {
                self.nav_name_buf = self.game_config.game.name.clone();
                self.nav_appid_buf = appid;
            }
            NavSel::GeneralSettings | NavSel::GlobalSettings | NavSel::ModuleEditor(_) => {}
        }
    }

    fn render_nav_settings(&mut self, ui: &mut egui::Ui) {
        if self.creating_profile {
            let label = if self.duplicating_preset.is_some() { "Duplicate profile name:" } else { "New profile name:" };
            ui.label(label);
            let resp = ui.text_edit_singleline(&mut self.nav_name_buf);
            if self.focus_nav_name {
                resp.request_focus();
                self.focus_nav_name = false;
            }
            ui.horizontal(|ui| {
                let name = self.nav_name_buf.trim().to_string();
                if ui.button("Create").clicked() && !name.is_empty() {
                    let preset = if let Some(src) = self.duplicating_preset.take() {
                        Preset { name: name.clone(), modules: src.modules, pin: None, parent: None }
                    } else {
                        Preset { name: name.clone(), ..Default::default() }
                    };
                    let _ = self.paths.save_preset(&preset);
                    self.refresh_presets();
                    self.editing_preset_buf = Some(preset);
                    self.nav_sel = NavSel::Profile(name.clone());
                    self.nav_name_buf = name;
                    self.creating_profile = false;
                    self.text_buffers.clear();
                }
                if ui.button("Cancel").clicked() {
                    self.duplicating_preset = None;
                    self.creating_profile = false;
                    self.reselect_current();
                }
            });
            return;
        }

        if self.creating_game {
            ui.label("Game name:");
            let resp = ui.text_edit_singleline(&mut self.nav_name_buf);
            if self.focus_nav_name {
                resp.request_focus();
                self.focus_nav_name = false;
            }
            ui.label("AppID:");
            ui.text_edit_singleline(&mut self.nav_appid_buf);
            ui.horizontal(|ui| {
                let name = self.nav_name_buf.trim().to_string();
                let appid = self.nav_appid_buf.trim().to_string();
                if ui.button("Create").clicked() && !name.is_empty() && !appid.is_empty() {
                    let mut gc = GameConfig::new(appid.clone(), name.clone());
                    // Apply the configured default profile to the new game.
                    gc.config.modules.preset = self.general_config.default_preset.clone();
                    let _ = self.paths.save_game(&gc);
                    self.refresh_games();
                    self.switch_game(&appid, &name);
                    self.nav_sel = NavSel::Game(appid);
                    self.nav_name_buf = self.game_config.game.name.clone();
                    self.creating_game = false;
                }
                if ui.button("Cancel").clicked() {
                    self.creating_game = false;
                    self.reselect_current();
                }
            });
            return;
        }

        match self.nav_sel.clone() {
            NavSel::GeneralSettings | NavSel::GlobalSettings | NavSel::ModuleEditor(_) => {
                // Settings for these live in the central panel.
            }

            NavSel::Profile(name) => {
                ui.horizontal(|ui| {
                    footer_label(ui, "Name");
                    let w = ui.available_width();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.nav_name_buf).desired_width(w),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let new_name = self.nav_name_buf.trim().to_string();
                        if !new_name.is_empty() && new_name != name {
                            self.rename_preset(&name, &new_name);
                        }
                    }
                });

                // Pin to top (up to 10), with a 1–10 slot id.
                let cur_pin = self.editing_preset_buf.as_ref().and_then(|p| p.pin);
                ui.horizontal(|ui| {
                    footer_label(ui, "Pin");
                    let mut pinned = cur_pin.is_some();
                    if styled_checkbox(ui, &mut pinned, "").clicked() {
                        if pinned {
                            let used: std::collections::HashSet<u8> =
                                self.preset_pins.values().copied().collect();
                            if let Some(id) = (1..=10).find(|id| !used.contains(id)) {
                                self.set_current_preset_pin(Some(id));
                            }
                        } else {
                            self.set_current_preset_pin(None);
                        }
                    }
                    if let Some(cur) = cur_pin {
                        ui.add_space(8.0);
                        // All slots selectable; picking a taken one swaps IDs.
                        let mut new_id: Option<u8> = None;
                        egui::ComboBox::from_id_salt("profile_pin_id")
                            .selected_text(cur.to_string())
                            .width(52.0)
                            .show_ui(ui, |ui| {
                                for id in 1..=10u8 {
                                    if ui.selectable_label(id == cur, id.to_string()).clicked() {
                                        new_id = Some(id);
                                    }
                                }
                            });
                        if let Some(id) = new_id {
                            if id != cur {
                                self.assign_pin(&name, id);
                            }
                        }
                    }
                });

                // Parent profile — any profile except self; cycles allowed but shown in red.
                let cur_parent = self.editing_preset_buf.as_ref().and_then(|p| p.parent.clone());
                let sep = icon_sep(self.general_config.mono_ui);
                ui.horizontal(|ui| {
                    footer_label(ui, "Parent");
                    let display = cur_parent.as_deref().unwrap_or("None");
                    egui::ComboBox::from_id_salt("profile_parent_id")
                        .selected_text(display)
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            let is_none = cur_parent.is_none();
                            if ui.selectable_label(is_none, "None").clicked() && !is_none {
                                self.set_parent_preset(&name, None);
                            }
                            for pname in self.all_presets.clone() {
                                if pname == name { continue; }
                                let selected = cur_parent.as_deref() == Some(&pname);
                                let is_cyclic = chain_would_have_cycle(&self.paths, &name, &pname);
                                if is_cyclic {
                                    let row_h = ui.spacing().interact_size.y;
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width(), row_h),
                                        egui::Sense::hover(),
                                    );
                                    if ui.is_rect_visible(rect) {
                                        let c = theme::COL_GLOBAL;
                                        let fill = egui::Color32::from_rgb(0x3f, 0x21, 0x25);
                                        ui.painter().rect(rect, ui.visuals().widgets.inactive.rounding, fill, egui::Stroke::new(1.0, c));
                                        ui.painter().text(
                                            rect.left_center() + egui::vec2(ui.spacing().button_padding.x, 0.0),
                                            egui::Align2::LEFT_CENTER,
                                            format!("\u{ea6c}{sep}{pname}"),
                                            egui::TextStyle::Body.resolve(ui.style()),
                                            c,
                                        );
                                    }
                                } else if ui.selectable_label(selected, pname.as_str()).clicked() && !selected {
                                    self.set_parent_preset(&name, Some(pname));
                                }
                            }
                        });
                });

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    if ui
                        .add_sized([ui.available_width(), 30.0], theme::danger_button("Delete Profile"))
                        .clicked()
                    {
                        self.confirm = Some(ConfirmAction::DeleteProfile(name.clone()));
                    }
                    if ui
                        .add_sized([ui.available_width(), 30.0], egui::Button::new("Duplicate Profile"))
                        .clicked()
                    {
                        self.duplicating_preset = self.paths.load_preset(&name).ok().flatten();
                        self.creating_profile = true;
                        self.nav_name_buf = format!("{} Copy", name);
                        self.focus_nav_name = true;
                    }
                });
            }

            NavSel::Game(appid) => {
                // AppID — editable; pressing Enter renames the games/<appid>.json file.
                ui.horizontal(|ui| {
                    footer_label(ui, "AppID");
                    let w = ui.available_width();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.nav_appid_buf).desired_width(w),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let new_appid = self.nav_appid_buf.trim().to_string();
                        self.rename_game_appid(&appid, &new_appid);
                    }
                });
                // Name — Enter commits.
                ui.horizontal(|ui| {
                    footer_label(ui, "Name");
                    let w = ui.available_width();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.nav_name_buf).desired_width(w),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let n = self.nav_name_buf.trim().to_string();
                        if !n.is_empty() && n != self.game_config.game.name {
                            self.game_config.game.name = n;
                            self.persist();
                            self.refresh_games();
                        }
                    }
                });

                let presets = self.all_presets.clone();
                let cur = self.game_config.config.modules.preset.clone().unwrap_or_default();
                let cur_label = if cur.is_empty() { "None".to_string() } else { cur.clone() };
                ui.horizontal(|ui| {
                    footer_label(ui, "Profile");
                    let w = ui.available_width();
                    let mut new_preset: Option<Option<String>> = None;
                    egui::ComboBox::from_id_salt("game_preset_sel")
                        .width(w)
                        .selected_text(cur_label)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(cur.is_empty(), "None").clicked() {
                                new_preset = Some(None);
                            }
                            for p in &presets {
                                if ui.selectable_label(cur == *p, p).clicked() {
                                    new_preset = Some(Some(p.clone()));
                                }
                            }
                        });
                    if let Some(p) = new_preset {
                        self.game_config.config.modules.preset = p.clone();
                        self.preset = p
                            .as_deref()
                            .and_then(|n| self.paths.load_preset(n).ok().flatten());
                        self.persist();
                    }
                });

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    if ui
                        .add_sized([ui.available_width(), 30.0], theme::danger_button("Delete Game"))
                        .clicked()
                    {
                        self.confirm = Some(ConfirmAction::DeleteGame(appid.clone()));
                    }
                });
            }
        }
    }

    /// Rename a game's AppID: rewrite its config under the new filename, drop the
    /// old file, and re-point the UI. No-op on empty/unchanged/colliding ids.
    fn rename_game_appid(&mut self, old: &str, new: &str) {
        if new.is_empty() || new == old || self.games.iter().any(|(a, _)| a == new) {
            self.nav_appid_buf = old.to_string();
            return;
        }
        self.game_config.game.appid = new.to_string();
        let _ = self.paths.save_game(&self.game_config);
        let _ = self.paths.delete_game(old);
        let name = self.game_config.game.name.clone();
        self.refresh_games();
        self.switch_game(new, &name);
        self.nav_sel = NavSel::Game(new.to_string());
    }

    /// Delete a game and switch to the next available one (or a blank scratch).
    fn delete_game(&mut self, appid: &str) {
        let _ = self.paths.delete_game(appid);
        self.refresh_games();
        if let Some((new_appid, new_name)) = self.games.first().cloned() {
            self.switch_game(&new_appid, &new_name);
            self.nav_sel = NavSel::Game(new_appid);
            self.nav_name_buf = self.game_config.game.name.clone();
        } else {
            self.appid = "0".to_string();
            self.game_config = GameConfig::new("0", "Scratch");
            self.nav_sel = NavSel::GlobalSettings;
            self.nav_name_buf = String::new();
        }
    }

    /// Delete a profile and return to the Global Profile view (`NavSel::GlobalSettings`).
    /// Any game referencing it falls back to no profile.
    fn delete_profile(&mut self, name: &str) {
        let _ = self.paths.delete_preset(name);
        if self.game_config.config.modules.preset.as_deref() == Some(name) {
            self.game_config.config.modules.preset = None;
            self.preset = None;
            self.persist();
        }
        self.refresh_presets();
        self.nav_sel = NavSel::GlobalSettings;
    }

    /// Clear all variable overrides in the current edit scope (game / profile /
    /// global), keeping the game's assigned profile.
    fn clear_current_settings(&mut self) {
        match &self.nav_sel {
            NavSel::GlobalSettings => {
                self.global_config.modules.clear();
                let _ = self.paths.save_global_config(&self.global_config);
            }
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf {
                    p.modules.clear();
                    let _ = self.paths.save_preset(p);
                }
            }
            NavSel::Game(_) | NavSel::GeneralSettings | NavSel::ModuleEditor(_) => {
                self.game_config.config.modules.authors.clear();
                self.persist();
            }
        }
        self.text_buffers.clear();
    }
}

/// Convert a Preset into a fake GameConfig so it can be passed as the "game" layer
/// to `resolve::resolve`, making its values show as Provenance::Game in the GUI.
fn preset_as_fake_game(preset: &Preset) -> GameConfig {
    let mut gc = GameConfig::new("__stub__", &preset.name);
    gc.config.modules.authors = preset.modules.clone();
    gc
}

/// One level of the module tree: named subfolders plus extensions at this level.
#[derive(Default)]
struct TreeNode {
    children: BTreeMap<String, TreeNode>,
    leaves: Vec<usize>,
}

impl TreeNode {
    fn insert(&mut self, comps: &[String], idx: usize) {
        match comps.split_first() {
            None => self.leaves.push(idx),
            Some((head, rest)) => self
                .children
                .entry(head.clone())
                .or_default()
                .insert(rest, idx),
        }
    }
}

impl GuiApp {
    /// Warning banner atop the module tree when some manifests failed to load or
    /// collide. Lazily built (nothing rendered when there are no errors); the
    /// per-error `Display` lines live inside a collapsible so the tree stays
    /// uncluttered.
    fn render_ext_errors_banner(&self, ui: &mut egui::Ui) {
        if self.extension_errors.is_empty() {
            return;
        }
        // A `Dup` entry means the module DID load (it's a config-namespace
        // collision, not a parse failure) — count the two kinds separately so
        // the header doesn't claim duplicates "failed to load".
        let (failed, dup) = self.extension_errors.iter().fold((0usize, 0usize), |(f, d), e| {
            match e {
                ExtensionLoadError::Parse { .. } => (f + 1, d),
                ExtensionLoadError::Dup { .. } => (f, d + 1),
            }
        });
        let header_text = match (failed, dup) {
            (0, d) => format!("\u{26A0} {d} duplicate module(s)"),
            (f, 0) => format!("\u{26A0} {f} module(s) failed to load"),
            (f, d) => format!("\u{26A0} {f} failed to load, {d} duplicate(s)"),
        };
        let header = egui::RichText::new(header_text).color(theme::COL_GLOBAL).strong();
        egui::CollapsingHeader::new(header)
            .id_salt("ext_load_errors")
            .default_open(false)
            .show(ui, |ui| {
                for err in &self.extension_errors {
                    ui.label(
                        egui::RichText::new(err.to_string())
                            .color(theme::DIM)
                            .small(),
                    );
                }
            });
        ui.add_space(6.0);
    }

    /// Render the left-panel module tree. In folder mode it mirrors the nested
    /// `extensions/` layout; in author mode it groups by extension Author. In
    /// both modes the built-in (backend) modules are collected under a single
    /// "Built-In" node shown last.
    ///
    /// Takes its data source as explicit parameters (`specs`/`dirs`/
    /// `is_folder_ext`, index-aligned) rather than always reading `self.cur_*` —
    /// S3b will pass `all_specs`/`all_dirs`/`all_is_folder_ext` here for the
    /// unfiltered preview column; this stage's one call site still passes the
    /// `cur_*` fields, so behaviour is unchanged.
    ///
    /// `specs`/`dirs`/`is_folder_ext`/`selected` can't be `&self.cur_specs`/
    /// `&mut self.selected_ext` etc. at the call site — that would borrow `self`
    /// immutably (or mutably, for `selected`) while also needing `&mut self` for
    /// the method receiver. The call site clones the `cur_*` Vecs and copies
    /// `selected_ext` into locals first (the same "clone the spec" pattern already
    /// used elsewhere in this file for the same reason), passes those in, then
    /// writes `selected_ext` back after the call.
    fn render_ext_tree(
        &mut self,
        ui: &mut egui::Ui,
        specs: &[Extension],
        dirs: &[PathBuf],
        is_folder_ext: &[bool],
        selected: &mut usize,
        show_inheritance: bool,
    ) {
        // Build per-extension icon lists: each entry is (icon_glyph, color).
        // Inheritance from a lower layer → ICON_INHERIT; edit at current scope → ICON_EDIT.
        // Resolve against the list actually being rendered, NOT `cur_specs`: the IDE
        // column passes `all_specs`, and a spec missing from the resolution gets no
        // badges at all (see `resolve_specs_for_editing`).
        let resolution = self.resolve_specs_for_editing(specs);
        let mono = self.general_config.mono_ui;
        let nav_sel = &self.nav_sel;
        let global_modules = &self.global_config.modules;
        let editing_modules = self.editing_preset_buf.as_ref().map(|p| &p.modules);
        let game_modules = &self.game_config.config.modules.authors;
        // Individual (un-merged) parent presets for depth-shaded arrows.
        // Profile context: [direct_parent, grandparent, …]
        let profile_parent_chain: Vec<Preset> = if let NavSel::Profile(_) = &self.nav_sel {
            self.editing_preset_buf.as_ref()
                .and_then(|p| p.parent.as_ref())
                .map(|pname| collect_parent_presets(&self.paths, pname))
                .unwrap_or_default()
        } else { Vec::new() };
        // Game context: [direct_preset, its_parent, grandparent, …]
        let game_preset_chain: Vec<Preset> = if matches!(self.nav_sel, NavSel::Game(_) | NavSel::ModuleEditor(_)) {
            let mut chain = Vec::new();
            if let Some(p) = &self.preset {
                chain.push(p.clone());
                if let Some(pname) = &p.parent {
                    chain.extend(collect_parent_presets(&self.paths, pname));
                }
            }
            chain
        } else { Vec::new() };

        let display_mode = self.general_config.inheritance_display;
        let icon_lists: Vec<Vec<(&'static str, Color32)>> = if !show_inheritance {
            vec![Vec::new(); specs.len()]
        } else { specs.iter().map(|spec| {
            // Variables the module still declares — a stored value for a removed
            // variable (e.g. an old `xkb_de`) must not count as "configured".
            let declared: std::collections::HashSet<&str> =
                spec.ui.values().flatten().map(|f| f.variable.as_str()).collect();
            // Fields whose Requires gate is currently closed don't contribute to
            // the launch command, so stored values behind them aren't "edited".
            let field_by_var: std::collections::HashMap<&str, &UiField> =
                spec.ui.values().flatten().map(|f| (f.variable.as_str(), f)).collect();
            let ext_res = resolution.exts.get(&spec.id());
            let has_in = |modules: &AuthorsMap| -> bool {
                modules.get(&spec.meta.author)
                    .and_then(|e| e.get(&spec.meta.name))
                    .map(|vars| vars.keys().any(|k| {
                        declared.contains(k.as_str())
                            && field_by_var.get(k.as_str())
                                .map_or(true, |f| field_visible(f, ext_res))
                    }))
                    .unwrap_or(false)
            };
            const DEPTH_NUM: [&str; 6] = ["1", "2", "3", "4", "5", "5+"];
            let push_chain_icons = |icons: &mut Vec<(&'static str, Color32)>, chain: &[Preset]| {
                match display_mode {
                    InheritanceDisplayMode::Color => {
                        for (i, p) in chain.iter().enumerate() {
                            if has_in(&p.modules) { icons.push((ICON_INHERIT, profile_depth_color(i))); }
                        }
                    }
                    InheritanceDisplayMode::Numbers => {
                        let contributing: Vec<usize> = chain.iter().enumerate()
                            .filter(|(_, p)| has_in(&p.modules))
                            .map(|(i, _)| i)
                            .collect();
                        if chain.len() <= 1 {
                            // Single-profile chain → plain arrow.
                            if !contributing.is_empty() { icons.push((ICON_INHERIT, COL_PROFILE)); }
                        } else {
                            // Multi-profile chain → always show numbers, no arrows.
                            for i in contributing { icons.push((DEPTH_NUM[i.min(5)], COL_PROFILE)); }
                        }
                    }
                    InheritanceDisplayMode::ArrowsOnly => {
                        if chain.iter().any(|p| has_in(&p.modules)) { icons.push((ICON_INHERIT, COL_PROFILE)); }
                    }
                }
            };
            let mut icons: Vec<(&'static str, Color32)> = Vec::new();
            match nav_sel {
                // ModuleEditor is a read-only view, not an edit scope — show the
                // same inheritance icons as the ambient game.
                NavSel::Game(_) | NavSel::ModuleEditor(_) => {
                    if has_in(global_modules) { icons.push((ICON_INHERIT, COL_GLOBAL)); }
                    push_chain_icons(&mut icons, &game_preset_chain);
                    if has_in(game_modules) { icons.push((ICON_EDIT, COL_GAME)); }
                }
                NavSel::Profile(_) => {
                    if has_in(global_modules) { icons.push((ICON_INHERIT, COL_GLOBAL)); }
                    push_chain_icons(&mut icons, &profile_parent_chain);
                    if editing_modules.map(|m| has_in(m)).unwrap_or(false) { icons.push((ICON_EDIT, COL_PROFILE)); }
                }
                NavSel::GlobalSettings => {
                    if has_in(global_modules) { icons.push((ICON_EDIT, COL_GLOBAL)); }
                }
                NavSel::GeneralSettings => {}
            }
            icons
        }).collect() };

        let mut builtins: Vec<usize> = Vec::new();
        let mut others: Vec<usize> = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
            if spec.backend.is_some() {
                builtins.push(i);
            } else {
                others.push(i);
            }
        }

        if self.group_by_author {
            let mut by_author: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            for &i in &others {
                by_author
                    .entry(specs[i].meta.author.clone())
                    .or_default()
                    .push(i);
            }
            for (author, items) in &by_author {
                egui::CollapsingHeader::new(egui::RichText::new(author).color(theme::ACCENT).strong())
                    .default_open(true)
                    .show(ui, |ui| {
                        for &i in items {
                            leaf(ui, &mut self.icon_cache, i, specs, &icon_lists, selected, mono);
                        }
                    });
            }
        } else {
            let mut root = TreeNode::default();
            for &i in &others {
                let comps: Vec<String> = dirs[i]
                    .iter()
                    .map(|c| c.to_string_lossy().into_owned())
                    .collect();
                root.insert(&comps, i);
            }
            render_node(ui, &mut self.icon_cache, &root, specs, is_folder_ext, &icon_lists, selected, mono);
        }

        if !builtins.is_empty() {
            egui::CollapsingHeader::new(egui::RichText::new("Built-In").color(theme::ACCENT).strong())
                .default_open(true)
                .show(ui, |ui| {
                    for &i in &builtins {
                        leaf(ui, &mut self.icon_cache, i, specs, &icon_lists, selected, mono);
                    }
                });
        }
    }
}

/// Render a tree node: subfolders (sorted) first, then extensions at this level.
/// A child folder whose single leaf has `is_folder_ext` set is collapsed into
/// the leaf directly (no CollapsingHeader wrapper).
fn render_node(ui: &mut egui::Ui, cache: &mut IconCenterCache, node: &TreeNode, specs: &[Extension], is_folder_ext: &[bool], icon_lists: &[Vec<(&'static str, Color32)>], selected: &mut usize, mono: bool) {
    let mut leaves: Vec<usize> = node.leaves.clone();
    for (name, child) in &node.children {
        // ponytail: skip the subfolder header when the folder IS the extension
        if child.children.is_empty() && child.leaves.len() == 1 && is_folder_ext.get(child.leaves[0]).copied().unwrap_or(false) {
            leaves.push(child.leaves[0]);
        } else {
            egui::CollapsingHeader::new(egui::RichText::new(name).color(theme::ACCENT).strong())
                .default_open(true)
                .show(ui, |ui| render_node(ui, cache, child, specs, is_folder_ext, icon_lists, selected, mono));
        }
    }
    leaves.sort_by(|&a, &b| specs[a].meta.name.cmp(&specs[b].meta.name));
    for &i in &leaves {
        leaf(ui, cache, i, specs, icon_lists, selected, mono);
    }
}

/// A fixed-width, faint, left-aligned footer label so the inputs/dropdown to its
/// right all start at the same x and fill to the same right edge. Allocates an
/// *exact* box (not content-sized) so every row's label column is identical.
/// COL_PROFILE (#6CC551) with HSV V decreased by 0.10 per depth level (max 5 steps).
/// depth 0 = full COL_PROFILE, depth 1 = 1-step dimmer, …, depth 5+ = fixed floor.
fn profile_depth_color(depth: usize) -> Color32 {
    let steps = depth.min(5) as f32;
    let h = 106.0_f32 + steps * 20.0;
    let (v, s) = (0.773_f32 - steps * 0.10, 0.588_f32);
    let c = v * s;
    let h6 = h / 60.0;
    let x = c * (1.0 - (h6 % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h6 as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color32::from_rgb(
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

fn footer_label(ui: &mut egui::Ui, text: &str) {
    const LABEL_W: f32 = 52.0;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(LABEL_W, ui.spacing().interact_size.y), egui::Sense::hover());
    ui.painter().text(
        egui::pos2(rect.left(), rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        egui::TextStyle::Body.resolve(ui.style()),
        theme::FAINT,
    );
}

/// Separator between a leading Nerd-Font icon and its text label. The mono space
/// glyph is wide, so one suffices; the proportional space is narrow, so use more.
fn icon_sep(mono: bool) -> &'static str {
    if mono {
        " "
    } else {
        "   "
    }
}

/// Build a monospace layout for the launch command, highlighting every
/// `%command%` token in the accent color.
pub(crate) fn command_job(cmd: &str) -> LayoutJob {
    let font = egui::FontId::monospace(11.5);
    let mut job = LayoutJob::default();
    let mut rest = cmd;
    while let Some(pos) = rest.find("%command%") {
        if pos > 0 {
            job.append(&rest[..pos], 0.0, TextFormat { font_id: font.clone(), color: theme::TEXT, ..Default::default() });
        }
        job.append("%command%", 0.0, TextFormat { font_id: font.clone(), color: theme::ACCENT, ..Default::default() });
        rest = &rest[pos + "%command%".len()..];
    }
    if !rest.is_empty() {
        job.append(rest, 0.0, TextFormat { font_id: font, color: theme::TEXT, ..Default::default() });
    }
    job
}

/// Decode the bundled logo PNG and upload it as a texture (painted sloth is the
/// fallback). Downsampled to ~64px for the small title-bar logo.
fn load_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    crate::image::load_logo_texture(ctx, crate::resources::logo_bytes(), 64, "ritz-logo")
}

/// Paint the stylized sloth logo mark inside `rect` (accent face + dark eye
/// patches + accent eye dots + a small nose). Fallback when the PNG fails.
fn paint_logo(p: &egui::Painter, rect: egui::Rect) {
    let c = rect.center();
    let r = rect.width().min(rect.height()) / 2.0;
    p.circle_filled(c, r, theme::ACCENT);
    let eye = egui::vec2(r * 0.42, -r * 0.12);
    let patch_r = r * 0.34;
    let dot_r = r * 0.13;
    for sx in [-1.0_f32, 1.0] {
        let center = c + egui::vec2(sx * eye.x, eye.y);
        p.circle_filled(center, patch_r, theme::HEAD);
        p.circle_filled(center, dot_r, theme::ACCENT);
    }
    p.circle_filled(c + egui::vec2(0.0, r * 0.4), r * 0.1, theme::HEAD);
}

/// A rounded "pill" chip (used for the title-bar breadcrumb) with the current
/// selection tint.
fn breadcrumb_chip(ui: &mut egui::Ui, text: &str) {
    let galley = ui.painter().layout_no_wrap(
        text.to_string(),
        egui::FontId::proportional(12.5),
        theme::TEXT,
    );
    let pad = egui::vec2(12.0, 4.0);
    let size = galley.size() + pad * 2.0;
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect(
        rect,
        egui::Rounding::same(rect.height() / 2.0),
        theme::SEL,
        egui::Stroke::new(1.0, theme::SELBD),
    );
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos, galley, theme::TEXT);
}

/// A selectable row that fills the full available width (left-aligned), so the
/// hover/selection highlight spans the whole column instead of hugging the text.
fn full_selectable(
    ui: &mut egui::Ui,
    selected: bool,
    text: impl Into<egui::WidgetText>,
) -> egui::Response {
    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
        ui.selectable_label(selected, text)
    })
    .inner
}

/// Gutter between two adjacent category tabs, in points. Tighter than the
/// global `item_spacing.x` (7px) — see [`nav_category_tab`] for the width
/// arithmetic this number is part of.
const TAB_GAP: f32 = 4.0;

/// Text size for a category tab's glyph + label, in points.
///
/// **This number is measured, not chosen.** The nav column is a fixed
/// [`NAV_W`] 280px; the header frame's 16px side margins and the category
/// frame's 8px inner margins leave **232px** for three tabs, i.e. `(232 −
/// 2×`[`TAB_GAP`]`) / 3 ≈ 74.7px` each. Worst case is `mono_ui` (the default),
/// where Geist Mono makes every glyph 0.6em wide and all three labels are
/// exactly 8 characters:
///
/// | size | glyph + gap + label (mono) | fits 74.7px? |
/// |------|----------------------------|--------------|
/// | 13px (Body)  | 8 + 8 + 64 = **80px** | no — clips |
/// | 12px         | 7 + 7 + 56 = **70px** | only just — 4.7px total slack |
/// | 11px (Small) | 6 + 7 + 48 = **61px** | yes — 13.7px slack |
///
/// So 11px, which is also an existing step of the type scale
/// (`TextStyle::Small`) rather than an invented size. *Consequence worth
/// knowing:* the fit is driven by the **label length**, not by this size — any
/// label longer than 8 characters clips again, so the three labels cannot grow
/// without revisiting this table.
///
/// *Why not icon-only with tooltips,* which would fit at full 13px: a tooltip
/// is not discoverable, and top-level navigation is exactly where a first-time
/// reader should not have to hover to find out where a button goes.
const TAB_TEXT_SIZE: f32 = 11.0;

/// One tab of the nav's category bar: `glyph` + `label`, centered in a cell of
/// exactly `tab_w` points.
///
/// *Why hand-painted instead of [`full_selectable`],* which the stacked-row
/// version used: `full_selectable` is `top_down_justified`, i.e. deliberately
/// full-width — the wrong primitive once three of these have to share one row.
/// Its selection fill is also a plain rounded rect, which in a horizontal strip
/// reads as "one cell is slightly lighter" rather than "this is the open tab".
///
/// Selected state therefore layers two cues, both from existing theme tokens:
///
/// - **fill** `SEL` (the same accent tint `full_selectable` uses) with a `SELBD`
///   hairline, rounded on **all four corners** at the same `8.0` radius every
///   other rounded container in the app uses (`Rounding::same(8.0)`, per
///   `docs/ui/STYLING-GUIDE.md`);
/// - **ink**: glyph in `ACCENT`, label in full-brightness `TEXT`.
///
/// *Why no underline anymore:* an earlier version rounded only the top two
/// corners and sat the cell on a separate 2px `ACCENT` underline across its
/// bottom edge, so the selected tab read as "sitting on a strip." In practice
/// that read as a flat outline bar rather than a selected tab. The fill/border
/// plus the accent icon + bright label already carry "this one is selected" on
/// their own — the underline was a second, redundant cue that also fought the
/// uniform rounding every other control in the app uses.
///
/// Unselected is transparent (a `HOV` wash on hover only), glyph in `FAINT`,
/// label in `DIM` — legible but clearly receded, so exactly one tab reads as
/// live. *Why color and not a bold weight:* the bold family
/// (`FontFamily::Name("bold")`) is reserved for the logo wordmark, and a weight
/// switch would reflow the label inside a fixed-width cell.
///
/// Icons go through a plain [`LayoutJob`], **not** [`IconCenterCache`]: that
/// cache corrects glyph *ink* for a square `icon_button`, whereas here the glyph
/// is just the first run of a text line, identical in construction to [`leaf`]'s.
fn nav_category_tab(
    ui: &mut egui::Ui,
    tab_w: f32,
    selected: bool,
    glyph: &str,
    label: &str,
    mono: bool,
) -> egui::Response {
    // Same font-independent icon→text gap idiom the tree leaves use, scaled to
    // the smaller tab text.
    let gap = if mono { 7.0 } else { 6.0 };
    let (icon_col, text_col) = if selected {
        (theme::ACCENT, theme::TEXT)
    } else {
        (theme::FAINT, theme::DIM)
    };
    let font_id = egui::FontId::new(TAB_TEXT_SIZE, egui::FontFamily::Proportional);
    let mut job = LayoutJob::default();
    job.append(glyph, 0.0, TextFormat { font_id: font_id.clone(), color: icon_col, ..Default::default() });
    job.append(label, gap, TextFormat { font_id, color: text_col, ..Default::default() });
    let galley = ui.fonts(|f| f.layout_job(job));

    // Cell height: the standard control height. No underline band anymore, so
    // there's no extra space to reserve beyond the normal control height.
    let h = ui.spacing().interact_size.y;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(tab_w, h), egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        // All four corners, matching the app-wide 8px rounding convention (see
        // `docs/ui/STYLING-GUIDE.md`, "Corner rounding is Rounding::same(8.0)
        // everywhere").
        let rounding = egui::Rounding::same(8.0);
        if selected {
            painter.rect(rect, rounding, theme::SEL, egui::Stroke::new(1.0, theme::SELBD));
        } else if resp.hovered() {
            painter.rect_filled(rect, rounding, theme::HOV);
        }
        // Centered on the whole cell now that there's no underline strip to
        // subtract.
        let pos = rect.center() - galley.size() * 0.5;
        ui.painter().galley(pos, galley, theme::TEXT);
    }
    resp
}

/// A single selectable extension leaf.
/// Colored icons prefix the name: ICON_INHERIT for inherited layers, ICON_EDIT for
/// the current editing scope. The name itself stays the default UI color.
fn leaf(ui: &mut egui::Ui, cache: &mut IconCenterCache, i: usize, specs: &[Extension], icon_lists: &[Vec<(&'static str, Color32)>], selected: &mut usize, mono: bool) {
    let spec = &specs[i];
    // Gap (points) between a leading icon and following text — font-independent.
    let gap = if mono { 8.0 } else { 7.0 };
    let name = spec.meta.name.clone();
    // Configurable (backend) modules get a cog, painted right-bound below.
    let has_cog = spec.backend.is_some();

    let icons = icon_lists.get(i).map(|v| v.as_slice()).unwrap_or(&[]);
    let resp = if icons.is_empty() {
        full_selectable(ui, *selected == i, name)
    } else {
        let font_id = ui.style().text_styles[&egui::TextStyle::Body].clone();
        let mut job = LayoutJob::default();
        for (idx, (icon, color)) in icons.iter().enumerate() {
            let lead = if idx == 0 { 0.0 } else { gap };
            job.append(icon, lead, TextFormat { font_id: font_id.clone(), color: *color, ..Default::default() });
        }
        // Explicit white (not PLACEHOLDER): a selected selectable resolves PLACEHOLDER
        // to the selection stroke color, which would tint the name blue.
        job.append(&name, gap, TextFormat { font_id, color: theme::TEXT, ..Default::default() });
        full_selectable(ui, *selected == i, job)
    };
    if resp.clicked() {
        *selected = i;
    }

    if has_cog {
        // Centered on its ink, not its layout box: `Align2::RIGHT_CENTER` put the
        // cog wherever the font's line box happened to fall, which read a couple
        // of pixels high next to the row text. `centered_pos` returns a corrected
        // top-left, so this MUST anchor LEFT_TOP or it double-corrects.
        let font = egui::TextStyle::Body.resolve(ui.style());
        let r = resp.rect;
        let center = egui::pos2(r.right() - 11.0 - ICON_CELL / 2.0, r.center().y);
        let pos = cache.centered_pos(ui, "\u{f013}", &font, center);
        ui.painter()
            .text(pos, egui::Align2::LEFT_TOP, "\u{f013}", font, theme::FAINT);
    }
}

/// The scope swatches (● Default · ● Global · ● Profile · ● Game). Designed to
/// be called inside a right-to-left layout so it reads left→right.
fn scope_legend(ui: &mut egui::Ui) {
    for (label, col) in [
        ("Game", COL_GAME),
        ("Profile", COL_PROFILE),
        ("Global", COL_GLOBAL),
        ("Default", theme::COL_DEFAULT),
    ] {
        ui.label(egui::RichText::new(format!("\u{25cf} {label}")).color(col).small());
    }
}

/// A scope-colored slider with a drag/type spinner. Must be called inside a
/// right-to-left layout: the spinner anchors to the right (growing leftward for
/// long values) and the track fills the remaining width, so the row stays a
/// fixed width regardless of the value's digit count.
fn scope_slider(
    ui: &mut egui::Ui,
    value: f64,
    min: f64,
    max: f64,
    step: f64,
    integer: bool,
    scope: Color32,
) -> Option<f64> {
    let snap = |mut nv: f64| {
        if step > 0.0 {
            nv = (nv / step).round() * step;
        }
        if integer {
            nv = nv.round();
        }
        nv.clamp(min, max)
    };

    let mut out = None;

    // Spinner first → rightmost in the right-to-left layout.
    let mut v = value;
    let speed = if integer { 1.0 } else { step.max(0.01) };
    let mut dv = egui::DragValue::new(&mut v).range(min..=max).speed(speed);
    dv = if integer { dv.fixed_decimals(0) } else { dv.max_decimals(2) };
    // Fixed width (~5 digits) so it doesn't grow when entering edit mode (egui's
    // edit field would otherwise expand to fill the available width).
    let h = ui.spacing().interact_size.y;
    if ui.add_sized([64.0, h], dv).changed() {
        out = Some(snap(v));
    }

    // Gap between the spinner and the track.
    ui.add_space(10.0);
    // Track fills the remaining width to the left of the spinner.
    // Subtract item_spacing.x: egui consumes it when the track widget is placed,
    // but available_width() does not subtract it in advance.
    let track_w = (ui.available_width() - ui.spacing().item_spacing.x).max(40.0);
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(track_w, 18.0), egui::Sense::click_and_drag());
    if let Some(pos) = resp.interact_pointer_pos() {
        let t = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
        let nv = snap(min + t * (max - min));
        if nv != value {
            out = Some(nv);
        }
    }

    if ui.is_rect_visible(rect) {
        let shown = out.unwrap_or(value);
        let cy = rect.center().y;
        let track = egui::Rect::from_min_max(
            egui::pos2(rect.left(), cy - 3.0),
            egui::pos2(rect.right(), cy + 3.0),
        );
        let t = if max > min { ((shown - min) / (max - min)).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let fill_x = rect.left() + t * rect.width();
        let painter = ui.painter();
        const RADIUS: f32 = 7.5;
        // Clamp so the handle circle never spills outside the track rect (e.g. when
        // value == min and fill_x == rect.left()).
        let handle_x = fill_x.clamp(rect.left() + RADIUS, rect.right() - RADIUS);
        painter.rect_filled(track, egui::Rounding::same(3.0), theme::BORDER);
        painter.rect_filled(
            egui::Rect::from_min_max(track.min, egui::pos2(fill_x, track.max.y)),
            egui::Rounding::same(3.0),
            scope,
        );
        painter.circle_filled(egui::pos2(handle_x, cy), RADIUS, scope);
        painter.circle_stroke(egui::pos2(handle_x, cy), RADIUS, egui::Stroke::new(2.5, theme::PANEL));
    }

    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tri {
    Unset,
    Enabled,
    Disabled,
}

fn tri_state(res: &resolve::ResolvedField) -> Tri {
    if res.provenance != Provenance::Game {
        Tri::Unset
    } else if res.enabled {
        Tri::Enabled
    } else {
        Tri::Disabled
    }
}

/// Paint an 18×18, 6px-rounded checkbox into `rect`: `color` fill + white check
/// when on, faint bordered box when off; both dimmed when `faded` (inherited).
fn paint_scope_box(painter: &egui::Painter, rect: egui::Rect, on: bool, faded: bool, color: Color32) {
    let round = egui::Rounding::same(6.0);
    let box_rect = rect.shrink(1.0);
    if on {
        let fill = if faded { color.gamma_multiply(0.35) } else { color };
        painter.rect_filled(box_rect, round, fill);
        let col = if faded { Color32::WHITE.gamma_multiply(0.6) } else { Color32::WHITE };
        let galley =
            painter.layout_no_wrap("\u{f00c}".to_owned(), egui::FontId::proportional(11.0), col);
        let ink_center = galley
            .rows
            .first()
            .and_then(|row| row.glyphs.first())
            .map(|g| g.pos.to_vec2() + g.uv_rect.offset + g.uv_rect.size * 0.5)
            .unwrap_or_else(|| galley.size() * 0.5);
        painter.galley(rect.center() - ink_center, galley, col);
    } else {
        // Inset the stroke by half its width so its outer edge sits exactly on
        // box_rect (draws inward only) — matching the filled box's size.
        painter.rect(box_rect.shrink(0.75), round, Color32::TRANSPARENT, egui::Stroke::new(1.5, theme::CHECK_OUTLINE));
    }
}

/// Lay out and sense a single clickable `[box] label` row: the whole rect (box +
/// gap + label) is one click target with a subtle hover background. `paint_box`
/// draws the 18×18 box. Returns the row's response.
/// `interactive: false` allocates the row with `Sense::hover()` instead of
/// `Sense::click()` and skips the hover highlight, so the row paints identically
/// but can never report a click. Used by the read-only (preview) render path,
/// where a returned `Some(Tri)` would write config — see [`scope_checkbox`].
fn checkbox_row(
    ui: &mut egui::Ui,
    label: &str,
    badge: Option<(&str, Color32)>,
    interactive: bool,
    paint_box: impl FnOnce(&egui::Painter, egui::Rect),
) -> egui::Response {
    const BOX: f32 = 18.0;
    const GAP: f32 = 7.0;
    let font = egui::TextStyle::Body.resolve(ui.style());
    let badge_galley = badge.map(|(text, col)| ui.painter().layout_no_wrap(text.to_owned(), font.clone(), col));
    let badge_w = badge_galley.as_ref().map(|g| g.size().x + GAP).unwrap_or(0.0);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, theme::TEXT);
    let size = egui::vec2(BOX + GAP + badge_w + galley.size().x, BOX.max(galley.size().y));
    let sense = if interactive { egui::Sense::click() } else { egui::Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if interactive && resp.hovered() {
            painter.rect_filled(rect.expand2(egui::vec2(4.0, 2.0)), egui::Rounding::same(6.0), theme::HOV);
        }
        let box_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.center().y - BOX / 2.0),
            egui::Vec2::splat(BOX),
        );
        paint_box(painter, box_rect);
        let mut tx = box_rect.right() + GAP;
        if let (Some(bg), Some((_, col))) = (badge_galley, badge) {
            painter.galley(egui::pos2(tx, rect.center().y - bg.size().y / 2.0), bg, col);
            tx += badge_w;
        }
        painter.galley(egui::pos2(tx, rect.center().y - galley.size().y / 2.0), galley, theme::TEXT);
    }
    resp
}

/// A plain on/off checkbox styled like the scope-row checkboxes. Box and label
/// form one clickable rect. Toggles `checked` on click; returns the response so
/// callers can attach a tooltip or check `.clicked()`.
/// Drop stored variables not present in `valid` (keyed by author/name/variable),
/// then prune any name/author maps left empty.
fn cleanup_modules(
    modules: &mut AuthorsMap,
    valid: &std::collections::HashSet<(String, String, String)>,
) {
    modules.retain(|author, names| {
        names.retain(|name, vars| {
            vars.retain(|var, _| {
                valid.contains(&(author.clone(), name.clone(), var.clone()))
            });
            !vars.is_empty()
        });
        !names.is_empty()
    });
}

/// The module-config row card, always in the neutral "Default" gray scheme:
/// gray-tinted backdrop, 8px rounding, fixed 39px height, 3px left scope bar.
/// Runs `content` inside the row and returns its value.
fn settings_card<R>(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let scope = theme::COL_DEFAULT;
    let tint = Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16);
    let inner = egui::Frame::none()
        .fill(tint)
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin { left: 12.0, right: 11.0, top: 0.0, bottom: 0.0 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.set_min_height(39.0);
                ui.spacing_mut().item_spacing.x = 8.0;
                content(ui)
            })
            .inner
        });
    let r = inner.response.rect;
    let bar_clip = egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + 3.0, r.max.y));
    ui.painter()
        .with_clip_rect(bar_clip)
        .rect_filled(r, egui::Rounding::same(8.0), scope);
    ui.add_space(8.0);
    inner.inner
}

/// A settings row: `label` (with optional `hover`) on the left, `control`
/// right-bound — mirroring a module's non-toggle field row.
fn settings_value_row<R>(
    ui: &mut egui::Ui,
    label: &str,
    hover: &str,
    control: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    settings_card(ui, |ui| {
        let left = ui.cursor().min.x;
        let resp = ui.label(egui::RichText::new(label).color(theme::TEXT));
        if !hover.is_empty() {
            resp.on_hover_text(hover);
        }
        let used = ui.cursor().min.x - left;
        ui.add_space((260.0 - used).max(0.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control)
            .inner
    })
}

/// A boolean settings row styled like a module Toggle: a gray scope-checkbox +
/// label filling the card. Toggles `value` and returns true if clicked.
fn settings_toggle_row(ui: &mut egui::Ui, label: &str, hover: &str, value: &mut bool) -> bool {
    let on = *value;
    let clicked = settings_card(ui, |ui| {
        let mut resp =
            checkbox_row(ui, label, None, true, |p, r| paint_scope_box(p, r, on, false, theme::COL_DEFAULT));
        if !hover.is_empty() {
            resp = resp.on_hover_text(hover);
        }
        resp.clicked()
    });
    if clicked {
        *value = !*value;
    }
    clicked
}

fn styled_checkbox(ui: &mut egui::Ui, checked: &mut bool, label: &str) -> egui::Response {
    let on = *checked;
    let resp = checkbox_row(ui, label, None, true, |p, r| paint_scope_box(p, r, on, false, COL_PROFILE));
    if resp.clicked() {
        *checked = !*checked;
    }
    resp
}

/// The tri-state a [`scope_checkbox`] renders, together with the effective
/// (inherited) on/off it falls back to when the state is `Unset`. The two are
/// only ever meaningful together, so they travel as one argument — which also
/// keeps `scope_checkbox` at the arity limit now that it takes `read_only`.
struct TriValue {
    state: Tri,
    inherited_on: bool,
}

/// A mockup-style checkbox over the tri-state model, with the label as part of
/// the click target:
/// - **left-click** toggles Enabled ⇄ Disabled (flipping from the current
///   effective on/off, so the first click on an inherited value sets the opposite)
/// - **right-click** resets to Unset (inherit from a lower scope)
///
/// `read_only` makes the row inert: it paints exactly the same box, badge and
/// label but is allocated non-interactively and always returns `None`, so the
/// caller can never reach `apply_tri` (which writes config *and* seeds
/// `text_buffers`). The gate is this parameter, not `ui.is_enabled()` — a
/// disabled parent `Ui` would grey the row out but is not what stops the write.
fn scope_checkbox(
    ui: &mut egui::Ui,
    label: &str,
    hover: &str,
    value: TriValue,
    scope: Color32,
    badge: Option<(&str, Color32)>,
    read_only: bool,
) -> Option<Tri> {
    let TriValue { state, inherited_on } = value;
    let on = match state {
        Tri::Enabled => true,
        Tri::Disabled => false,
        Tri::Unset => inherited_on,
    };
    let faded = state == Tri::Unset;
    let mut resp = checkbox_row(ui, label, badge, !read_only, |p, r| {
        paint_scope_box(p, r, on, faded, scope)
    });
    if !hover.is_empty() {
        resp = resp.on_hover_text(hover);
    }

    if read_only {
        None
    } else if resp.secondary_clicked() {
        Some(Tri::Unset)
    } else if resp.clicked() {
        Some(if on { Tri::Disabled } else { Tri::Enabled })
    } else {
        None
    }
}

/// Query the focused window's class via `hyprctl activewindow -j`. Prefers
/// `initialClass` (what the hypr-monctl plugin matches on), falling back to `class`.
fn detect_active_window_class() -> Option<String> {
    let out = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let pick = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    pick("initialClass").or_else(|| pick("class"))
}

fn num_value(v: f64, integer: bool) -> Value {
    if integer {
        json!(v as i64)
    } else {
        json!((v * 1000.0).round() / 1000.0)
    }
}

fn number_range(field: &UiField) -> (f64, f64, f64) {
    match &field.options {
        Some(OptionsSpec::Range { min, max, step }) => (*min, *max, step.unwrap_or(1.0)),
        _ => (0.0, 100.0, 1.0),
    }
}

fn option_values(field: &UiField) -> Vec<(String, String)> {
    let vals = match &field.options {
        Some(OptionsSpec::List(v)) => v.clone(),
        _ => Vec::new(),
    };
    let labels = field.display_options.clone();
    vals.iter()
        .enumerate()
        .map(|(i, v)| {
            let label = labels
                .as_ref()
                .and_then(|l| l.get(i))
                .cloned()
                .unwrap_or_else(|| v.clone());
            (v.clone(), label)
        })
        .collect()
}

fn field_visible(field: &UiField, ext_res: Option<&resolve::ExtResolution>) -> bool {
    condition::eval_opt(field.requires.as_deref(), &|n| {
        ext_res
            .and_then(|e| e.fields.get(n))
            .map(|f| f.var.truthy)
            .unwrap_or(false)
    })
    .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn editor_control_width_fills_remainder_and_floors() {
        // Whatever is left of the row after the label column, minus a right-pinned
        // reservation — this is what makes every editor textbox share one right
        // edge instead of each ending where its own content ran out.
        assert_eq!(editor_control_width(400.0, 0.0), 400.0);
        assert_eq!(editor_control_width(400.0, ACTION_COL_W), 400.0 - ACTION_COL_W);
        // Never collapses, however narrow the window (or deep the nesting) gets.
        assert_eq!(editor_control_width(90.0, ACTION_COL_W), MIN_CONTROL_W);
        assert_eq!(editor_control_width(-50.0, 0.0), MIN_CONTROL_W);
    }

    #[test]
    fn entry_title_falls_back_to_numbered_placeholder() {
        assert_eq!(entry_title("PROTON_LOG", "Variable", 0), "PROTON_LOG");
        // Blank / whitespace-only entries still get a header (a card's title row
        // anchors its action cluster, so it must never be empty).
        assert_eq!(entry_title("", "Variable", 0), "Variable 1");
        assert_eq!(entry_title("   ", "Section", 2), "Section 3");
        // Surrounding whitespace is trimmed off a real title.
        assert_eq!(entry_title("  Display  ", "Section", 0), "Display");
    }

    #[test]
    fn parse_env_row_splits_on_first_equals() {
        assert_eq!(parse_env_row("FOO=a=b"), ("FOO".to_string(), "a=b".to_string()));
        assert_eq!(parse_env_row("FOO=bar"), ("FOO".to_string(), "bar".to_string()));
        // No `=` at all: whole string is the name, value is empty.
        assert_eq!(parse_env_row("FOO"), ("FOO".to_string(), String::new()));
    }

    #[test]
    fn is_valid_env_name_enforces_posix_charset() {
        assert!(is_valid_env_name("FOO"));
        assert!(is_valid_env_name("_FOO_bar9"));
        // Empty name, leading digit, and non-alnum chars are all invalid.
        assert!(!is_valid_env_name(""));
        assert!(!is_valid_env_name("1BAD"));
        assert!(!is_valid_env_name("FOO-BAR"));
        assert!(!is_valid_env_name("FOO BAR"));
    }

    #[test]
    fn env_row_name_validity_gates_persistence() {
        // Mirrors the filter `render_env_pair_field` applies before persisting:
        // an empty or invalid name means the row is dropped.
        let rows = ["=x", "1BAD=x", "", "GOOD=x"];
        let kept: Vec<&str> = rows
            .iter()
            .filter(|r| {
                let (n, _) = parse_env_row(r);
                let n = n.trim();
                !n.is_empty() && is_valid_env_name(n)
            })
            .copied()
            .collect();
        assert_eq!(kept, vec!["GOOD=x"]);
    }

    #[test]
    fn editor_exit_target_restores_prior_view_or_falls_back_to_game() {
        // A real prior view is restored verbatim.
        assert_eq!(
            editor_exit_target(Some(NavSel::GlobalSettings), "42"),
            NavSel::GlobalSettings
        );
        assert_eq!(
            editor_exit_target(Some(NavSel::Profile("Perf".into())), "42"),
            NavSel::Profile("Perf".into())
        );
        // No stored view → ambient game.
        assert_eq!(editor_exit_target(None, "42"), NavSel::Game("42".into()));
        // Never land back on the editor (would be a stuck selection) → ambient game.
        assert_eq!(
            editor_exit_target(Some(NavSel::ModuleEditor("A::B::1.0".into())), "42"),
            NavSel::Game("42".into())
        );
    }

    #[test]
    fn cleanup_drops_undeclared_vars_and_prunes_empties() {
        // Ritze/Core has a live `kbd_layout` and a stale `xkb_de`; the whole
        // Other/Gone module is gone.
        let mut modules: AuthorsMap = serde_json::from_value(json!({
            "Ritze": { "Core": { "kbd_layout": "de", "xkb_de": true } },
            "Other": { "Gone": { "removed": true } }
        }))
        .unwrap();

        let valid: std::collections::HashSet<(String, String, String)> =
            [("Ritze", "Core", "kbd_layout")]
                .iter()
                .map(|(a, n, var)| (a.to_string(), n.to_string(), var.to_string()))
                .collect();

        cleanup_modules(&mut modules, &valid);

        // Stale var dropped, live one kept, the dead module pruned entirely.
        assert!(!modules["Ritze"]["Core"].contains_key("xkb_de"));
        assert_eq!(modules["Ritze"]["Core"]["kbd_layout"], json!("de"));
        assert!(!modules.contains_key("Other"));
    }

    /// Build a draft from an on-disk manifest JSON, mirroring `ensure_draft`.
    fn draft_from(manifest: Value, editable: bool) -> ModuleDraft {
        let spec: Extension = serde_json::from_value(manifest).unwrap();
        let baseline = serde_json::to_value(&spec).unwrap();
        let baseline_vars: std::collections::HashSet<String> = spec
            .ui
            .values()
            .flatten()
            .map(|f| f.variable.clone())
            .collect();
        let sections = spec.ui.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut ext = spec;
        ext.ui.clear();
        let identity = PendingIdentity {
            author: ext.meta.author.clone(),
            name: ext.meta.name.clone(),
            var_edits: baseline_vars
                .iter()
                .map(|v: &String| (v.clone(), v.clone()))
                .collect(),
        };
        ModuleDraft {
            id: ext.id(),
            manifest: PathBuf::from("/nonexistent/mod.json"),
            editable,
            ext,
            sections,
            baseline,
            baseline_vars,
            name_error: None,
            identity,
            identity_error: None,
        }
    }

    fn sample_manifest() -> Value {
        json!({
            "Extension": {"Name": "Sample", "Author": "Ritze", "Version": "1.0"},
            "UI": {"Main": [{"Type": "toggle", "Variable": "enabled"}]}
        })
    }

    #[test]
    fn save_gate_predicate_requires_all_conditions() {
        // All four must hold for Save to enable.
        assert!(save_gate(true, true, true, false));
        assert!(!save_gate(false, true, true, false)); // not dirty
        assert!(!save_gate(true, false, true, false)); // invalid
        assert!(!save_gate(true, true, false, false)); // a Requires won't parse
        assert!(!save_gate(true, true, true, true)); // name collision
    }

    #[test]
    fn clean_draft_is_not_saveable_and_does_not_hold_autosave() {
        let draft = draft_from(sample_manifest(), true);
        // A freshly-loaded draft equals disk: not dirty → Save disabled, interlock off.
        assert!(!draft.dirty());
        assert!(!draft.save_enabled());
        assert!(!config_autosave_held(draft.dirty()));
    }

    #[test]
    fn valid_dirty_editable_draft_is_saveable_and_holds_autosave() {
        let mut draft = draft_from(sample_manifest(), true);
        draft.ext.meta.description = Some("now edited".to_string());
        assert!(draft.dirty());
        assert!(draft.save_enabled());
        // Interlock engages while the draft is dirty…
        assert!(config_autosave_held(draft.dirty()));
        // …and releases once the edit is reverted to match disk.
        draft.ext.meta.description = None;
        assert!(!draft.dirty());
        assert!(!config_autosave_held(draft.dirty()));
    }

    #[test]
    fn bundled_module_is_never_saveable() {
        let mut draft = draft_from(sample_manifest(), false); // not editable
        draft.ext.meta.description = Some("edited".to_string());
        assert!(draft.dirty());
        assert!(!draft.save_enabled(), "bundled modules must not save (Fork to edit)");
    }

    #[test]
    fn invalid_draft_write_is_refused() {
        // Add a field with an empty Variable → `validate` fails → Save refused.
        let mut draft = draft_from(sample_manifest(), true);
        draft.sections[0].1.push(new_field(String::new()));
        assert!(draft.dirty());
        assert!(!core_extension::validate(&draft.snapshot()).is_ok());
        assert!(!draft.save_enabled(), "invalid manifest must block Save");
    }

    #[test]
    fn unparseable_requires_blocks_save() {
        let mut draft = draft_from(sample_manifest(), true);
        // A trailing operator never parses.
        draft.sections[0].1[0].requires = Some("enabled AND".to_string());
        assert!(!all_requires_parse(&draft.snapshot()));
        assert!(!draft.save_enabled(), "a bad Requires must block Save");
    }

    #[test]
    fn newly_added_field_variable_is_editable_existing_is_locked() {
        let draft = draft_from(sample_manifest(), true);
        // The on-disk field is locked (its variable is in the baseline set)…
        assert!(draft.baseline_vars.contains("enabled"));
        // …while a brand-new variable is not, so its Variable stays editable.
        assert!(!draft.baseline_vars.contains("new_var"));
    }

    fn renames_of(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs.iter().map(|(o, n)| (o.to_string(), n.to_string())).collect()
    }

    #[test]
    fn chained_rename_is_rejected_but_free_rename_is_accepted() {
        let current = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        // Free rename to a fresh name → accepted.
        assert!(validate_var_renames(&current, &renames_of(&[("a", "z")])).is_ok());
        // Chained rename: a → b while b is still a live variable → rejected.
        assert!(validate_var_renames(&current, &renames_of(&[("a", "b")])).is_err());
        // Swap (a→b, b→a) is a chain in both directions → rejected.
        assert!(validate_var_renames(&current, &renames_of(&[("a", "b"), ("b", "a")])).is_err());
        // Two vars renamed to the same new name → rejected.
        assert!(validate_var_renames(&current, &renames_of(&[("a", "x"), ("b", "x")])).is_err());
        // Rename to empty → rejected.
        assert!(validate_var_renames(&current, &renames_of(&[("a", "")])).is_err());
        // No renames → trivially ok.
        assert!(validate_var_renames(&current, &IndexMap::new()).is_ok());
    }

    #[test]
    fn staged_identity_edits_do_not_touch_the_save_path() {
        let mut draft = draft_from(sample_manifest(), true);
        // No pending change on a fresh draft.
        assert!(!draft.has_pending_identity());
        // Staging an Author/Name/Variable change must NOT make the draft dirty or
        // saveable — it travels the Rename path only, never Save/autosave.
        draft.identity.author = "Someone".to_string();
        draft.identity.name = "Renamed".to_string();
        *draft.identity.var_edits.get_mut("enabled").unwrap() = "on".to_string();
        assert!(draft.has_pending_identity());
        assert!(!draft.dirty(), "identity edits must not dirty the snapshot");
        assert!(!draft.save_enabled(), "identity edits must not enable Save");
        assert!(!config_autosave_held(draft.dirty()), "identity edits must not hold autosave");
        // The staged variable rename surfaces as an old→new entry…
        assert_eq!(
            draft.changed_var_renames(),
            renames_of(&[("enabled", "on")])
        );
        // …and a non-colliding identity change is committable.
        assert!(draft.compute_identity_error(false).is_none());
        // A colliding (Author, Name) blocks it.
        assert!(draft.compute_identity_error(true).is_some());
    }

    fn spec_named(author: &str, name: &str, version: &str) -> Extension {
        serde_json::from_value(json!({
            "Extension": {"Name": name, "Author": author, "Version": version},
            "UI": {"Main": [{"Type": "toggle", "Variable": "enabled"}]}
        }))
        .unwrap()
    }

    #[test]
    fn name_collision_is_version_blind_and_excludes_self() {
        // Two loaded modules; the second shares (Author,Name) with a candidate but
        // differs in Version — still a collision (config keys on Author+Name).
        let specs = vec![
            spec_named("Ritze", "Alpha", "1.0"),
            spec_named("Ritze", "Beta", "2.0"),
        ];
        let manifests = vec![PathBuf::from("/a/alpha.json"), PathBuf::from("/a/beta.json")];

        // Same Author+Name, different Version → collides.
        assert!(name_collides(&specs, &manifests, "Ritze", "Beta", None));
        // Distinct Name → free.
        assert!(!name_collides(&specs, &manifests, "Ritze", "Gamma", None));
        // Excluding the very module by manifest path clears the self-collision.
        assert!(!name_collides(
            &specs,
            &manifests,
            "Ritze",
            "Beta",
            Some(Path::new("/a/beta.json")),
        ));
        // Excluding a *different* path still reports the collision.
        assert!(name_collides(
            &specs,
            &manifests,
            "Ritze",
            "Beta",
            Some(Path::new("/a/alpha.json")),
        ));
    }

    #[test]
    fn slug_uniquify_suffixes_on_clash() {
        // The base is free → used as-is.
        let taken: std::collections::HashSet<String> = ["ritze__mod.json".to_string()]
            .into_iter()
            .collect();
        assert_eq!(uniquify_slug("ritze__other", |c| taken.contains(c)), "ritze__other.json");
        // The base is taken → first free suffix is `-2`.
        assert_eq!(uniquify_slug("ritze__mod", |c| taken.contains(c)), "ritze__mod-2.json");
        // `-2` also taken → `-3`.
        let taken2: std::collections::HashSet<String> =
            ["ritze__mod.json".to_string(), "ritze__mod-2.json".to_string()]
                .into_iter()
                .collect();
        assert_eq!(uniquify_slug("ritze__mod", |c| taken2.contains(c)), "ritze__mod-3.json");
    }

    #[test]
    fn sanitize_slug_maps_non_alnum_and_guards_empty() {
        assert_eq!(sanitize_slug("My Module!"), "my_module");
        assert_eq!(sanitize_slug("LSFG-VK"), "lsfg_vk");
        assert_eq!(sanitize_slug("  "), "module");
    }

    #[test]
    fn deferred_reorder_and_add_apply_without_mid_iteration_mutation() {
        let mut draft = draft_from(sample_manifest(), true);
        apply_deferred(&mut draft, Deferred::SectionAdd);
        assert_eq!(draft.sections.len(), 2);
        apply_deferred(&mut draft, Deferred::Section(0, RowAction::Down));
        assert_eq!(draft.sections[1].0, "Main");
        apply_deferred(&mut draft, Deferred::FieldAdd(1));
        assert_eq!(draft.sections[1].1.len(), 2);
    }
}
