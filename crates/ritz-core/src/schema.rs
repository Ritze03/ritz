//! Serde types mirroring the extension JSON format.
//!
//! Field names follow the JSON exactly (`Extension`, `UI`, `ENV_VARS`, …). Unknown
//! fields are ignored for forward-compatibility. Section order in `UI` is preserved
//! via `IndexMap` so the GUI renders deterministically.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A complete extension definition (one JSON file).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Extension {
    #[serde(rename = "Extension")]
    pub meta: ExtensionMeta,

    /// Optional: restrict this extension to specific Steam AppIds. Omit = global.
    #[serde(rename = "AppIds", default)]
    pub app_ids: Option<Vec<String>>,

    /// Optional: route to a built-in runtime backend handler (e.g. "lsfg-vk").
    #[serde(rename = "Backend", default)]
    pub backend: Option<String>,

    /// Optional: only load this extension on the named desktop (matched against
    /// `$XDG_CURRENT_DESKTOP`, e.g. "Hyprland"). Omit = available everywhere.
    #[serde(rename = "RequiresDesktop", default)]
    pub requires_desktop: Option<String>,

    /// UI sections in declared order; each section is a list of fields.
    #[serde(rename = "UI", default)]
    pub ui: IndexMap<String, Vec<UiField>>,

    #[serde(rename = "ENV_VARS", default)]
    pub env_vars: Vec<EnvVarSpec>,

    #[serde(rename = "WRAPPERS", default)]
    pub wrappers: Vec<WrapperSpec>,

    #[serde(rename = "GAME_ENV_VARS", default)]
    pub game_env_vars: Vec<EnvVarSpec>,

    #[serde(rename = "GAME_LAUNCH_ARGS", default)]
    pub game_launch_args: Vec<ArgSpec>,

    #[serde(rename = "Hooks", default)]
    pub hooks: Option<Hooks>,

    #[serde(rename = "ScriptBuilders", default)]
    pub script_builders: Vec<ScriptBuilder>,
}

impl Extension {
    /// Stable identity used for variable scoping and config storage:
    /// `<Author>::<Name>::<Version>`.
    pub fn id(&self) -> String {
        format!(
            "{}::{}::{}",
            self.meta.author, self.meta.name, self.meta.version
        )
    }

    /// True if this extension applies to the given appid (global extensions
    /// apply to all).
    pub fn applies_to(&self, appid: &str) -> bool {
        match &self.app_ids {
            None => true,
            Some(ids) => ids.iter().any(|id| id == appid),
        }
    }

    /// Iterate all UI fields across all sections (order preserved).
    pub fn fields(&self) -> impl Iterator<Item = &UiField> {
        self.ui.values().flat_map(|fields| fields.iter())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionMeta {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Author")]
    pub author: String,
    #[serde(rename = "Version")]
    pub version: String,
    #[serde(rename = "Description", default)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldType {
    Toggle,
    Selection,
    Integer,
    Float,
    String,
    /// A list of strings, edited as a growing slot list. Resolves to its entries
    /// joined by newlines (so consumers that shell-split — args/wrappers — and
    /// hook scripts that loop the lines get one item per entry).
    #[serde(rename = "multi_string")]
    MultiString,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiField {
    #[serde(rename = "Name", default)]
    pub name: Option<String>,
    #[serde(rename = "Description", default)]
    pub description: Option<String>,
    #[serde(rename = "Type")]
    pub field_type: FieldType,
    #[serde(rename = "Variable")]
    pub variable: String,
    #[serde(rename = "Default", default)]
    pub default: Option<Value>,
    #[serde(rename = "Options", default)]
    pub options: Option<OptionsSpec>,
    #[serde(rename = "DisplayOptions", default)]
    pub display_options: Option<Vec<String>>,
    /// GUI visibility gate. `global:` references are rejected at load time.
    #[serde(rename = "Requires", default)]
    pub requires: Option<String>,
}

impl UiField {
    /// True if this variable is published to the global build-phase scope.
    pub fn is_global(&self) -> bool {
        self.variable.starts_with("global:")
    }
}

/// `Options` is either a fixed list (selection) or a numeric range (integer/float).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OptionsSpec {
    List(Vec<String>),
    Range {
        min: f64,
        max: f64,
        #[serde(default)]
        step: Option<f64>,
    },
}

// ---- Builder block specs -------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvVarSpec {
    #[serde(rename = "Name")]
    pub name: String,
    #[serde(rename = "Requires", default)]
    pub requires: Option<String>,
    #[serde(rename = "Builder", default)]
    pub builder: Vec<EnvBuilderEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvBuilderEntry {
    #[serde(rename = "Requires", default)]
    pub requires: Option<String>,
    #[serde(rename = "Type")]
    pub op: EnvOp,
    #[serde(rename = "Value", default)]
    pub value: Option<String>,
    #[serde(rename = "Separator", default)]
    pub separator: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EnvOp {
    Set,
    Append,
    Unset,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrapperSpec {
    /// e.g. `"gamescope {OPTIONS} --"` — `{OPTIONS}` is filled from `builder`.
    #[serde(rename = "CommandSyntax")]
    pub command_syntax: String,
    #[serde(rename = "Requires", default)]
    pub requires: Option<String>,
    /// Lower = more left / higher priority in the wrapper chain.
    #[serde(rename = "Priority", default)]
    pub priority: i64,
    #[serde(rename = "Builder", default)]
    pub builder: Vec<WrapperBuilderEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrapperBuilderEntry {
    #[serde(rename = "Requires", default)]
    pub requires: Option<String>,
    /// Always an "add" op for wrappers; `Type` in JSON is accepted but ignored.
    #[serde(rename = "Value")]
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArgSpec {
    #[serde(rename = "Requires", default)]
    pub requires: Option<String>,
    #[serde(rename = "Value")]
    pub value: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Hooks {
    #[serde(rename = "PreLaunch", default)]
    pub pre_launch: Option<HookSpec>,
    #[serde(rename = "PostSpawn", default)]
    pub post_spawn: Option<HookSpec>,
    #[serde(rename = "OnGameReady", default)]
    pub on_game_ready: Option<HookSpec>,
    #[serde(rename = "PostExit", default)]
    pub post_exit: Option<HookSpec>,
}

/// A lifecycle hook: either a bare script path (`"pre.sh"`, runs blocking) or a
/// detailed form (`{ "Script": "post.sh", "Background": true }`) that can opt into
/// non-blocking execution — ritz spawns it and does not wait.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HookSpec {
    Simple(String),
    Detailed {
        #[serde(rename = "Script")]
        script: String,
        #[serde(rename = "Background", default)]
        background: bool,
    },
}

impl HookSpec {
    /// The script path, relative to the extension directory.
    pub fn script(&self) -> &str {
        match self {
            HookSpec::Simple(s) => s,
            HookSpec::Detailed { script, .. } => script,
        }
    }

    /// True if the hook runs in the background (spawned, not waited on).
    pub fn background(&self) -> bool {
        match self {
            HookSpec::Simple(_) => false,
            HookSpec::Detailed { background, .. } => *background,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptBuilder {
    /// Target launch block: ENV_VARS | WRAPPERS | GAME_ENV_VARS | GAME_LAUNCH_ARGS.
    #[serde(rename = "Block")]
    pub block: String,
    #[serde(rename = "Script")]
    pub script: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hookspec_accepts_string_and_object_forms() {
        let hooks: Hooks = serde_json::from_value(serde_json::json!({
            "PreLaunch": "pre.sh",
            "PostSpawn": { "Script": "post.sh", "Background": true },
            "PostExit": { "Script": "exit.sh" }
        }))
        .unwrap();

        let pre = hooks.pre_launch.unwrap();
        assert_eq!(pre.script(), "pre.sh");
        assert!(!pre.background());

        let post = hooks.post_spawn.unwrap();
        assert_eq!(post.script(), "post.sh");
        assert!(post.background());

        // Object form without Background defaults to blocking.
        let exit = hooks.post_exit.unwrap();
        assert_eq!(exit.script(), "exit.sh");
        assert!(!exit.background());
    }
}
