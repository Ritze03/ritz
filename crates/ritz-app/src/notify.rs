//! Desktop notifications via `notify-send`. Best-effort: failures are ignored.

use std::process::{Command, Stdio};

pub fn send(summary: &str, body: &str) {
    let _ = Command::new("notify-send")
        .arg("-a")
        .arg("Ritz Launcher")
        .arg(summary)
        .arg(body)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}
