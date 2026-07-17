# Working with the Docs

**Mandatory working rule for agents, linked (force-loaded) from the repo-root `CLAUDE.md`.**

The `docs/` folder is the project's shared memory. It is only useful while it stays true, so
maintaining it is part of doing the work — not an afterthought. A doc that lies is worse than
no doc, because the next agent trusts it.

## Read before you touch

Before working on an area, read the doc that covers it:

- `architecture/overview.md` to orient — the module map and the "where to look for X"
  cheat-sheet.
- Then the specific `architecture/` or `features/` doc for the part you're changing.

The code is the source of truth; the docs are a cache that lets you navigate it fast. If a doc
and the code disagree, trust the code — then fix the doc.

## Update after you change

If you add or change something the docs describe, **update the doc in the same commit** as the
change (the same discipline good changelog hygiene follows). In particular:

- **New** feature, module, component, config option, or capability → update the relevant
  `features/` / `architecture/` doc, and add a line to the `docs/README.md` index if you
  created a new page.
- **Renamed / removed** things → fix every mention, including any `@`-reference or link in
  `CLAUDE.md`.
- **Behaviour that drifted** from a doc → correct the doc; don't leave a stale claim behind.

Document to the extent that helps future work. Don't over-document trivia — a doc nobody needs
is maintenance debt (YAGNI applies to docs too).

## Record the *why*, not just the *what*

The most valuable — and least recoverable — part of a doc is the reasoning behind a design
choice. Code shows *what* it does; it rarely shows *why this way and not the obvious
alternative*.

So whenever a design decision is made, capture the rationale in the relevant `.md`:

- If the user **explains why** something is done a certain way — in a message, or by answering
  a question you asked — add that reasoning to the doc.
- If **you asked "why?"** and got an answer, write it down so the next agent doesn't have to
  ask again.
- Prefer a short **`Why:`** note next to the thing it explains over a separate essay.

A design choice that lives only in a chat message is gone the moment the context window rolls.
The doc is where it survives.

## The `@` convention in CLAUDE.md

`CLAUDE.md` references docs two ways, and the distinction is deliberate:

- **`@path`** — force-loads the file into *every* session's context. Reserved for the
  must-always-know set: everything in `claude-instructions/` plus `meta/TERMINOLOGY.md`.
- **plain path** (`` `docs/…` ``) — an optional, on-demand read. Everything else.

When you add a doc, pick the right one: does *every* task need it in context, or only tasks
that touch that area? **Default to a plain link** — force-loading spends context budget on
every run, so keep the `@` set small.
