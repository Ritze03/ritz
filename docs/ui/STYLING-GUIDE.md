# Styling Guide

Ritz's GUI (`crates/ritz-app`) is built on egui and skinned with a single "Graphite"
theme. Colors, fonts, and widget shapes all live in
`crates/ritz-app/src/theme.rs:apply` and `crates/ritz-app/src/fonts.rs:install` — this
page documents the conventions a contributor must follow to keep new UI consistent
with the rest of the app. The **top-level rule**: reference colors and buttons by
*role* (`theme::ACCENT`, `theme::scope_color(...)`, `theme::primary_button(...)`),
never hard-code a raw `Color32::from_rgb(...)` at a call site — so a future re-theme
(the "Graphite redesign" is mid-flight) only touches `theme.rs`.

## Color palette + accent

All chrome colors are `pub const`s in `crates/ritz-app/src/theme.rs`:

| Token | Role |
| --- | --- |
| `theme::ACCENT` | Brand accent (`#5B8BF0`, indigo/blue) — section labels, primary buttons, logo, slider default |
| `theme::HEAD` | Title-bar background |
| `theme::PANEL` / `theme::PANEL2` | Main panel and footer band backgrounds |
| `theme::BORDER` | All hairline borders/dividers |
| `theme::TEXT` / `theme::DIM` / `theme::FAINT` | Primary / secondary / tertiary text |
| `theme::FIELD` | Input & code-block background |
| `theme::BTN` / `theme::BTNBD` | Secondary button fill / border |
| `theme::PRIMARY_TEXT` | Text color drawn on top of an accent-filled primary button |
| `theme::SEL` / `theme::SELBD` / `theme::HOV` | Derived selection/hover tints (accent @ ~16%/~42%, white @ ~5%) |
| `theme::selection_tint(base)` | Derives a `(fill, border)` pair at `SEL`/`SELBD`'s own alphas from an *arbitrary* base color, not just `ACCENT` — `selection_tint(ACCENT)` reproduces `SEL`/`SELBD` byte-for-byte. Use this instead of hand-picking new tint literals whenever something needs the "selected" treatment in a color other than the brand accent (e.g. the nav's per-tab colours, 2026-07-19) |
| `theme::EDIT_L0`…`EDIT_L3` | Module-editor nesting shades (module → block → field → builder row), each one step lighter than the base panel so nested cards read as a hierarchy |

```rust
// Correct — reference by role:
ui.label(egui::RichText::new(label).color(theme::ACCENT).small());

// Wrong — do NOT hard-code a literal:
ui.label(egui::RichText::new(label).color(Color32::from_rgb(0x5B, 0x8B, 0xF0)));
```

Do NOT introduce a new raw `Color32::from_rgb(...)` literal in `gui.rs` for anything
that already has a role token above — add a new `pub const` to `theme.rs` instead, so
every color stays discoverable from one file.

`theme::apply(ctx)` (`crates/ritz-app/src/theme.rs:apply`) wires these into
`egui::Visuals` once at startup (panel fill, selection colors, widget states for
`inactive`/`hovered`/`active`/`open`) and sets the type scale and spacing — see
Spacing/layout below. Any new persistent visual state should go through `Visuals`
here rather than being special-cased per-widget.

## Scope-color convention

Ritz overlays extension config at three scopes (Global → Profile → Game), and the
whole settings UI color-codes *which scope a value's currently-effective setting came
from* using a second, independent palette — kept from the original launcher and never
merged with the accent palette:

| Token | Scope |
| --- | --- |
| `theme::COL_GLOBAL` | Global scope — red (`#E15554`). *Also* doubles as the destructive/danger color (Delete, Cancel) |
| `theme::COL_PROFILE` | Profile scope — green (`#6CC533`-ish) |
| `theme::COL_GAME` | Game scope — blue (`#4D9DE0`) |
| `theme::COL_DEFAULT` | No override anywhere; value comes from the extension's own default — neutral gray, not blue |

`theme::scope_color(provenance: Provenance) -> Color32`
(`crates/ritz-app/src/theme.rs:scope_color`) maps a `ritz_core::resolve::Provenance`
to its display color. Every field row, checkbox, and inheritance badge in the settings
tree gets its tint from this function (or a caller-computed variant of it), never a
literal.

*Why the Global/danger overlap:* `COL_GLOBAL` red is reused for `danger_button`
(`crates/ritz-app/src/theme.rs:danger_button`) — Delete/Cancel actions share the same
red as the Global scope tint. This is intentional (not a bug to "fix" by adding a
separate danger color): both signal "high blast radius."

That reuse is now *derived*, not retyped (2026-07-19, issue #13): `danger_button`'s
faint border comes from `theme::danger_border()`, which reads `COL_GLOBAL`'s channels
and applies `DANGER_BORDER_ALPHA` (82, ~32%). It used to be a second hand-written
`Color32::from_rgba_unmultiplied(0xE1, 0x55, 0x54, 82)` literal that would have
silently diverged if the red were ever retuned. The test
`theme::tests::danger_border_matches_col_global` pins the derived value to the old
literal, so the refactor is provably a no-op at the pixel level.

*Why not route it through `selection_tint`:* that helper premultiplies by hand
specifically to reproduce `SEL`/`SELBD`, and its two alphas (~16% / ~42%) aren't this
border's 32%. Sending the stroke through it would have changed the rendered color —
egui's `from_rgba_unmultiplied` blends in linear space and lands elsewhere — so
`danger_border` keeps the original call and only stops duplicating the channels.

## Diagnostic severity colours

A second, small role pair — *how serious a message is*, independent of scope. Added
2026-07-19 (issue #39) and consumed only through
`crate::gui::DiagSeverity::color()`; never reference the raw tokens from a message
renderer.

| Token | Role |
| --- | --- |
| `theme::COL_WARN` | Warning — amber (`#E1A854`). Something unfinished or lossy, but nothing is being refused |
| `theme::COL_ERROR` | Error — `#E15554`, **exactly `COL_GLOBAL`'s red**. The reason an action is refused |

*Why `COL_ERROR` is not a new red:* that red is already this app's danger colour (see
"Why the Global/danger overlap" above — Delete, Cancel, `danger_button`). A second red
for errors would be a distinction the eye cannot make and the palette does not mean.
`COL_ERROR` is defined as `COL_GLOBAL` rather than as a copied literal, so the two
cannot drift; it exists as its own name so call sites can say which *role* they mean,
and so the pair can diverge later by editing one line.

*Why that specific amber, and not a generic orange:* `COL_WARN` is `COL_GLOBAL` with
the green channel lifted and the other two held **byte-identical** (`0xE1` red, `0x54`
blue). That is what keeps it in the palette rather than beside it. In HSL it lands at
roughly (36°, 70%, 61%) against `COL_GLOBAL`'s (0°, 68%, 61%) — the same lightness,
effectively the same saturation, one hue step away. The whole scope family already
sits in a narrow L 55–61% / S 50–70% band (`COL_PROFILE` ≈ 104°/50%/55%, `COL_GAME` ≈
209°/70%/59%), so a stock `#FFA500`-style orange — far brighter and fully saturated —
would have been the single loudest colour in the app, which is the opposite of what a
"nothing is broken" rank should be. Deriving it from the red also means a future
retune of `COL_GLOBAL` shows up as a deliberate divergence rather than a silent
mismatch; `theme::tests::severity_colors_derive_from_the_danger_red` asserts the
derivation, not just the hex.

*Why this was not done sooner:* `DiagSeverity::Warning` and `::Error` both resolved to
`COL_GLOBAL` and were told apart by icon alone, which was fine while the diagnostics
band held one or two warnings. 60a56b3 added three lints that all rank as `Warning`,
so an ordinary draft could paint the whole band danger-red — a screen with nothing
refused reading exactly like a screen with Save blocked. See
`docs/features/settings-gui.md`, "Diagnostics band".

*Why `Provenance::Game` is special-cased at call sites:* when editing a value at the
layer that's currently open, `resolve::Provenance` reports it as `Provenance::Game`
even if you're actually editing a Profile or Global layer (the layer under edit is
loaded as a fake game to resolve it) — see `crates/ritz-app/src/gui.rs:render_field`.
Callers must color by `self.editing_scope_color()` in that branch, not blindly
`theme::scope_color(res.provenance)`, or a Profile-scope edit would incorrectly show
blue instead of green:

```rust
let scope = match res.provenance {
    resolve::Provenance::Game => self.editing_scope_color(),
    resolve::Provenance::Preset if /* numeric depth badges enabled */ => {
        profile_depth_color(preset_depth.unwrap())
    }
    p => theme::scope_color(p),
};
```

Do NOT call `theme::scope_color(res.provenance)` directly inside a field-row renderer
without this Game-provenance override — it will mis-color edits made at Profile or
Global scope.

## Fonts

Fonts are installed once via `crate::fonts::install(ctx, mono_ui)`
(`crates/ritz-app/src/fonts.rs:install`), bundling the Geist family from
`crate::resources`:

- **Geist Mono** (TTF) — the default UI font (`mono_ui = true`); also always used for
  `TextStyle::Monospace` (command previews, value badges).
- **Geist** (TTF) — proportional sans, used for UI text only when the user turns
  `mono_ui` off.
- **Geist Mono Nerd Font** (OTF) — icon glyphs only, installed as a fallback family
  after the text font so icon codepoints (`\uf...`) resolve without affecting text
  glyph edges.
- **Geist Bold** — a separate named family (`FontFamily::Name("bold".into())`), used
  only for the logo wordmark.

*Why TTF-first with the Nerd Font as fallback, not primary:* mixing an OTF icon font
in as primary blurs regular text edges; keeping Geist (TTF) first and appending the
icon font only as a fallback keeps body text crisp while still resolving `\uf...`
icon codepoints.

`fonts::install` is safe to call again at runtime (e.g. toggling `mono_ui` in
settings) — it fully replaces `FontDefinitions` each call.

Type scale is set in `theme::apply` (`crates/ritz-app/src/theme.rs:apply`), not in
`fonts.rs`:

| `TextStyle` | Size |
| --- | --- |
| `Heading` | 19.0 |
| `Body` / `Button` | 13.0 |
| `Small` | 11.0 |
| `Monospace` | 12.0 |

Section/column headers don't use `TextStyle::Heading` — they use the dedicated
`theme::header_label` / `theme::section_label` helpers (see below) at 11–12px, which
is deliberately smaller than body text (uppercase + letter-weight carries the
hierarchy instead of size).

## Spacing / layout idioms

Global spacing is set once in `theme::apply`:

```rust
s.spacing.item_spacing = egui::vec2(7.0, 7.0);
s.spacing.button_padding = egui::vec2(9.0, 5.0);
s.spacing.interact_size.y = 23.0;
```

Corner rounding is `Rounding::same(8.0)` everywhere (`round` / `round_small` in
`theme::apply`) — buttons, fields, cards, combo/menu popups all share the same 8px
radius. *Why 8px and not 14px:* 8px keeps a flat edge at the app's compact control
heights; a larger radius would round the short buttons/fields into pill ends.

### Icon → text gap: always `icon_sep`

Whenever a Nerd Font glyph and its label live in **one string**, separate them with
`crates/ritz-app/src/gui.rs:icon_sep(mono)` — never a hardcoded space run:

```rust
// Correct:
let sep = icon_sep(self.general_config.mono_ui);
ui.label(format!("\u{f071}{sep}{msg}"));

// Wrong — a literal gap that is right in at most one of the two UI fonts:
ui.label(format!("\u{f071} {msg}"));      // too tight in proportional
ui.label("\u{f09b}  ritz-game-launcher"); // mono's gap, worn by the sans font
```

*Why a helper and not a literal:* `mono_ui` is a user setting, and the space glyph's
width differs sharply between Geist Mono and Geist. One mono space reads as a gap;
one proportional space reads as no gap at all. `icon_sep` returns `" "` for mono and
`"   "` for proportional so the *visual* gap stays constant across the toggle. Swept
file-wide 2026-07-19 (issue #24) — the module create/fork name check, the carry-over
report, the IDE preview's unresolved-module notice, and both GitHub footer links were
still on literals.

This applies only to glyph+text in one string. A glyph passed as its **own** argument
gets a measured pixel gap instead and must not use `icon_sep`: `icon_button` (6.0px)
and `nav_category_tab` (6–7px by font) lay the two out separately, and a standalone
glyph with no adjacent text (the checkbox tick, the settings gear) needs no separator
at all.

Field rows in the settings tree follow one fixed layout, in
`crates/ritz-app/src/gui.rs:render_field`:

- Row card: `egui::Frame::none()` filled with the scope color at ~16% alpha
  (`Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16)`), 8px
  rounding, `inner_margin { left: 12.0, right: 5.0, top: 0.0, bottom: 0.0 }`.
- Fixed `ui.set_min_height(39.0)` so the backdrop doesn't grow when a taller control
  (e.g. a multi-line editor) appears in the row.
- The checkbox+label column reserves a fixed **260px**; the value editor fills the
  remainder, laid out `egui::Layout::right_to_left(egui::Align::Center)` so editor
  controls stay right-bound regardless of label length.
- A 3px left accent bar is drawn separately, clipped to the row's rounded rect, so it
  follows the card's corner curvature exactly:

```rust
let bar_clip = egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + 3.0, r.max.y));
ui.painter().with_clip_rect(bar_clip).rect_filled(r, egui::Rounding::same(8.0), scope);
```

Do NOT build a new settings-field row from scratch — call
`crates/ritz-app/src/gui.rs:render_field` (or, for list-typed fields,
`crates/ritz-app/src/gui.rs:render_multi_string_field`) so every field in the tree
keeps the same card shape, scope tint, and 260px label column.

## Reusable `render_*` / `theme::*` helper patterns

Two families of reusable helpers back the GUI; prefer them over ad hoc `ui.button`/
`ui.label` calls with inline styling.

**Button variants** (`crates/ritz-app/src/theme.rs`):

- `theme::primary_button(text)` — solid `ACCENT` fill, `PRIMARY_TEXT`, bold. Use for
  the one primary action on a screen (e.g. "Launch Game").
- `theme::danger_button(text)` — transparent fill, `COL_GLOBAL` red text + faint red
  border. Use for destructive/abort actions (Delete, Cancel, Clean Up).
- `theme::secondary_button(text)` — `BTN` fill + `BTNBD` border. The default for
  everything else (Reload, Open Folder, Cancel-that-isn't-destructive).

```rust
if ui.add(theme::primary_button("Launch Game")).clicked() { /* ... */ }
if ui.add(theme::danger_button("Delete Profile")).clicked() { /* ... */ }
```

**Section/header labels** (`crates/ritz-app/src/theme.rs`):

- `theme::header_label(text)` — uppercased, `FAINT` gray, 11px, strong. Used for
  neutral column headers ("Modules", "Profiles / Games", "Credits").
- `theme::section_label(text)` — uppercased, `ACCENT`, 12px, strong. Used for
  emphasized settings-section headers.

```rust
ui.label(theme::header_label("Modules"));
ui.label(theme::section_label(section));
```

Do NOT build a header label with `egui::RichText::new(text.to_uppercase())` inline at
the call site — use `theme::header_label`/`theme::section_label` so the uppercase
transform, color, and size stay in one place.

**`render_*` methods on the app struct** (`crates/ritz-app/src/gui.rs`), one per
panel/widget region, e.g. `render_title_bar`, `render_nav_panel`, `render_nav_tree`,
`render_field`, `render_value_editor`, `render_general_settings_panel`, `render_about`,
`render_confirm_dialog`. Each takes `&mut self` (or `&self` when read-only, e.g.
`render_about`) plus the target `&mut egui::Ui`, and returns `bool` when it can mutate
state the caller needs to react to (e.g. `render_field` returns whether the value
changed, `render_confirm_dialog` returns whether the dialog is still open). Follow
this shape for any new panel: a `render_<name>` method, one region of the tree per
method, `bool` return only when the caller needs a changed/open signal.

## Related links

- `crates/ritz-app/src/theme.rs` — all color tokens, button variants, `apply()`.
- `crates/ritz-app/src/fonts.rs` — font bundle and installation.
- `crates/ritz-app/src/gui.rs` — `render_*` panel methods and the field-row layout.
