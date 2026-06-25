//! On-disk configuration: `general.json`, `presets/<name>.json`, `games/<appid>.json`.
//!
//! Storage convention for variable values:
//! - present non-null value = explicit override (enabled, with that value)
//! - `null`                 = explicitly disabled (overrides an inherited-on)
//! - absent                 = inherit from the layer below
//!
//! Nested module layout:
//! `Modules → <Author> → <ExtName> → <var> = value`.

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

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| RitzError::Io {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let s = serde_json::to_string_pretty(value).map_err(|source| RitzError::Json {
        path: path.display().to_string(),
        source,
    })?;
    std::fs::write(path, s).map_err(|source| RitzError::Io {
        path: path.display().to_string(),
        source,
    })
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
