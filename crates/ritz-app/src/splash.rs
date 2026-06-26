//! The launch splash: a small, fixed-size countdown window with three keys —
//! `Q` cancel, `W` launch now, `E` edit config. Times out to launch.
//!
//! Styled to match the manager ("Graphite" theme) and shows the animated logo
//! nodding along a U-arc with a soft shadow.
//!
//! The editor opens as a *separate process* (see `main`), so the splash simply
//! closes and reports the chosen action.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use egui::text::LayoutJob;
use egui::Color32;
use ritz_core::config::Paths;

use crate::theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SplashAction {
    Launch,
    Cancel,
    Edit,
}

/// Show the splash for `timeout_secs`. Returns the chosen action (timeout =>
/// Launch). With no display, returns `Launch` immediately.
pub fn show(game_name: &str, appid: &str, timeout_secs: u64, profile: Option<&str>) -> SplashAction {
    if !has_display() || timeout_secs == 0 {
        return SplashAction::Launch;
    }

    // Match the manager's font preference.
    let mono_ui = Paths::discover()
        .load_general()
        .map(|g| g.mono_ui)
        .unwrap_or(true);

    let mut app = SplashApp {
        game_name: game_name.to_string(),
        appid: appid.to_string(),
        profile: profile.map(str::to_string),
        timeout: Duration::from_secs(timeout_secs),
        start: Instant::now(),
        logo: None,
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 410.0])
            .with_resizable(false),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "Ritz — Launching",
        options,
        Box::new(move |cc| {
            crate::fonts::install(&cc.egui_ctx, mono_ui);
            crate::theme::apply(&cc.egui_ctx);
            app.logo = crate::image::load_logo_texture(
                &cc.egui_ctx,
                crate::resources::logo_1024_bytes(),
                256,
                "ritz-splash-logo",
            );
            Ok(Box::new(app))
        }),
    );

    // `finish` exits the process directly on every action, so reaching here means
    // the window was closed externally (compositor/WM) — treat that as Launch.
    SplashAction::Launch
}

fn has_display() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var_os("DISPLAY").is_some()
}

struct SplashApp {
    game_name: String,
    appid: String,
    profile: Option<String>,
    timeout: Duration,
    start: Instant,
    logo: Option<egui::TextureHandle>,
}

impl SplashApp {
    fn finish(&self, _ctx: &egui::Context, action: SplashAction) {
        // Exit the process immediately rather than going through eframe's
        // graceful teardown — the window vanishes the instant the action is
        // chosen (otherwise the editor's slow startup leaves the splash lingering).
        let code = match action {
            SplashAction::Launch => crate::SPLASH_LAUNCH,
            SplashAction::Cancel => crate::SPLASH_CANCEL,
            SplashAction::Edit => crate::SPLASH_EDIT,
        };
        std::process::exit(code);
    }
}

impl eframe::App for SplashApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Keyboard shortcuts.
        let mut pending: Option<SplashAction> = None;
        ctx.input(|i| {
            if i.key_pressed(egui::Key::Q) || i.key_pressed(egui::Key::Escape) {
                pending = Some(SplashAction::Cancel);
            } else if i.key_pressed(egui::Key::W) || i.key_pressed(egui::Key::Enter) {
                pending = Some(SplashAction::Launch);
            } else if i.key_pressed(egui::Key::E) {
                pending = Some(SplashAction::Edit);
            }
        });
        if let Some(action) = pending {
            self.finish(ctx, action);
            return;
        }

        let remaining = self.timeout.saturating_sub(self.start.elapsed());
        if remaining.is_zero() {
            self.finish(ctx, SplashAction::Launch);
            return;
        }

        let t = self.start.elapsed().as_secs_f32();

        let clicked = bottom_buttons(ctx, qwe_button_row);
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme::PANEL).inner_margin(egui::Margin::same(22.0)))
            .show(ctx, |ui| {
                let heading = format!("Launching {}", self.game_name);
                launch_view(
                    ui,
                    self.logo.as_ref(),
                    t,
                    &heading,
                    &self.appid,
                    self.profile.as_deref(),
                    Some(remaining.as_secs_f32()),
                );
            });

        if let Some(b) = clicked {
            self.finish(ctx, qwe_action(b));
            return;
        }

        ctx.request_repaint();
    }
}

/// Render a screen's button row pinned to the bottom of the window. `row` draws
/// the (centered) buttons and returns whatever the caller needs.
fn bottom_buttons<R>(ctx: &egui::Context, row: impl FnOnce(&mut egui::Ui) -> R) -> R {
    egui::TopBottomPanel::bottom("splash_buttons")
        .frame(
            egui::Frame::none()
                .fill(theme::PANEL)
                .inner_margin(egui::Margin { left: 22.0, right: 22.0, top: 12.0, bottom: 18.0 }),
        )
        .show_separator_line(false)
        .show(ctx, |ui| row(ui))
        .inner
}

/// Map a `qwe_button_row` index to a `SplashAction`.
fn qwe_action(b: u8) -> SplashAction {
    match b {
        0 => SplashAction::Cancel,
        1 => SplashAction::Launch,
        _ => SplashAction::Edit,
    }
}

/// The normal splash body: nodding logo, heading, AppID, an optional countdown
/// (`remaining`), and the resolved profile. Buttons are drawn separately in the
/// bottom panel.
fn launch_view(
    ui: &mut egui::Ui,
    logo: Option<&egui::TextureHandle>,
    t: f32,
    heading: &str,
    appid: &str,
    profile: Option<&str>,
    remaining: Option<f32>,
) {
    let (area, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 168.0), egui::Sense::hover());
    paint_logo_anim(ui.painter(), area, t, logo, 120.0);

    ui.vertical_centered(|ui| {
        ui.heading(heading);
        ui.label(egui::RichText::new(format!("AppID {appid}")).color(theme::FAINT));
        profile_label(ui, profile);

        // Big blue line (countdown / "Ready to Launch!"), centered in the space
        // left between the profile line and the bottom button panel.
        let (text, font) = match remaining {
            Some(secs) => (
                format!("{secs:.1}s"),
                egui::FontId::new(38.0, egui::FontFamily::Monospace),
            ),
            None => (
                "Ready to Launch!".to_string(),
                egui::FontId::new(38.0, egui::FontFamily::Proportional),
            ),
        };
        let h = ui.fonts(|f| f.row_height(&font));
        // +9: optical correction for the bottom button panel's top inner margin,
        // which sits below the CentralPanel's measured bottom edge.
        ui.add_space((((ui.available_height() - h) / 2.0) + 9.0).max(0.0));
        ui.label(egui::RichText::new(text).font(font).color(theme::ACCENT));
    });
}

/// A centered "Profile <name>" line: label dim, name in the text color.
fn profile_label(ui: &mut egui::Ui, profile: Option<&str>) {
    let font = egui::TextStyle::Body.resolve(ui.style());
    let mut job = LayoutJob::default();
    job.append("Profile  ", 0.0, egui::TextFormat { font_id: font.clone(), color: theme::DIM, ..Default::default() });
    job.append(profile.unwrap_or("None"), 0.0, egui::TextFormat { font_id: font, color: theme::TEXT, ..Default::default() });
    ui.label(job);
}

/// Left pad to horizontally center a button row with the given labels.
fn buttons_pad(ui: &egui::Ui, labels: &[&str]) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let bpad = ui.spacing().button_padding.x * 2.0;
    let gap = ui.spacing().item_spacing.x;
    let total: f32 = labels
        .iter()
        .map(|l| {
            ui.fonts(|f| f.layout_no_wrap((*l).to_string(), font.clone(), Color32::WHITE).size().x)
                + bpad
        })
        .sum::<f32>()
        + gap * (labels.len() as f32 - 1.0);
    ((ui.available_width() - total) * 0.5).max(0.0)
}

/// Centered `[Q] Cancel` / `[W] Launch` / `[E] Edit` row. Returns 0/1/2 on click.
fn qwe_button_row(ui: &mut egui::Ui) -> Option<u8> {
    let labels = ["[Q] Cancel", "[W] Launch", "[E] Edit"];
    let mut clicked = None;
    ui.horizontal(|ui| {
        ui.add_space(buttons_pad(ui, &labels));
        if ui.add(theme::danger_button(labels[0])).clicked() { clicked = Some(0); }
        if ui.add(theme::primary_button(labels[1])).clicked() { clicked = Some(1); }
        if ui.add(theme::secondary_button(labels[2])).clicked() { clicked = Some(2); }
    });
    clicked
}

/// Paint the nodding logo (U-arc + tilt about a low pivot) and its soft shadow,
/// driven by elapsed time `t`. See the design spec in the splash rework plan.
fn paint_logo_anim(
    painter: &egui::Painter,
    area: egui::Rect,
    t: f32,
    logo: Option<&egui::TextureHandle>,
    logo_px: f32,
) {
    let logo_dim = logo_px;
    #[allow(non_snake_case)]
    let LOGO = logo_dim;
    let k = LOGO / 300.0;

    // Ping-ponged, eased phase.
    let sweep = t / 1.9;
    let raw = sweep.fract();
    let tt = if (sweep as u64) % 2 == 0 { raw } else { 1.0 - raw };
    let e = tt * tt * (3.0 - 2.0 * tt); // smoothstep ≈ ease-in-out
    let s = 1.0 - (2.0 * e - 1.0).powi(2); // 0 at ends, 1 at the mid-dip
    let lerp = |a: f32, b: f32, p: f32| a + (b - a) * p;

    let x = lerp(-23.0, 23.0, e) * k;
    let y = (-9.0 + 31.0 * s) * k; // U dip: -9 at ends, +22 at middle
    let rot = lerp(-4.5, 4.5, e).to_radians();

    let cx = area.center().x;
    let rest_cy = area.top() + LOGO * 0.5 + 12.0;
    let shadow_cy = rest_cy + LOGO * 0.5 + 20.0;

    // Shadow (drawn first, beneath the logo).
    let sh_scale = 0.93 + 0.10 * s;
    let sh_alpha = 0.20 + 0.08 * s;
    paint_soft_ellipse(
        painter,
        egui::pos2(cx + x, shadow_cy),
        LOGO * 0.34 * sh_scale,
        LOGO * 0.09 * sh_scale,
        sh_alpha,
    );

    // Logo (translated to the arc position, rotated about a low pivot).
    if let Some(tex) = logo {
        let rect = egui::Rect::from_center_size(
            egui::pos2(cx + x, rest_cy + y),
            egui::vec2(LOGO, LOGO),
        );
        let mut mesh = egui::Mesh::with_texture(tex.id());
        mesh.add_rect_with_uv(
            rect,
            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        let pivot = egui::pos2(rect.center().x, rect.top() + 0.78 * LOGO);
        mesh.rotate(egui::emath::Rot2::from_angle(rot), pivot);
        painter.add(mesh);
    }
}

/// A soft (faked-blur) filled ellipse: a few stacked translucent ellipses,
/// larger and fainter outward.
fn paint_soft_ellipse(painter: &egui::Painter, center: egui::Pos2, rx: f32, ry: f32, alpha: f32) {
    for (scale, a_mul) in [(1.8, 0.25), (1.35, 0.5), (1.0, 1.0)] {
        let col = Color32::from_black_alpha((alpha * a_mul * 255.0).clamp(0.0, 255.0) as u8);
        let pts: Vec<egui::Pos2> = (0..28)
            .map(|i| {
                let th = i as f32 / 28.0 * std::f32::consts::TAU;
                egui::pos2(center.x + rx * scale * th.cos(), center.y + ry * scale * th.sin())
            })
            .collect();
        painter.add(egui::Shape::convex_polygon(pts, col, egui::Stroke::NONE));
    }
}

// ---- New-game picker -------------------------------------------------------

/// The choice made on the new-game splash. The config is created by the caller.
pub enum NewGameChoice {
    Cancel,
    Launch { name: String, profile: Option<String> },
    Edit { name: String, profile: Option<String> },
}

/// 3-step wizard: name → pick a profile → confirm (normal splash view).
enum NewState {
    Naming,
    Choosing,
    Confirm(Option<String>), // the chosen profile
}

/// Show the new-game wizard. With no display, launches with the default profile.
pub fn show_new(
    appid: &str,
    pins: &[(u8, String)],
    default_profile: Option<&str>,
    name_guess: &str,
) -> NewGameChoice {
    if !has_display() {
        return NewGameChoice::Launch {
            name: name_guess.to_string(),
            profile: default_profile.map(|s| s.to_string()),
        };
    }
    let mono_ui = Paths::discover().load_general().map(|g| g.mono_ui).unwrap_or(true);

    let result = Arc::new(Mutex::new(None::<NewGameChoice>));
    let mut app = NewGameApp {
        appid: appid.to_string(),
        pins: pins.to_vec(),
        default_profile: default_profile.map(|s| s.to_string()),
        name_buf: name_guess.to_string(),
        focus_name: true,
        state: NewState::Naming,
        result: result.clone(),
        logo: None,
        start: Instant::now(),
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 410.0])
            .with_resizable(false),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "Ritz — New Game",
        options,
        Box::new(move |cc| {
            crate::fonts::install(&cc.egui_ctx, mono_ui);
            crate::theme::apply(&cc.egui_ctx);
            app.logo = crate::image::load_logo_texture(
                &cc.egui_ctx,
                crate::resources::logo_1024_bytes(),
                256,
                "ritz-splash-logo",
            );
            Ok(Box::new(app))
        }),
    );

    let chosen = result.lock().unwrap().take().unwrap_or(NewGameChoice::Cancel);
    chosen
}

struct NewGameApp {
    appid: String,
    pins: Vec<(u8, String)>,
    default_profile: Option<String>,
    name_buf: String,
    focus_name: bool,
    state: NewState,
    result: Arc<Mutex<Option<NewGameChoice>>>,
    logo: Option<egui::TextureHandle>,
    start: Instant,
}

impl NewGameApp {
    fn finish(&self, ctx: &egui::Context, choice: NewGameChoice) {
        *self.result.lock().unwrap() = Some(choice);
        ctx.send_viewport_cmd(egui::ViewportCommand::Close);
    }
}

/// Keybind label for a slot id: 1–9 as-is, 10 shown as "0".
fn keybind_char(id: u8) -> char {
    if id == 10 { '0' } else { (b'0' + id) as char }
}

impl eframe::App for NewGameApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let t = self.start.elapsed().as_secs_f32();

        // Key intents, per state.
        let mut cancel = false;
        let mut confirm_name = false; // Naming → Choosing
        let mut pick: Option<Option<String>> = None; // Choosing → Confirm(profile)
        let mut qwe: Option<u8> = None; // Confirm action (0=Q,1=W,2=E)
        ctx.input(|i| match &self.state {
            NewState::Naming => {
                // Read at the top of update, before the TextEdit consumes the key.
                if i.key_pressed(egui::Key::Escape) {
                    cancel = true;
                } else if i.key_pressed(egui::Key::Enter) {
                    confirm_name = true;
                }
            }
            NewState::Choosing => {
                if i.key_pressed(egui::Key::Q) || i.key_pressed(egui::Key::Escape) {
                    cancel = true;
                } else if i.key_pressed(egui::Key::W) {
                    pick = Some(self.default_profile.clone());
                } else {
                    let nums = [
                        (egui::Key::Num0, 0u8), (egui::Key::Num1, 1), (egui::Key::Num2, 2),
                        (egui::Key::Num3, 3), (egui::Key::Num4, 4), (egui::Key::Num5, 5),
                        (egui::Key::Num6, 6), (egui::Key::Num7, 7), (egui::Key::Num8, 8),
                        (egui::Key::Num9, 9),
                    ];
                    for (key, n) in nums {
                        if i.key_pressed(key) {
                            let id = if n == 0 { 10 } else { n };
                            if let Some((_, name)) = self.pins.iter().find(|(pid, _)| *pid == id) {
                                pick = Some(Some(name.clone()));
                            }
                        }
                    }
                }
            }
            NewState::Confirm(_) => {
                if i.key_pressed(egui::Key::Q) || i.key_pressed(egui::Key::Escape) {
                    cancel = true;
                } else if i.key_pressed(egui::Key::W) || i.key_pressed(egui::Key::Enter) {
                    qwe = Some(1);
                } else if i.key_pressed(egui::Key::E) {
                    qwe = Some(2);
                }
            }
        });

        if cancel {
            self.finish(ctx, NewGameChoice::Cancel);
            return;
        }
        if let Some(profile) = pick {
            self.state = NewState::Confirm(profile);
        }

        let mut card_pick: Option<String> = None;
        let mut use_default = false;

        // Buttons pinned to the bottom, per state.
        bottom_buttons(ctx, |ui| match &self.state {
            NewState::Naming => {
                ui.horizontal(|ui| {
                    ui.add_space(buttons_pad(ui, &["Cancel", "Confirm"]));
                    if ui.add(theme::danger_button("Cancel")).clicked() { cancel = true; }
                    if ui.add(theme::primary_button("Confirm")).clicked() { confirm_name = true; }
                });
            }
            NewState::Choosing => {
                ui.horizontal(|ui| {
                    ui.add_space(buttons_pad(ui, &["[W] Use Default"]));
                    if ui.add(theme::primary_button("[W] Use Default")).clicked() { use_default = true; }
                });
            }
            NewState::Confirm(_) => {
                if let Some(b) = qwe_button_row(ui) {
                    qwe = Some(b);
                }
            }
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme::PANEL).inner_margin(egui::Margin::same(22.0)))
            .show(ctx, |ui| match &self.state {
                NewState::Naming => {
                    let (area, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 168.0),
                        egui::Sense::hover(),
                    );
                    paint_logo_anim(ui.painter(), area, t, self.logo.as_ref(), 120.0);
                    ui.vertical_centered(|ui| {
                        ui.heading("New Game detected!");
                        ui.label(
                            egui::RichText::new(format!("AppID {}", self.appid)).color(theme::FAINT),
                        );
                    });
                    ui.add_space(18.0);
                    ui.vertical_centered(|ui| {
                        ui.label(
                            egui::RichText::new("Enter the Name of the new Game")
                                .color(theme::DIM),
                        );
                    });
                    ui.add_space(3.0);
                    egui::Frame::none()
                        .fill(theme::FIELD)
                        .stroke(egui::Stroke::new(1.0, theme::BORDER))
                        .rounding(egui::Rounding::same(8.0))
                        .inner_margin(egui::Margin::symmetric(11.0, 9.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            let resp = ui.add(
                                egui::TextEdit::singleline(&mut self.name_buf)
                                    .frame(false)
                                    .hint_text(
                                        egui::RichText::new("Enter new Games Name here.")
                                            .color(theme::FAINT),
                                    )
                                    .desired_width(f32::INFINITY),
                            );
                            if self.focus_name {
                                resp.request_focus();
                                self.focus_name = false;
                            }
                        });
                }
                NewState::Choosing => {
                    let (area, _) = ui.allocate_exact_size(
                        egui::vec2(ui.available_width(), 168.0),
                        egui::Sense::hover(),
                    );
                    paint_logo_anim(ui.painter(), area, t, self.logo.as_ref(), 120.0);
                    ui.vertical_centered(|ui| ui.heading("Choose a profile"));
                    ui.add_space(14.0);
                    if self.pins.is_empty() {
                        ui.vertical_centered(|ui| {
                            ui.label(egui::RichText::new("No pinned profiles.").color(theme::FAINT));
                        });
                    } else {
                        if self.pins.len() <= 5 {
                            let pad = ((ui.available_height() - 40.0) / 2.0).max(0.0);
                            ui.add_space(pad);
                        }
                        // Size cards to fit 5-across in the available width (cap at
                        // 100) so a full row never overflows and stays centered.
                        let gap = ui.spacing().item_spacing.x;
                        let card_w = ((ui.available_width() - 4.0 * gap) / 5.0).min(100.0);
                        for chunk in self.pins.chunks(5) {
                            let n = chunk.len() as f32;
                            let row_w = n * card_w + (n - 1.0) * gap;
                            ui.horizontal(|ui| {
                                ui.add_space(((ui.available_width() - row_w) * 0.5).max(0.0));
                                for (id, name) in chunk {
                                    if pin_card(ui, keybind_char(*id), name, card_w).clicked() {
                                        card_pick = Some(name.clone());
                                    }
                                }
                            });
                            ui.add_space(8.0);
                        }
                    }
                }
                NewState::Confirm(profile) => {
                    let heading = format!("Added {}", self.name_buf.trim());
                    launch_view(
                        ui,
                        self.logo.as_ref(),
                        t,
                        &heading,
                        &self.appid,
                        profile.as_deref(),
                        None,
                    );
                }
            });

        if cancel {
            self.finish(ctx, NewGameChoice::Cancel);
            return;
        }
        if confirm_name && !self.name_buf.trim().is_empty() {
            self.state = NewState::Choosing;
        }
        if let Some(name) = card_pick {
            self.state = NewState::Confirm(Some(name));
        }
        if use_default {
            self.state = NewState::Confirm(self.default_profile.clone());
        }
        if let Some(b) = qwe {
            if let NewState::Confirm(profile) = &self.state {
                let name = self.name_buf.trim().to_string();
                let profile = profile.clone();
                let choice = match b {
                    0 => NewGameChoice::Cancel,
                    2 => NewGameChoice::Edit { name, profile },
                    _ => NewGameChoice::Launch { name, profile },
                };
                self.finish(ctx, choice);
                return;
            }
        }

        ctx.request_repaint();
    }
}

/// A `width`×40 profile card: blue outline, transparent fill, keybind + (clipped) name.
fn pin_card(ui: &mut egui::Ui, keybind: char, name: &str, width: f32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(width, 40.0), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let hovered = resp.hovered();
        let painter = ui.painter();
        let stroke = egui::Stroke::new(if hovered { 2.0 } else { 1.5 }, theme::ACCENT);
        painter.rect(rect, egui::Rounding::same(8.0), Color32::TRANSPARENT, stroke);
        painter.text(
            egui::pos2(rect.center().x, rect.top() + 13.0),
            egui::Align2::CENTER_CENTER,
            keybind.to_string(),
            egui::FontId::proportional(16.0),
            theme::ACCENT,
        );
        let short = fit_text(ui, name, width - 12.0, egui::FontId::proportional(11.0));
        ui.painter().text(
            egui::pos2(rect.center().x, rect.bottom() - 11.0),
            egui::Align2::CENTER_CENTER,
            short,
            egui::FontId::proportional(11.0),
            theme::TEXT,
        );
    }
    resp
}

/// Truncate `text` with a trailing ".." until it fits `max_w` in `font`.
fn fit_text(ui: &egui::Ui, text: &str, max_w: f32, font: egui::FontId) -> String {
    let width = |s: &str| {
        ui.fonts(|f| f.layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x)
    };
    if width(text) <= max_w {
        return text.to_string();
    }
    let mut s: String = text.to_string();
    while !s.is_empty() && width(&format!("{s}..")) > max_w {
        s.pop();
    }
    format!("{s}..")
}

// ---- Non-Steam id setup -----------------------------------------------------

/// Show the non-Steam id-setup screen: the user names the game and gets a
/// copyable `RITZ_APPID=<id> ritz %command%` to put in their Steam launch
/// options. Writes nothing and never launches (the caller returns Cancel).
pub fn show_unknown(existing_ids: &[String]) {
    if !has_display() {
        return;
    }
    let mono_ui = Paths::discover().load_general().map(|g| g.mono_ui).unwrap_or(true);
    let mut app = UnknownApp {
        existing: existing_ids.to_vec(),
        name_buf: String::new(),
        focus_name: true,
        copied: false,
        logo: None,
        start: Instant::now(),
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([560.0, 460.0])
            .with_resizable(false),
        ..Default::default()
    };
    let _ = eframe::run_native(
        "Ritz — New Game",
        options,
        Box::new(move |cc| {
            crate::fonts::install(&cc.egui_ctx, mono_ui);
            crate::theme::apply(&cc.egui_ctx);
            app.logo = crate::image::load_logo_texture(
                &cc.egui_ctx,
                crate::resources::logo_1024_bytes(),
                256,
                "ritz-splash-logo",
            );
            Ok(Box::new(app))
        }),
    );
}

struct UnknownApp {
    existing: Vec<String>,
    name_buf: String,
    focus_name: bool,
    copied: bool,
    logo: Option<egui::TextureHandle>,
    start: Instant,
}

impl eframe::App for UnknownApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let t = self.start.elapsed().as_secs_f32();
        let id = sanitize_appid(&self.name_buf);
        let conflict = !id.is_empty() && self.existing.iter().any(|e| e == &id);

        let mut close = false;
        let mut copy = false;
        ctx.input(|i| {
            if i.key_pressed(egui::Key::Escape) {
                close = true;
            } else if i.key_pressed(egui::Key::Enter) {
                copy = true;
            }
        });

        let copy_label = if self.copied { "Copied!" } else { "Copy (Enter)" };
        bottom_buttons(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.add_space(buttons_pad(ui, &[copy_label, "Close (Esc)"]));
                if ui.add_enabled(!id.is_empty(), theme::primary_button(copy_label)).clicked() { copy = true; }
                if ui.add(theme::secondary_button("Close (Esc)")).clicked() { close = true; }
            });
        });

        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme::PANEL).inner_margin(egui::Margin::same(22.0)))
            .show(ctx, |ui| {
                let (area, _) = ui.allocate_exact_size(
                    egui::vec2(ui.available_width(), 168.0),
                    egui::Sense::hover(),
                );
                paint_logo_anim(ui.painter(), area, t, self.logo.as_ref(), 120.0);
                ui.vertical_centered(|ui| {
                    ui.heading("New Game detected!");
                    ui.label(
                        egui::RichText::new("Non-Steam game — set a stable ID")
                            .color(theme::FAINT),
                    );
                });
                ui.add_space(14.0);
                egui::Frame::none()
                    .fill(theme::FIELD)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(egui::Margin::symmetric(11.0, 9.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.name_buf)
                                .frame(false)
                                .hint_text(
                                    egui::RichText::new("Enter new Games Name here.")
                                        .color(theme::FAINT),
                                )
                                .desired_width(f32::INFINITY),
                        );
                        if self.focus_name {
                            resp.request_focus();
                            self.focus_name = false;
                        }
                        if resp.changed() {
                            self.copied = false;
                        }
                    });
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new("Set this in the game's launch options:")
                        .color(theme::DIM),
                );
                ui.add_space(4.0);
                let cmd_text = if id.is_empty() {
                    "RITZ_APPID=<name> ritz %command%".to_string()
                } else {
                    format!("RITZ_APPID={id} ritz %command%")
                };
                egui::Frame::none()
                    .fill(theme::FIELD)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(egui::Margin::symmetric(11.0, 9.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(egui::Label::new(
                            egui::RichText::new(cmd_text)
                                .monospace()
                                .color(if id.is_empty() { theme::FAINT } else { theme::TEXT }),
                        ).wrap().selectable(true));
                    });
                if conflict {
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new(format!(
                            "ID \"{id}\" already exists — pick another name."
                        ))
                        .color(theme::COL_GLOBAL),
                    );
                }
            });

        if copy && !id.is_empty() {
            copy_to_clipboard(ctx, &format!("RITZ_APPID={id} ritz %command%"));
            self.copied = true;
        }
        if close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        ctx.request_repaint();
    }
}

/// Copy `text` to the system clipboard so it survives this short-lived process:
/// pipe it to the platform clipboard daemon (wl-copy on Wayland, xclip/xsel on
/// X11) which forks to own the selection. Falls back to egui's clipboard (needs
/// a running clipboard manager) if none are installed.
fn copy_to_clipboard(ctx: &egui::Context, text: &str) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let feed = |cmd: &str, args: &[&str]| -> bool {
        let Ok(mut child) = Command::new(cmd)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        else {
            return false;
        };
        // Write + close stdin so the tool reads EOF and forks to serve the
        // selection; we deliberately don't wait on the child.
        if let Some(mut stdin) = child.stdin.take() {
            let _ = stdin.write_all(text.as_bytes());
        }
        true
    };

    let ok = if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        feed("wl-copy", &[])
    } else {
        feed("xclip", &["-selection", "clipboard"]) || feed("xsel", &["--clipboard", "--input"])
    };
    if !ok {
        ctx.copy_text(text.to_string());
    }
}

/// Build a safe RITZ_APPID from a name: whitespace → `_`, keep `[A-Za-z0-9_-]`.
fn sanitize_appid(name: &str) -> String {
    name.trim()
        .chars()
        .map(|c| if c.is_whitespace() { '_' } else { c })
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}
