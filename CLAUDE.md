# ritz

A Linux game launcher built in Rust with an egui GUI. Two-crate Cargo workspace:

- **`crates/ritz-core`** — the launcher engine: data-driven extension/module system,
  scoped config, launch-command assembly, process supervision.
- **`crates/ritz-app`** — the egui front end: settings GUI, splash / new-game wizard.

Games are configured through a data-driven **extension/module** system rather than
hard-coded launch logic. Start with `docs/architecture/overview.md` to navigate the
code.

<!-- superdoc:start v2 -->
## Docs: read before you touch, update after you change

Full discipline — what to read, when to update, how to record the *why* — lives in the
always-loaded `docs/claude-instructions/documentation.md` below; this is a pointer, not
a restatement.

Reference docs — plain links, read the one relevant to your task on demand:

- **`docs/architecture/overview.md`** — codebase navigation map (module map, data flow,
  "where to look for X"). **Start here to navigate the code.**
- Per-feature behaviour — `docs/features/`
- Styling rules — `docs/ui/STYLING-GUIDE.md`

## Always in context (force-loaded, mandatory)

`@` is reserved for the must-always-know set. Everything above is a plain link: an
optional, on-demand read.

- **Terminology** — @docs/meta/TERMINOLOGY.md — project vocabulary; use these meanings,
  ask before acting on an undefined term, and keep it current.
- **Documentation discipline** — @docs/claude-instructions/documentation.md — read
  before you touch, update after you change, record design rationale (the *why*).
- **Version policy** — @docs/claude-instructions/documentation-version-policy.md — how
  this project bumps doc/version markers.

Path base: `CLAUDE.md` uses repo-root-relative paths (`@docs/...`, `` `docs/...` ``);
links *inside* docs are docs-relative. Precedence: `CLAUDE.md` is authoritative for
rules, docs are the reference — reconcile to `CLAUDE.md` on conflict. `@`-budget:
force-load a file only if its absence would let the agent do the wrong thing on *any*
task.
<!-- superdoc:end -->
