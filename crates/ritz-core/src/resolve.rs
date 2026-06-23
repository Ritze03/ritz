//! Three-layer variable resolution: `extension Default ← preset ← game override`.
//!
//! Produces, per extension:
//! - a [`VarStore`] for the launch builder, and
//! - per-field [`ResolvedField`] details (effective value + provenance + enabled)
//!   that drive the GUI's tri-state inheritance display.

use std::collections::HashMap;

use indexmap::IndexMap;
use serde_json::Value;

use crate::config::{GameConfig, Preset};
use crate::schema::{Extension, FieldType, UiField};
use crate::variables::{resolve_field, ResolvedVar, VarStore};

/// Where a field's effective value came from (lowest → highest priority).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Default,
    Global,
    Preset,
    Game,
}

#[derive(Debug, Clone)]
pub struct ResolvedField {
    pub var: ResolvedVar,
    pub provenance: Provenance,
    /// Whether the field is "on" (toggle true, or non-toggle enable-checkbox on).
    pub enabled: bool,
    /// Effective stored value (`None` = no value / inherited default absent).
    pub raw: Option<Value>,
}

#[derive(Debug, Clone, Default)]
pub struct ExtResolution {
    /// Local (non-global) variable values for the builder.
    pub local: HashMap<String, ResolvedVar>,
    /// Per-field detail keyed by variable name, in declaration order.
    pub fields: IndexMap<String, ResolvedField>,
}

#[derive(Debug, Clone, Default)]
pub struct Resolution {
    /// Shared global variables (bare name → value), published by `global:` fields.
    pub global: HashMap<String, ResolvedVar>,
    /// Per-extension resolution keyed by extension id.
    pub exts: IndexMap<String, ExtResolution>,
}

impl Resolution {
    /// Build the [`VarStore`] used to evaluate one extension during launch-build.
    pub fn var_store(&self, ext_id: &str) -> VarStore {
        let mut store = VarStore::with_global(self.global.clone());
        if let Some(ext) = self.exts.get(ext_id) {
            for (name, var) in &ext.local {
                store.insert_local(name.clone(), var.clone());
            }
        }
        store
    }
}

fn value_to_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Resolve a single field's effective value across four layers (lowest → highest):
/// extension default ← global ← preset ← game.
fn resolve_one(
    field: &UiField,
    author: &str,
    name: &str,
    game: Option<&GameConfig>,
    preset: Option<&Preset>,
    global: Option<&Preset>,
) -> ResolvedField {
    let var = &field.variable;

    let (raw, provenance) = if let Some(v) = game.and_then(|g| g.get_value(author, name, var)) {
        (Some(v.clone()), Provenance::Game)
    } else if let Some(v) = preset.and_then(|p| p.get_value(author, name, var)) {
        (Some(v.clone()), Provenance::Preset)
    } else if let Some(v) = global.and_then(|g| g.get_value(author, name, var)) {
        (Some(v.clone()), Provenance::Global)
    } else {
        (field.default.clone(), Provenance::Default)
    };

    // Determine enabled + the string fed to truthiness/interpolation.
    let (enabled, raw_str): (bool, Option<String>) = match field.field_type {
        FieldType::Toggle => {
            let on = matches!(&raw, Some(Value::Bool(true)));
            (on, Some(if on { "true".into() } else { "false".into() }))
        }
        FieldType::MultiString => {
            // Stored as an array of strings; resolve to the non-empty entries
            // joined by newlines (truthy when at least one entry is present).
            let entries: Vec<String> = match &raw {
                Some(Value::Array(a)) => a
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => Vec::new(),
            };
            if entries.is_empty() {
                (false, None)
            } else {
                (true, Some(entries.join("\n")))
            }
        }
        _ => match &raw {
            Some(Value::Null) | None => (false, None),
            Some(v) => (true, Some(value_to_string(v))),
        },
    };

    let resolved = resolve_field(field, enabled, raw_str.as_deref());
    ResolvedField {
        var: resolved,
        provenance,
        enabled,
        raw,
    }
}

/// Resolve all applicable extensions across four layers:
/// extension default ← global ← preset ← game.
pub fn resolve(
    exts: &[Extension],
    game: Option<&GameConfig>,
    preset: Option<&Preset>,
    global: Option<&Preset>,
) -> Resolution {
    let mut resolution = Resolution::default();

    for ext in exts {
        let author = &ext.meta.author;
        let name = &ext.meta.name;
        let mut ext_res = ExtResolution::default();

        for field in ext.fields() {
            let rf = resolve_one(field, author, name, game, preset, global);

            if field.is_global() {
                let bare = field
                    .variable
                    .strip_prefix(crate::variables::GLOBAL_PREFIX)
                    .unwrap_or(&field.variable)
                    .to_string();
                resolution.global.insert(bare, rf.var.clone());
            } else {
                ext_res
                    .local
                    .insert(field.variable.clone(), rf.var.clone());
            }
            ext_res.fields.insert(field.variable.clone(), rf);
        }

        resolution.exts.insert(ext.id(), ext_res);
    }

    resolution
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ext() -> Extension {
        serde_json::from_value(json!({
            "Extension": {"Name": "amd", "Author": "Ritze", "Version": "1.0"},
            "UI": {
                "RADV": [
                    {"Type": "toggle", "Variable": "radv_enabled", "Default": false},
                    {"Type": "toggle", "Variable": "radv_aco", "Default": true},
                    {"Type": "selection", "Variable": "backend",
                     "Options": ["auto", "sdl"]},
                    {"Type": "toggle", "Variable": "global:hdr", "Default": false}
                ]
            }
        }))
        .unwrap()
    }

    #[test]
    fn default_layer() {
        let exts = [ext()];
        let res = resolve(&exts, None, None, None);
        let e = &res.exts["Ritze::amd::1.0"];
        assert_eq!(e.fields["radv_enabled"].provenance, Provenance::Default);
        assert!(!e.fields["radv_enabled"].var.truthy);
        assert!(e.fields["radv_aco"].var.truthy); // default true
        // selection with no default is disabled/falsy
        assert!(!e.fields["backend"].var.truthy);
    }

    #[test]
    fn multi_string_joins_entries_by_newline() {
        let exts: [Extension; 1] = [serde_json::from_value(json!({
            "Extension": {"Name": "scripts", "Author": "Ritze", "Version": "1.0"},
            "UI": { "S": [ {"Type": "multi_string", "Variable": "cmds"} ] }
        }))
        .unwrap()];
        let mut game = GameConfig::new("730", "cs2");
        game.set_value("Ritze", "scripts", "cmds", json!(["echo one", "echo two"]));

        let res = resolve(&exts, Some(&game), None, None);
        let v = &res.exts["Ritze::scripts::1.0"].fields["cmds"].var;
        assert!(v.truthy);
        assert_eq!(v.value, "echo one\necho two");

        // Empty list → falsy, empty value (so RITZ_VAR_* is "" and hooks skip).
        let mut empty = GameConfig::new("730", "cs2");
        empty.set_value("Ritze", "scripts", "cmds", json!([] as [String; 0]));
        let res2 = resolve(&exts, Some(&empty), None, None);
        let v2 = &res2.exts["Ritze::scripts::1.0"].fields["cmds"].var;
        assert!(!v2.truthy);
        assert_eq!(v2.value, "");
    }

    #[test]
    fn preset_then_game_override() {
        let exts = [ext()];
        let preset: Preset = serde_json::from_value(json!({
            "Name": "FPS",
            "Modules": {"Ritze": {"amd": {"radv_enabled": true, "backend": "sdl"}}}
        }))
        .unwrap();
        let mut game = GameConfig::new("730", "cs2");
        game.set_value("Ritze", "amd", "backend", json!("auto"));

        let res = resolve(&exts, Some(&game), Some(&preset), None);
        let e = &res.exts["Ritze::amd::1.0"];

        // radv_enabled inherited from preset
        assert_eq!(e.fields["radv_enabled"].provenance, Provenance::Preset);
        assert!(e.fields["radv_enabled"].var.truthy);
        // backend overridden by game
        assert_eq!(e.fields["backend"].provenance, Provenance::Game);
        assert_eq!(e.fields["backend"].var.value, "auto");
    }

    #[test]
    fn null_disables_inherited() {
        let exts = [ext()];
        let preset: Preset = serde_json::from_value(json!({
            "Name": "FPS",
            "Modules": {"Ritze": {"amd": {"backend": "sdl"}}}
        }))
        .unwrap();
        let mut game = GameConfig::new("730", "cs2");
        game.set_value("Ritze", "amd", "backend", json!(null));

        let res = resolve(&exts, Some(&game), Some(&preset), None);
        let e = &res.exts["Ritze::amd::1.0"];
        assert_eq!(e.fields["backend"].provenance, Provenance::Game);
        assert!(!e.fields["backend"].enabled);
        assert!(!e.fields["backend"].var.truthy);
    }

    #[test]
    fn global_published_and_visible_in_var_store() {
        let exts = [ext()];
        let mut game = GameConfig::new("730", "cs2");
        game.set_value("Ritze", "amd", "global:hdr", json!(true));

        let res = resolve(&exts, Some(&game), None, None);
        assert!(res.global.contains_key("hdr"));

        let store = res.var_store("Ritze::amd::1.0");
        assert!(store.truthy("global:hdr"));
        // local does NOT contain the global var under its prefixed name
        assert!(!store.truthy("hdr"));
    }
}
