//! `ritz` — Linux Steam launch wrapper.
//!
//! Invocation modes:
//! - `ritz` (no args, or no SteamAppId)      → settings GUI
//! - `ritz %command%`                        → coordinator: splash → (edit) → launch
//! - `ritz --print %command%`                → dry-run: print the assembled command
//! - `ritz --splash <appid> -- %command%`    → splash window (own process)
//! - `ritz --edit [--launch] <appid> [-- %command%]` → settings GUI (own process)
//!
//! The splash and editor each run in their **own process** so their windows are
//! guaranteed to despawn on exit (winit also allows only one event loop per
//! process). The top-level launch process is windowless: it spawns the splash,
//! then optionally the editor, then supervises the game.

mod backends;
mod context;
mod fonts;
mod gui;
mod icon_center;
mod image;
mod hooks;
mod notify;
mod resources;
mod splash;
mod supervisor;
mod theme;

use std::env;
use std::process::ExitCode;

use anyhow::Result;
use context::AppContext;
use ritz_core::config::GameConfig;
use ritz_core::steam::SteamCommand;

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();

    let result = match args.first().map(String::as_str) {
        Some("--print") => cmd_print(&args[1..]).map(|_| 0),
        Some("--splash") => cmd_splash(&args[1..]),
        Some("--edit") => cmd_edit(&args[1..]),
        None => cmd_gui().map(|_| 0),
        Some(_) => cmd_launch(&args),
    };

    match result {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("ritz: error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

// Exit codes used by the `--splash` subprocess to report the user's choice.
pub(crate) const SPLASH_LAUNCH: i32 = 0;
pub(crate) const SPLASH_CANCEL: i32 = 10;
pub(crate) const SPLASH_EDIT: i32 = 11;

/// Coordinator (windowless): show the splash in its own process, optionally the
/// editor, then supervise the game. Returns the game's exit code so Steam
/// reflects its true state.
fn cmd_launch(command: &[String]) -> Result<i32> {
    let env_appid = env::var("RITZ_APPID").ok().or_else(|| env::var("SteamAppId").ok());
    let steam = SteamCommand::parse(command, env_appid);
    let appid = steam.appid.clone().unwrap_or_else(|| "unknown".to_string());

    match run_splash_subprocess(&appid, command) {
        SPLASH_CANCEL => {
            eprintln!("ritz: launch cancelled by user.");
            return Ok(1);
        }
        SPLASH_EDIT => {
            if !edit_in_subprocess(&appid, command) {
                eprintln!("ritz: launch cancelled from editor.");
                return Ok(1);
            }
        }
        _ => {} // SPLASH_LAUNCH (or any unexpected code) → launch
    }

    let ctx = AppContext::load()?;
    let resolved = ctx.resolve_game(&steam)?;
    supervisor::run(&ctx, &resolved, &steam)
}

/// Run `ritz --splash <appid> -- <command>` and return its exit code.
fn run_splash_subprocess(appid: &str, command: &[String]) -> i32 {
    let exe = match env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("ritz: cannot locate self for splash: {e}");
            return SPLASH_LAUNCH;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--splash").arg(appid).arg("--").args(command);
    cmd.env("SteamAppId", appid);
    match cmd.status() {
        Ok(status) => status.code().unwrap_or(SPLASH_LAUNCH),
        Err(e) => {
            eprintln!("ritz: failed to show splash: {e}");
            SPLASH_LAUNCH
        }
    }
}

/// Run `ritz --edit --launch <appid> -- <command>`; returns true to continue the
/// launch, false to cancel.
fn edit_in_subprocess(appid: &str, command: &[String]) -> bool {
    let exe = match env::current_exe() {
        Ok(exe) => exe,
        Err(e) => {
            eprintln!("ritz: cannot locate self to open editor: {e}");
            return true;
        }
    };
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--edit")
        .arg("--launch")
        .arg(appid)
        .arg("--")
        .args(command);
    cmd.env("SteamAppId", appid);
    match cmd.status() {
        Ok(status) => status.success(), // exit 0 = continue, non-zero = cancel
        Err(e) => {
            eprintln!("ritz: failed to open editor: {e}");
            true
        }
    }
}

/// Splash subprocess: parse `[appid, "--", command…]`, show the splash, exit with
/// the action code.
fn cmd_splash(args: &[String]) -> Result<i32> {
    let appid = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("--splash requires an appid"))?
        .clone();
    let command = match args.iter().position(|a| a == "--") {
        Some(pos) => args[pos + 1..].to_vec(),
        None => Vec::new(),
    };

    let steam = SteamCommand::parse(&command, Some(appid.clone()));
    let ctx = AppContext::load()?;

    // Non-Steam launch with no id: guide the user to set a stable RITZ_APPID,
    // then stop (no launch, no config until they do).
    if appid == "unknown" && ctx.paths.load_game(&appid)?.is_none() {
        let existing: Vec<String> = ctx.list_games().into_iter().map(|(id, _)| id).collect();
        splash::show_unknown(&existing);
        return Ok(SPLASH_CANCEL);
    }

    // First launch of an unconfigured game: show the "new game" picker (no
    // countdown). It writes the game config so the parent can launch/edit it.
    if ctx.paths.load_game(&appid)?.is_none() {
        let pins = gather_pinned_profiles(&ctx);
        let default_profile = ctx.general.default_preset.clone();
        // Name guess: Steam API for numeric ids → else the install-folder name →
        // else empty (never the appid or "proton"). Fetch before opening so it
        // doesn't pop in.
        let name_guess = appid
            .bytes()
            .all(|b| b.is_ascii_digit())
            .then(|| fetch_steam_name(&appid))
            .flatten()
            .or_else(|| steam.game_name.clone())
            .unwrap_or_default();

        return Ok(
            match splash::show_new(&appid, &pins, default_profile.as_deref(), &name_guess) {
                splash::NewGameChoice::Cancel => SPLASH_CANCEL,
                splash::NewGameChoice::Launch { name, profile } => {
                    create_new_game(&ctx, &appid, &name, profile)?;
                    SPLASH_LAUNCH
                }
                splash::NewGameChoice::Edit { name, profile } => {
                    create_new_game(&ctx, &appid, &name, profile)?;
                    SPLASH_EDIT
                }
            },
        );
    }

    let resolved = ctx.resolve_game(&steam)?;
    let timeout = resolved
        .game_config
        .config
        .general
        .splash_timeout_secs
        .unwrap_or(ctx.general.splash_timeout_secs);
    let profile = resolved.game_config.config.modules.preset.as_deref();
    // The game is already configured — show its saved name, not the name derived
    // from the Steam command (which can be the install-folder/"proton").
    let name = resolved.game_config.game.name.clone();

    Ok(match splash::show(&name, &resolved.appid, timeout, profile) {
        splash::SplashAction::Launch => SPLASH_LAUNCH,
        splash::SplashAction::Cancel => SPLASH_CANCEL,
        splash::SplashAction::Edit => SPLASH_EDIT,
    })
}

/// Fetch a game's display name from the public Steam store API. Best-effort:
/// returns `None` on any failure (curl missing, offline, bad JSON, unknown id).
fn fetch_steam_name(appid: &str) -> Option<String> {
    let url =
        format!("https://store.steampowered.com/api/appdetails?appids={appid}&filters=basic");
    let out = std::process::Command::new("curl")
        .args(["-s", "--connect-timeout", "2", "--max-time", "5", &url])
        .output()
        .ok()?;
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    let entry = json.get(appid)?;
    if entry.get("success")?.as_bool() != Some(true) {
        return None;
    }
    entry
        .get("data")?
        .get("name")?
        .as_str()
        .map(str::to_string)
        .filter(|s| !s.is_empty())
}

/// Pinned profiles (slot id, name), sorted by id, at most 10.
fn gather_pinned_profiles(ctx: &AppContext) -> Vec<(u8, String)> {
    let mut v: Vec<(u8, String)> = ctx
        .paths
        .list_presets()
        .into_iter()
        .filter_map(|n| {
            ctx.paths
                .load_preset(&n)
                .ok()
                .flatten()
                .and_then(|p| p.pin.map(|id| (id, n)))
        })
        .collect();
    v.sort_by_key(|(id, _)| *id);
    v.truncate(10);
    v
}

/// Create and save a new game config with the given name and (optional) profile.
fn create_new_game(ctx: &AppContext, appid: &str, name: &str, profile: Option<String>) -> Result<()> {
    let mut gc = GameConfig::new(appid, name);
    gc.config.modules.preset = profile;
    ctx.paths.save_game(&gc)?;
    Ok(())
}

/// No-arg mode: open the settings manager GUI.
fn cmd_gui() -> Result<()> {
    let ctx = AppContext::load()?;
    gui::run_manager(&ctx)
}

/// Edit mode: open the settings GUI for one game.
/// `args` = `[("--launch")? , appid, "--", command…]`.
fn cmd_edit(args: &[String]) -> Result<i32> {
    let launch_mode = args.iter().any(|a| a == "--launch");
    let appid = args
        .iter()
        .find(|a| !a.starts_with("--"))
        .ok_or_else(|| anyhow::anyhow!("--edit requires an appid"))?
        .clone();
    let game_command = match args.iter().position(|a| a == "--") {
        Some(pos) => args[pos + 1..].to_vec(),
        None => vec!["%command%".to_string()],
    };

    let ctx = AppContext::load()?;
    let name = ctx
        .paths
        .load_game(&appid)?
        .map(|g| g.game.name)
        .unwrap_or_else(|| appid.clone());

    let outcome = gui::run(&ctx, &appid, &name, game_command, launch_mode)?;
    Ok(match outcome {
        gui::EditOutcome::Continue => 0,
        gui::EditOutcome::Cancel => 1,
    })
}

/// Dry-run: parse the `%command%`, load extensions + config, resolve, and print
/// the assembled launch command.
fn cmd_print(command: &[String]) -> Result<()> {
    let env_appid = env::var("RITZ_APPID").ok().or_else(|| env::var("SteamAppId").ok());
    let steam = SteamCommand::parse(command, env_appid);

    let ctx = AppContext::load()?;
    let resolved = ctx.resolve_game(&steam)?;
    let launch = resolved.build_launch(&steam)?;

    eprintln!("AppId:      {}", resolved.appid);
    if let Some(name) = &steam.game_name {
        eprintln!("Game:       {name}");
    }
    eprintln!("Extensions: {}", resolved.specs.len());
    if let Some(p) = &resolved.preset {
        eprintln!("Preset:     {}", p.name);
    }
    println!("{launch}");
    Ok(())
}
