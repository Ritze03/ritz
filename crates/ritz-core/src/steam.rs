//! Parsing of Steam's expanded `%command%`.
//!
//! Real captured structure (native Linux, no Proton) is a nested-`--` chain:
//!
//! ```text
//! steam-launch-wrapper
//!   -- reaper SteamLaunch AppId=<id>
//!   -- <SteamLinuxRuntime_{sniper,soldier}>/_v2-entry-point --verb=waitforexitandrun
//!   [ -- <SteamLinuxRuntime>/scout-on-soldier-entry-point-v2 ]
//!   -- <game-exe> [game args]
//! ```
//!
//! Design: **wrap, don't dissect.** `raw` (the entire `%command%`) is preserved
//! verbatim as the game command; ritz prepends its wrappers to the left. Segment
//! splitting is used only to (a) derive a default game name and (b) optionally
//! strip injected *overlay* wrappers (mangohud/gamemoderun/…) — never the runtime.


#[derive(Debug, Clone)]
pub struct SteamCommand {
    /// AppId from environment, or parsed from a `reaper` `AppId=` token.
    pub appid: Option<String>,
    /// The full `%command%` tokens, verbatim.
    pub raw: Vec<String>,
    /// Default human name derived from the final-segment executable basename.
    pub game_name: Option<String>,
}

impl SteamCommand {
    /// Parse the tokens that follow the `ritz` binary in argv.
    pub fn parse(args: &[String], env_appid: Option<String>) -> Self {
        let appid = env_appid.or_else(|| parse_appid_token(args));
        let game_name = install_dir_name(args);
        SteamCommand {
            appid,
            raw: args.to_vec(),
            game_name,
        }
    }

}

fn parse_appid_token(args: &[String]) -> Option<String> {
    args.iter()
        .find_map(|tok| tok.strip_prefix("AppId=").map(|s| s.to_string()))
}

/// Derive the game name from its Steam install folder: the `<DIR>` in
/// `steamapps/common/<DIR>/…`, skipping the runtime/proton dirs. Reliable for
/// both native and Proton games (where the final-segment exe is just `proton`).
/// Returns `None` for non-standard installs — callers must not invent a name.
fn install_dir_name(args: &[String]) -> Option<String> {
    const MARKER: &str = "steamapps/common/";
    for tok in args {
        let Some(idx) = tok.find(MARKER) else { continue };
        let dir = tok[idx + MARKER.len()..].split('/').next().unwrap_or("");
        if dir.is_empty() {
            continue;
        }
        let lower = dir.to_ascii_lowercase();
        if lower.starts_with("steamlinuxruntime") || lower.starts_with("proton") {
            continue;
        }
        return Some(dir.to_string());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn toks(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    /// CS2 (sniper, single runtime level).
    fn cs2() -> Vec<String> {
        toks(&[
            "/home/mo/.local/share/Steam/ubuntu12_32/steam-launch-wrapper",
            "--",
            "/home/mo/.local/share/Steam/ubuntu12_32/reaper",
            "SteamLaunch",
            "AppId=730",
            "--",
            "/home/mo/.local/share/Steam/steamapps/common/SteamLinuxRuntime_sniper/_v2-entry-point",
            "--verb=waitforexitandrun",
            "--",
            "/home/mo/Schinken/Linux/Steam/steamapps/common/Counter-Strike Global Offensive/game/cs2.sh",
            "-steam",
        ])
    }

    /// Flappy Lord (soldier, nested scout level).
    fn flappy() -> Vec<String> {
        toks(&[
            "/home/mo/.local/share/Steam/ubuntu12_32/steam-launch-wrapper",
            "--",
            "/home/mo/.local/share/Steam/ubuntu12_32/reaper",
            "SteamLaunch",
            "AppId=3746030",
            "--",
            "/home/mo/.local/share/Steam/steamapps/common/SteamLinuxRuntime_soldier/_v2-entry-point",
            "--verb=waitforexitandrun",
            "--",
            "/home/mo/Schinken/Linux/Steam/steamapps/common/SteamLinuxRuntime/scout-on-soldier-entry-point-v2",
            "--",
            "/home/mo/Schinken/Linux/Steam/steamapps/common/Flappy Lord/Flappy Lord.x86_64",
        ])
    }

    #[test]
    fn appid_from_env_wins() {
        let c = SteamCommand::parse(&cs2(), Some("999".into()));
        assert_eq!(c.appid.as_deref(), Some("999"));
    }

    #[test]
    fn appid_from_reaper_token() {
        let c = SteamCommand::parse(&cs2(), None);
        assert_eq!(c.appid.as_deref(), Some("730"));
        let f = SteamCommand::parse(&flappy(), None);
        assert_eq!(f.appid.as_deref(), Some("3746030"));
    }

    /// PEAK (Proton): final segment starts with `proton`, real name is the
    /// install folder. Mirrors /tmp/ritz-cmd.txt.
    fn peak() -> Vec<String> {
        toks(&[
            "/home/mo/.local/share/Steam/ubuntu12_32/steam-launch-wrapper",
            "--",
            "/home/mo/.local/share/Steam/ubuntu12_32/reaper",
            "SteamLaunch",
            "AppId=3527290",
            "--",
            "/home/mo/Schinken/Linux/Steam/steamapps/common/SteamLinuxRuntime_4/_v2-entry-point",
            "--verb=waitforexitandrun",
            "--",
            "/usr/share/steam/compatibilitytools.d/proton-cachyos-slr/proton",
            "waitforexitandrun",
            "/home/mo/Schinken/Linux/Steam/steamapps/common/PEAK/PEAK.exe",
            "-force-vulkan",
        ])
    }

    #[test]
    fn name_from_install_folder() {
        // Skips the runtime dir, returns the game's install folder.
        assert_eq!(
            SteamCommand::parse(&cs2(), None).game_name.as_deref(),
            Some("Counter-Strike Global Offensive")
        );
        assert_eq!(
            SteamCommand::parse(&flappy(), None).game_name.as_deref(),
            Some("Flappy Lord")
        );
        // Proton: not "proton" — the install folder (runtime + proton dirs skipped).
        assert_eq!(
            SteamCommand::parse(&peak(), None).game_name.as_deref(),
            Some("PEAK")
        );
        // Non-standard install (no steamapps/common) → None, never invented.
        assert_eq!(
            SteamCommand::parse(&toks(&["/opt/mygame/run.sh"]), None).game_name,
            None
        );
    }

    #[test]
    fn raw_is_preserved_verbatim() {
        let c = SteamCommand::parse(&cs2(), None);
        assert_eq!(c.raw, cs2());
    }
}
