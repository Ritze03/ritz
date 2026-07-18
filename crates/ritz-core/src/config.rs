//! On-disk configuration: `general.json`, `presets/<name>.json`, `games/<appid>.json`.
//!
//! Storage convention for variable values:
//! - present non-null value = explicit override (enabled, with that value)
//! - `null`                 = explicitly disabled (overrides an inherited-on)
//! - absent                 = inherit from the layer below
//!
//! Nested module layout:
//! `Modules → <Author> → <ExtName> → <var> = value`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{Result, RitzError};

pub type VarMap = IndexMap<String, Value>;
pub type ExtMap = IndexMap<String, VarMap>;
pub type AuthorsMap = IndexMap<String, ExtMap>;

// ---- general.json --------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum InheritanceDisplayMode {
    /// One arrow per level, hue/value shifted darker the deeper the ancestor.
    #[default]
    Color,
    /// Numeric labels (1, 2, …) per depth; single arrow when only one level contributes.
    Numbers,
    /// Plain green arrows only — stacked in the nav tree, single in the module tree.
    ArrowsOnly,
}

fn default_splash() -> u64 {
    3
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    #[serde(rename = "SplashTimeoutSecs", default = "default_splash")]
    pub splash_timeout_secs: u64,
    #[serde(rename = "DefaultPreset", default)]
    pub default_preset: Option<String>,
    /// Render the whole UI in the monospace font (default) vs proportional sans.
    #[serde(rename = "MonoUi", default = "default_true")]
    pub mono_ui: bool,
    /// Touch mode: allow dragging content to scroll (off by default).
    #[serde(rename = "TouchMode", default)]
    pub touch_mode: bool,
    /// Let the settings rows fill the whole pane instead of capping their width.
    #[serde(rename = "FullWidth", default)]
    pub full_width: bool,
    /// What closing the launch-mode editor (window X) does: true = launch the
    /// game, false = cancel the launch.
    #[serde(rename = "EditorCloseLaunches", default = "default_true")]
    pub editor_close_launches: bool,
    /// Auto-size the launch command preview box to its content instead of fixed height.
    #[serde(rename = "DynamicPreview", default)]
    pub dynamic_preview: bool,
    /// How inherited values are indicated in the module and nav trees.
    #[serde(rename = "InheritanceDisplay", default)]
    pub inheritance_display: InheritanceDisplayMode,
    /// Tint or label fields in the module settings panel by chain depth.
    #[serde(rename = "ShowFieldInheritance", default = "default_true")]
    pub show_field_inheritance: bool,
}

impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            splash_timeout_secs: default_splash(),
            default_preset: None,
            mono_ui: true,
            touch_mode: false,
            full_width: false,
            editor_close_launches: true,
            dynamic_preview: false,
            inheritance_display: InheritanceDisplayMode::Color,
            show_field_inheritance: true,
        }
    }
}

// ---- presets/<name>.json -------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Preset {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Modules", default)]
    pub modules: AuthorsMap,
    /// If pinned, a 1–10 slot id used to show the profile at the top of the tree.
    #[serde(rename = "Pin", default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<u8>,
    /// Optional parent profile — its values form a layer below this profile's own.
    #[serde(rename = "Parent", default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
}

impl Preset {
    pub fn get_value(&self, author: &str, ext: &str, var: &str) -> Option<&Value> {
        lookup(&self.modules, author, ext, var)
    }

    pub fn set_value(&mut self, author: &str, ext: &str, var: &str, value: Value) {
        self.modules
            .entry(author.to_string()).or_default()
            .entry(ext.to_string()).or_default()
            .insert(var.to_string(), value);
    }

    pub fn unset_value(&mut self, author: &str, ext: &str, var: &str) {
        let Some(exts) = self.modules.get_mut(author) else { return };
        if let Some(vars) = exts.get_mut(ext) {
            vars.shift_remove(var);
            if vars.is_empty() { exts.shift_remove(ext); }
        }
        if exts.is_empty() { self.modules.shift_remove(author); }
    }
}

// ---- games/<appid>.json --------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameConfig {
    #[serde(rename = "Game")]
    pub game: GameMeta,
    #[serde(rename = "Config", default)]
    pub config: GameBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GameMeta {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "AppId")]
    pub appid: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GameBody {
    #[serde(rename = "General", default)]
    pub general: GeneralOverrides,
    #[serde(rename = "Modules", default)]
    pub modules: Modules,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GeneralOverrides {
    #[serde(
        rename = "SplashTimeoutSecs",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub splash_timeout_secs: Option<u64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Modules {
    #[serde(rename = "Preset", default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    #[serde(flatten)]
    pub authors: AuthorsMap,
}

impl GameConfig {
    pub fn new(appid: impl Into<String>, name: impl Into<String>) -> Self {
        Self {
            game: GameMeta {
                name: name.into(),
                appid: appid.into(),
            },
            config: GameBody::default(),
        }
    }

    pub fn get_value(&self, author: &str, ext: &str, var: &str) -> Option<&Value> {
        lookup(&self.config.modules.authors, author, ext, var)
    }

    /// Set (override) a variable value.
    pub fn set_value(&mut self, author: &str, ext: &str, var: &str, value: Value) {
        self.config
            .modules
            .authors
            .entry(author.to_string())
            .or_default()
            .entry(ext.to_string())
            .or_default()
            .insert(var.to_string(), value);
    }

    /// Remove an override (reset-to-inherit); prunes now-empty parent maps.
    pub fn unset_value(&mut self, author: &str, ext: &str, var: &str) {
        let authors = &mut self.config.modules.authors;
        let Some(exts) = authors.get_mut(author) else {
            return;
        };
        if let Some(vars) = exts.get_mut(ext) {
            vars.shift_remove(var);
            if vars.is_empty() {
                exts.shift_remove(ext);
            }
        }
        if exts.is_empty() {
            authors.shift_remove(author);
        }
    }
}

/// Look up a stored variable override (`author` → `ext` → `var`).
fn lookup<'a>(authors: &'a AuthorsMap, author: &str, ext: &str, var: &str) -> Option<&'a Value> {
    authors.get(author)?.get(ext)?.get(var)
}

/// Migrate the old version-nested layout (`… → <ExtName> → <Version> → vars`) to
/// the flat one (`… → <ExtName> → vars`). A module's stored value is detected as
/// old-format when every entry is itself an object (variable values are always
/// scalars/null), in which case the per-version maps are merged (last wins).
fn migrate_authors(authors: &mut AuthorsMap) {
    for exts in authors.values_mut() {
        for vars in exts.values_mut() {
            if !vars.is_empty() && vars.values().all(Value::is_object) {
                let mut flat = VarMap::new();
                for inner in vars.values() {
                    if let Some(obj) = inner.as_object() {
                        for (k, v) in obj {
                            flat.insert(k.clone(), v.clone());
                        }
                    }
                }
                *vars = flat;
            }
        }
    }
}

// ---- Paths + IO ----------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Paths {
    pub base: PathBuf,
}

impl Paths {
    /// `$RITZ_CONFIG_DIR` if set, else `~/.config/ritz`.
    pub fn discover() -> Self {
        let base = std::env::var_os("RITZ_CONFIG_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                dirs::config_dir()
                    .unwrap_or_else(|| PathBuf::from("."))
                    .join("ritz")
            });
        Self { base }
    }

    pub fn general(&self) -> PathBuf {
        self.base.join("general.json")
    }
    pub fn presets_dir(&self) -> PathBuf {
        self.base.join("profiles")
    }
    pub fn preset(&self, name: &str) -> PathBuf {
        self.presets_dir().join(format!("{name}.json"))
    }
    pub fn games_dir(&self) -> PathBuf {
        self.base.join("games")
    }
    pub fn game(&self, appid: &str) -> PathBuf {
        self.games_dir().join(format!("{appid}.json"))
    }
    pub fn global_config(&self) -> PathBuf {
        self.base.join("global.json")
    }
    pub fn user_extensions(&self) -> PathBuf {
        self.base.join("extensions")
    }
    pub fn plugins_dir(&self) -> PathBuf {
        self.base.join("plugins")
    }

    pub fn load_global_config(&self) -> Result<Preset> {
        let mut cfg: Preset = read_json_opt(&self.global_config())?.unwrap_or_default();
        migrate_authors(&mut cfg.modules);
        Ok(cfg)
    }

    pub fn save_global_config(&self, cfg: &Preset) -> Result<()> {
        write_json(&self.global_config(), cfg)
    }

    pub fn load_general(&self) -> Result<GeneralConfig> {
        match read_json_opt(&self.general())? {
            Some(c) => Ok(c),
            None => Ok(GeneralConfig::default()),
        }
    }

    pub fn save_general(&self, cfg: &GeneralConfig) -> Result<()> {
        write_json(&self.general(), cfg)
    }

    pub fn load_game(&self, appid: &str) -> Result<Option<GameConfig>> {
        let mut cfg: Option<GameConfig> = read_json_opt(&self.game(appid))?;
        if let Some(c) = cfg.as_mut() {
            migrate_authors(&mut c.config.modules.authors);
        }
        Ok(cfg)
    }

    pub fn save_game(&self, cfg: &GameConfig) -> Result<()> {
        write_json(&self.game(&cfg.game.appid), cfg)
    }

    pub fn load_preset(&self, name: &str) -> Result<Option<Preset>> {
        let mut cfg: Option<Preset> = read_json_opt(&self.preset(name))?;
        if let Some(c) = cfg.as_mut() {
            migrate_authors(&mut c.modules);
        }
        Ok(cfg)
    }

    pub fn save_preset(&self, preset: &Preset) -> Result<()> {
        write_json(&self.preset(&preset.name), preset)
    }

    /// Sorted list of preset names (stems of `*.json` in `presets/`).
    pub fn list_presets(&self) -> Vec<String> {
        let mut names = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.presets_dir()) else {
            return names;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
        names.sort();
        names
    }

    /// Sorted list of game appids (stems of `*.json` in `games/`). Mirrors
    /// [`list_presets`](Self::list_presets); used by the scope sweep to visit
    /// every per-game config file.
    pub fn list_games(&self) -> Vec<String> {
        let mut ids = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.games_dir()) else {
            return ids;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    ids.push(stem.to_string());
                }
            }
        }
        ids.sort();
        ids
    }

    pub fn delete_game(&self, appid: &str) -> Result<()> {
        let p = self.game(appid);
        std::fs::remove_file(&p).map_err(|source| RitzError::Io {
            path: p.display().to_string(),
            source,
        })
    }

    pub fn delete_preset(&self, name: &str) -> Result<()> {
        let p = self.preset(name);
        std::fs::remove_file(&p).map_err(|source| RitzError::Io {
            path: p.display().to_string(),
            source,
        })
    }
}

fn read_json_opt<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match std::fs::read_to_string(path) {
        Ok(s) => {
            let value = serde_json::from_str(&s).map_err(|source| RitzError::Json {
                path: path.display().to_string(),
                source,
            })?;
            Ok(Some(value))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(RitzError::Io {
            path: path.display().to_string(),
            source,
        }),
    }
}

/// Atomically write `bytes` to `path`: write to a sibling temp file, then
/// `fs::rename` it into place (rename is atomic within a filesystem, so readers
/// never observe a half-written file). The temp path is `path` with a literal
/// `.tmp` **suffix appended to the OsString** — NOT `with_extension`, which would
/// truncate multi-dot names (e.g. `foo.bar.json` → `foo.tmp`). No fsync, matching
/// the `resources.rs` bootstrap precedent (durability is not required here; the
/// swap-in-place semantics are).
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| RitzError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).map_err(|source| RitzError::Io {
        path: tmp.display().to_string(),
        source,
    })?;
    std::fs::rename(&tmp, path).map_err(|source| RitzError::Io {
        path: path.display().to_string(),
        source,
    })
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let s = serde_json::to_string_pretty(value).map_err(|source| RitzError::Json {
        path: path.display().to_string(),
        source,
    })?;
    write_atomic(path, s.as_bytes())
}

// ---- Module-config remap (fork / rename migration) -----------------------
//
// A module's stored config keys on `Author → Name → var` only (Version-blind;
// see `migrate_authors`). Forking or renaming a module therefore has to move
// that stored config from one `(Author, Name)` namespace to another across
// *every* scope file. These primitives do the moving; the GUI (Phase 3 stage 2)
// wires them to the Fork / Rename buttons.

/// Copy one module's stored config from `from = (author, name)` to
/// `to = (author, name)` **within a single [`AuthorsMap`]** (one scope file's
/// worth of config).
///
/// - `rename` maps a source var name to a new name (absent = keep the name).
/// - `drop` names source vars to skip entirely.
/// - **Copy-only:** the source entry is left intact (pruning, if wanted, is the
///   [`remap_all_scopes`] driver's job via `remove_source`).
/// - **Clobber-guard:** if the destination already holds a value for a target
///   var, that var is **skipped** (never overwritten) so a move can't silently
///   destroy a pre-existing value.
///
/// Returns the **source** var names that landed nowhere — either listed in
/// `drop`, or skipped by the clobber-guard — for the caller to report. Running
/// it twice is idempotent w.r.t. the map: the second pass finds every
/// destination already populated and changes nothing (reporting each source var
/// as "landed nowhere").
pub fn remap_module_config(
    authors: &mut AuthorsMap,
    from: (&str, &str),
    to: (&str, &str),
    rename: &IndexMap<String, String>,
    drop: &HashSet<String>,
) -> Vec<String> {
    let mut dropped = Vec::new();
    // Snapshot the source vars up front (copy-only: the source is left intact,
    // and cloning sidesteps the aliasing when `from == to`).
    let source: VarMap = match authors.get(from.0).and_then(|e| e.get(from.1)) {
        Some(vars) => vars.clone(),
        None => return dropped,
    };
    let target = authors
        .entry(to.0.to_string())
        .or_default()
        .entry(to.1.to_string())
        .or_default();
    for (var, value) in &source {
        if drop.contains(var) {
            dropped.push(var.clone());
            continue;
        }
        let dest = rename.get(var).cloned().unwrap_or_else(|| var.clone());
        // In-place, same name: the value already lives at its destination.
        if from == to && dest == *var {
            continue;
        }
        if target.contains_key(&dest) {
            // Clobber-guard: never overwrite an existing destination value.
            dropped.push(var.clone());
            continue;
        }
        target.insert(dest, value.clone());
    }
    dropped
}

/// Apply [`remap_module_config`] to one scope's map, then (if `remove_source`)
/// prune the vars that were moved *out* of the source. Returns the dropped-var
/// report for this scope.
///
/// Prune safety: clobber-skipped source vars are **kept** (removing them would
/// be silent data loss, since they never reached the destination); intentional
/// `drop`s are removed; for an in-place rename (`from == to`) the kept-name vars
/// stay put and only the renamed old names are removed.
fn remap_one_scope(
    authors: &mut AuthorsMap,
    from: (&str, &str),
    to: (&str, &str),
    rename: &IndexMap<String, String>,
    drop: &HashSet<String>,
    remove_source: bool,
) -> Vec<String> {
    let source_vars: Vec<String> = authors
        .get(from.0)
        .and_then(|e| e.get(from.1))
        .map(|vars| vars.keys().cloned().collect())
        .unwrap_or_default();

    let dropped = remap_module_config(authors, from, to, rename, drop);

    if remove_source && !source_vars.is_empty() {
        // `dropped` = drop-set ∪ clobber-skipped; the clobbered ones must NOT be
        // removed from the source (they landed nowhere).
        let clobbered: HashSet<&String> =
            dropped.iter().filter(|v| !drop.contains(*v)).collect();
        if let Some(vars) = authors.get_mut(from.0).and_then(|e| e.get_mut(from.1)) {
            for v in &source_vars {
                if clobbered.contains(v) {
                    continue;
                }
                if from == to {
                    let dest = rename.get(v).map(String::as_str).unwrap_or(v.as_str());
                    if dest == v.as_str() && !drop.contains(v) {
                        continue; // stays in place
                    }
                }
                vars.shift_remove(v);
            }
        }
        // Prune now-empty maps.
        if let Some(exts) = authors.get_mut(from.0) {
            if exts.get(from.1).is_some_and(|vars| vars.is_empty()) {
                exts.shift_remove(from.1);
            }
            if exts.is_empty() {
                authors.shift_remove(from.0);
            }
        }
    }

    dropped
}

/// Apply a module-config remap across **every scope file**: `global.json`, each
/// `profiles/*.json`, and each `games/*.json`. `migrate_authors` runs on load so
/// each map is already flat.
///
/// `remove_source=true` prunes the source module entry (and any emptied author
/// map) in each scope *after* copying — turning a copy into a move. Only files
/// whose [`AuthorsMap`] actually changed are rewritten (via the atomic write
/// path). Returns one `(scope-label, dropped-vars)` entry per scope that dropped
/// at least one var.
pub fn remap_all_scopes(
    paths: &Paths,
    from: (&str, &str),
    to: (&str, &str),
    rename: &IndexMap<String, String>,
    drop: &HashSet<String>,
    remove_source: bool,
) -> Result<Vec<(String, Vec<String>)>> {
    let mut reports = Vec::new();

    // global.json (load_global_config yields a default when the file is absent;
    // an unchanged default is never written back).
    {
        let mut cfg = paths.load_global_config()?;
        let before = cfg.modules.clone();
        let dropped = remap_one_scope(&mut cfg.modules, from, to, rename, drop, remove_source);
        if cfg.modules != before {
            paths.save_global_config(&cfg)?;
        }
        if !dropped.is_empty() {
            reports.push(("global".to_string(), dropped));
        }
    }

    // profiles/*.json
    for name in paths.list_presets() {
        let Some(mut preset) = paths.load_preset(&name)? else {
            continue;
        };
        let before = preset.modules.clone();
        let dropped =
            remap_one_scope(&mut preset.modules, from, to, rename, drop, remove_source);
        if preset.modules != before {
            paths.save_preset(&preset)?;
        }
        if !dropped.is_empty() {
            reports.push((format!("profile:{name}"), dropped));
        }
    }

    // games/*.json
    for appid in paths.list_games() {
        let Some(mut game) = paths.load_game(&appid)? else {
            continue;
        };
        let before = game.config.modules.authors.clone();
        let dropped = remap_one_scope(
            &mut game.config.modules.authors,
            from,
            to,
            rename,
            drop,
            remove_source,
        );
        if game.config.modules.authors != before {
            paths.save_game(&game)?;
        }
        if !dropped.is_empty() {
            reports.push((format!("game:{appid}"), dropped));
        }
    }

    Ok(reports)
}

/// Fork: copy a parent module's stored config into the fork's `(Author, Name)`
/// namespace across all scopes, **keeping** the parent's config. A fork is a
/// full copy, so no vars are renamed or dropped.
pub fn snapshot_config_to_fork(
    paths: &Paths,
    parent: (&str, &str),
    fork: (&str, &str),
) -> Result<Vec<(String, Vec<String>)>> {
    remap_all_scopes(
        paths,
        parent,
        fork,
        &IndexMap::new(),
        &HashSet::new(),
        false,
    )
}

/// Rename: move a module's stored config to a new `(Author, Name)` and/or rename
/// its vars across all scopes, **pruning** the source.
pub fn migrate_renamed_module(
    paths: &Paths,
    from: (&str, &str),
    to: (&str, &str),
    var_rename: &IndexMap<String, String>,
) -> Result<Vec<(String, Vec<String>)>> {
    remap_all_scopes(paths, from, to, var_rename, &HashSet::new(), true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn game_config_roundtrip() {
        let mut g = GameConfig::new("730", "Counter-Strike 2");
        g.config.modules.preset = Some("FPS".into());
        g.set_value("Ritze", "amd", "radv_aco", json!(true));
        g.set_value("Ritze", "gamescope", "backend", json!("sdl"));

        let s = serde_json::to_string_pretty(&g).unwrap();
        let back: GameConfig = serde_json::from_str(&s).unwrap();
        assert_eq!(back.game.appid, "730");
        assert_eq!(back.config.modules.preset.as_deref(), Some("FPS"));
        assert_eq!(back.get_value("Ritze", "amd", "radv_aco"), Some(&json!(true)));
        assert_eq!(
            back.get_value("Ritze", "gamescope", "backend"),
            Some(&json!("sdl"))
        );
    }

    #[test]
    fn unset_prunes_empty_maps() {
        let mut g = GameConfig::new("730", "cs2");
        g.set_value("Ritze", "amd", "radv_aco", json!(true));
        g.unset_value("Ritze", "amd", "radv_aco");
        assert!(g.config.modules.authors.is_empty());
    }

    #[test]
    fn preset_lookup() {
        let preset: Preset = serde_json::from_value(json!({
            "Name": "FPS",
            "Modules": { "Ritze": { "amd": { "radv_aco": true } } }
        }))
        .unwrap();
        assert_eq!(preset.get_value("Ritze", "amd", "radv_aco"), Some(&json!(true)));
    }

    fn authors_from(v: Value) -> AuthorsMap {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn remap_module_config_moves_renames_drops_and_clobber_guards() {
        let mut a = authors_from(json!({
            "Ritze": {
                "Old": { "keep": 1, "ren": 2, "gone": 3, "clash": 4 },
                "New": { "clash": 100 }
            }
        }));
        let mut rename = IndexMap::new();
        rename.insert("ren".to_string(), "ren2".to_string());
        let mut drop = HashSet::new();
        drop.insert("gone".to_string());

        let dropped =
            remap_module_config(&mut a, ("Ritze", "Old"), ("Ritze", "New"), &rename, &drop);

        // Values moved; renamed var under its new name.
        assert_eq!(a["Ritze"]["New"]["keep"], json!(1));
        assert_eq!(a["Ritze"]["New"]["ren2"], json!(2));
        // Clobber-guard: pre-existing destination value untouched.
        assert_eq!(a["Ritze"]["New"]["clash"], json!(100));
        // Dropped var never landed.
        assert!(!a["Ritze"]["New"].contains_key("gone"));
        // Copy-only: source left intact.
        assert_eq!(a["Ritze"]["Old"].len(), 4);
        // Reported: the drop-set var and the clobber-skipped var (source names;
        // order follows the map's iteration order, so compare as a set).
        let sorted = |mut v: Vec<String>| {
            v.sort();
            v
        };
        assert_eq!(sorted(dropped), vec!["clash".to_string(), "gone".to_string()]);

        // Idempotent w.r.t. the map: a second pass changes nothing.
        let snapshot = a.clone();
        let dropped2 =
            remap_module_config(&mut a, ("Ritze", "Old"), ("Ritze", "New"), &rename, &drop);
        assert_eq!(a, snapshot, "second remap must not mutate the map");
        // Everything now lands nowhere (dest populated / dropped).
        assert_eq!(
            sorted(dropped2),
            vec![
                "clash".to_string(),
                "gone".to_string(),
                "keep".to_string(),
                "ren".to_string()
            ]
        );
    }

    #[test]
    fn list_games_returns_stems() {
        let base = std::env::temp_dir().join(format!("ritz-listgames-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = Paths { base: base.clone() };
        std::fs::create_dir_all(paths.games_dir()).unwrap();
        std::fs::write(paths.game("244210"), "{}").unwrap();
        std::fs::write(paths.game("2483190"), "{}").unwrap();
        // Non-JSON is ignored.
        std::fs::write(paths.games_dir().join("notes.txt"), "x").unwrap();

        assert_eq!(paths.list_games(), vec!["244210", "2483190"]);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn remap_all_scopes_moves_across_every_scope_kind() {
        let base = std::env::temp_dir().join(format!("ritz-remapscopes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let paths = Paths { base: base.clone() };
        std::fs::create_dir_all(paths.presets_dir()).unwrap();
        std::fs::create_dir_all(paths.games_dir()).unwrap();

        // --- scopes that HOLD the module config (must move) ---
        std::fs::write(
            paths.global_config(),
            r#"{"Name":"","Modules":{"Ritze":{"Old":{"v":1}}}}"#,
        )
        .unwrap();
        std::fs::write(
            paths.preset("p1"),
            r#"{"Name":"p1","Modules":{"Ritze":{"Old":{"v":2}}}}"#,
        )
        .unwrap();
        std::fs::write(
            paths.game("100"),
            r#"{"Game":{"Name":"g","AppId":"100"},"Config":{"Modules":{"Ritze":{"Old":{"v":3}}}}}"#,
        )
        .unwrap();

        // --- untouched scopes (different module) written COMPACT; if the sweep
        //     rewrites them they'd become pretty-printed, so byte-equality proves
        //     "only changed files rewritten". ---
        let untouched_preset = r#"{"Name":"p2","Modules":{"Other":{"Mod":{"x":9}}}}"#;
        let untouched_game =
            r#"{"Game":{"Name":"h","AppId":"200"},"Config":{"Modules":{"Other":{"Mod":{"x":9}}}}}"#;
        std::fs::write(paths.preset("p2"), untouched_preset).unwrap();
        std::fs::write(paths.game("200"), untouched_game).unwrap();

        let reports = remap_all_scopes(
            &paths,
            ("Ritze", "Old"),
            ("Ritze", "New"),
            &IndexMap::new(),
            &HashSet::new(),
            true,
        )
        .unwrap();
        assert!(reports.is_empty(), "clean move drops nothing: {reports:?}");

        // Global moved + source pruned.
        let g = paths.load_global_config().unwrap();
        assert_eq!(g.modules["Ritze"]["New"]["v"], json!(1));
        assert!(!g.modules["Ritze"].contains_key("Old"));
        // Profile moved + source pruned.
        let p = paths.load_preset("p1").unwrap().unwrap();
        assert_eq!(p.modules["Ritze"]["New"]["v"], json!(2));
        assert!(!p.modules["Ritze"].contains_key("Old"));
        // Game moved + source pruned.
        let game = paths.load_game("100").unwrap().unwrap();
        assert_eq!(game.config.modules.authors["Ritze"]["New"]["v"], json!(3));
        assert!(!game.config.modules.authors["Ritze"].contains_key("Old"));

        // Untouched files not rewritten (still byte-identical / compact).
        assert_eq!(
            std::fs::read_to_string(paths.preset("p2")).unwrap(),
            untouched_preset
        );
        assert_eq!(
            std::fs::read_to_string(paths.game("200")).unwrap(),
            untouched_game
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn migrate_flattens_old_version_layer() {
        let mut authors: AuthorsMap = serde_json::from_value(json!({
            "Ritze": { "Core": { "1.0": { "kbd_layout": "de" } } }
        }))
        .unwrap();
        migrate_authors(&mut authors);
        assert_eq!(authors["Ritze"]["Core"]["kbd_layout"], json!("de"));
        // Already-flat data is left untouched.
        let mut flat: AuthorsMap = serde_json::from_value(json!({
            "Ritze": { "Core": { "kbd_layout": "us" } }
        }))
        .unwrap();
        migrate_authors(&mut flat);
        assert_eq!(flat["Ritze"]["Core"]["kbd_layout"], json!("us"));
    }
}
