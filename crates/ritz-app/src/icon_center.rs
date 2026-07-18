//! Reusable visual centering for icon glyphs.
//!
//! Font glyphs carry their own bearings and line metrics, so anchoring an icon
//! with `Align2::CENTER_CENTER` centers its *layout* box, not its visible ink.
//! The layout box is the glyph's advance width (side bearings included) by the
//! font's line height — line height is identical for every glyph while the ink
//! sits at a different height in each, so a gear and a trash can get the same
//! layout box but visibly different centers, and a row of icons reads ragged.
//!
//! Fix: lay each glyph out once, read the tight bounding box of the actually
//! rendered mesh (`Galley::mesh_bounds`), and cache the resulting ink center.
//! After that, any icon can be drawn dead-center in a fixed cell for free.
//!
//! Usage: call [`IconCenterCache::centered_pos`] for the draw position, then
//! `painter.text(pos, Align2::LEFT_TOP, ..)`. **Every** call site must anchor
//! `LEFT_TOP` — anchoring `CENTER_CENTER` on top of a corrected position
//! double-corrects and pushes the glyph off by its own ink offset.
//!
//! Ported from ForzaTelemetryV3's `iconcache.rs`. *Why the key differs:* Forza
//! keys on `(char, size)` because it has exactly one icon font. Ritz's icons are
//! emoji/unicode codepoints resolved through egui's font *fallback* chain, so
//! the same char at the same size can resolve to a different family (UI text
//! font vs. the Nerd Font fallback) with a different ink box — the family is
//! part of the identity of the measurement and must be in the key.

use std::collections::HashMap;

use egui::{Color32, FontFamily, FontId, Pos2, Ui, Vec2};

/// Cache key: glyph + size + resolved font family. See the module docs for why
/// the family is included (ritz resolves icons through a fallback chain).
pub type IconKey = (char, u32, FontFamily);

/// Build the cache key for one glyph rendered with `font`.
///
/// Icon strings are one code point per glyph, so the leading char identifies it;
/// the size is keyed by its bit pattern because `f32` is not `Hash`/`Eq`.
pub fn icon_key(icon: &str, font: &FontId) -> IconKey {
    (
        icon.chars().next().unwrap_or(' '),
        font.size.to_bits(),
        font.family.clone(),
    )
}

/// Per-(glyph, size, family) cache of the rendered ink center, in galley-local
/// points.
#[derive(Default)]
pub struct IconCenterCache {
    ink_center: HashMap<IconKey, Vec2>,
}

impl IconCenterCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns the top-left position at which to paint `icon` (as a single-glyph
    /// galley, anchored `Align2::LEFT_TOP`) so its visible ink is centered on
    /// `center`. The measurement is cached per glyph + size + family.
    ///
    /// Must NOT be called from inside a `ctx.fonts(|f| …)` closure: laying out
    /// text takes the same font lock and would deadlock.
    pub fn centered_pos(&mut self, ui: &Ui, icon: &str, font: &FontId, center: Pos2) -> Pos2 {
        let key = icon_key(icon, font);
        let ink_center = *self.ink_center.entry(key).or_insert_with(|| {
            let galley = ui
                .painter()
                .layout_no_wrap(icon.to_owned(), font.clone(), Color32::WHITE);
            // mesh_bounds is the tight box around the rendered triangles — the
            // glyph's actual ink, not the font's line box. An empty or missing
            // glyph produces no triangles and a non-positive box; fall back to
            // the layout center so the icon still lands somewhere sane instead
            // of at the galley origin.
            if galley.mesh_bounds.is_positive() {
                galley.mesh_bounds.center().to_vec2()
            } else {
                galley.rect.center().to_vec2()
            }
        });
        center - ink_center
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_separates_size_and_family() {
        let a = icon_key("\u{f1f8}", &FontId::new(13.0, FontFamily::Proportional));
        let same = icon_key("\u{f1f8}", &FontId::new(13.0, FontFamily::Proportional));
        let bigger = icon_key("\u{f1f8}", &FontId::new(14.0, FontFamily::Proportional));
        let mono = icon_key("\u{f1f8}", &FontId::new(13.0, FontFamily::Monospace));
        let other_glyph = icon_key("\u{f062}", &FontId::new(13.0, FontFamily::Proportional));
        assert_eq!(a, same);
        // Size and family both change the ink box, so both must split the key —
        // this is the ritz-specific deviation from ForzaV3's (char, size) key.
        assert_ne!(a, bigger);
        assert_ne!(a, mono);
        assert_ne!(a, other_glyph);
    }

    #[test]
    fn key_uses_leading_char_and_tolerates_empty() {
        let font = FontId::new(12.0, FontFamily::Proportional);
        assert_eq!(icon_key("ab", &font).0, 'a');
        // An empty icon string must not panic; it degrades to a space key.
        assert_eq!(icon_key("", &font).0, ' ');
    }
}
