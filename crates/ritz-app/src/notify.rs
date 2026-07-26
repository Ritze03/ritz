//! Desktop notifications via `notify-send`. Best-effort: failures are ignored.

use std::path::PathBuf;
use std::process::{Command, Stdio};

use ritz_core::config::Paths;

/// Export the bundled app logo to the config folder so `notify-send` can carry it
/// as the notification's icon, and hand back that path.
///
/// The logo is normally read straight from the embed and never exported (see
/// `resources.rs`), but `notify-send -i` needs a real file on disk, so we drop a
/// copy at `<config>/ritz-icon.png` the first time a notification fires. Missing-only
/// so we don't churn the file on every notification. Best-effort: returns `None` on
/// any IO error and the notification simply goes out without an icon.
fn icon_path() -> Option<PathBuf> {
    let path = Paths::discover().base.join("ritz-icon.png");
    if !path.exists() {
        std::fs::create_dir_all(path.parent()?).ok()?;
        std::fs::write(&path, crate::resources::logo_bytes()).ok()?;
    }
    Some(path)
}

pub fn send(summary: &str, body: &str) {
    let mut cmd = Command::new("notify-send");
    cmd.arg("-a").arg("Ritz Launcher");
    if let Some(icon) = icon_path() {
        cmd.arg("-i").arg(icon);
    }
    let _ = cmd
        .arg(summary)
        .arg(body)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
