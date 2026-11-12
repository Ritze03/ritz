# Changelog

All notable user-facing changes, newest first. Categories:

**Added** (new features),
**Fixed** (bug and behaviour fixes),
**Removed** (things taken out),
**Info** (notes worth knowing).

## 2026-10-29

### Removed
- **Monitor control module**: removed, because the project behind it is no longer
  maintained.

### Fixed
- **Installer**: it now works on systems with no default Rust setup.

## 2026-10-24

### Added
- **Installer**: a single script installs, updates and removes the app, and adds a menu
  entry.

## 2026-10-03

### Added
- **Installer script**: a quick way to install, update and remove the app.

## 2026-09-29

### Added
- **Inheritance preview**: a new window shows which settings come from which profile,
  with changes highlighted.

## 2026-09-22

### Fixed
- **Inheritance badges**: they are hidden when a higher profile overrides them anyway.

## 2026-08-17

### Info
- **Hyprland**: updated for Hyprland 0.56.2.

## 2026-08-04

### Added
- **MangoHud and Taskset**: two new modules for the performance overlay and CPU pinning.

## 2026-08-01

### Info
- **Default settings**: new profiles start with updated default General Settings.

### Removed
- **Small VRAM flag**: removed an unverified option from the VKD3D module.

## 2026-07-31

### Added
- **More toggles**: new options for Proton, VKD3D and Mesa/AMD.
- **Website**: a new project website with real screenshots.

### Fixed
- **Launch arguments**: they are no longer wrongly quoted in the preview.
- **Module list**: it no longer stays greyed out after editing in IDE mode.
- **Mesa shader cache**: its size setting is no longer applied when you did not set it.
- **Option fields**: text fields no longer grow wider with each entry.

## 2026-07-26

### Added
- **Notification icon**: notifications now show the app logo.

## 2026-07-20

### Info
- **Interface update**: the app moved to a newer graphics toolkit, with the same look.

## 2026-07-19

### Added
- **IDE Mode**: edit modules in a new workspace with tabs, a live preview and a game
  selector.
- **Warnings**: new hints for common settings mistakes.

### Fixed
- **Unsaved changes**: closing the app or switching tabs now asks before discarding
  edits.
- **Renaming modules**: renaming and saving now work independently and more reliably.
- **Settings detection**: it no longer writes to the wrong profile.

## 2026-07-18

### Added
- **Module editor**: create, copy, edit, rename and delete your own modules.
- **PulseAudio module**: a new audio module.
- **Custom game variables**: a new place for per-game environment variables.

### Fixed
- **Config files**: they are now saved safely, and broken modules are skipped with a
  warning.

## 2026-07-14

### Info
- **VKD3D options**: added flags for the GPU upload heap, descriptor heap and pipeline
  cache.

## 2026-06-26

### Fixed
- **New-game window**: it is taller and always shows the launch options box.

## 2026-06-25

### Added
- **Inheritance views**: choose how inherited settings are shown, with depth markers per
  field.

### Fixed
- **Live reload**: changes to a profile or its parents are now picked up right away.
- **Text fields**: they no longer show old values after switching profiles.

## 2026-06-24

### Added
- **Profile inheritance**: profiles can build on a parent profile.

## 2026-06-23

### Added
- **Profile copying**: duplicate a profile.
- **List fields**: lists such as scripts are now edited as proper lists.

### Fixed
- **Edit markers**: unused fields show the right edit indicator.

## 2026-06-22

### Added
- **First release**: a Linux game launcher with a settings window, per-game profiles,
  modules for common tools and a splash screen.
- **Splash screen**: shows the saved game name for configured games.
