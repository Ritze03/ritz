//! Font setup. Bundles the Geist family:
//! - **Geist Mono** (TTF) — monospace text (the default UI font); crisp TTF outlines.
//! - **Geist** (TTF) — proportional sans, used for UI text when `mono_ui` is off.
//! - **Geist Mono Nerd Font** (OTF) — supplies icon glyphs only, as a fallback.
//!
//! Text always comes from a crisp TTF face; only `\uf…` icon glyphs fall back to
//! the patched Nerd Font, so edges stay clean.

use egui::{FontData, FontDefinitions, FontFamily};

/// Install fonts. When `mono_ui` is true (default) UI text uses Geist Mono;
/// when false it uses proportional Geist. Safe to call again at runtime to
/// switch fonts live.
pub fn install(ctx: &egui::Context, mono_ui: bool) {
    let mut fonts = FontDefinitions::default();

    fonts.font_data.insert(
        "mono".to_owned(),
        std::sync::Arc::new(FontData::from_static(crate::resources::mono_font_bytes())),
    );
    fonts.font_data.insert(
        "sans".to_owned(),
        std::sync::Arc::new(FontData::from_static(crate::resources::sans_font_bytes())),
    );
    fonts.font_data.insert(
        "icons".to_owned(),
        std::sync::Arc::new(FontData::from_static(crate::resources::icon_font_bytes())),
    );
    fonts.font_data.insert(
        "bold".to_owned(),
        std::sync::Arc::new(FontData::from_static(crate::resources::bold_font_bytes())),
    );

    // A named "bold" family (Geist Bold) for the logo wordmark.
    fonts
        .families
        .insert(FontFamily::Name("bold".into()), vec!["bold".to_owned()]);

    // Proportional (UI text): mono or sans, with the icon font as glyph fallback.
    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    prop.clear();
    prop.push(if mono_ui { "mono" } else { "sans" }.to_owned());
    prop.push("icons".to_owned());

    // Monospace (command preview, value badges): always Geist Mono + icons.
    let monof = fonts.families.entry(FontFamily::Monospace).or_default();
    monof.clear();
    monof.push("mono".to_owned());
    monof.push("icons".to_owned());

    ctx.set_fonts(fonts);
}
