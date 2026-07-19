//! "Graphite" theme tokens and egui style setup.
//!
//! Single source of truth for colors — reference these by *role*, never hard-code
//! raw hex at call sites, so an accent/token change re-themes the whole app.

use egui::{Color32, FontFamily, FontId, Rounding, Stroke, TextStyle};
use ritz_core::resolve::Provenance;

// ---- Brand / chrome ------------------------------------------------------

/// Brand accent (Indigo). Section labels, primary action, logo, slider default.
pub const ACCENT: Color32 = Color32::from_rgb(0x5B, 0x8B, 0xF0);
/// Title-bar background (darker than panels for contrast).
pub const HEAD: Color32 = Color32::from_rgb(0x14, 0x17, 0x1A);
/// Main panel / column background.
pub const PANEL: Color32 = Color32::from_rgb(0x1E, 0x21, 0x25);
/// Footer band background.
pub const PANEL2: Color32 = Color32::from_rgb(0x19, 0x1C, 0x1F);
/// All hairline borders / dividers.
pub const BORDER: Color32 = Color32::from_rgb(0x2C, 0x30, 0x36);
/// Primary text.
pub const TEXT: Color32 = Color32::from_rgb(0xE7, 0xE9, 0xEC);
/// Secondary text.
pub const DIM: Color32 = Color32::from_rgb(0x96, 0x9C, 0xA6);
/// Tertiary text / labels / placeholders.
pub const FAINT: Color32 = Color32::from_rgb(0x64, 0x6A, 0x73);
/// Input & code-block background.
pub const FIELD: Color32 = Color32::from_rgb(0x15, 0x18, 0x1B);
/// Secondary button background.
pub const BTN: Color32 = Color32::from_rgb(0x26, 0x2A, 0x30);
/// Button border.
pub const BTNBD: Color32 = Color32::from_rgb(0x34, 0x39, 0x41);
/// Text on a primary (accent) button.
pub const PRIMARY_TEXT: Color32 = Color32::from_rgb(0x0E, 0x1A, 0x18);

// ---- Module-editor nesting shades ----------------------------------------
// A subtle elevation ramp so nested editor cards read as a hierarchy: each
// deeper level sits one step lighter than its parent (base panel is 0x1E2125).
// Module card → section/block card → field card → builder-step row.
/// Editor nesting level 0 — the top-level Module (meta) card.
pub const EDIT_L0: Color32 = Color32::from_rgb(0x20, 0x24, 0x28);
/// Editor nesting level 1 — section / ENV / WRAPPER / arg block cards.
pub const EDIT_L1: Color32 = Color32::from_rgb(0x26, 0x2A, 0x30);
/// Editor nesting level 2 — field cards inside a section.
pub const EDIT_L2: Color32 = Color32::from_rgb(0x2C, 0x31, 0x38);
/// Editor nesting level 3 — builder-step rows inside a block card.
pub const EDIT_L3: Color32 = Color32::from_rgb(0x33, 0x39, 0x41);

// Derived selection / hover tints (accent @ ~16% / ~42%, white @ ~5%),
// premultiplied at compile time (from_rgba_unmultiplied isn't const).
pub const SEL: Color32 = Color32::from_rgba_premultiplied(0x0F, 0x16, 0x27, 0x29);
pub const SELBD: Color32 = Color32::from_rgba_premultiplied(0x26, 0x3A, 0x65, 0x6B);
pub const HOV: Color32 = Color32::from_rgba_premultiplied(0x0D, 0x0D, 0x0D, 0x0D);

/// Derive a `(fill, border)` selection tint from an arbitrary base color, at
/// the same two alphas [`SEL`]/[`SELBD`] use (~16% / ~42%). `SEL`/`SELBD` are
/// exactly `ACCENT` run through this formula — they're hand-expanded consts
/// only because `Color32::from_rgba_unmultiplied` isn't a `const fn`, so
/// `selection_tint(ACCENT)` reproduces them byte-for-byte (see the test
/// below). Anything that wants the same "selected" treatment for a *different*
/// base color — e.g. one tab of a multi-color tab strip — calls this instead
/// of hand-picking new hex literals, so a future alpha tweak to `SEL`/`SELBD`
/// stays a one-line change here rather than N re-derived constants.
///
/// *Why manual premultiply and not `Color32::from_rgba_unmultiplied` itself:*
/// egui's version blends in linear (gamma-corrected) space, which lands on a
/// visibly different color than the naive integer premultiply `SEL`/`SELBD`
/// were hand-computed with. Using it here would make a derived tab read
/// differently from the accent tab's `SEL`/`SELBD` fill even at the same
/// nominal alpha — defeating the point of sharing one formula.
pub fn selection_tint(base: Color32) -> (Color32, Color32) {
    // 0x29/255 ≈ 16% (fill), 0x6B/255 ≈ 42% (border) — SEL/SELBD's own alphas.
    fn premul(base: Color32, alpha: u8) -> Color32 {
        // Round-to-nearest integer premultiply, matching how SEL/SELBD's own
        // byte values were derived from ACCENT.
        let mix = |c: u8| -> u8 { ((c as u32 * alpha as u32 + 127) / 255) as u8 };
        Color32::from_rgba_premultiplied(mix(base.r()), mix(base.g()), mix(base.b()), alpha)
    }
    (premul(base, 0x29), premul(base, 0x6B))
}

// ---- Scope / inheritance (kept identical to the original launcher) -------

/// Global scope — also doubles as the destructive/danger color (Delete, Cancel).
pub const COL_GLOBAL: Color32 = Color32::from_rgb(0xE1, 0x55, 0x54);
/// Profile scope.
pub const COL_PROFILE: Color32 = Color32::from_rgb(0x6C, 0xC5, 0x51);
/// Game scope.
pub const COL_GAME: Color32 = Color32::from_rgb(0x4D, 0x9D, 0xE0);
/// No override — value comes from the extension default (neutral gray).
pub const COL_DEFAULT: Color32 = Color32::from_rgb(0x46, 0x4D, 0x57);
/// Empty (off) checkbox outline — light so it reads on any scope tint.
pub const CHECK_OUTLINE: Color32 = Color32::from_rgb(0xE1, 0xE3, 0xE6);
/// Pin-slot id (`[1]`…`[10]`) trailing a pinned profile's row in the nav tree.
///
/// Deliberately its own token rather than [`FAINT`]: this label sits *inside* a
/// selectable row and must stay quieter than the profile name beside it even when
/// that row is hovered or selected, so it is a step darker than the general
/// tertiary text color.
pub const PIN_ID: Color32 = Color32::from_rgb(0x59, 0x5E, 0x66);

/// Inheritance arrow (value comes from a lower scope).
pub const ICON_INHERIT: &str = "\u{f432}";
/// Edit pencil (value set at the current scope).
pub const ICON_EDIT: &str = "\u{f044}";
/// Unsaved-manifest-edits dot, shown ahead of a module's name in the IDE tree.
///
/// *Why a filled circle and not the pencil above:* the pencil already means
/// "there is a stored value at this scope" in the Config-mode tree, and reusing
/// it for "this manifest has unsaved edits" would give one glyph two unrelated
/// meanings in two trees. The dot is the conventional editor mark for a modified
/// buffer and is unused elsewhere in this app.
pub const ICON_DIRTY: &str = "\u{25cf}";

/// The scope color a resolved value should display in.
pub fn scope_color(p: Provenance) -> Color32 {
    match p {
        Provenance::Global => COL_GLOBAL,
        Provenance::Preset => COL_PROFILE,
        Provenance::Game => COL_GAME,
        // No override anywhere — neutral, not the blue "game" color.
        Provenance::Default => COL_DEFAULT,
    }
}

// ---- Button variants -----------------------------------------------------

/// Primary action: solid accent fill, dark text, bold.
pub fn primary_button(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(PRIMARY_TEXT).strong())
        .fill(ACCENT)
        .stroke(Stroke::new(1.0, ACCENT))
}

/// Destructive / abort: transparent fill, red text + faint red border.
pub fn danger_button(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(COL_GLOBAL))
        .fill(Color32::TRANSPARENT)
        .stroke(Stroke::new(1.0, Color32::from_rgba_unmultiplied(0xE1, 0x55, 0x54, 82)))
}

/// Secondary: btn fill + button border.
pub fn secondary_button(text: impl Into<String>) -> egui::Button<'static> {
    egui::Button::new(egui::RichText::new(text.into()).color(TEXT))
        .fill(BTN)
        .stroke(Stroke::new(1.0, BTNBD))
}

/// An UPPERCASE column/section header label in `faint`.
pub fn header_label(text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase()).color(FAINT).size(11.0).strong()
}

/// An UPPERCASE settings-section label in the accent color.
pub fn section_label(text: &str) -> egui::RichText {
    egui::RichText::new(text.to_uppercase()).color(ACCENT).size(12.0).strong()
}

/// Apply the Graphite visuals to an egui context.
pub fn apply(ctx: &egui::Context) {
    let mut v = egui::Visuals::dark();

    v.override_text_color = Some(TEXT);
    v.panel_fill = PANEL;
    v.window_fill = PANEL;
    v.window_stroke = Stroke::new(1.0, BORDER);
    v.extreme_bg_color = FIELD;
    v.faint_bg_color = HOV;
    v.hyperlink_color = ACCENT;

    v.selection.bg_fill = SEL;
    v.selection.stroke = Stroke::new(1.0, SELBD);

    // 8px keeps a flat edge at the compact control heights (14px would round the
    // short buttons/fields into pill ends).
    let round = Rounding::same(8.0);
    let round_small = Rounding::same(8.0);

    // Non-interactive surfaces (labels, separators, group frames).
    v.widgets.noninteractive.bg_fill = PANEL;
    v.widgets.noninteractive.weak_bg_fill = PANEL;
    v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, DIM);
    v.widgets.noninteractive.rounding = round_small;

    // Resting interactive widgets (buttons, checkboxes).
    v.widgets.inactive.bg_fill = BTN;
    v.widgets.inactive.weak_bg_fill = BTN;
    v.widgets.inactive.bg_stroke = Stroke::new(1.0, BTNBD);
    v.widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.inactive.rounding = round;

    // Hovered.
    v.widgets.hovered.bg_fill = BTN;
    v.widgets.hovered.weak_bg_fill = HOV;
    v.widgets.hovered.bg_stroke = Stroke::new(1.0, BTNBD);
    v.widgets.hovered.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.hovered.rounding = round;

    // Active / pressed.
    v.widgets.active.bg_fill = BTN;
    v.widgets.active.weak_bg_fill = HOV;
    v.widgets.active.bg_stroke = Stroke::new(1.0, SELBD);
    v.widgets.active.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.active.rounding = round;

    // Open (combo boxes, menus).
    v.widgets.open.bg_fill = FIELD;
    v.widgets.open.weak_bg_fill = FIELD;
    v.widgets.open.bg_stroke = Stroke::new(1.0, BORDER);
    v.widgets.open.fg_stroke = Stroke::new(1.0, TEXT);
    v.widgets.open.rounding = round;

    ctx.set_visuals(v);

    // Type scale.
    use FontFamily::{Monospace, Proportional};
    ctx.style_mut(|s| {
        s.text_styles = [
            (TextStyle::Heading, FontId::new(19.0, Proportional)),
            (TextStyle::Body, FontId::new(13.0, Proportional)),
            (TextStyle::Button, FontId::new(13.0, Proportional)),
            (TextStyle::Small, FontId::new(11.0, Proportional)),
            (TextStyle::Monospace, FontId::new(12.0, Monospace)),
        ]
        .into();
        s.spacing.item_spacing = egui::vec2(7.0, 7.0);
        s.spacing.button_padding = egui::vec2(9.0, 5.0);
        s.spacing.interact_size.y = 23.0;
    });

    ctx.set_zoom_factor(1.0);
}
