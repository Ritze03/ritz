//! lsfg-vk backend: writes `~/.config/lsfg-vk/conf.toml`, sets `LSFG_PROCESS`,
//! and handles the activation-delay hot-patch.
//!
//! All conf.toml writes funnel through [`write_conf`] so the pre-launch write,
//! the activation-delay thread, and live-reload don't race.

use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use anyhow::{Context, Result};
use indexmap::IndexMap;
use ritz_core::builder::EnvAction;
use ritz_core::lsfg_toml::{self, LsfgSettings};

use crate::backends::Backend;
use crate::context::ResolvedGame;
use crate::notify;

pub const LSFG_EXT_ID: &str = "Ritze::LSFG-VK::1.0";

/// Serializes all conf.toml writes (pre-launch / delay thread / live-reload).
fn conf_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

fn conf_path() -> PathBuf {
    if let Some(p) = std::env::var_os("RITZ_LSFG_CONF") {
        return PathBuf::from(p);
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("lsfg-vk/conf.toml")
}

/// Read-modify-write the conf.toml under the lock.
fn write_conf(transform: impl FnOnce(&str) -> ritz_core::Result<String>) -> Result<()> {
    let _guard = conf_lock().lock().unwrap();
    let path = conf_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let updated = transform(&existing)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, updated).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

#[derive(Default)]
pub struct LsfgBackend;

impl LsfgBackend {
    fn read_settings(&self, game: &ResolvedGame) -> (LsfgSettings, u64) {
        let id = LSFG_EXT_ID;
        let multiplier = game
            .value(id, "multiplier")
            .and_then(|s| s.parse::<u32>().ok())
            .unwrap_or(2);
        let flow_scale = game.value(id, "flow_scale").and_then(|s| s.parse::<f64>().ok());
        let present_mode = match game.value(id, "present_mode") {
            Some(m) if !m.is_empty() && m != "default" => Some(m.to_string()),
            _ => None,
        };
        let activation_delay = game
            .value(id, "activation_delay")
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(0);

        let settings = LsfgSettings {
            multiplier,
            flow_scale,
            performance_mode: Some(game.truthy(id, "performance_mode")),
            hdr_mode: Some(game.truthy(id, "hdr_mode")),
            present_mode,
        };
        (settings, activation_delay)
    }
}

impl Backend for LsfgBackend {
    fn ext_id(&self) -> &str {
        LSFG_EXT_ID
    }

    fn pre_launch(
        &self,
        game: &ResolvedGame,
        game_env: &mut IndexMap<String, EnvAction>,
    ) -> Result<()> {
        let (mut settings, delay) = self.read_settings(game);
        let target = settings.multiplier;

        // Activation delay: start at pass-through (1), bump later.
        if delay > 0 {
            settings.multiplier = 1;
        }
        write_conf(|existing| lsfg_toml::apply(existing, &settings))
            .context("writing lsfg conf.toml")?;

        // Identify the process to lsfg-vk.
        game_env.insert(
            "LSFG_PROCESS".to_string(),
            EnvAction::Set(lsfg_toml::RITZ_EXE.to_string()),
        );

        if delay > 0 {
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(delay));
                match write_conf(|existing| lsfg_toml::set_multiplier(existing, target)) {
                    Ok(()) => notify::send(
                        "LSFG activated",
                        &format!("Frame generation multiplier set to {target}x"),
                    ),
                    Err(e) => eprintln!("ritz: lsfg activation-delay patch failed: {e:#}"),
                }
            });
        }
        Ok(())
    }

    fn live_reload(&self, game: &ResolvedGame) -> Result<()> {
        let (settings, _delay) = self.read_settings(game);
        write_conf(|existing| lsfg_toml::apply(existing, &settings))
            .context("live-reloading lsfg conf.toml")
    }
}
