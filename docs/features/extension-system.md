# Extension System — JSON modules compiled into a launch command

An extension (called a *module* in the GUI) is a single JSON file that declares UI
fields, per-field variables, and *builder* blocks. At launch time ritz resolves each
field to a value, evaluates gating conditions, and assembles the enabled builders into
the final environment variables, wrapper chain, and game arguments. This is the engine
that turns "the user ticked HDR and set an FPS cap" into
`DXVK_HDR=1 DXVK_CONFIG='dxvk.maxFrameRate = 60' gamescope -f -- %command%`.

## How it works

The pipeline has three stages: **parse → resolve → build**.

### 1. Parse — JSON into typed specs

`crates/ritz-core/src/schema.rs:Extension` is the serde mirror of one JSON file.
Field names match the JSON verbatim (`Extension`, `UI`, `ENV_VARS`, …) and unknown
keys are ignored for forward-compatibility. `UI` is an `IndexMap` so section and field
order is preserved for deterministic GUI rendering. Each extension has a stable identity
`<Author>::<Name>::<Version>` via `crates/ritz-core/src/schema.rs:Extension::id` — that
id keys both variable scoping and config storage. An extension created by forking
another in the GUI editor may carry `ForkedFrom` (`"Author::Name"` of the parent) in its
metadata; it's provenance/display only and never participates in `id` or config lookup,
so a fork's manifest round-trips identically to a hand-written one.

A module contributes to five ordered blocks (see
`crates/ritz-core/src/builder.rs:LaunchCommand`):

- `ENV_VARS` / `GAME_ENV_VARS` — `crates/ritz-core/src/schema.rs:EnvVarSpec`, each with
  a list of `crates/ritz-core/src/schema.rs:EnvBuilderEntry` ops.
- `WRAPPERS` — `crates/ritz-core/src/schema.rs:WrapperSpec`.
- `GAME_LAUNCH_ARGS` — `crates/ritz-core/src/schema.rs:ArgSpec`.
- (`GAME_LAUNCH_COMMAND` is the game's own `%command%`, passed in verbatim.)

### 2. Resolve — fields into variable values

Every UI field (`crates/ritz-core/src/schema.rs:UiField`) owns a `Variable` name.
`crates/ritz-core/src/resolve.rs:resolve` walks all applicable extensions and resolves
each field's effective value across four layers, lowest → highest priority:
**extension `Default` ← global preset ← preset ← game override**
(`crates/ritz-core/src/resolve.rs:Provenance`). *Why four layers and a tracked
provenance:* the GUI shows tri-state inheritance indicators (a field can be inherited,
overridden, or reset), so the resolver must remember not just the value but *where it
came from*. A JSON `null` override explicitly disables an inherited value rather than
falling back to the default — see `null_disables_inherited` in `resolve.rs`.

The result is a `crates/ritz-core/src/resolve.rs:Resolution`, which produces a
per-extension `crates/ritz-core/src/variables.rs:VarStore` via
`crates/ritz-core/src/resolve.rs:Resolution::var_store`. Each resolved value is a
`crates/ritz-core/src/variables.rs:ResolvedVar` carrying two things the builder needs:
`truthy` (drives gating and empty-block skipping) and `value` (the string substituted
into `{var}` templates). The mapping from raw config + field type to these two is
`crates/ritz-core/src/variables.rs:resolve_field` — e.g. a `selection` with no chosen
value is falsy, and a `multi_string` joins its entries with newlines. *Why the
newline join:* consumers that shell-split (args, wrappers) and hook scripts that loop
lines both get exactly one item per entry.

**Scope resolution.** A variable name is local to its own extension *unless* it begins
with `global:` (`crates/ritz-core/src/variables.rs:GLOBAL_PREFIX`), in which case it
reads from a shared cross-extension map published by any `global:`-declared field. The
`global:` prefix is preserved in the stored name so a lookup can route by it
(`crates/ritz-core/src/variables.rs:VarStore::get`). *Why a separate global scope:* it
lets one module react to another (e.g. a Vulkan module enabling HDR only when the
compositor module's `global:hdr` is on) without hard-coding cross-module coupling —
but only in the build phase. A UI-context `Requires` referencing `global:` is rejected
at load time by `crates/ritz-core/src/variables.rs:ui_requires_is_valid`. *Why the
rejection:* GUI field visibility is evaluated per-module before the global map exists,
so a `global:` reference there would silently always be false.

### 3. Build — enabled builders into a launch command

`crates/ritz-core/src/builder.rs:build` runs each block through its own assembler,
every one gated by `Requires`.

**Requires is a grammar, not a flag.** `crates/ritz-core/src/condition.rs:parse` parses
a small boolean expression — identifiers joined by `AND` / `OR`, `!` / `NOT` for
negation, precedence `! > AND > OR`, no parentheses
(`crates/ritz-core/src/condition.rs:Expr`). An empty or absent `Requires` means "always
true" (`crates/ritz-core/src/condition.rs:eval_opt`). *Why a grammar and not a single
boolean:* real gates are conditional on combinations — "FSR upscaling AND not native
resolution", "gpl OR tear_free set" — and expressing those inline in the JSON is far
cheaper than a builder entry per combination. Gating is checked at three nesting levels:
the whole `EnvVarSpec`, each individual `EnvBuilderEntry`, and each `WrapperSpec` /
`ArgSpec`.

**Env assembly with Set / Append / Unset.** For each env var name, builder entries fold
into one accumulator (`crates/ritz-core/src/builder.rs:build_env_block`) using
`crates/ritz-core/src/schema.rs:EnvOp`:

- `Set` — replace the accumulated value.
- `Append` — join onto it with a `Separator` (default `,`).
- `Unset` — mark the variable for removal (rendered as `env -u NAME`).

*Why Append + a configurable separator exists:* many env vars are really packed
mini-config strings, not scalars. `DXVK_CONFIG` (see the example below) is a `; `-joined
list of `key = value` pairs, `RADV_PERFTEST` is a `,`-joined feature list. Append lets
several independently-gated fields each contribute one fragment to the same variable,
and only the fragments whose `Requires` passed get joined — so an unset FPS cap simply
doesn't appear in the string rather than emitting an empty fragment. Values are run
through `crates/ritz-core/src/variables.rs:VarStore::interpolate` first, replacing
`{var}` with its resolved value (`{{`/`}}` escape to literal braces, unknown names → empty).

**Wrappers.** `crates/ritz-core/src/builder.rs:build_wrappers` fills each wrapper's
`{OPTIONS}` placeholder from its enabled builder entries, interpolates, then shell-splits
via `shlex`. Wrappers are sorted by `Priority` (lower = further left / outermost),
extension order breaking ties. *Why priority ordering:* wrappers nest
(`gamemoderun gamescope -- %command%`), and the outer/inner order is semantic, not
declaration order — a gamescope module and a gamemode module must compose correctly
regardless of which one loaded first.

**Final shape.** `crates/ritz-core/src/builder.rs:LaunchCommand` renders as
`[ENV_VARS] [WRAPPERS] [GAME_ENV_VARS] [GAME_LAUNCH_COMMAND] [GAME_LAUNCH_ARGS]`.
`GAME_ENV_VARS` are game-only: when a wrapper is present they are emitted through an
`env` shim *after* the wrapper's `--`
(`crates/ritz-core/src/builder.rs:LaunchCommand::exec_plan`). *Why the shim:* whole-chain
`ENV_VARS` are seen by the wrapper process too (gamescope, gamemode), but a var like
`MANGOHUD=1` must reach only the game, not the wrapper — the `env` shim scopes it to the
right side of the `--`. With no wrapper, chain env and game env share one scope and are
merged.

## Using it

1. Author (or edit) a JSON file under `resources/extensions/` — one file per module.
2. Declare `UI` sections and fields; each field's `Variable` becomes referenceable in
   `Requires` conditions and `{var}` templates.
3. Declare the builder blocks (`ENV_VARS`, `WRAPPERS`, `GAME_ENV_VARS`,
   `GAME_LAUNCH_ARGS`) that consume those variables.
4. In the launcher, toggling a field re-resolves and the assembled command updates live
   (visible via the launch-command preview / `ritz --print`).

Optional top-level keys gate *whether a module loads at all*: `AppIds` restricts it to
specific Steam AppIds (`crates/ritz-core/src/schema.rs:Extension::applies_to`), and
`RequiresDesktop` restricts it to a named `$XDG_CURRENT_DESKTOP`.

## Options

Top-level JSON keys of one module file:

| Key | Meaning |
| --- | --- |
| `Extension` | Metadata: `Name`, `Author`, `Version`, optional `Description`, optional `ForkedFrom` (`crates/ritz-core/src/schema.rs:ExtensionMeta.forked_from`). Identity is `Author::Name::Version`. |
| `AppIds` | Optional list of Steam AppIds; omit = applies to all games. |
| `Backend` | Optional built-in backend, one of five values across two mechanisms: `lsfg-vk` / `hypr-monctl` route to a real runtime handler implementing `crates/ritz-app/src/backends/mod.rs:Backend` (see [runtime-backends.md](runtime-backends.md)); `custom-env` / `custom-game-env` / `custom-args` are *not* trait impls — they're a builder pre-pass that expands a `multi_string` list into env vars/args (see [launch-command-assembly.md](launch-command-assembly.md#backend-pre-pass)). |
| `RequiresDesktop` | Optional desktop gate matched against `$XDG_CURRENT_DESKTOP`. |
| `UI` | Ordered sections → fields (`Type`, `Variable`, `Default`, `Options`, `Requires`, …). |
| `ENV_VARS` | Whole-chain env vars, each a `Set`/`Append`/`Unset` builder. |
| `WRAPPERS` | Wrapper commands with `{OPTIONS}` and a `Priority`. |
| `GAME_ENV_VARS` | Game-only env vars (emitted via `env` shim past the wrapper `--`). |
| `GAME_LAUNCH_ARGS` | Extra args appended after the game command. |
| `Hooks` | Lifecycle scripts (`PreLaunch`, `PostSpawn`, `OnGameReady`, `PostExit`). |
| `ScriptBuilders` | Script that emits a block's entries dynamically at build time. |

Field `Type` values: `toggle`, `selection`, `integer`, `float`, `string`,
`multi_string` (`crates/ritz-core/src/schema.rs:FieldType`).

`EnvBuilderEntry` keys: `Type` (`set` / `append` / `unset`), `Value`, `Requires`,
`Separator` (append only, default `,`).

### Worked example — `resources/extensions/default/dxvk.json`

Four independently-gated fields (`fps_limit`, `gpl`, `max_frame_latency`, `tear_free`)
all `append` into one `DXVK_CONFIG` env var with `Separator: "; "`. Only the fields the
user actually set contribute a fragment, so the final value is a clean `; `-joined
config string. Separately, `DXVK_HUD` and `DXVK_HDR` each `set` a var gated on a single
field via `Requires`.

## Related links

- [Scoped configuration & inheritance](scoped-config.md) — how the
  four-layer `Default ← global ← preset ← game` resolution and the tri-state GUI display
  work (the resolve stage in depth).
- [Architecture overview](../architecture/overview.md) — where this engine sits in the
  launch pipeline.
- [Terminology](../meta/TERMINOLOGY.md) — *module* vs *extension*, *builder*, *scope*,
  *provenance*.
