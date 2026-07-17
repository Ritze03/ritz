# Terminology

Project-specific vocabulary. When the user uses a term defined here, use the same
meaning.

**If the user uses a non-standard term that is not defined here, ask what they mean by
it before acting on it.** Once its meaning is clear, add it to this file. Keep this
file up to date automatically: whenever the user introduces or clarifies a term, add or
amend the entry here.

## Terms

- **Extension / module** — a JSON file describing one unit of launch-time
  functionality (e.g. Gamescope, AMD, LSFG-VK). The two words are used
  interchangeably in the README and UI. Deserializes to
  `crates/ritz-core/src/schema.rs:Extension`; ships bundled under
  `resources/extensions/default/` and `resources/extensions/built-in/`, or drops in
  by the user under `~/.config/ritz/extensions/`.

- **UI field** — one configurable control an extension exposes in the settings GUI
  (toggle, selection, integer, float, string, multi_string). Defined by
  `crates/ritz-core/src/schema.rs:UiField`; grouped into named sections in an
  extension's `"UI"` map (`crates/ritz-core/src/schema.rs:Extension.ui`), rendered in
  declared order.

- **`ENV_VARS`** — the extension JSON block listing environment variables applied to
  the whole launch chain (wrapper + game). Maps to
  `crates/ritz-core/src/schema.rs:Extension.env_vars`, a list of
  `crates/ritz-core/src/schema.rs:EnvVarSpec`. See also `GAME_ENV_VARS`, which applies
  only to the game process. *Why the split:* wrappers like gamescope need some vars
  (e.g. renderer selection) before they even launch the game, so `ENV_VARS` reaches the
  whole chain while `GAME_ENV_VARS` is injected only after the wrapper's `--`
  (README.md, "How it works").

- **Builder** — the list of steps (`Builder` array) that computes a single
  `ENV_VARS`/`WRAPPERS`/`GAME_ENV_VARS`/`GAME_LAUNCH_ARGS` entry's final value, one step
  per condition/value pair. Env-var builders use
  `crates/ritz-core/src/schema.rs:EnvBuilderEntry` (each step has its own `EnvOp` and
  optional `Requires`); wrapper builders use
  `crates/ritz-core/src/schema.rs:WrapperBuilderEntry`, which is always an implicit
  "add" op.

- **`EnvOp` (set/append/unset)** — the operation a single `Builder` step performs on an
  env var: `Set` replaces the value, `Append` joins onto the existing value (using the
  step's `Separator`, default none), `Unset` clears it. Defined at
  `crates/ritz-core/src/schema.rs:EnvOp`.

- **`Requires` condition grammar** — the boolean expression language used everywhere a
  field, builder step, wrapper, or arg needs to be gated on other variables' values.
  Grammar: `expr := and ("OR" and)*`, `and := atom ("AND" atom)*`,
  `atom := "!" atom | IDENT` — precedence `! > AND > OR`, no parentheses, empty string
  = always true. Parser and evaluator live in
  `crates/ritz-core/src/condition.rs:parse` /
  `crates/ritz-core/src/condition.rs:eval_opt`. A bare identifier can reference a
  `global:`-prefixed variable to gate on the global build-phase scope; `UiField`'s own
  `Requires` rejects `global:` references at load time
  (`crates/ritz-core/src/schema.rs:UiField.requires`).

- **Scope (extension default / global / profile / game)** — the four-layer precedence
  chain a variable's effective value resolves through, lowest to highest:
  **extension default → global → profile → game**. Higher scopes win; unset at a scope
  means "inherit from the layer below" (README.md, "Scopes & colors";
  `crates/ritz-core/src/config.rs` module doc, "present/null/absent" convention).
  Storage: global lives in `global.json`
  (`crates/ritz-core/src/config.rs:Paths.global_config`), profile in
  `profiles/<name>.json` (`crates/ritz-core/src/config.rs:Paths.preset`), game in
  `games/<appid>.json` (`crates/ritz-core/src/config.rs:Paths.game`).

- **Profile** — a reusable, named bundle of module settings that can be assigned to
  one or more games (README.md, "Profiles"). On disk it's a
  `crates/ritz-core/src/config.rs:Preset` (the JSON key is `"Name"`, the file is
  `profiles/<name>.json`); a profile may itself have a `Parent` profile forming an
  extra inheritance layer below it
  (`crates/ritz-core/src/config.rs:Preset.parent`).

- **Backend** — an extension's optional route to a built-in Rust runtime handler
  (rather than pure JSON-declared env/wrapper/arg blocks), named in the extension's
  `"Backend"` field (`crates/ritz-core/src/schema.rs:Extension.backend`), e.g.
  `"lsfg-vk"`, `"hypr-monctl"`, `"custom-env"`, `"custom-args"` — see
  `resources/extensions/built-in/lsfg-vk/lsfg-vk.json`. Also used, unrelated, as a
  per-field option value inside the Gamescope module (`resources/extensions/default/gamescope.json`,
  the "Backend" UI field selecting gamescope's own SDL/wayland backend) — the two uses
  are independent; context (extension-level JSON key vs. a `Selection` field's name)
  disambiguates them.

- **Wrapper vs env var vs launch arg** — the three ways a module can affect the launch
  command, corresponding to three of the five assembly blocks
  (README.md, "How it works", `crates/ritz-core/src/schema.rs`):
  - **wrapper** (`WRAPPERS` / `crates/ritz-core/src/schema.rs:WrapperSpec`) — a command
    prepended left of `%command%` (e.g. `gamescope … --`), ordered by `Priority`
    (lower = further left).
  - **env var** (`ENV_VARS`/`GAME_ENV_VARS` /
    `crates/ritz-core/src/schema.rs:EnvVarSpec`) — a variable set in the child
    process environment, chain-wide or game-only.
  - **launch arg** (`GAME_LAUNCH_ARGS` / `crates/ritz-core/src/schema.rs:ArgSpec`) — a
    bare argument appended after `%command%`.

- **Splash** — the on-screen countdown GUI shown before every launch once a game's
  identity is set, giving a window to open settings before the game starts
  (README.md, "splash screen on every launch"). Its timeout is configured by
  `crates/ritz-core/src/config.rs:GeneralConfig.splash_timeout_secs` (per-game override
  via `crates/ritz-core/src/config.rs:GeneralOverrides.splash_timeout_secs`).

- **`%command%` (the Steam launch token)** — Steam's placeholder for the game's actual
  launch command, substituted into a game's Steam launch options
  (e.g. `ritz %command%`). Ritz preserves it verbatim as one of the five assembly
  blocks — `[ENV_VARS] [WRAPPERS] [GAME_ENV_VARS] %command% [GAME_LAUNCH_ARGS]`
  (README.md, "How it works") — never stripping the `SteamLinuxRuntime` container it
  wraps.

- **`RequiresDesktop` gate** — an extension-level (not field-level) gate restricting
  the whole extension to a named desktop session, matched against
  `$XDG_CURRENT_DESKTOP` (e.g. `"Hyprland"`); omitted means available everywhere.
  Field: `crates/ritz-core/src/schema.rs:Extension.requires_desktop`. Filtered at
  load time in `AppContext::load` (see `project_requires_desktop.md` memory). Example:
  `resources/extensions/built-in/hypr-monctl/hypr-monctl.json`.
