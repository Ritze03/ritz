# Splash & New-Game Wizard — the window between Steam and the game

Every launch through Steam briefly shows a small always-on-top window: a countdown
splash for a game ritz already knows, or a short naming/profile wizard the first
time it sees a new AppID. Both live in `crates/ritz-app/src/splash.rs` and run in
their own subprocess.

## How it works

- `crates/ritz-app/src/main.rs:cmd_launch` is the windowless coordinator: it spawns
  `ritz --splash <appid> -- <command>` as a **separate subprocess**
  (`main.rs:run_splash_subprocess`) and blocks on its exit code before deciding
  whether to launch, open the editor, or abort. *Why a subprocess:* `eframe`/winit
  only allow one event loop per process, and the splash needs to vanish the
  instant a choice is made — see `splash.rs` module doc.
- Exit codes are the IPC channel back to the coordinator:
  `main.rs:SPLASH_LAUNCH` (0), `main.rs:SPLASH_CANCEL` (10), `main.rs:SPLASH_EDIT`
  (11). `splash.rs:SplashApp::finish` and `NewGameApp`/`UnknownApp` call
  `std::process::exit(code)` directly rather than returning through eframe's
  normal teardown. *Why:* graceful shutdown leaves the window visible for a
  couple of frames while the process winds down — long enough to be visible if
  the next step (the editor) is slow to start. An immediate `exit` makes the
  splash disappear the instant a key is pressed.
- `main.rs:cmd_splash` picks **which** of the three screens to show, in order:
  1. Unconfigured **and** a non-numeric/unresolvable AppID (`appid == "unknown"`
     and no saved game config) → `splash::show_unknown` — the "set a stable
     `RITZ_APPID`" screen.
  2. Configured game not yet known to ritz (`ctx.paths.load_game(&appid)` is
     `None`) → `splash::show_new` — the naming/profile wizard.
  3. Otherwise → `splash::show` — the normal countdown splash.
- All three share `splash.rs:bottom_buttons` (buttons pinned to a bottom panel)
  and `splash.rs:paint_logo_anim` (the nodding-logo animation), and match the
  Graphite theme via `crate::theme::apply`.
- With no display available (`splash.rs:has_display` — checks
  `WAYLAND_DISPLAY`/`DISPLAY`), every entry point short-circuits: `show` returns
  `Launch` immediately, `show_new` returns `Launch` with the guessed name and
  default profile, so a headless Steam session (or CI) is never blocked on a
  window that can't render. *Why:* ritz must still work when SSH'd in or run
  without a compositor — silently proceeding beats hanging forever.

### The normal countdown splash — `splash::show`

- Fixed 560×410 window, non-resizable. Renders `splash.rs:launch_view`: the
  nodding logo, `Launching <name>`, `AppID <appid>`, the resolved profile
  (`splash.rs:profile_label`), and a large countdown (`{secs:.1}s`) that ticks
  down from `splash_timeout_secs` (per-game override, else the global default —
  see [Scoped Config](scoped-config.md)).
- Controls, always active: **Q** or **Esc** → cancel, **W** or **Enter** →
  launch now, **E** → edit config. The same three actions are also clickable
  buttons in the bottom row (`splash.rs:qwe_button_row`,
  `splash.rs:qwe_action`).
- Timeout expiring is treated identically to pressing **W** — `Launch`.
- If the window is closed by the compositor/WM instead of a key/button (e.g.
  Alt-F4), `splash::show` falls through to `SplashAction::Launch` after
  `eframe::run_native` returns — *Why:* `finish` normally never returns (it
  calls `process::exit`), so reaching that fallback line only happens on an
  external close, and treating it as "launch anyway" is safer than silently
  cancelling a game the user didn't mean to stop.

### The new-game wizard — `splash::show_new`

A 3-step wizard (`splash.rs:NewState`: `Naming` → `Choosing` → `Confirm`) shown
the first time ritz sees an AppID it has no saved config for. It writes nothing
itself — `main.rs:cmd_splash` calls `main.rs:create_new_game` after the wizard
returns `Launch`/`Edit`, using the name and profile the user picked.

1. **Naming** — a heading, `AppID <id>`, and a text field pre-filled with a best
   guess (`main.rs:cmd_splash`'s `name_guess`: the Steam store API name for a
   numeric AppID, else the install-folder name from the parsed launch command,
   else empty — never the raw AppID or the literal `"proton"`). **Enter** or the
   **Confirm** button advances to *Choosing*, but only once the name is
   non-empty (`splash.rs:NewGameApp::update`, `confirm_name && !name_buf.trim().is_empty()`).
2. **Choosing** — pick a profile from up to 10 pinned profiles
   (`main.rs:gather_pinned_profiles`, sorted by pin slot, truncated to 10), shown
   as a row of `splash.rs:pin_card`s (keybind digit + clipped name, 5 per row).
   Pressing digit **1–9** or **0** (mapped to slot 10 via
   `splash.rs:keybind_char`) picks that profile directly; **W** or the
   **"Use Default"** button picks the configured default profile
   (`ctx.general.default_preset`) instead; **Q**/**Esc** cancels the whole
   wizard. *Why keybind digits instead of arrow+enter:* a returning user can
   blind-pick a profile in one keystroke without waiting for the window to
   render.
3. **Confirm** — reuses the same `splash.rs:launch_view` as the normal splash
   (heading `Added <name>`, AppID, chosen profile) but with `remaining: None`,
   so it shows the static **"Ready to Launch!"** line instead of a countdown —
   there's no timeout on this screen, the user must actively choose **Q**
   cancel / **W** launch / **E** edit.

*Why no countdown on the wizard:* the whole point is a one-time decision (name +
profile) the user hasn't made before; auto-launching on a timer would risk
saving a wrong guessed name permanently.

### The non-Steam ID setup screen — `splash::show_unknown`

Shown when ritz can't resolve any AppID at all (a non-Steam shortcut launched
without `SteamAppId`/`RITZ_APPID` set). 560×460 — taller than the other two
screens to fit a second read-only field. It **writes nothing and never
launches**; `main.rs:cmd_splash` always returns `SPLASH_CANCEL` after it closes,
so the very first launch attempt only ever gets the user to the point of setting
up Steam's launch options correctly.

- A name field (`splash.rs:UnknownApp::name_buf`) feeds
  `splash.rs:sanitize_appid`, which turns free text into a safe id
  (whitespace → `_`, keep `[A-Za-z0-9_-]`).
- Below it, an **always-visible**, read-only, monospace launch-options box shows
  `RITZ_APPID=<id> ritz %command%` (placeholder text while the id is empty), for
  the user to copy into the non-Steam shortcut's Steam launch options.
  *Why always visible rather than only after a name is entered:* it doubles as
  in-place instructions for what the field is for — the user sees the exact
  target format (`RITZ_APPID=<name> ritz %command%`) before they've typed
  anything, instead of an empty screen with no hint of what happens next. This
  was a deliberate widening of the window (410 → 460) so the box has room
  without crowding the name field or the conflict warning.
- **Enter** or the **"Copy (Enter)"** button (disabled while the id is empty)
  copies the full `RITZ_APPID=... ritz %command%` line via
  `splash.rs:copy_to_clipboard`, which shells out to `wl-copy` (Wayland) or
  `xclip`/`xsel` (X11) and falls back to egui's own clipboard only if none of
  those binaries are present. *Why shell out instead of just using egui's
  clipboard:* egui's clipboard only survives as long as the process is alive
  and a clipboard manager is watching it; piping through the platform's
  clipboard daemon lets the text survive after this short-lived subprocess
  exits.
- If the sanitized id collides with an existing game's AppID
  (`splash.rs:UnknownApp::conflict`, checked against `existing_ids` passed in
  from `main.rs:cmd_splash`), a warning line tells the user to pick another
  name — it does not block copying.
- **Esc** or **"Close (Esc)"** closes the window with no side effect.

## Using it

From the user's perspective, nothing is invoked directly — it's all driven by
`ritz %command%` being the Steam launch command:

1. **Known game, already configured** → the countdown splash appears for
   `splash_timeout_secs`; do nothing and it launches, or hit `Q`/`W`/`E`.
2. **Known AppID, first time** → the naming wizard: type a name, Enter, pick a
   profile (digit key or "Use Default"), then `W` to launch or `E` to tweak
   settings before the first run.
3. **Non-Steam shortcut with no AppID configured** → the ID-setup screen: type a
   name, copy the shown `RITZ_APPID=... ritz %command%` line, paste it into the
   shortcut's Steam launch options, close, then relaunch — this time it will be
   recognized as AppID 1's "known game" case (or the wizard case, on the very
   next launch).

## Related links

- [Process Supervisor](process-supervisor.md) — what runs after the splash hands
  off `SPLASH_LAUNCH`.
- [Scoped Config](scoped-config.md) — where `splash_timeout_secs` and the
  resolved profile/preset shown on the splash come from.
