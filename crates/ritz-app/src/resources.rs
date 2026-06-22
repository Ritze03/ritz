//! Bundled resources, embedded into the binary at compile time.
//!
//! The entire `resources/` tree (font, extensions, plugins) is baked in via
//! `include_dir`. On startup [`bootstrap`] copies the `extensions/` and
//! `plugins/` subtrees into the user's config directory (always overwriting
//! bundled files so updates propagate), so the binary is fully self-contained.

use std::path::Path;

use anyhow::{Context, Result};
use include_dir::{include_dir, Dir, DirEntry};
use ritz_core::config::Paths;

static RESOURCES: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../resources");

/// The bundled monospace text font (Geist Mono, TTF — crisper than the OTF).
pub fn mono_font_bytes() -> &'static [u8] {
    RESOURCES
        .get_file("assets/GeistMono-Regular.ttf")
        .expect("bundled mono font is present")
        .contents()
}

/// The bundled proportional sans text font (Geist).
pub fn sans_font_bytes() -> &'static [u8] {
    RESOURCES
        .get_file("assets/Geist-Regular.ttf")
        .expect("bundled sans font is present")
        .contents()
}

/// The bundled bold sans font (Geist Bold) — used for the logo wordmark.
pub fn bold_font_bytes() -> &'static [u8] {
    RESOURCES
        .get_file("assets/Geist-Bold.ttf")
        .expect("bundled bold font is present")
        .contents()
}

/// The bundled Nerd Font (Geist Mono patched) — used only for icon glyphs.
pub fn icon_font_bytes() -> &'static [u8] {
    RESOURCES
        .get_file("assets/GeistMonoNerdFont-Regular.otf")
        .expect("bundled icon font is present")
        .contents()
}

/// The bundled app logo (128×128 PNG).
pub fn logo_bytes() -> &'static [u8] {
    RESOURCES
        .get_file("assets/logo/logo_128.png")
        .expect("bundled logo is present")
        .contents()
}

/// The bundled high-res app logo (1024×1024 PNG) — used by the splash animation.
pub fn logo_1024_bytes() -> &'static [u8] {
    RESOURCES
        .get_file("assets/logo/logo_1024.png")
        .expect("bundled 1024 logo is present")
        .contents()
}

/// Export bundled `extensions/` and `plugins/` into the config directory,
/// only creating files that don't already exist (never touching the user's).
pub fn bootstrap(paths: &Paths) -> Result<()> {
    export(paths, false)
}

/// Re-export bundled `extensions/` and `plugins/`, overwriting existing files.
/// Backs the explicit "Re-Export Modules and Plugins" action.
pub fn reexport(paths: &Paths) -> Result<()> {
    export(paths, true)
}

fn export(paths: &Paths, overwrite: bool) -> Result<()> {
    for top in ["extensions", "plugins"] {
        if let Some(dir) = RESOURCES.get_dir(top) {
            export_dir(dir, &paths.base, overwrite)?;
        }
    }
    Ok(())
}

fn export_dir(dir: &Dir, base: &Path, overwrite: bool) -> Result<()> {
    for entry in dir.entries() {
        match entry {
            DirEntry::Dir(sub) => export_dir(sub, base, overwrite)?,
            DirEntry::File(file) => {
                // `file.path()` is relative to the resources root, e.g.
                // "extensions/amd.json" or "plugins/hypr-monctl.so".
                let dest = base.join(file.path());
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)
                        .with_context(|| format!("creating {}", parent.display()))?;
                }
                // On startup (not overwriting) leave any existing file untouched.
                if !overwrite && dest.exists() {
                    continue;
                }
                // Skip if unchanged, and never write in place: a bundled file
                // (e.g. hypr-monctl.so) may be mmap'd by another process
                // (Hyprland has the plugin dlopen'd). Truncating + rewriting the
                // same inode corrupts that live mapping and crashes it. Writing a
                // temp file + rename swaps the dir entry to a fresh inode, leaving
                // the mapped one intact.
                if std::fs::read(&dest).is_ok_and(|cur| cur == file.contents()) {
                    continue;
                }
                let tmp = dest.with_extension("tmp");
                std::fs::write(&tmp, file.contents())
                    .with_context(|| format!("writing {}", tmp.display()))?;
                std::fs::rename(&tmp, &dest)
                    .with_context(|| format!("installing {}", dest.display()))?;
            }
        }
    }
    Ok(())
}
