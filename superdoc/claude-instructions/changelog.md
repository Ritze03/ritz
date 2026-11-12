# Changelog Maintenance

**Mandatory working rule for agents, linked (force-loaded) from the repo-root `CLAUDE.md`.**

`CHANGELOG.md` in the **repo root** is the user-facing "what changed". Write for users, not
developers.

## Plain language (hard rule)

Entries must be short and understandable by a user with **no technical knowledge**. Say
**what got better for the user**, never **how**. No file names, function names, flags,
libraries, protocols, config keys, commit ids or implementation details. One short sentence
per bullet. Drop anything a non-technical user would not notice.

Example: a fix for a variable-refresh-rate bug becomes
`- **Variable refresh rate**: fixed an issue with variable refresh rate.`

## The shape

```
# Changelog

<short preamble naming the categories>

## YYYY-MM-DD

### Added
- **Short name**: one short sentence.
```

- **`## YYYY-MM-DD`**: date only, no version number (see
  [`documentation-version-policy.md`](documentation-version-policy.md)).
- **`### Added` / `### Fixed` / `### Removed` / `### Info`**: the only four categories.
  *Added* = new features, *Fixed* = bug and behaviour fixes, *Removed* = things taken out,
  *Info* = notes worth knowing. Omit a category with no entries.
- **`- **Name**: sentence.`**: bold lead-in, colon, one short sentence.

## Rules

- **Maintain it as you work.** Every commit that changes what users get adds a bullet in the
  **same commit**, under **today's** `## YYYY-MM-DD` block. If today's block does not exist,
  create it at the top. Never change a past block's date or rewrite past entries.
- **Skip internal-only changes**: docs, refactors, tests, CI, typos. A day of only such work
  adds no block.
- **No agent-facing content**: no instructions or notes to the next agent. Users read this.
- **Anonymize**: no hostnames, usernames, emails, real names, absolute home paths or private
  project names.
- Wrap lines at about 90 columns.
