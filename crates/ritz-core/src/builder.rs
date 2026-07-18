//! The `LaunchCommandBuilder`: assembles a [`LaunchCommand`] from a set of
//! extensions and their resolved variables.
//!
//! Block order: `[ENV_VARS] [WRAPPERS] [GAME_ENV_VARS] [GAME_LAUNCH_COMMAND] [GAME_LAUNCH_ARGS]`.
//!
//! - ENV_VARS apply to the whole chain (the child process environment).
//! - GAME_ENV_VARS apply only to the game, emitted via an `env` shim after the
//!   wrapper `--` (so wrappers like gamescope don't see them). With no wrapper
//!   they merge into the chain env.
//! - GAME_LAUNCH_COMMAND is the entire `%command%` preserved verbatim.

use std::fmt;

use indexmap::IndexMap;

use crate::condition;
use crate::error::Result;
use crate::schema::{EnvOp, Extension};
use crate::variables::VarStore;

/// One extension paired with its resolved variables for this build.
pub struct ExtInput<'a> {
    pub spec: &'a Extension,
    pub vars: &'a VarStore,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvAction {
    Set(String),
    Unset,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LaunchCommand {
    /// Whole-chain environment (applies to wrappers + game).
    pub env_vars: IndexMap<String, EnvAction>,
    /// Wrapper tokens in priority order (each wrapper's `--` is included in its syntax).
    pub wrappers: Vec<String>,
    /// Game-only environment (emitted via `env` shim).
    pub game_env_vars: IndexMap<String, EnvAction>,
    /// The entire `%command%`, verbatim.
    pub game_command: Vec<String>,
    /// Extra args appended after the game command.
    pub game_args: Vec<String>,
}

fn req(requires: &Option<String>, vars: &VarStore) -> Result<bool> {
    condition::eval_opt(requires.as_deref(), &vars.lookup_fn())
}

/// Accumulator for one env var name across all contributing builder entries.
#[derive(Default)]
struct EnvAccum {
    applied: bool,
    unset: bool,
    value: String,
}

fn build_env_block<'a>(
    inputs: &[ExtInput<'a>],
    select: impl Fn(&'a Extension) -> &'a [crate::schema::EnvVarSpec],
) -> Result<IndexMap<String, EnvAction>> {
    let mut accums: IndexMap<String, EnvAccum> = IndexMap::new();

    for input in inputs {
        for spec in select(input.spec) {
            if !req(&spec.requires, input.vars)? {
                continue;
            }
            let resolved_name = input.vars.interpolate(&spec.name);
            if resolved_name.is_empty() { continue; }
            let accum = accums.entry(resolved_name).or_default();
            for entry in &spec.builder {
                if !req(&entry.requires, input.vars)? {
                    continue;
                }
                let value = input
                    .vars
                    .interpolate(entry.value.as_deref().unwrap_or(""));
                match entry.op {
                    EnvOp::Set => {
                        accum.applied = true;
                        accum.unset = false;
                        accum.value = value;
                    }
                    EnvOp::Append => {
                        accum.applied = true;
                        accum.unset = false;
                        let sep = entry.separator.as_deref().unwrap_or(",");
                        if accum.value.is_empty() {
                            accum.value = value;
                        } else {
                            accum.value = format!("{}{}{}", accum.value, sep, value);
                        }
                    }
                    EnvOp::Unset => {
                        accum.applied = true;
                        accum.unset = true;
                        accum.value.clear();
                    }
                }
            }
        }
    }

    let mut out = IndexMap::new();
    for (name, accum) in accums {
        if !accum.applied {
            continue;
        }
        let action = if accum.unset {
            EnvAction::Unset
        } else {
            EnvAction::Set(accum.value)
        };
        out.insert(name, action);
    }
    Ok(out)
}

fn build_wrappers(inputs: &[ExtInput<'_>]) -> Result<Vec<String>> {
    // (priority, extension order, tokens)
    let mut collected: Vec<(i64, usize, Vec<String>)> = Vec::new();

    for (idx, input) in inputs.iter().enumerate() {
        for wrapper in &input.spec.wrappers {
            if !req(&wrapper.requires, input.vars)? {
                continue;
            }
            let mut opts: Vec<String> = Vec::new();
            for entry in &wrapper.builder {
                if !req(&entry.requires, input.vars)? {
                    continue;
                }
                opts.push(input.vars.interpolate(&entry.value));
            }
            let options_str = opts.join(" ");
            let rendered = wrapper.command_syntax.replace("{OPTIONS}", &options_str);
            let rendered = input.vars.interpolate(&rendered);
            let tokens = shlex::split(&rendered)
                .unwrap_or_else(|| rendered.split_whitespace().map(String::from).collect());
            collected.push((wrapper.priority, idx, tokens));
        }
    }

    collected.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
    Ok(collected.into_iter().flat_map(|(_, _, tokens)| tokens).collect())
}

fn build_args(inputs: &[ExtInput<'_>]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for input in inputs {
        for arg in &input.spec.game_launch_args {
            if !req(&arg.requires, input.vars)? {
                continue;
            }
            let value = input.vars.interpolate(&arg.value);
            let tokens = shlex::split(&value)
                .unwrap_or_else(|| value.split_whitespace().map(String::from).collect());
            out.extend(tokens);
        }
    }
    Ok(out)
}

/// Apply a list-backed custom-env / custom-game-env backend: each non-empty line
/// of `list` is split on the FIRST `=` into `name=value`, inserted as a
/// [`EnvAction::Set`]. Lines without `=` or with an empty name are skipped. The
/// split-on-newline-first guarantees a name can never contain a newline.
fn apply_list_env(env: &mut IndexMap<String, EnvAction>, list: &str) {
    for line in list.split('\n') {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        env.insert(name.to_string(), EnvAction::Set(value.to_string()));
    }
}

/// Build the full launch command.
pub fn build(inputs: &[ExtInput<'_>], game_command: &[String]) -> Result<LaunchCommand> {
    let mut env_vars = build_env_block(inputs, |e| &e.env_vars)?;
    let wrappers = build_wrappers(inputs)?;
    let mut game_env_vars = build_env_block(inputs, |e| &e.game_env_vars)?;
    let mut game_args = build_args(inputs)?;

    // Backend pre-pass: list-backed custom modules expand a single multi_string
    // scalar (`entries.join("\n")`) into N env vars / args. A module's backend is
    // a bare string; an empty/unset list var resolves to "" and is a no-op.
    for input in inputs {
        match input.spec.backend.as_deref() {
            Some("custom-env") => apply_list_env(&mut env_vars, input.vars.value("env")),
            Some("custom-game-env") => {
                apply_list_env(&mut game_env_vars, input.vars.value("game_env"))
            }
            Some("custom-args") => {
                for line in input.vars.value("args").split('\n') {
                    if !line.is_empty() {
                        // Verbatim: no shlex/word-splitting — spaces stay in one arg.
                        game_args.push(line.to_string());
                    }
                }
            }
            _ => {}
        }
    }

    Ok(LaunchCommand {
        env_vars,
        wrappers,
        game_env_vars,
        game_command: game_command.to_vec(),
        game_args,
    })
}

// ---- Display (for `ritz --print` and GUI preview) ------------------------

/// Minimal shell quoting: only quote tokens that need it. Leaves `KEY=VAL`,
/// paths, and common flag characters unquoted.
fn shq(s: &str) -> String {
    if s.is_empty() {
        return "''".to_string();
    }
    let safe = s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "-_=/.,:+@%".contains(c));
    if safe {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

/// Render an env map as a *prefix* (`KEY=VAL …`, or `env -u KEY …` when unsets
/// are present).
fn render_prefix_env(env: &IndexMap<String, EnvAction>) -> Vec<String> {
    if env.is_empty() {
        return Vec::new();
    }
    let has_unset = env.values().any(|a| matches!(a, EnvAction::Unset));
    let mut out = Vec::new();
    if has_unset {
        out.push("env".to_string());
        for (k, action) in env {
            match action {
                EnvAction::Unset => {
                    out.push("-u".to_string());
                    out.push(shq(k));
                }
                EnvAction::Set(v) => out.push(format!("{}={}", k, shq(v))),
            }
        }
    } else {
        for (k, action) in env {
            if let EnvAction::Set(v) = action {
                out.push(format!("{}={}", k, shq(v)));
            }
        }
    }
    out
}

/// Render a game-only env map as an `env` shim (always prefixed with `env`).
fn render_env_shim(env: &IndexMap<String, EnvAction>) -> Vec<String> {
    let mut out = vec!["env".to_string()];
    for (k, action) in env {
        match action {
            EnvAction::Unset => {
                out.push("-u".to_string());
                out.push(shq(k));
            }
            EnvAction::Set(v) => out.push(format!("{}={}", k, shq(v))),
        }
    }
    out
}

impl LaunchCommand {
    /// Flat token list for display / preview (shell-quoted).
    pub fn display_tokens(&self) -> Vec<String> {
        let mut parts: Vec<String> = Vec::new();
        if self.wrappers.is_empty() {
            // No wrapper: game env and chain env share the same scope — merge.
            let mut merged = self.env_vars.clone();
            for (k, v) in &self.game_env_vars {
                merged.insert(k.clone(), v.clone());
            }
            parts.extend(render_prefix_env(&merged));
            parts.extend(self.game_command.iter().map(|t| shq(t)));
            parts.extend(self.game_args.iter().map(|t| shq(t)));
        } else {
            parts.extend(render_prefix_env(&self.env_vars));
            parts.extend(self.wrappers.iter().map(|t| shq(t)));
            if !self.game_env_vars.is_empty() {
                parts.extend(render_env_shim(&self.game_env_vars));
            }
            parts.extend(self.game_command.iter().map(|t| shq(t)));
            parts.extend(self.game_args.iter().map(|t| shq(t)));
        }
        parts
    }
}

impl fmt::Display for LaunchCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.display_tokens().join(" "))
    }
}

/// A spawnable plan: program + argv tail + the environment to apply to the child.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecPlan {
    pub program: String,
    pub args: Vec<String>,
    /// Whole-chain environment to apply to the child process.
    pub env: IndexMap<String, EnvAction>,
}

impl LaunchCommand {
    /// Produce a spawnable [`ExecPlan`], or `None` if there is no game command.
    ///
    /// Game-only env is realized via the `env` shim inside argv when a wrapper is
    /// present (so wrappers don't see it); with no wrapper it merges into the
    /// child's process environment.
    pub fn exec_plan(&self) -> Option<ExecPlan> {
        if self.game_command.is_empty() {
            return None;
        }
        let mut env = self.env_vars.clone();
        let mut argv: Vec<String> = Vec::new();

        if self.wrappers.is_empty() {
            for (k, v) in &self.game_env_vars {
                env.insert(k.clone(), v.clone());
            }
            argv.extend(self.game_command.iter().cloned());
            argv.extend(self.game_args.iter().cloned());
        } else {
            argv.extend(self.wrappers.iter().cloned());
            if !self.game_env_vars.is_empty() {
                argv.push("env".to_string());
                for (k, action) in &self.game_env_vars {
                    match action {
                        EnvAction::Unset => {
                            argv.push("-u".to_string());
                            argv.push(k.clone());
                        }
                        EnvAction::Set(v) => argv.push(format!("{k}={v}")),
                    }
                }
            }
            argv.extend(self.game_command.iter().cloned());
            argv.extend(self.game_args.iter().cloned());
        }

        let program = argv.remove(0);
        Some(ExecPlan {
            program,
            args: argv,
            env,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::variables::ResolvedVar;
    use serde_json::json;

    fn ext(json: serde_json::Value) -> Extension {
        serde_json::from_value(json).unwrap()
    }

    fn toks(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn env_set_clear_append_ordering() {
        // RADV_PERFTEST: clear (set ""), then append aco, append gpl.
        let spec = ext(json!({
            "Extension": {"Name": "amd", "Author": "Ritze", "Version": "1.0"},
            "ENV_VARS": [{
                "Name": "RADV_PERFTEST",
                "Requires": "radv_enabled",
                "Builder": [
                    {"Requires": "radv_clear", "Type": "set", "Value": ""},
                    {"Requires": "radv_aco", "Type": "append", "Value": "aco"},
                    {"Requires": "radv_gpl", "Type": "append", "Value": "gpl"}
                ]
            }]
        }));
        let mut vars = VarStore::new();
        for v in ["radv_enabled", "radv_clear", "radv_aco", "radv_gpl"] {
            vars.insert_local(v, ResolvedVar::new(true, "true"));
        }
        let lc = build(&[ExtInput { spec: &spec, vars: &vars }], &[]).unwrap();
        assert_eq!(
            lc.env_vars.get("RADV_PERFTEST"),
            Some(&EnvAction::Set("aco,gpl".to_string()))
        );
    }

    #[test]
    fn env_block_skipped_when_requires_false() {
        let spec = ext(json!({
            "Extension": {"Name": "amd", "Author": "Ritze", "Version": "1.0"},
            "ENV_VARS": [{
                "Name": "RADV_PERFTEST", "Requires": "radv_enabled",
                "Builder": [{"Requires": "radv_aco", "Type": "append", "Value": "aco"}]
            }]
        }));
        let vars = VarStore::new(); // nothing truthy
        let lc = build(&[ExtInput { spec: &spec, vars: &vars }], &[]).unwrap();
        assert!(lc.env_vars.is_empty());
    }

    #[test]
    fn wrapper_options_and_interpolation() {
        let spec = ext(json!({
            "Extension": {"Name": "gamescope", "Author": "Ritze", "Version": "1.0"},
            "WRAPPERS": [{
                "CommandSyntax": "gamescope {OPTIONS} --",
                "Requires": "gs_enabled",
                "Priority": 100,
                "Builder": [
                    {"Requires": "fullscreen", "Type": "add", "Value": "--fullscreen"},
                    {"Requires": "backend", "Type": "add", "Value": "--backend {backend}"}
                ]
            }]
        }));
        let mut vars = VarStore::new();
        vars.insert_local("gs_enabled", ResolvedVar::new(true, "true"));
        vars.insert_local("fullscreen", ResolvedVar::new(true, "true"));
        vars.insert_local("backend", ResolvedVar::new(true, "auto"));
        let lc = build(&[ExtInput { spec: &spec, vars: &vars }], &[]).unwrap();
        assert_eq!(
            lc.wrappers,
            toks(&["gamescope", "--fullscreen", "--backend", "auto", "--"])
        );
    }

    #[test]
    fn wrapper_priority_ordering() {
        let outer = ext(json!({
            "Extension": {"Name": "gamescope", "Author": "Ritze", "Version": "1.0"},
            "WRAPPERS": [{"CommandSyntax": "gamescope --", "Requires": "", "Priority": 100, "Builder": []}]
        }));
        let inner = ext(json!({
            "Extension": {"Name": "gamemode", "Author": "Ritze", "Version": "1.0"},
            "WRAPPERS": [{"CommandSyntax": "gamemoderun", "Requires": "", "Priority": 50, "Builder": []}]
        }));
        let vars = VarStore::new();
        let inputs = [
            ExtInput { spec: &outer, vars: &vars },
            ExtInput { spec: &inner, vars: &vars },
        ];
        let lc = build(&inputs, &[]).unwrap();
        // priority 50 (gamemoderun) comes before priority 100 (gamescope)
        assert_eq!(lc.wrappers, toks(&["gamemoderun", "gamescope", "--"]));
    }

    #[test]
    fn full_assembly_with_game_env_shim() {
        let spec = ext(json!({
            "Extension": {"Name": "cs2", "Author": "Ritze", "Version": "1.0"},
            "ENV_VARS": [{
                "Name": "RADV_PERFTEST", "Requires": "",
                "Builder": [{"Requires": "", "Type": "append", "Value": "aco"}]
            }],
            "WRAPPERS": [{
                "CommandSyntax": "gamescope {OPTIONS} --", "Requires": "", "Priority": 100,
                "Builder": [{"Requires": "", "Type": "add", "Value": "-f"}]
            }],
            "GAME_ENV_VARS": [{
                "Name": "MANGOHUD", "Requires": "",
                "Builder": [{"Requires": "", "Type": "set", "Value": "1"}]
            }],
            "GAME_LAUNCH_ARGS": [{"Requires": "", "Value": "-condebug"}]
        }));
        let vars = VarStore::new();
        let game = toks(&["/games/cs2"]);
        let lc = build(&[ExtInput { spec: &spec, vars: &vars }], &game).unwrap();
        assert_eq!(
            lc.to_string(),
            "RADV_PERFTEST=aco gamescope -f -- env MANGOHUD=1 /games/cs2 -condebug"
        );
    }

    #[test]
    fn exec_plan_with_wrapper_uses_env_shim() {
        let spec = ext(json!({
            "Extension": {"Name": "x", "Author": "Ritze", "Version": "1.0"},
            "WRAPPERS": [{"CommandSyntax": "gamescope {OPTIONS} --", "Requires": "", "Priority": 100,
                "Builder": [{"Requires": "", "Type": "add", "Value": "-f"}]}],
            "GAME_ENV_VARS": [{"Name": "MANGOHUD", "Requires": "",
                "Builder": [{"Requires": "", "Type": "set", "Value": "1"}]}],
            "ENV_VARS": [{"Name": "RADV_PERFTEST", "Requires": "",
                "Builder": [{"Requires": "", "Type": "set", "Value": "aco"}]}]
        }));
        let vars = VarStore::new();
        let game = toks(&["/games/cs2", "-steam"]);
        let lc = build(&[ExtInput { spec: &spec, vars: &vars }], &game).unwrap();
        let plan = lc.exec_plan().unwrap();
        assert_eq!(plan.program, "gamescope");
        assert_eq!(
            plan.args,
            toks(&["-f", "--", "env", "MANGOHUD=1", "/games/cs2", "-steam"])
        );
        assert_eq!(
            plan.env.get("RADV_PERFTEST"),
            Some(&EnvAction::Set("aco".into()))
        );
    }

    #[test]
    fn exec_plan_no_wrapper_merges_env() {
        let spec = ext(json!({
            "Extension": {"Name": "x", "Author": "Ritze", "Version": "1.0"},
            "GAME_ENV_VARS": [{"Name": "MANGOHUD", "Requires": "",
                "Builder": [{"Requires": "", "Type": "set", "Value": "1"}]}]
        }));
        let vars = VarStore::new();
        let game = toks(&["/steam-wrapper", "--", "/games/cs2"]);
        let lc = build(&[ExtInput { spec: &spec, vars: &vars }], &game).unwrap();
        let plan = lc.exec_plan().unwrap();
        assert_eq!(plan.program, "/steam-wrapper");
        assert_eq!(plan.args, toks(&["--", "/games/cs2"]));
        assert_eq!(plan.env.get("MANGOHUD"), Some(&EnvAction::Set("1".into())));
    }

    #[test]
    fn custom_backends_list_expansion() {
        // custom-env: list scalar (multi_string join) → N env vars, split on FIRST `=`.
        let env_spec = ext(json!({
            "Extension": {"Name": "Custom Env", "Author": "Ritze", "Version": "1.0"},
            "Backend": "custom-env",
            "UI": {"E": [{"Type": "multi_string", "Variable": "env"}]}
        }));
        let mut env_vars = VarStore::new();
        // Mirrors resolve.rs: multi_string resolves to entries.join("\n").
        // Includes a no-`=` line, an empty-name line, and an empty line — all skipped.
        env_vars.insert_local(
            "env",
            ResolvedVar::new(true, "FOO=bar\nBAZ=qux=1\nNOEQ\n=nokey\n"),
        );
        let lc = build(&[ExtInput { spec: &env_spec, vars: &env_vars }], &[]).unwrap();
        assert_eq!(lc.env_vars.get("FOO"), Some(&EnvAction::Set("bar".into())));
        // Split on the FIRST `=`: value keeps its remaining `=`.
        assert_eq!(lc.env_vars.get("BAZ"), Some(&EnvAction::Set("qux=1".into())));
        assert_eq!(lc.env_vars.len(), 2, "no-eq/empty-name/empty lines skipped");
        // No env var name ever contains a newline.
        assert!(lc.env_vars.keys().all(|k| !k.contains('\n')));

        // custom-game-env: same expansion, into game_env_vars, var `game_env`.
        let genv_spec = ext(json!({
            "Extension": {"Name": "Custom Game Env", "Author": "Ritze", "Version": "1.0"},
            "Backend": "custom-game-env",
            "UI": {"E": [{"Type": "multi_string", "Variable": "game_env"}]}
        }));
        let mut genv = VarStore::new();
        genv.insert_local("game_env", ResolvedVar::new(true, "MANGOHUD=1"));
        let lc = build(&[ExtInput { spec: &genv_spec, vars: &genv }], &[]).unwrap();
        assert_eq!(lc.game_env_vars.get("MANGOHUD"), Some(&EnvAction::Set("1".into())));
        assert!(lc.env_vars.is_empty());

        // custom-args: one arg per line, literal (no shlex) — spaces preserved.
        let args_spec = ext(json!({
            "Extension": {"Name": "Game Launch Args", "Author": "Ritze", "Version": "1.0"},
            "Backend": "custom-args",
            "UI": {"A": [{"Type": "multi_string", "Variable": "args"}]}
        }));
        let mut args = VarStore::new();
        args.insert_local("args", ResolvedVar::new(true, "-foo\n--bar baz"));
        let lc = build(&[ExtInput { spec: &args_spec, vars: &args }], &[]).unwrap();
        assert_eq!(lc.game_args, toks(&["-foo", "--bar baz"]));

        // Empty/unset list var → no-op.
        let empty = VarStore::new();
        let lc = build(&[ExtInput { spec: &env_spec, vars: &empty }], &[]).unwrap();
        assert!(lc.env_vars.is_empty());
    }

    #[test]
    fn no_wrapper_merges_game_env() {
        let spec = ext(json!({
            "Extension": {"Name": "x", "Author": "Ritze", "Version": "1.0"},
            "GAME_ENV_VARS": [{
                "Name": "MANGOHUD", "Requires": "",
                "Builder": [{"Requires": "", "Type": "set", "Value": "1"}]
            }]
        }));
        let vars = VarStore::new();
        let game = toks(&["/games/cs2"]);
        let lc = build(&[ExtInput { spec: &spec, vars: &vars }], &game).unwrap();
        // no `env` shim when there is no wrapper
        assert_eq!(lc.to_string(), "MANGOHUD=1 /games/cs2");
    }
}
