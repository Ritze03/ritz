# Documentation Version Policy

**Active policy: date-based.** This project does not use version numbers for docs or the
changelog — every dated block is identified purely by its date, newest first. There is nothing
to bump.

## How to apply it every run

- When you add a changelog entry or dated doc block, stamp it with **today's date** in
  `YYYY-MM-DD` format — no version number, no `[x.y.z]` header, just the date.
- Put new blocks at the **top** (newest date first). If a block for **today** already exists,
  add your entry to it instead of creating a second block for the same day.
- **Never invent or bump a version number.** If a version marker exists elsewhere in the repo
  (a package manifest, a `VERSION` file, etc.) for release-management reasons, it is out of
  scope for docs — do not touch it as part of doc maintenance.
- **Never rewrite a past block's date.** Once a day's block exists, its date is fixed; new
  work goes in a new block (or today's block, if it's already today).
- If you're unsure whether something counts as a new dated block or belongs in an existing
  one, default to: same calendar day → same block; different day → new block on top.
