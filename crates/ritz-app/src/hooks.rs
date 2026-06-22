//! Extension scripts: lifecycle hooks (PreLaunch / PostSpawn / OnGameReady /
//! PostExit) and ScriptBuilders (dynamic block contributions).
//!
//! Hooks and scripts run with cwd = the extension directory and the resolved
//! variables exported as `RITZ_VAR_<name>` and `RITZ_APPID`. A hook may opt into
//! background (non-blocking) execution via `{ "Script": ..., "Background": true }`.
//! ScriptBuilders
//! additionally receive the extension's resolved variables as JSON on stdin and
//! the appid as `argv[1]`, and print contributions to stdout.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use ritz_core::builder::{EnvAction, LaunchCommand};
use ritz_core::extension::LoadedExtension;
use ritz_core::schema::{HookSpec, Hooks};

use crate::context::ResolvedGame;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    PreLaunch,
    PostSpawn,
    OnGameReady,
    PostExit,
}

fn script_for(hooks: &Hooks, stage: Stage) -> Option<&HookSpec> {
    match stage {
        Stage::PreLaunch => hooks.pre_launch.as_ref(),
        Stage::PostSpawn => hooks.post_spawn.as_ref(),
        Stage::OnGameReady => hooks.on_game_ready.as_ref(),
        Stage::PostExit => hooks.post_exit.as_ref(),
    }
}

/// Resolved variables for one extension as `RITZ_VAR_<name>` env pairs.
fn var_env(game: &ResolvedGame, ext_id: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    env.insert("RITZ_APPID".to_string(), game.appid.clone());
    if let Some(ext) = game.resolution.exts.get(ext_id) {
        for (name, field) in &ext.fields {
            let value = if field.var.value.is_empty() {
                field.var.truthy.to_string()
            } else {
                field.var.value.clone()
            };
            env.insert(format!("RITZ_VAR_{name}"), value);
        }
    }
    env
}

/// Run the hook for a stage across all applicable extensions. Errors are logged,
/// not fatal — a misbehaving hook must not abort the game.
pub fn run_stage(stage: Stage, game: &ResolvedGame, exts: &[&LoadedExtension]) {
    for ext in exts {
        let Some(hooks) = &ext.spec.hooks else { continue };
        let Some(hook) = script_for(hooks, stage) else {
            continue;
        };
        let env = var_env(game, &ext.spec.id());
        let script = hook.script();
        let result = if hook.background() {
            spawn_background(&ext.dir, script, &env)
        } else {
            run_one(&ext.dir, script, &env)
        };
        if let Err(e) = result {
            eprintln!(
                "ritz: hook {script} ({:?}) for {} failed: {e:#}",
                stage, ext.spec.meta.name
            );
        }
    }
}

/// Run a hook and wait for it to finish (the default).
fn run_one(dir: &Path, script: &str, env: &BTreeMap<String, String>) -> Result<()> {
    let path = dir.join(script);
    let status = Command::new("sh")
        .arg(path)
        .current_dir(dir)
        .envs(env)
        .status()
        .with_context(|| format!("spawning hook {script}"))?;
    if !status.success() {
        anyhow::bail!("hook {script} exited with {status}");
    }
    Ok(())
}

/// Spawn a hook in the background (not waited on) — for watchers, notification
/// sounds, gamemode-by-pid, etc. The child outlives this call.
fn spawn_background(dir: &Path, script: &str, env: &BTreeMap<String, String>) -> Result<()> {
    let path = dir.join(script);
    Command::new("sh")
        .arg(path)
        .current_dir(dir)
        .envs(env)
        .stdin(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning background hook {script}"))?;
    Ok(())
}

/// Run all ScriptBuilders, injecting their output into the launch command.
pub fn apply_script_builders(
    launch: &mut LaunchCommand,
    game: &ResolvedGame,
    exts: &[&LoadedExtension],
) {
    for ext in exts {
        for sb in &ext.spec.script_builders {
            match run_script_builder(&ext.dir, &sb.script, game, &ext.spec.id()) {
                Ok(out) => inject(launch, &sb.block, &out),
                Err(e) => eprintln!(
                    "ritz: ScriptBuilder {} for {} failed: {e:#}",
                    sb.script, ext.spec.meta.name
                ),
            }
        }
    }
}

fn run_script_builder(
    dir: &Path,
    script: &str,
    game: &ResolvedGame,
    ext_id: &str,
) -> Result<String> {
    // Resolved vars for this extension as a JSON object on stdin.
    let mut obj = serde_json::Map::new();
    if let Some(ext) = game.resolution.exts.get(ext_id) {
        for (name, field) in &ext.fields {
            obj.insert(name.clone(), serde_json::Value::String(field.var.value.clone()));
        }
    }
    let stdin_json = serde_json::Value::Object(obj).to_string();

    let mut child = Command::new("sh")
        .arg(dir.join(script))
        .arg(&game.appid)
        .current_dir(dir)
        .envs(var_env(game, ext_id))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning ScriptBuilder {script}"))?;

    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin_json.as_bytes())?;
    let out = child.wait_with_output()?;
    if !out.status.success() {
        anyhow::bail!("ScriptBuilder {script} exited with {}", out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Inject ScriptBuilder stdout into the named launch block.
fn inject(launch: &mut LaunchCommand, block: &str, output: &str) {
    match block {
        "ENV_VARS" | "GAME_ENV_VARS" => {
            let target = if block == "ENV_VARS" {
                &mut launch.env_vars
            } else {
                &mut launch.game_env_vars
            };
            for line in output.lines() {
                let line = line.trim();
                if line.is_empty() {
                    continue;
                }
                if let Some((k, v)) = line.split_once('=') {
                    target.insert(k.to_string(), EnvAction::Set(v.to_string()));
                }
            }
        }
        "GAME_LAUNCH_ARGS" => {
            for line in output.lines() {
                launch.game_args.extend(split_tokens(line));
            }
        }
        "WRAPPERS" => {
            for line in output.lines() {
                launch.wrappers.extend(split_tokens(line));
            }
        }
        other => eprintln!("ritz: ScriptBuilder targets unknown block `{other}`"),
    }
}

fn split_tokens(line: &str) -> Vec<String> {
    let line = line.trim();
    if line.is_empty() {
        return Vec::new();
    }
    shlex::split(line).unwrap_or_else(|| line.split_whitespace().map(String::from).collect())
}
