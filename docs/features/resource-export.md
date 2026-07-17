# Resource Export — embedded assets and the first-run copy to config

Ritz ships as one self-contained binary: fonts, the default/built-in extension
manifests, and native plugin `.so` files are baked in at compile time and, for the
extensions/plugins half, copied out to the user's config directory on first run so the
rest of the app can treat them as ordinary files on disk.

## How it works

- `crates/ritz-app/src/resources.rs` embeds the whole `resources/` tree with
  `include_dir!("$CARGO_MANIFEST_DIR/../../resources")` into a `static RESOURCES: Dir`.
  Everything under `resources/` (fonts in `assets/`, manifests in `extensions/`, native
  backends in `plugins/`) is part of the binary; there is no runtime dependency on the
  source tree.
- **Fonts and the logo are read straight from the embed, never exported.**
  `crates/ritz-app/src/resources.rs:mono_font_bytes` / `sans_font_bytes` /
  `bold_font_bytes` / `icon_font_bytes` / `logo_bytes` / `logo_1024_bytes` hand back
  `&'static [u8]` slices from `RESOURCES.get_file(...)`. `crates/ritz-app/src/fonts.rs:install`
  loads those bytes into `egui::FontDefinitions` on startup (and again, live, whenever
  `mono_ui` is toggled) — no filesystem round-trip for fonts at all.
- **Extensions and plugins are exported to disk**, because
  `crates/ritz-core/src/extension.rs:discover` reads `.json` manifests off a real
  directory, and the shipped `.so` plugins have to exist as loadable files for their
  native backends. `crates/ritz-app/src/resources.rs:bootstrap` is called once per
  process start from `crates/ritz-app/src/context.rs:AppContext::load`, before
  extensions are loaded, and copies the `extensions/` and `plugins/` subtrees from the
  embed into `paths.base` (`~/.config/ritz`, or `$RITZ_CONFIG_DIR`) — landing at
  `Paths::user_extensions` (`extensions/`) and `Paths::plugins_dir` (`plugins/`) in
  `crates/ritz-core/src/config.rs`.
- **Discovery reads only that one exported directory.** At runtime
  `crates/ritz-app/src/context.rs:extension_dirs` returns `vec![paths.user_extensions()]`
  (or a single `$RITZ_EXTENSIONS_DIR` override, used by tests) — there's no separate
  "shipped" search path at runtime; the bundled manifests and any user drop-in placed
  in the same `extensions/` folder are discovered identically by
  `crates/ritz-core/src/extension.rs:discover`, which recurses to any depth and treats
  every `.json` file as a manifest.
- **Export is missing-only on startup, overwrite-everything only when asked.**
  `crates/ritz-app/src/resources.rs:export_dir` does the actual file-by-file copy and
  takes an `overwrite` flag: `bootstrap` calls it with `overwrite = false` (skip any
  file that already exists), `crates/ritz-app/src/resources.rs:reexport` calls it with
  `overwrite = true`. *Why export-missing-only:* the launcher must ship working
  defaults on first run, but a user who edited a shipped manifest or dropped in their
  own extension must never have it silently clobbered on the next launch — bootstrap is
  strictly additive, and only the explicit "Re-Export Modules and Plugins" action is
  allowed to overwrite.
- **Bundled files are never written in place, even on overwrite.** When a destination
  differs from the embedded contents, `export_dir` writes to a `.tmp` sibling
  (`dest.with_extension("tmp")`) then `std::fs::rename`s it over the real path, instead
  of truncating the existing file. *Why atomic `.so` writes:* a plugin like
  `hypr-monctl.so` under `plugins/` may be `dlopen`'d and `mmap`'d live by Hyprland;
  truncating and rewriting the same inode would corrupt that mapping and crash the
  compositor. The temp-file-then-rename swaps the directory entry to a fresh inode and
  leaves whatever's already mapped untouched. This applies to every exported file, not
  just `.so`s, and `export_dir` also skips the write entirely when the on-disk bytes
  already match the embed, so an unchanged file is never touched at all — not even via
  the atomic path.
- **Overwriting still requires the user's explicit, confirmed consent.** Re-export is
  reachable only through `ConfirmAction::ReExportResources`, wired to the danger-styled
  "Re-Export Modules and Plugins" button (`crates/ritz-app/src/gui.rs`, around line
  1816) behind a confirmation modal (`crates/ritz-app/src/gui.rs:render_confirm_dialog`)
  that warns local edits will be lost. *Why gate it behind confirmation:* the same
  operation that fixes a corrupted or outdated shipped manifest is destructive to any
  user customization of that same file — there's no way to distinguish the two cases
  from the file alone, so the app asks instead of guessing.

## Using it

- **First run / every normal launch:** nothing to do — `AppContext::load` calls
  `bootstrap` automatically before extensions are loaded, so the config directory
  always has every shipped module and plugin present, without touching anything a user
  already customized.
- **Settings → Maintenance Actions** (`crates/ritz-app/src/gui.rs`, around line 1800)
  exposes two destructive, confirmed actions:
  - **Re-Export Modules and Plugins** — re-runs export with `overwrite = true`, restoring
    every shipped extension manifest and plugin `.so` to its bundled state, then calls
    `reload_extensions` so the running editor picks up the restored files immediately.
    Use this to recover from a manually broken shipped manifest or to pick up updated
    bundled defaults that `bootstrap` won't touch because a same-named file already
    exists.
  - **Clean Up Configs** — a related but separate maintenance action
    (`crates/ritz-app/src/gui.rs:config_cleanup`): not a resource-export operation, it
    prunes stored variable values that no longer correspond to any currently loaded
    module (e.g. after an extension update renames or removes a field), across the
    global config, every profile, and every game.

## Related links

- [Scoped Config](scoped-config.md) — the "Export on startup: missing-only, never
  overwrite" section covers the same `bootstrap`/`reexport` split from the config-layer
  side (why user overrides must never be clobbered).
- [Extension System](extension-system.md) — how the manifests this page exports get
  parsed, resolved, and built into a launch command once they're on disk.
- [Bundled Modules](bundled-modules.md) — the catalogue of shipped manifests under
  `resources/extensions/` that `bootstrap`/`reexport` copy out.
- [runtime-backends.md](runtime-backends.md) — the native `.so` backends (e.g.
  `hypr-monctl.so`) whose atomic-write handling is described here.
