//! hypr-monctl backend: per-game display vibrance / brightness / temperature via
//! a Hyprland plugin, driven over `hyprctl eval`.
//!
//! The plugin must expose the Lua functions `vibr_register` / `vibr_unregister`
//! (the ritz equivalent of hyprvibr). The plugin owns focus detection — ritz only
//! registers a rule for the game's window class at launch and unregisters it on
//! exit. Settings are live-reloadable while the game runs.
//!
//! Lifecycle:
//! - `pre_launch`  → (optional) `hyprctl plugin load <so>`, then `vibr_register`
//! - `live_reload` → re-`vibr_register` (replaces the rule)
//! - `post_exit`   → `vibr_unregister`

use std::process::{Command, Stdio};

use anyhow::Result;
use ritz_core::config::Paths;

use crate::backends::Backend;
use crate::context::ResolvedGame;

pub const HYPR_EXT_ID: &str = "Ritze::Hypr-Monctl::1.0";

#[derive(Debug, Clone)]
struct Settings {
    class: String,
    sat: f64,
    brightness: f64,
    temperature: f64,
    plugin_path: Option<String>,
}

#[derive(Default)]
pub struct HyprMonctlBackend;

impl HyprMonctlBackend {
    fn read(&self, game: &ResolvedGame) -> Settings {
        let id = HYPR_EXT_ID;
        // Window class override; fall back to the game's name.
        let class = match game.value(id, "window_class") {
            Some(c) if !c.is_empty() => c.to_string(),
            _ => game.game_config.game.name.clone(),
        };
        let num = |var: &str, default: f64| {
            game.value(id, var)
                .and_then(|s| s.parse::<f64>().ok())
                .unwrap_or(default)
        };
        // Default to the plugin dropped into the config dir on startup; allow an
        // explicit override via the `plugin_path` variable if one is ever set.
        let plugin_path = match game.value(id, "plugin_path") {
            Some(p) if !p.is_empty() => Some(p.to_string()),
            _ => {
                let bundled = Paths::discover().plugins_dir().join("hypr-monctl.so");
                bundled
                    .exists()
                    .then(|| bundled.to_string_lossy().into_owned())
            }
        };

        Settings {
            class,
            sat: num("saturation", 1.0),
            brightness: num("brightness", 1.0),
            temperature: num("temperature", 0.0),
            plugin_path,
        }
    }

    /// Clear every rule, then register the current class. Clearing first
    /// guarantees no stale registration from a previous window class lingers.
    fn apply(&self, s: &Settings) {
        run(&clear_cmd());
        run(&register_cmd(&s.class, s.sat, s.brightness, s.temperature));
    }
}

impl Backend for HyprMonctlBackend {
    fn ext_id(&self) -> &str {
        HYPR_EXT_ID
    }

    fn pre_launch(
        &self,
        game: &ResolvedGame,
        _game_env: &mut indexmap::IndexMap<String, ritz_core::builder::EnvAction>,
    ) -> Result<()> {
        let s = self.read(game);
        if let Some(path) = &s.plugin_path {
            ensure_plugin_loaded(path);
        }
        self.apply(&s);
        Ok(())
    }

    fn live_reload(&self, game: &ResolvedGame) -> Result<()> {
        self.apply(&self.read(game));
        Ok(())
    }

    fn post_exit(&self, _game: &ResolvedGame) -> Result<()> {
        // Clear everything ritz set; the plugin restores the monitor.
        run(&clear_cmd());
        Ok(())
    }
}

/// Run a command, ignoring output; log spawn failures (a missing plugin function
/// is the user's responsibility, not fatal to the game).
fn run(cmd: &[String]) {
    if cmd.is_empty() {
        return;
    }
    let status = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if let Err(e) = status {
        eprintln!("ritz: hypr-monctl: failed to run `{}`: {e}", cmd[0]);
    }
}

/// The `hyprctl plugin load <so>` command.
pub fn ensure_plugin_cmd(plugin_so: &str) -> Vec<String> {
    vec![
        "hyprctl".into(),
        "plugin".into(),
        "load".into(),
        plugin_so.into(),
    ]
}

/// Load the plugin only if it isn't already loaded. `hyprctl plugin load` is NOT
/// idempotent — re-loading errors with "Cannot load a plugin twice!" — so we
/// check `hyprctl plugin list` first (matching the plugin by its .so file stem,
/// which Hyprland reports as the plugin name).
fn ensure_plugin_loaded(plugin_so: &str) {
    let name = std::path::Path::new(plugin_so)
        .file_stem()
        .and_then(|s| s.to_str());
    if let Some(name) = name {
        if let Ok(out) = Command::new("hyprctl")
            .args(["plugin", "list"])
            .output()
        {
            if String::from_utf8_lossy(&out.stdout).contains(name) {
                return; // already loaded
            }
        }
    }
    run(&ensure_plugin_cmd(plugin_so));
}

/// `hyprctl monctl register <class> <sat> <brightness> <temperature>`
///
/// The plugin exposes a `monctl` hyprctl command (not eval-visible Lua functions,
/// which aren't reachable from `hyprctl eval` on current Hyprland).
pub fn register_cmd(class: &str, sat: f64, brightness: f64, temperature: f64) -> Vec<String> {
    vec![
        "hyprctl".into(),
        "monctl".into(),
        "register".into(),
        class.into(),
        sat.to_string(),
        brightness.to_string(),
        temperature.to_string(),
    ]
}

/// `hyprctl monctl clear` — remove all registered rules.
pub fn clear_cmd() -> Vec<String> {
    vec!["hyprctl".into(), "monctl".into(), "clear".into()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_command_format() {
        assert_eq!(
            register_cmd("cs2", 1.8, 1.05, 0.1),
            vec!["hyprctl", "monctl", "register", "cs2", "1.8", "1.05", "0.1"]
        );
    }

    #[test]
    fn clear_command_format() {
        assert_eq!(clear_cmd(), vec!["hyprctl", "monctl", "clear"]);
    }

    #[test]
    fn plugin_load_command() {
        assert_eq!(
            ensure_plugin_cmd("/path/hyprvibr.so"),
            vec!["hyprctl", "plugin", "load", "/path/hyprvibr.so"]
        );
    }
}
