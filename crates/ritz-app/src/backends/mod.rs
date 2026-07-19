//! Runtime backends: extensions whose values feed a built-in handler with a
//! process lifecycle (apply / game-ready / live-reload / cleanup) rather than the
//! JSON command builder.
//!
//! `lsfg-vk` is the reference backend (M5). `hypr-monctl` is a documented seam
//! (M8).

pub mod hypr_monctl;
pub mod lsfg;

use anyhow::Result;
use indexmap::IndexMap;
use ritz_core::builder::EnvAction;

use crate::context::ResolvedGame;

pub trait Backend: Send {
    /// Extension id this backend handles (e.g. "Ritze::LSFG-VK::1.0").
    fn ext_id(&self) -> &str;

    /// Whether this backend is active for the game (extension present + enabled).
    fn enabled(&self, game: &ResolvedGame) -> bool {
        game.truthy(self.ext_id(), "enabled")
    }

    /// Apply before launch; may inject game-only env vars.
    fn pre_launch(
        &self,
        game: &ResolvedGame,
        game_env: &mut IndexMap<String, EnvAction>,
    ) -> Result<()> {
        let _ = (game, game_env);
        Ok(())
    }

    /// Run once the real game process/window has appeared.
    fn on_game_ready(&self, game: &ResolvedGame) -> Result<()> {
        let _ = game;
        Ok(())
    }

    /// Re-apply hot-reloadable settings after a config change.
    fn live_reload(&self, game: &ResolvedGame) -> Result<()> {
        let _ = game;
        Ok(())
    }

    /// Clean up after the game exits.
    fn post_exit(&self, game: &ResolvedGame) -> Result<()> {
        let _ = game;
        Ok(())
    }

    /// Process name to await for the game-ready stage, if any.
    fn ready_process(&self, game: &ResolvedGame) -> Option<String> {
        let _ = game;
        None
    }
}

/// All registered backends. `hypr_monctl` is registered but inert (deferred).
pub fn registry() -> Vec<Box<dyn Backend>> {
    vec![
        Box::new(lsfg::LsfgBackend),
        Box::new(hypr_monctl::HyprMonctlBackend),
    ]
}

/// Backends active for the given game.
pub fn active(game: &ResolvedGame) -> Vec<Box<dyn Backend>> {
    registry().into_iter().filter(|b| b.enabled(game)).collect()
}
