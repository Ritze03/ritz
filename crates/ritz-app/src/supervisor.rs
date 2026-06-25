//! Process supervision: ritz stays the process Steam tracks for the whole game
//! lifetime. It spawns the assembled command, waits on it, forwards its exit
//! code, fires the game-ready stage, polls for live config reloads, and forwards
//! termination signals to the child.

use std::os::unix::process::ExitStatusExt;
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ritz_core::builder::EnvAction;
use ritz_core::steam::SteamCommand;

use crate::backends::{self, Backend};
use crate::context::{AppContext, ResolvedGame};
use crate::hooks::{self, Stage};

const POLL: Duration = Duration::from_millis(500);
const READY_TIMEOUT: Duration = Duration::from_secs(60);

/// Build, launch, and supervise the game. Returns the child's exit code.
pub fn run(ctx: &AppContext, game: &ResolvedGame, steam: &SteamCommand) -> Result<i32> {
    let exts = ctx.applicable(&game.appid);
    let backends = backends::active(game);

    // 1. Build the launch command.
    let mut launch = game.build_launch(steam)?;

    // 2. Backend pre-launch (may inject game-only env).
    for b in &backends {
        b.pre_launch(game, &mut launch.game_env_vars)
            .with_context(|| format!("backend pre_launch ({})", b.ext_id()))?;
    }

    // 3. ScriptBuilders contribute dynamic block content.
    hooks::apply_script_builders(&mut launch, game, &exts);

    let plan = launch
        .exec_plan()
        .context("no game command to launch (empty %command%)")?;

    // 4. PreLaunch hooks, then spawn.
    hooks::run_stage(Stage::PreLaunch, game, &exts);

    let mut cmd = Command::new(&plan.program);
    cmd.args(&plan.args);
    for (k, action) in &plan.env {
        match action {
            EnvAction::Set(v) => {
                cmd.env(k, v);
            }
            EnvAction::Unset => {
                cmd.env_remove(k);
            }
        }
    }

    eprintln!("ritz: launching `{}`", plan.program);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawning `{}`", plan.program))?;

    hooks::run_stage(Stage::PostSpawn, game, &exts);

    // 5. Supervise.
    let exit = supervise_loop(ctx, game, steam, &exts, &backends, &mut child)?;

    // 6. PostExit hooks + backend cleanup.
    hooks::run_stage(Stage::PostExit, game, &exts);
    for b in &backends {
        if let Err(e) = b.post_exit(game) {
            eprintln!("ritz: backend post_exit ({}) failed: {e:#}", b.ext_id());
        }
    }

    Ok(exit)
}

fn supervise_loop(
    ctx: &AppContext,
    game: &ResolvedGame,
    steam: &SteamCommand,
    exts: &[&ritz_core::extension::LoadedExtension],
    backends: &[Box<dyn Backend>],
    child: &mut Child,
) -> Result<i32> {
    // Termination forwarding.
    let term = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, term.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, term.clone())?;
    let mut forwarded = false;

    // Game-ready: which process names to await.
    let ready_names: Vec<String> = backends
        .iter()
        .filter_map(|b| b.ready_process(game))
        .chain(steam.game_name.clone())
        .collect();
    let mut ready_fired = ready_names.is_empty();
    let start = Instant::now();

    // Live reload: watch game config + every preset file in the assigned chain.
    let mut watched: Vec<(std::path::PathBuf, Option<std::time::SystemTime>)> = {
        let mut v = vec![];
        let p = ctx.paths.game(&game.appid);
        v.push((p.clone(), mtime(&p)));
        let mut cur = game.preset.as_ref().map(|p| p.name.clone());
        let mut seen = std::collections::HashSet::new();
        while let Some(name) = cur {
            if !seen.insert(name.clone()) { break; }
            let p = ctx.paths.preset(&name);
            v.push((p.clone(), mtime(&p)));
            cur = ctx.paths.load_preset(&name).ok().flatten().and_then(|p| p.parent);
        }
        v
    };

    loop {
        if let Some(status) = child.try_wait()? {
            let code = status
                .code()
                .unwrap_or_else(|| 128 + status.signal().unwrap_or(0));
            return Ok(code);
        }

        if term.load(Ordering::Relaxed) && !forwarded {
            forward_term(child);
            forwarded = true;
        }

        if !ready_fired {
            if ready_names.iter().any(|n| process_running(n)) {
                hooks::run_stage(Stage::OnGameReady, game, exts);
                for b in backends {
                    if let Err(e) = b.on_game_ready(game) {
                        eprintln!("ritz: backend on_game_ready ({}) failed: {e:#}", b.ext_id());
                    }
                }
                ready_fired = true;
            } else if start.elapsed() > READY_TIMEOUT {
                ready_fired = true; // give up; game may have no matching process name
            }
        }

        // Live config reload — trigger if any watched file changed.
        let any_changed = watched.iter_mut().any(|(path, last)| {
            let now = mtime(path);
            if now != *last { *last = now; true } else { false }
        });
        if any_changed {
            if let Ok(updated) = ctx.resolve_game(steam) {
                for b in backends {
                    if let Err(e) = b.live_reload(&updated) {
                        eprintln!("ritz: backend live_reload ({}) failed: {e:#}", b.ext_id());
                    }
                }
            }
        }

        std::thread::sleep(POLL);
    }
}

fn forward_term(child: &Child) {
    let pid = nix::unistd::Pid::from_raw(child.id() as i32);
    let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGTERM);
}

/// True if a process with the exact name is running (via `pgrep -x`).
fn process_running(name: &str) -> bool {
    Command::new("pgrep")
        .arg("-x")
        .arg(name)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn mtime(path: &std::path::Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}
