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

use crate::theme::{self, COL_GAME, COL_GLOBAL, COL_PROFILE, ICON_EDIT, ICON_INHERIT};

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
    Discard,
    Fork,
    Delete,
    Rename,
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
        let global = Some(&self.global_config);
        match &self.nav_sel {
            NavSel::GeneralSettings => {
                resolve::resolve(&self.cur_specs, Some(&self.game_config), self.preset.as_ref(), global)
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
                resolve::resolve(&self.cur_specs, Some(&self.game_config), effective, global)
            }
            NavSel::GlobalSettings => {
                let fake = preset_as_fake_game(&self.global_config);
                resolve::resolve(&self.cur_specs, Some(&fake), None, None)
            }
            NavSel::Profile(_) => match &self.editing_preset_buf {
                Some(p) => {
                    let parent = p.parent.as_ref()
                        .map(|n| collect_parent_chain(&self.paths, n));
                    let fake = preset_as_fake_game(p);
                    resolve::resolve(&self.cur_specs, Some(&fake), parent.as_ref(), global)
                }
                None => resolve::resolve(&self.cur_specs, None, None, global),
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
        // Drop the draft and reload from disk; ensure_draft rebuilds it clean.
        self.module_draft = None;
        self.reload_extensions();
        self.flush_config_writes_if_clean();
    }

    /// Discard the in-memory draft (Revert). ensure_draft reloads a clean copy on
    /// the next frame; releasing the interlock flushes any held config writes.
    fn discard_module(&mut self) {
        self.module_draft = None;
        self.flush_config_writes_if_clean();
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

        // Keep the module-editor draft in sync with the selected module.
        if let NavSel::ModuleEditor(id) = self.nav_sel.clone() {
            self.ensure_draft(&id);
            self.refresh_draft_name_error();
            self.refresh_identity_state();
        }

        let edit_resolution = self.resolve_for_editing();
        let game_resolution = self.resolve_for_game();
        let mut changed = false;

        self.render_title_bar(ctx);

        egui::SidePanel::left("nav")
            .exact_width(280.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(theme::PANEL))
            .show(ctx, |ui| {
                self.render_nav_panel(ui);
            });

        if !matches!(self.nav_sel, NavSel::GeneralSettings) {
        let mut open_create = false;
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
                    ui.horizontal(|ui| {
                        ui.label(theme::header_label("Modules"));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .add(theme::secondary_button("\u{f067} New"))
                                .on_hover_text("Create a new custom module")
                                .clicked()
                            {
                                open_create = true;
                            }
                        });
                    });
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
                            self.render_ext_tree(ui);
                        });
                });
        });
        if open_create {
            self.open_create_dialog();
        }
        } // end if !GeneralSettings

        // Assemble the launch preview. In the module editor, splice the in-memory
        // draft over its on-disk entry so the preview reflects unsaved edits, and
        // lint for set/set env collisions across modules.
        let (preview, collisions): (String, Vec<String>) =
            if matches!(self.nav_sel, NavSel::ModuleEditor(_)) {
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
        {
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
                self.render_module_editor(ui, &id);
                return;
            }

            let edit_ctx_label: Option<String> = match &self.nav_sel {
                NavSel::GlobalSettings => {
                    Some("Editing Global Settings — applies to all games".to_string())
                }
                NavSel::Profile(name) => Some(format!("Editing Profile: {name}")),
                NavSel::ModuleEditor(_) => None, // handled above; unreachable here
                NavSel::Game(_) => {
                    Some(format!("Editing Game: {}", self.game_config.game.name))
                }
                NavSel::GeneralSettings => None,
            };

            let Some(spec) = self.cur_specs.get(self.selected_ext).cloned() else {
                ui.label("No extensions apply to this game.");
                return;
            };

            // Detail header: title + version/author + description, bottom border.
            ui.add_space(6.0);
            let mut open_inspector = false;
            let mut open_fork = false;
            ui.horizontal(|ui| {
                ui.heading(&spec.meta.name);
                ui.add_space(6.0);
                ui.label(egui::RichText::new(format!("v{}", spec.meta.version)).color(theme::FAINT));
                ui.label(egui::RichText::new(format!("by {}", spec.meta.author)).color(theme::FAINT));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(theme::secondary_button("\u{f013}  Inspect"))
                        .on_hover_text("View this module's manifest (read-only)")
                        .clicked()
                    {
                        open_inspector = true;
                    }
                    if ui
                        .add(theme::secondary_button("Fork"))
                        .on_hover_text("Create an editable copy of this module")
                        .clicked()
                    {
                        open_fork = true;
                    }
                });
            });
            if open_inspector {
                self.nav_sel = NavSel::ModuleEditor(spec.id());
            }
            if open_fork {
                self.open_fork_dialog(&spec.id());
            }
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

            let ext_id = spec.id();
            let ext_res = edit_resolution.exts.get(&ext_id);

            egui::ScrollArea::vertical()
                // Fill the panel (don't shrink to content) so the scrollbar sits at
                // the far right while the content stays capped/left-aligned.
                .auto_shrink([false, false])
                .drag_to_scroll(self.general_config.touch_mode)
                .show(ui, |ui| {
                ui.vertical(|ui| {
                    // Fill the pane (less an 18px scrollbar gutter), but cap at ~743px
                    // unless "Use full UI width" is on. Using `min(..)` lets it scale
                    // DOWN on narrow windows instead of overflowing.
                    let avail = (ui.available_width() - 18.0).max(300.0);
                    let max_w = if self.general_config.full_width { avail } else { avail.min(743.0) };
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
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    scope_legend(ui);
                                });
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
                                if self.render_field(ui, &spec, field, res, preset_depth) {
                                    changed = true;
                                }
                            }
                        }
                        if any_section {
                            ui.add_space(10.0);
                            ui.label(egui::RichText::new(
                                "Colours mark where each value resolves from — Global, Profile, or this Game. \
                                 Left-click a checkbox to toggle, right-click to reset to inherited.",
                            ).color(theme::FAINT).small());
                        }
                    }
                });
            });
        });

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

impl GuiApp {
    /// Phase 2: editable manifest editor. Edits an in-memory [`ModuleDraft`] with a
    /// live preview and an explicit Save. Bundled modules render the same widgets
    /// but disabled (Save shows a "Fork to edit" tooltip). `id` is the module's
    /// `Extension::id()`; the draft is (re)loaded by [`ensure_draft`] each frame.
    fn render_module_editor(&mut self, ui: &mut egui::Ui, id: &str) {
        let touch = self.general_config.touch_mode;
        let full_width = self.general_config.full_width;
        let mut action = TopAction::None;

        // Transient dropped-var report from the last fork snapshot, dismissible.
        if let Some(msg) = self.carryover_report.clone() {
            let mut dismiss = false;
            editor_card(ui, |ui| {
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
        {
            let Some(draft) = self.module_draft.as_mut() else {
                ui.label(egui::RichText::new("This module is no longer available.").color(theme::DIM));
                return;
            };
            if draft.id != id {
                ui.label(egui::RichText::new("Loading\u{2026}").color(theme::DIM));
                return;
            }
            let editable = draft.editable;
            let dirty = draft.dirty();
            let snap = draft.snapshot();
            let validate_err = core_extension::validate(&snap).err().map(|e| e.to_string());
            let sections_unique = draft.sections_unique();
            let req_ok = all_requires_parse(&snap);
            let save_on = draft.save_enabled();
            let has_identity = draft.has_pending_identity();
            let identity_err = draft.identity_error.clone();

            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(&draft.ext.meta.name);
                ui.add_space(6.0);
                ui.label(
                    egui::RichText::new(format!("v{}", draft.ext.meta.version)).color(theme::FAINT),
                );
                ui.label(
                    egui::RichText::new(format!("by {}", draft.ext.meta.author)).color(theme::FAINT),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let mut save = ui.add_enabled(save_on, theme::primary_button("Save"));
                    if !editable {
                        save = save.on_hover_text("Fork to edit");
                    }
                    if save.clicked() {
                        action = TopAction::Save;
                    }
                    if ui
                        .add_enabled(dirty && editable, theme::secondary_button("Discard"))
                        .clicked()
                    {
                        action = TopAction::Discard;
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
                    if editable {
                        let rename = ui
                            .add_enabled(
                                has_identity && identity_err.is_none(),
                                theme::secondary_button("Rename"),
                            )
                            .on_hover_text(
                                "Apply the staged Author / Name / Variable changes \u{2014} migrates saved settings across all scopes",
                            );
                        if rename.clicked() {
                            action = TopAction::Rename;
                        }
                    }
                    if editable
                        && ui
                            .add(theme::danger_button("Delete"))
                            .on_hover_text("Delete this user module")
                            .clicked()
                    {
                        action = TopAction::Delete;
                    }
                });
            });

            // Status line under the header.
            if !editable {
                ui.label(
                    egui::RichText::new("Bundled module \u{2014} read-only. Fork to edit (coming soon).")
                        .color(theme::COL_GLOBAL)
                        .small(),
                );
            } else if dirty {
                ui.label(
                    egui::RichText::new(
                        "Unsaved changes \u{2014} config autosave paused until you Save or Discard.",
                    )
                    .color(theme::COL_PROFILE)
                    .small(),
                );
            }
            // Explain *why* Save is greyed for a schema problem. Duplicate section
            // names collapse when folded into the `IndexMap` before `validate` sees
            // them, so that case is caught by `sections_unique` separately.
            if editable && !sections_unique {
                ui.label(
                    egui::RichText::new(
                        "Cannot save: two UI sections share a name \u{2014} rename one.",
                    )
                    .color(theme::COL_GLOBAL)
                    .small(),
                );
            } else if editable {
                if let Some(reason) = &validate_err {
                    ui.label(
                        egui::RichText::new(format!("Cannot save: {reason}"))
                            .color(theme::COL_GLOBAL)
                            .small(),
                    );
                }
            }
            if editable && !req_ok {
                ui.label(
                    egui::RichText::new("A Requires expression does not parse.")
                        .color(theme::COL_GLOBAL)
                        .small(),
                );
            }
            // Pending-identity feedback: why Rename is blocked, or a ready prompt.
            if editable && has_identity {
                match &identity_err {
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
            ui.add_space(10.0);
            ui.separator();

            let mut deferred: Vec<Deferred> = Vec::new();
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .drag_to_scroll(touch)
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        let avail = (ui.available_width() - 18.0).max(300.0);
                        let max_w = if full_width { avail } else { avail.min(743.0) };
                        ui.set_max_width(max_w);
                        ui.add_enabled_ui(editable, |ui| {
                            render_editor_body(ui, draft, &mut deferred);
                        });
                    });
                });
            // Apply structural edits AFTER the render loop (never mid-iteration).
            for d in deferred {
                apply_deferred(draft, d);
            }
        }
        match action {
            TopAction::Save => self.save_module(),
            TopAction::Discard => self.discard_module(),
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
}

/// A labeled card matching the tint/rounding used elsewhere in the app, holding
/// the editable widgets of one field / block entry.
fn editor_card(ui: &mut egui::Ui, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::none()
        .fill(theme::FIELD)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(6.0);
}

/// A `+`-prefixed secondary button used for "add" affordances.
fn add_button(ui: &mut egui::Ui, label: &str) -> egui::Response {
    ui.add(theme::secondary_button(format!("\u{f067} {label}")))
}

/// The reusable `[↑][↓][🗑]` row-action widget. Returns the action the user
/// clicked; the caller records a path-addressed [`Deferred`] and applies the
/// container-correct op after the render loop.
fn row_actions(ui: &mut egui::Ui, idx: usize, len: usize) -> RowAction {
    let mut a = RowAction::None;
    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
        if ui.add(theme::danger_button("\u{f1f8}")).on_hover_text("Remove").clicked() {
            a = RowAction::Remove;
        }
        if ui
            .add_enabled(idx + 1 < len, theme::secondary_button("\u{f063}"))
            .on_hover_text("Move down")
            .clicked()
        {
            a = RowAction::Down;
        }
        if ui
            .add_enabled(idx > 0, theme::secondary_button("\u{f062}"))
            .on_hover_text("Move up")
            .clicked()
        {
            a = RowAction::Up;
        }
    });
    a
}

/// Edit an `Option<String>` as a single line (empty text → `None`).
fn opt_text_edit(ui: &mut egui::Ui, val: &mut Option<String>, hint: &str) {
    let mut s = val.clone().unwrap_or_default();
    ui.add(
        egui::TextEdit::singleline(&mut s)
            .hint_text(hint)
            .desired_width(f32::INFINITY),
    );
    *val = if s.is_empty() { None } else { Some(s) };
}

/// A raw single-line `Requires` editor, live-validated via `condition::parse`.
/// The text turns red on a parse error; the wordy error is shown only once the
/// field is not actively being edited.
fn requires_edit(ui: &mut egui::Ui, val: &mut Option<String>) {
    let mut s = val.clone().unwrap_or_default();
    let parsed = condition::parse(&s);
    let ok = s.trim().is_empty() || parsed.is_ok();
    let color = if ok { theme::DIM } else { theme::COL_GLOBAL };
    let resp = ui.add(
        egui::TextEdit::singleline(&mut s)
            .hint_text("Requires (optional)")
            .text_color(color)
            .desired_width(f32::INFINITY),
    );
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
fn render_editor_body(ui: &mut egui::Ui, draft: &mut ModuleDraft, deferred: &mut Vec<Deferred>) {
    let ModuleDraft {
        ext,
        sections,
        baseline_vars,
        identity,
        ..
    } = draft;

    // ── Meta ────────────────────────────────────────────────────────────────
    ui.add_space(6.0);
    editor_card(ui, |ui| {
        ui.label(egui::RichText::new("Module").color(theme::TEXT).strong());
        // Author + Name are STAGED identity edits: they mutate `identity`, never
        // `ext.meta`, so they don't touch the draft snapshot / Save gate and are
        // committed only through the Rename button.
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Author").color(theme::DIM).small());
                ui.add(
                    egui::TextEdit::singleline(&mut identity.author).desired_width(220.0),
                );
            });
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(egui::RichText::new("Name").color(theme::DIM).small());
                ui.add(egui::TextEdit::singleline(&mut identity.name).desired_width(220.0));
            });
        });
        ui.label(
            egui::RichText::new(
                "Author & Name are applied via Rename (migrates saved settings). Version is fixed.",
            )
            .color(theme::FAINT)
            .small(),
        );
        ui.add_space(6.0);
        ui.label(egui::RichText::new("Description").color(theme::DIM).small());
        opt_text_edit(ui, &mut ext.meta.description, "Description");
        ui.add_space(4.0);
        ui.label(egui::RichText::new("Backend (advanced, optional)").color(theme::DIM).small());
        opt_text_edit(ui, &mut ext.backend, "Backend");
    });

    // ── UI sections ─────────────────────────────────────────────────────────
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(theme::section_label("UI Sections"));
        if add_button(ui, "Add section").clicked() {
            deferred.push(Deferred::SectionAdd);
        }
    });
    ui.add_space(6.0);
    let sec_len = sections.len();
    for (si, (name, fields)) in sections.iter_mut().enumerate() {
        editor_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(name).desired_width(220.0));
                let a = row_actions(ui, si, sec_len);
                if a != RowAction::None {
                    deferred.push(Deferred::Section(si, a));
                }
            });
            ui.add_space(4.0);
            let f_len = fields.len();
            for (fi, field) in fields.iter_mut().enumerate() {
                render_field_editor(
                    ui,
                    field,
                    si,
                    fi,
                    f_len,
                    baseline_vars,
                    &mut identity.var_edits,
                    deferred,
                );
            }
            if add_button(ui, "Add field").clicked() {
                deferred.push(Deferred::FieldAdd(si));
            }
        });
    }

    // ── Output blocks ─────────────────────────────────────────────────────────
    render_env_block_editor(ui, "ENV_VARS", false, &mut ext.env_vars, deferred);
    render_env_block_editor(ui, "GAME_ENV_VARS", true, &mut ext.game_env_vars, deferred);
    render_wrapper_block_editor(ui, &mut ext.wrappers, deferred);
    render_arg_block_editor(ui, &mut ext.game_launch_args, deferred);
}

/// One editable UI-field card. An *existing* field's `Variable` (present on disk)
/// is renamed as a **staged identity edit** — the buffer lives in
/// `var_edits[current-name]`, so `field.variable` itself is untouched (no draft
/// dirtiness) until the Rename action migrates config and rewrites the manifest.
/// A newly added field's `Variable` is edited directly (no config to migrate).
fn render_field_editor(
    ui: &mut egui::Ui,
    field: &mut UiField,
    si: usize,
    fi: usize,
    f_len: usize,
    baseline_vars: &std::collections::HashSet<String>,
    var_edits: &mut IndexMap<String, String>,
    deferred: &mut Vec<Deferred>,
) {
    editor_card(ui, |ui| {
        ui.horizontal(|ui| {
            let mut name = field.name.clone().unwrap_or_default();
            ui.add(
                egui::TextEdit::singleline(&mut name)
                    .hint_text("Field label")
                    .desired_width(220.0),
            );
            field.name = if name.is_empty() { None } else { Some(name) };
            let a = row_actions(ui, fi, f_len);
            if a != RowAction::None {
                deferred.push(Deferred::Field(si, fi, a));
            }
        });

        // Variable — for an existing (on-disk) field this edits the STAGED rename
        // buffer (committed via Rename); a newly added field edits it directly.
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Variable").color(theme::DIM).small());
            if baseline_vars.contains(&field.variable) {
                let key = field.variable.clone();
                let buf = var_edits.entry(key.clone()).or_insert_with(|| key.clone());
                ui.add(egui::TextEdit::singleline(buf).desired_width(200.0));
                if buf.trim() != key {
                    ui.label(
                        egui::RichText::new("(rename pending)")
                            .color(theme::COL_PROFILE)
                            .small(),
                    );
                }
            } else {
                ui.add(egui::TextEdit::singleline(&mut field.variable).desired_width(200.0));
            }
        });

        // Type.
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Type").color(theme::DIM).small());
            egui::ComboBox::from_id_salt((si, fi, "type"))
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

        opt_text_edit(ui, &mut field.description, "Description");
        requires_edit(ui, &mut field.requires);

        // Type-specific detail.
        match field.field_type {
            FieldType::Selection => {
                ui.label(egui::RichText::new("Options").color(theme::DIM).small());
                let opts = ensure_list(&mut field.options);
                let ol = opts.len();
                for (oi, opt) in opts.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(egui::TextEdit::singleline(opt).desired_width(200.0));
                        let a = row_actions(ui, oi, ol);
                        if a != RowAction::None {
                            deferred.push(Deferred::FieldOpt(si, fi, oi, a));
                        }
                    });
                }
                if add_button(ui, "Add option").clicked() {
                    deferred.push(Deferred::FieldOptAdd(si, fi));
                }
            }
            FieldType::Integer | FieldType::Float => {
                let (mut min, mut max, mut step) = number_range(field);
                let mut ch = false;
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Min").color(theme::DIM).small());
                    ch |= ui.add(egui::DragValue::new(&mut min)).changed();
                    ui.label(egui::RichText::new("Max").color(theme::DIM).small());
                    ch |= ui.add(egui::DragValue::new(&mut max)).changed();
                    ui.label(egui::RichText::new("Step").color(theme::DIM).small());
                    ch |= ui.add(egui::DragValue::new(&mut step)).changed();
                });
                if ch {
                    field.options = Some(OptionsSpec::Range { min, max, step: Some(step) });
                }
            }
            FieldType::Toggle => {
                let mut def = field.default.as_ref().and_then(|v| v.as_bool()).unwrap_or(false);
                if ui.checkbox(&mut def, "Default on").changed() {
                    field.default = Some(json!(def));
                }
            }
            FieldType::String | FieldType::MultiString => {}
        }
    });
}

/// ENV_VARS / GAME_ENV_VARS editor. `game` selects which block (for deferral).
fn render_env_block_editor(
    ui: &mut egui::Ui,
    title: &str,
    game: bool,
    specs: &mut [EnvVarSpec],
    deferred: &mut Vec<Deferred>,
) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(theme::section_label(title));
        if add_button(ui, "Add variable").clicked() {
            deferred.push(Deferred::EnvAdd(game));
        }
    });
    ui.add_space(6.0);
    let len = specs.len();
    for (ei, spec) in specs.iter_mut().enumerate() {
        editor_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Name").color(theme::DIM).small());
                ui.add(egui::TextEdit::singleline(&mut spec.name).desired_width(220.0));
                let a = row_actions(ui, ei, len);
                if a != RowAction::None {
                    deferred.push(Deferred::Env(game, ei, a));
                }
            });
            requires_edit(ui, &mut spec.requires);
            let sl = spec.builder.len();
            for (bi, step) in spec.builder.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    egui::ComboBox::from_id_salt((game, ei, bi, "op"))
                        .selected_text(env_op_label(step.op))
                        .width(80.0)
                        .show_ui(ui, |ui| {
                            for op in [EnvOp::Set, EnvOp::Append, EnvOp::Unset] {
                                ui.selectable_value(&mut step.op, op, env_op_label(op));
                            }
                        });
                    let mut v = step.value.clone().unwrap_or_default();
                    ui.add(
                        egui::TextEdit::singleline(&mut v)
                            .hint_text("Value")
                            .desired_width(160.0),
                    );
                    step.value = if v.is_empty() { None } else { Some(v) };
                    let mut sep = step.separator.clone().unwrap_or_default();
                    ui.label(egui::RichText::new("Sep").color(theme::DIM).small());
                    ui.add(egui::TextEdit::singleline(&mut sep).desired_width(48.0));
                    step.separator = if sep.is_empty() { None } else { Some(sep) };
                    let a = row_actions(ui, bi, sl);
                    if a != RowAction::None {
                        deferred.push(Deferred::EnvStep(game, ei, bi, a));
                    }
                });
                requires_edit(ui, &mut step.requires);
            }
            if add_button(ui, "Add step").clicked() {
                deferred.push(Deferred::EnvStepAdd(game, ei));
            }
        });
    }
}

/// WRAPPERS editor: CommandSyntax, an editable Priority (lower = outermost), and
/// per-step option values.
fn render_wrapper_block_editor(
    ui: &mut egui::Ui,
    wrappers: &mut [WrapperSpec],
    deferred: &mut Vec<Deferred>,
) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(theme::section_label("WRAPPERS"));
        if add_button(ui, "Add wrapper").clicked() {
            deferred.push(Deferred::WrapperAdd);
        }
    });
    ui.add_space(6.0);
    let len = wrappers.len();
    for (wi, w) in wrappers.iter_mut().enumerate() {
        editor_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut w.command_syntax)
                        .hint_text("gamescope {OPTIONS} --")
                        .desired_width(260.0),
                );
                let a = row_actions(ui, wi, len);
                if a != RowAction::None {
                    deferred.push(Deferred::Wrapper(wi, a));
                }
            });
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new("Priority (lower = outermost)").color(theme::DIM).small());
                ui.add(egui::DragValue::new(&mut w.priority));
            });
            requires_edit(ui, &mut w.requires);
            let sl = w.builder.len();
            for (bi, step) in w.builder.iter_mut().enumerate() {
                ui.horizontal(|ui| {
                    ui.add(
                        egui::TextEdit::singleline(&mut step.value)
                            .hint_text("Option")
                            .desired_width(200.0),
                    );
                    let a = row_actions(ui, bi, sl);
                    if a != RowAction::None {
                        deferred.push(Deferred::WrapperStep(wi, bi, a));
                    }
                });
                requires_edit(ui, &mut step.requires);
            }
            if add_button(ui, "Add option").clicked() {
                deferred.push(Deferred::WrapperStepAdd(wi));
            }
        });
    }
}

/// GAME_LAUNCH_ARGS editor: one Value + Requires per arg.
fn render_arg_block_editor(
    ui: &mut egui::Ui,
    args: &mut [ArgSpec],
    deferred: &mut Vec<Deferred>,
) {
    ui.add_space(12.0);
    ui.horizontal(|ui| {
        ui.label(theme::section_label("GAME_LAUNCH_ARGS"));
        if add_button(ui, "Add argument").clicked() {
            deferred.push(Deferred::ArgAdd);
        }
    });
    ui.add_space(6.0);
    let len = args.len();
    for (ai, arg) in args.iter_mut().enumerate() {
        editor_card(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut arg.value)
                        .hint_text("Argument")
                        .desired_width(260.0),
                );
                let a = row_actions(ui, ai, len);
                if a != RowAction::None {
                    deferred.push(Deferred::Arg(ai, a));
                }
            });
            requires_edit(ui, &mut arg.requires);
        });
    }
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

    fn render_field(
        &mut self,
        ui: &mut egui::Ui,
        spec: &Extension,
        field: &UiField,
        res: &resolve::ResolvedField,
        preset_depth: Option<usize>,
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
                return self.render_env_pair_field(ui, field, scope);
            }
            return self.render_multi_string_field(ui, field, scope);
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
                    if let Some(new) =
                        scope_checkbox(ui, &label, &hover, state, res.var.truthy, scope, badge)
                    {
                        self.apply_tri(field, res, new);
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
                        let editable = state == Tri::Enabled;
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_enabled_ui(editable, |ui| {
                                changed |= self.render_value_editor(ui, spec, field, res, scope, editable);
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
    fn render_multi_string_field(&mut self, ui: &mut egui::Ui, field: &UiField, scope: Color32) -> bool {
        let Some(spec) = self.cur_specs.get(self.selected_ext) else { return false };
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
        let mut entries = self.multi_edit.remove(&key).unwrap_or(stored.clone());

        let tint = Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16);
        let row_h = ui.spacing().interact_size.y;
        let btn_w = row_h + ui.spacing().item_spacing.x;
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
                                .hint_text(egui::RichText::new("command").color(theme::FAINT)),
                        );
                        if icon_button(ui, row_h, "\u{f467}", theme::COL_GLOBAL).clicked() {
                            to_delete = Some(i);
                        }
                    });
                }
                if ui.button("+ Add").clicked() {
                    entries.push(String::new());
                }
            });

        if let Some(i) = to_delete {
            entries.remove(i);
        }

        // Persist the non-empty entries when they differ from what's stored.
        let cleaned: Vec<String> = entries.iter().filter(|s| !s.trim().is_empty()).cloned().collect();
        let mut changed = false;
        if cleaned != stored {
            if cleaned.is_empty() {
                self.unset_current(field);
            } else {
                self.set_current(field, json!(cleaned));
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

        self.multi_edit.insert(key, entries);
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
    fn render_env_pair_field(&mut self, ui: &mut egui::Ui, field: &UiField, scope: Color32) -> bool {
        let Some(spec) = self.cur_specs.get(self.selected_ext) else { return false };
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
        let mut entries = self.multi_edit.remove(&key).unwrap_or(stored.clone());

        let tint = Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16);
        let row_h = ui.spacing().interact_size.y;
        let btn_w = row_h + ui.spacing().item_spacing.x;
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
                                .hint_text(egui::RichText::new("value").color(theme::FAINT)),
                        );
                        if icon_button(ui, row_h, "\u{f467}", theme::COL_GLOBAL).clicked() {
                            to_delete = Some(i);
                        }
                    });
                    *entry = format!("{n}={v}");
                }
                if ui.button("+ Add").clicked() {
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
        if cleaned != stored {
            if cleaned.is_empty() {
                self.unset_current(field);
            } else {
                self.set_current(field, json!(cleaned));
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

        self.multi_edit.insert(key, entries);
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

    /// Apply a new tri-state to the current field's stored value.
    fn apply_tri(&mut self, field: &UiField, res: &resolve::ResolvedField, new: Tri) {
        match new {
            Tri::Unset => self.unset_current(field),
            Tri::Disabled => {
                let val = if field.field_type == FieldType::Toggle {
                    json!(false)
                } else {
                    Value::Null
                };
                self.set_current(field, val);
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
                self.set_current(field, val);
            }
        }
    }

    /// Value editor for a non-toggle field. When `editable` is false the widgets
    /// are drawn read-only (the caller has disabled the ui) and just display the
    /// effective value, so no persistent edit buffer is touched.
    fn render_value_editor(
        &mut self,
        ui: &mut egui::Ui,
        spec: &Extension,
        field: &UiField,
        res: &resolve::ResolvedField,
        scope: Color32,
        editable: bool,
    ) -> bool {
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
                if let Some(v) = picked {
                    self.set_current(field, json!(v));
                    changed = true;
                }
            }
            FieldType::Integer | FieldType::Float => {
                let integer = field.field_type == FieldType::Integer;
                let (min, max, step) = number_range(field);
                let v: f64 = res.var.value.parse().unwrap_or(min);
                if let Some(nv) = scope_slider(ui, v, min, max, step, integer, scope) {
                    self.set_current(field, num_value(nv, integer));
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
                        self.set_current(field, json!(v));
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

    /// Set a value for a field on the *currently active edit target*.
    fn set_current(&mut self, field: &UiField, value: Value) {
        let Some(spec) = self.cur_specs.get(self.selected_ext) else { return };
        let (a, n) = (spec.meta.author.clone(), spec.meta.name.clone());
        match &self.nav_sel.clone() {
            NavSel::GlobalSettings => {
                self.global_config.set_value(&a, &n, &field.variable, value);
            }
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf {
                    p.set_value(&a, &n, &field.variable, value);
                }
            }
            NavSel::Game(_) | NavSel::GeneralSettings | NavSel::ModuleEditor(_) => {
                self.game_config.set_value(&a, &n, &field.variable, value);
            }
        }
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
        });
    }

    /// Apply a finished detection (if any). Returns true if the config changed.
    fn poll_detect(&mut self, ctx: &egui::Context) -> bool {
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
            self.game_config.set_value(&author, &name, &var, json!(c.clone()));
            self.text_buffers.insert(format!("{ext_id}::{var}"), c);
            return true;
        }
        false
    }

    /// Remove an override for a field on the *currently active edit target* (reset to inherit).
    fn unset_current(&mut self, field: &UiField) {
        let Some(spec) = self.cur_specs.get(self.selected_ext) else { return };
        let (a, n) = (spec.meta.author.clone(), spec.meta.name.clone());
        self.text_buffers.remove(&format!("{}::{}", spec.id(), field.variable));
        match &self.nav_sel.clone() {
            NavSel::GlobalSettings => {
                self.global_config.unset_value(&a, &n, &field.variable);
            }
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf {
                    p.unset_value(&a, &n, &field.variable);
                }
            }
            NavSel::Game(_) | NavSel::GeneralSettings | NavSel::ModuleEditor(_) => {
                self.game_config.unset_value(&a, &n, &field.variable);
            }
        }
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
        // Cap the row width like the module panel: fill the pane (less an 18px
        // gutter), but cap at ~743px unless "Use full UI width" is on.
        let avail = (ui.available_width() - 18.0).max(300.0);
        let max_w = if self.general_config.full_width { avail } else { avail.min(743.0) };
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
        // Always show the bottom band (it's empty for General/Global Settings).
        egui::TopBottomPanel::bottom("nav_settings")
            .exact_height(198.0)
            .show_separator_line(true)
            .frame(egui::Frame::none()
                .fill(theme::PANEL2)
                .inner_margin(egui::Margin::same(14.0)))
            .show_inside(ui, |ui| {
                self.render_nav_settings(ui);
            });
        egui::TopBottomPanel::top("nav_header")
            .show_separator_line(false)
            .frame(egui::Frame::none()
                .fill(theme::PANEL)
                .inner_margin(egui::Margin { left: 16.0, right: 16.0, top: 14.0, bottom: 8.0 }))
            .show_inside(ui, |ui| {
                ui.label(theme::header_label("Profiles / Games"));
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
                        self.render_nav_tree(ui);
                    });
            });
    }

    fn render_nav_tree(&mut self, ui: &mut egui::Ui) {
        // Collect any nav action to apply after rendering (avoids borrow conflicts).
        enum NavAction {
            SelectGeneral,
            SelectGlobal,
            SelectProfile(String),
            SelectGame(String, String), // appid, name
            StartCreateProfile,
            StartCreateGame,
        }
        let mut action: Option<NavAction> = None;

        let sep = icon_sep(self.general_config.mono_ui);
        let gap = if self.general_config.mono_ui { 8.0 } else { 7.0 };
        let is_general = self.nav_sel == NavSel::GeneralSettings;
        if full_selectable(ui, is_general, egui::RichText::new(format!("\u{f013}{sep}General Settings")).color(theme::FAINT)).clicked() {
            action = Some(NavAction::SelectGeneral);
        }
        let is_global = self.nav_sel == NavSel::GlobalSettings;
        if full_selectable(ui, is_global, egui::RichText::new(format!("\u{f0ac}{sep}Global Settings")).color(COL_GLOBAL)).clicked() {
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
            Some(NavAction::SelectGeneral) => {
                self.nav_sel = NavSel::GeneralSettings;
                self.creating_profile = false;
                self.duplicating_preset = None;
                self.creating_game = false;
                self.text_buffers.clear();
            }
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

    /// Delete a profile and return to Global Settings. Any game referencing it
    /// falls back to no profile.
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
    fn render_ext_tree(&mut self, ui: &mut egui::Ui) {
        // Build per-extension icon lists: each entry is (icon_glyph, color).
        // Inheritance from a lower layer → ICON_INHERIT; edit at current scope → ICON_EDIT.
        let resolution = self.resolve_for_editing();
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
        let icon_lists: Vec<Vec<(&'static str, Color32)>> = if !self.show_inheritance {
            vec![Vec::new(); self.cur_specs.len()]
        } else { self.cur_specs.iter().map(|spec| {
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
        for (i, spec) in self.cur_specs.iter().enumerate() {
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
                    .entry(self.cur_specs[i].meta.author.clone())
                    .or_default()
                    .push(i);
            }
            for (author, items) in &by_author {
                egui::CollapsingHeader::new(egui::RichText::new(author).color(theme::ACCENT).strong())
                    .default_open(true)
                    .show(ui, |ui| {
                        for &i in items {
                            leaf(ui, i, &self.cur_specs, &icon_lists, &mut self.selected_ext, mono);
                        }
                    });
            }
        } else {
            let mut root = TreeNode::default();
            for &i in &others {
                let comps: Vec<String> = self.cur_dirs[i]
                    .iter()
                    .map(|c| c.to_string_lossy().into_owned())
                    .collect();
                root.insert(&comps, i);
            }
            render_node(ui, &root, &self.cur_specs, &self.cur_is_folder_ext, &icon_lists, &mut self.selected_ext, mono);
        }

        if !builtins.is_empty() {
            egui::CollapsingHeader::new(egui::RichText::new("Built-In").color(theme::ACCENT).strong())
                .default_open(true)
                .show(ui, |ui| {
                    for &i in &builtins {
                        leaf(ui, i, &self.cur_specs, &icon_lists, &mut self.selected_ext, mono);
                    }
                });
        }
    }
}

/// Render a tree node: subfolders (sorted) first, then extensions at this level.
/// A child folder whose single leaf has `is_folder_ext` set is collapsed into
/// the leaf directly (no CollapsingHeader wrapper).
fn render_node(ui: &mut egui::Ui, node: &TreeNode, specs: &[Extension], is_folder_ext: &[bool], icon_lists: &[Vec<(&'static str, Color32)>], selected: &mut usize, mono: bool) {
    let mut leaves: Vec<usize> = node.leaves.clone();
    for (name, child) in &node.children {
        // ponytail: skip the subfolder header when the folder IS the extension
        if child.children.is_empty() && child.leaves.len() == 1 && is_folder_ext.get(child.leaves[0]).copied().unwrap_or(false) {
            leaves.push(child.leaves[0]);
        } else {
            egui::CollapsingHeader::new(egui::RichText::new(name).color(theme::ACCENT).strong())
                .default_open(true)
                .show(ui, |ui| render_node(ui, child, specs, is_folder_ext, icon_lists, selected, mono));
        }
    }
    leaves.sort_by(|&a, &b| specs[a].meta.name.cmp(&specs[b].meta.name));
    for &i in &leaves {
        leaf(ui, i, specs, icon_lists, selected, mono);
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

/// A single selectable extension leaf.
/// Colored icons prefix the name: ICON_INHERIT for inherited layers, ICON_EDIT for
/// the current editing scope. The name itself stays the default UI color.
fn leaf(ui: &mut egui::Ui, i: usize, specs: &[Extension], icon_lists: &[Vec<(&'static str, Color32)>], selected: &mut usize, mono: bool) {
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
        let font = egui::TextStyle::Body.resolve(ui.style());
        let r = resp.rect;
        ui.painter().text(
            egui::pos2(r.right() - 11.0, r.center().y),
            egui::Align2::RIGHT_CENTER,
            "\u{f013}",
            font,
            theme::FAINT,
        );
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

/// A square icon button that paints the glyph centered on its *ink* rect (Nerd
/// Font glyphs have asymmetric side bearings, so the default advance-box
/// centering looks off). Uses the standard widget background/hover.
fn icon_button(ui: &mut egui::Ui, size: f32, glyph: &str, color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = *ui.style().interact(&resp);
        let painter = ui.painter();
        painter.rect(rect, visuals.rounding, visuals.weak_bg_fill, visuals.bg_stroke);
        let galley =
            painter.layout_no_wrap(glyph.to_owned(), egui::FontId::proportional(13.0), color);
        let ink_center = galley
            .rows
            .first()
            .and_then(|row| row.glyphs.first())
            .map(|g| g.pos.to_vec2() + g.uv_rect.offset + g.uv_rect.size * 0.5)
            .unwrap_or_else(|| galley.size() * 0.5);
        painter.galley(rect.center() - ink_center, galley, color);
    }
    resp
}

/// Lay out and sense a single clickable `[box] label` row: the whole rect (box +
/// gap + label) is one click target with a subtle hover background. `paint_box`
/// draws the 18×18 box. Returns the row's response.
fn checkbox_row(
    ui: &mut egui::Ui,
    label: &str,
    badge: Option<(&str, Color32)>,
    paint_box: impl FnOnce(&egui::Painter, egui::Rect),
) -> egui::Response {
    const BOX: f32 = 18.0;
    const GAP: f32 = 7.0;
    let font = egui::TextStyle::Body.resolve(ui.style());
    let badge_galley = badge.map(|(text, col)| ui.painter().layout_no_wrap(text.to_owned(), font.clone(), col));
    let badge_w = badge_galley.as_ref().map(|g| g.size().x + GAP).unwrap_or(0.0);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, theme::TEXT);
    let size = egui::vec2(BOX + GAP + badge_w + galley.size().x, BOX.max(galley.size().y));
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if resp.hovered() {
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
            checkbox_row(ui, label, None, |p, r| paint_scope_box(p, r, on, false, theme::COL_DEFAULT));
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
    let resp = checkbox_row(ui, label, None, |p, r| paint_scope_box(p, r, on, false, COL_PROFILE));
    if resp.clicked() {
        *checked = !*checked;
    }
    resp
}

/// A mockup-style checkbox over the tri-state model, with the label as part of
/// the click target:
/// - **left-click** toggles Enabled ⇄ Disabled (flipping from the current
///   effective on/off, so the first click on an inherited value sets the opposite)
/// - **right-click** resets to Unset (inherit from a lower scope)
fn scope_checkbox(
    ui: &mut egui::Ui,
    label: &str,
    hover: &str,
    state: Tri,
    inherited_on: bool,
    scope: Color32,
    badge: Option<(&str, Color32)>,
) -> Option<Tri> {
    let on = match state {
        Tri::Enabled => true,
        Tri::Disabled => false,
        Tri::Unset => inherited_on,
    };
    let faded = state == Tri::Unset;
    let mut resp = checkbox_row(ui, label, badge, |p, r| paint_scope_box(p, r, on, faded, scope));
    if !hover.is_empty() {
        resp = resp.on_hover_text(hover);
    }

    if resp.secondary_clicked() {
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
