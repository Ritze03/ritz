//! Shared application context: locating extensions, loading config, resolving a
//! game, and assembling its launch command. Used by `--print`, the splash, the
//! GUI, and the supervisor.

use std::path::PathBuf;

use anyhow::{Context, Result};
use ritz_core::builder::{self, ExtInput, LaunchCommand};
use ritz_core::config::{AuthorsMap, GameConfig, GeneralConfig, Paths, Preset};
use ritz_core::extension::{self, LoadedExtension};
use ritz_core::resolve::{self, Resolution};
use ritz_core::schema::Extension;
use ritz_core::steam::SteamCommand;

pub struct AppContext {
    pub paths: Paths,
    pub general: GeneralConfig,
    pub extensions: Vec<LoadedExtension>,
}

impl AppContext {
    pub fn load() -> Result<Self> {
        let paths = Paths::discover();
        // Export bundled extensions + plugin into the config dir (skips existing).
        crate::resources::bootstrap(&paths).context("exporting bundled resources")?;
        let general = paths.load_general().context("loading general.json")?;
        let extensions = load_extensions(&paths)?;
        Ok(Self {
            paths,
            general,
            extensions,
        })
    }

    /// Extensions that apply to a given appid (global + appid-scoped).
    pub fn applicable(&self, appid: &str) -> Vec<&LoadedExtension> {
        self.extensions
            .iter()
            .filter(|e| e.spec.applies_to(appid))
            .collect()
    }

    /// Load the preset referenced by a game config (or the general default).
    pub fn load_preset_for(&self, game_config: &GameConfig) -> Result<Option<Preset>> {
        let preset_name = game_config
            .config
            .modules
            .preset
            .clone()
            .or_else(|| self.general.default_preset.clone());
        match preset_name {
            Some(name) => self.paths.load_preset(&name).context("loading preset"),
            None => Ok(None),
        }
    }

    /// Specs (cloned) for the extensions applicable to an appid.
    pub fn specs_for(&self, appid: &str) -> Vec<Extension> {
        self.applicable(appid).iter().map(|e| e.spec.clone()).collect()
    }

    /// Discover saved games as `(appid, name)` pairs.
    pub fn list_games(&self) -> Vec<(String, String)> {
        let mut games = Vec::new();
        let Ok(entries) = std::fs::read_dir(self.paths.games_dir()) else {
            return games;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(Some(gc)) = (|| -> Result<Option<GameConfig>> {
                Ok(serde_json::from_str(&std::fs::read_to_string(&path)?).ok())
            })() {
                games.push((gc.game.appid, gc.game.name));
            }
        }
        games.sort();
        games
    }

    /// Resolve a game: load (or synthesize) its config + preset, resolve all
    /// applicable extensions.
    pub fn resolve_game(&self, steam: &SteamCommand) -> Result<ResolvedGame> {
        let appid = steam.appid.clone().unwrap_or_else(|| "unknown".to_string());

        let game_config = self
            .paths
            .load_game(&appid)
            .with_context(|| format!("loading games/{appid}.json"))?
            .unwrap_or_else(|| {
                GameConfig::new(
                    appid.clone(),
                    steam.game_name.clone().unwrap_or_else(|| appid.clone()),
                )
            });

        self.resolve_with(game_config, &appid)
    }

    /// Resolve an explicit (possibly unsaved) game config — used to preview a
    /// hypothetical profile before the config is written.
    pub fn resolve_with(&self, game_config: GameConfig, appid: &str) -> Result<ResolvedGame> {
        let preset = self.load_preset_for(&game_config)?;

        // A cyclic parent chain would loop forever; exit silently instead of hanging.
        if let Some(ref p) = preset {
            if p.parent.is_some() && preset_has_cycle(&self.paths, &p.name) {
                std::process::exit(0);
            }
        }

        let global = self.paths.load_global_config().unwrap_or_default();
        let specs = self.specs_for(appid);
        // Merge the parent chain (if any) into the effective preset layer.
        let merged: Option<Preset> = preset.as_ref().and_then(|p| {
            let pname = p.parent.as_ref()?;
            let mut base = collect_parent_chain(&self.paths, pname);
            merge_modules(&mut base.modules, &p.modules);
            Some(base)
        });
        let effective = merged.as_ref().or(preset.as_ref());
        let resolution = resolve::resolve(&specs, Some(&game_config), effective, Some(&global));

        Ok(ResolvedGame {
            appid: appid.to_string(),
            specs,
            resolution,
            game_config,
            preset,
        })
    }
}

/// Assemble a launch command from resolved specs + a game command.
pub fn assemble_launch(
    specs: &[Extension],
    resolution: &Resolution,
    game_cmd: &[String],
) -> Result<LaunchCommand> {
    let stores: Vec<_> = specs.iter().map(|s| resolution.var_store(&s.id())).collect();
    let inputs: Vec<ExtInput> = specs
        .iter()
        .zip(stores.iter())
        .map(|(spec, vars)| ExtInput { spec, vars })
        .collect();
    Ok(builder::build(&inputs, game_cmd)?)
}

pub struct ResolvedGame {
    pub appid: String,
    pub specs: Vec<Extension>,
    pub resolution: Resolution,
    pub game_config: GameConfig,
    pub preset: Option<Preset>,
}

impl ResolvedGame {
    /// Resolved field detail for a variable in an extension.
    pub fn field(&self, ext_id: &str, var: &str) -> Option<&resolve::ResolvedField> {
        self.resolution.exts.get(ext_id).and_then(|e| e.fields.get(var))
    }

    pub fn truthy(&self, ext_id: &str, var: &str) -> bool {
        self.field(ext_id, var).map(|f| f.var.truthy).unwrap_or(false)
    }

    pub fn value(&self, ext_id: &str, var: &str) -> Option<&str> {
        self.field(ext_id, var).map(|f| f.var.value.as_str())
    }

    /// Assemble the launch command for this game.
    pub fn build_launch(&self, steam: &SteamCommand) -> Result<LaunchCommand> {
        assemble_launch(&self.specs, &self.resolution, &steam.raw)
    }
}

/// Extensions are loaded from the config directory, which [`crate::resources::bootstrap`]
/// populates from the bundled set on startup. `$RITZ_EXTENSIONS_DIR` overrides
/// this (used by tests).
/// Whether the running desktop matches `required` (compared against
/// `$XDG_CURRENT_DESKTOP`, which may be a `:`-separated list, case-insensitive).
fn desktop_matches(required: &str) -> bool {
    std::env::var("XDG_CURRENT_DESKTOP")
        .map(|v| v.split(':').any(|c| c.eq_ignore_ascii_case(required)))
        .unwrap_or(false)
}

pub fn extension_dirs(paths: &Paths) -> Vec<PathBuf> {
    if let Some(dir) = std::env::var_os("RITZ_EXTENSIONS_DIR") {
        return vec![PathBuf::from(dir)];
    }
    vec![paths.user_extensions()]
}

// ── Parent-chain helpers ─────────────────────────────────────────────────────

pub(crate) fn merge_modules(base: &mut AuthorsMap, overlay: &AuthorsMap) {
    for (author, exts) in overlay {
        for (ext, vars) in exts {
            for (var, val) in vars {
                base.entry(author.clone()).or_default()
                    .entry(ext.clone()).or_default()
                    .insert(var.clone(), val.clone());
            }
        }
    }
}

/// Walk the parent chain from `preset_name`; return true if any node is revisited.
pub(crate) fn preset_has_cycle(paths: &Paths, preset_name: &str) -> bool {
    let mut current = preset_name.to_string();
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(current.clone()) { return true; }
        match paths.load_preset(&current).ok().flatten().and_then(|p| p.parent) {
            Some(next) => current = next,
            None => return false,
        }
    }
}

/// Returns true if setting `proposed_parent` as the parent of `profile` would
/// introduce any cycle (including internal cycles in proposed_parent's chain).
pub(crate) fn chain_would_have_cycle(paths: &Paths, profile: &str, proposed_parent: &str) -> bool {
    let mut current = proposed_parent.to_string();
    let mut seen = std::collections::HashSet::new();
    seen.insert(profile.to_string());
    loop {
        if !seen.insert(current.clone()) { return true; }
        match paths.load_preset(&current).ok().flatten().and_then(|p| p.parent) {
            Some(next) => current = next,
            None => return false,
        }
    }
}

/// Walk the chain from `first_parent` upward (stops on any repeated node),
/// merge root → direct-parent (root = lowest priority), return combined Preset.
pub(crate) fn collect_parent_chain(paths: &Paths, first_parent: &str) -> Preset {
    let mut chain: Vec<Preset> = Vec::new();
    let mut current = first_parent.to_string();
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(current.clone()) { break; }
        if let Some(p) = paths.load_preset(&current).ok().flatten() {
            let next = p.parent.clone();
            chain.push(p);
            match next { Some(n) => current = n, None => break }
        } else { break; }
    }
    chain.reverse();
    let mut base = Preset::default();
    for p in &chain { merge_modules(&mut base.modules, &p.modules); }
    base
}

/// Walk the chain from `first_parent`, return each preset individually in order
/// (index 0 = direct parent, index 1 = grandparent, …). No merging. Stops on cycle.
pub(crate) fn collect_parent_presets(paths: &Paths, first_parent: &str) -> Vec<Preset> {
    let mut chain: Vec<Preset> = Vec::new();
    let mut current = first_parent.to_string();
    let mut seen = std::collections::HashSet::new();
    loop {
        if !seen.insert(current.clone()) { break; }
        if let Some(p) = paths.load_preset(&current).ok().flatten() {
            let next = p.parent.clone();
            chain.push(p);
            match next { Some(n) => current = n, None => break }
        } else { break; }
    }
    chain
}

/// Load all extensions from disk, dropping those gated to a desktop we're not
/// running (e.g. hypr-monctl off Hyprland) so they never appear in the GUI nor
/// get resolved/applied. Shared by [`AppContext::load`] and the GUI's hot-reload.
pub fn load_extensions(paths: &Paths) -> Result<Vec<LoadedExtension>> {
    let dirs = extension_dirs(paths);
    let mut extensions = extension::load_all(&dirs).context("loading extensions")?;
    extensions.retain(|e| {
        e.spec
            .requires_desktop
            .as_deref()
            .map_or(true, desktop_matches)
    });
    // Deterministic alphabetical order — `load_all` follows filesystem order,
    // which varies between reads, so the author-mode tree (which shows modules in
    // this order within each author) would otherwise shuffle on reload.
    extensions.sort_by(|a, b| a.spec.meta.name.cmp(&b.spec.meta.name));
    Ok(extensions)
}
