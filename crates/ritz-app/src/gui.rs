//! The settings manager GUI: renders each extension's UI tree dynamically,
//! honors `Requires` visibility, shows tri-state inheritance (default / preset /
//! game override) with reset-to-inherit, and previews the live launch command.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use egui::text::{LayoutJob, TextFormat};
use egui::Color32;
use ritz_core::condition;
use ritz_core::config::{AuthorsMap, GameConfig, GeneralConfig, Paths, Preset};
use ritz_core::resolve::{self, Provenance, Resolution};
use ritz_core::schema::{Extension, FieldType, OptionsSpec, UiField};
use serde_json::{json, Value};

use crate::context::{self, chain_would_have_cycle, collect_parent_chain, merge_modules, AppContext};

use crate::theme::{self, COL_GAME, COL_GLOBAL, COL_PROFILE, ICON_EDIT, ICON_INHERIT};

#[derive(Debug, Clone, PartialEq)]
enum NavSel {
    GeneralSettings, // splash timeout, default preset — shown in central panel
    GlobalSettings,  // extension-variable global overrides — shown in central panel (ext editor)
    Profile(String),
    Game(String), // appid
}

/// A destructive action awaiting confirmation in a small dialog.
#[derive(Debug, Clone)]
enum ConfirmAction {
    DeleteGame(String),    // appid
    DeleteProfile(String), // preset name
    ClearSettings,
    ReExportResources,
    ConfigCleanup,
}

/// What the user chose to do with the launch when the editor was opened from the
/// splash (launch mode). In standalone mode this is always `Continue`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditOutcome {
    Continue,
    Cancel,
}

/// Open the manager with no specific game (picks the first saved game, or a
/// scratch config if none exist).
pub fn run_manager(ctx: &AppContext) -> Result<()> {
    let games = ctx.list_games();
    let (appid, name) = games
        .first()
        .cloned()
        .unwrap_or_else(|| ("0".to_string(), "Scratch".to_string()));
    run(ctx, &appid, &name, vec!["%command%".to_string()], false)?;
    Ok(())
}

/// Open the editor focused on a specific game.
///
/// When `launch_mode` is true (opened from the splash) the window shows a top bar
/// with "Continue" / "Cancel launch"; the returned [`EditOutcome`] tells the
/// caller whether to proceed with the launch.
pub fn run(
    ctx: &AppContext,
    appid: &str,
    name: &str,
    game_command: Vec<String>,
    launch_mode: bool,
) -> Result<EditOutcome> {
    let mut app = GuiApp::new(ctx, appid, name, game_command);
    app.launch_mode = launch_mode;
    let outcome = app.outcome.clone();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([1200.0, 650.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Ritz — Settings",
        options,
        Box::new(|cc| {
            crate::fonts::install(&cc.egui_ctx, app.general_config.mono_ui);
            crate::theme::apply(&cc.egui_ctx);
            app.logo = load_logo(&cc.egui_ctx);
            Ok(Box::new(app))
        }),
    )
    .map_err(|e| anyhow::anyhow!("egui error: {e}"))?;

    let outcome = *outcome.lock().unwrap();
    Ok(outcome)
}

/// The settings editor.
pub struct GuiApp {
    paths: Paths,
    default_preset: Option<String>,
    all_specs: Vec<Extension>,
    /// Tree folder for each entry in `all_specs` (relative to the extensions root).
    all_dirs: Vec<PathBuf>,
    all_is_folder_ext: Vec<bool>,
    cur_specs: Vec<Extension>,
    /// Tree folder for each entry in `cur_specs`, index-aligned.
    cur_dirs: Vec<PathBuf>,
    cur_is_folder_ext: Vec<bool>,
    /// Group the module tree by extension Author instead of folder.
    group_by_author: bool,
    /// Show the per-module inheritance/edit icons in the tree.
    show_inheritance: bool,
    games: Vec<(String, String)>,
    appid: String,
    game_command: Vec<String>,
    selected_ext: usize,
    game_config: GameConfig,
    preset: Option<Preset>,
    text_buffers: HashMap<String, String>,
    /// In-progress edit lists for `multi_string` fields, keyed by scope+ext+var.
    multi_edit: HashMap<String, Vec<String>>,
    /// True when opened from the splash to gate a launch.
    launch_mode: bool,
    /// The launch decision (shared so [`run`] can read it after the window closes).
    outcome: Arc<Mutex<EditOutcome>>,
    /// In-progress window-class detection, if any.
    detect: Option<Detect>,
    // Navigator panel state
    nav_sel: NavSel,
    all_presets: Vec<String>,
    /// Pinned profiles: name → slot id (1–10), cached from the preset files.
    preset_pins: HashMap<String, u8>,
    general_config: GeneralConfig,
    /// Global extension-variable overrides (stored as a preset in global.json).
    global_config: Preset,
    /// The preset currently loaded for editing when a Profile is selected in nav.
    editing_preset_buf: Option<Preset>,
    /// Shared rename/create name buffer for the nav settings panel.
    nav_name_buf: String,
    /// AppID buffer used only when creating a new game.
    nav_appid_buf: String,
    creating_profile: bool,
    duplicating_preset: Option<Preset>,
    creating_game: bool,
    /// Focus the create-profile/game name box on the next frame.
    focus_nav_name: bool,
    /// A destructive action awaiting confirmation, if any.
    confirm: Option<ConfirmAction>,
    /// App logo texture (loaded once the egui context exists).
    logo: Option<egui::TextureHandle>,
}

#[derive(Clone)]
enum DetectStatus {
    Waiting,
    Done(Option<String>),
}

/// A pending "Detect window class" operation targeting one field.
struct Detect {
    result: Arc<Mutex<DetectStatus>>,
    start: Instant,
    /// Extension id (`author::name::version`), for the text-buffer key.
    ext_id: String,
    author: String,
    name: String,
    var: String,
}

impl GuiApp {
    /// Build an editor for a game (does not open a window).
    pub fn new(ctx: &AppContext, appid: &str, name: &str, game_command: Vec<String>) -> GuiApp {
        let all_specs: Vec<Extension> = ctx.extensions.iter().map(|e| e.spec.clone()).collect();
        let all_dirs: Vec<PathBuf> = ctx.extensions.iter().map(|e| e.rel_dir.clone()).collect();
        let all_is_folder_ext: Vec<bool> = ctx.extensions.iter().map(|e| e.is_folder_ext).collect();
        let general_config = ctx.paths.load_general().unwrap_or_default();
        let global_config = ctx.paths.load_global_config().unwrap_or_default();
        let all_presets = ctx.paths.list_presets();
        // Closing the editor window (X) defaults to the configured action.
        let close_outcome = if general_config.editor_close_launches {
            EditOutcome::Continue
        } else {
            EditOutcome::Cancel
        };
        let mut app = GuiApp {
            paths: ctx.paths.clone(),
            default_preset: ctx.general.default_preset.clone(),
            all_specs,
            all_dirs,
            all_is_folder_ext,
            cur_specs: Vec::new(),
            cur_dirs: Vec::new(),
            cur_is_folder_ext: Vec::new(),
            group_by_author: true,
            show_inheritance: true,
            games: ctx.list_games(),
            appid: String::new(),
            game_command,
            selected_ext: 0,
            game_config: GameConfig::new(appid, name),
            preset: None,
            text_buffers: HashMap::new(),
            multi_edit: HashMap::new(),
            launch_mode: false,
            outcome: Arc::new(Mutex::new(close_outcome)),
            detect: None,
            nav_sel: NavSel::Game(appid.to_string()),
            all_presets,
            preset_pins: HashMap::new(),
            general_config,
            global_config,
            editing_preset_buf: None,
            nav_name_buf: name.to_string(),
            nav_appid_buf: String::new(),
            creating_profile: false,
            duplicating_preset: None,
            creating_game: false,
            focus_nav_name: false,
            confirm: None,
            logo: None,
        };
        app.switch_game(appid, name);
        app.nav_name_buf = app.game_config.game.name.clone();
        app.refresh_presets();
        app
    }

    fn switch_game(&mut self, appid: &str, name: &str) {
        self.appid = appid.to_string();
        self.nav_appid_buf = appid.to_string();
        self.game_config = self
            .paths
            .load_game(appid)
            .ok()
            .flatten()
            .unwrap_or_else(|| GameConfig::new(appid, name));
        let preset_name = self
            .game_config
            .config
            .modules
            .preset
            .clone()
            .or_else(|| self.default_preset.clone());
        self.preset = preset_name.and_then(|n| self.paths.load_preset(&n).ok().flatten());
        self.rebuild_cur_specs();
        self.text_buffers.clear();
        self.multi_edit.clear();
    }

    /// Rebuild `cur_specs` (the modules applicable to the current game) from
    /// `all_specs`, preserving the selected module by id where possible.
    fn rebuild_cur_specs(&mut self) {
        let prev_id = self.cur_specs.get(self.selected_ext).map(|s| s.id());
        self.cur_specs = Vec::new();
        self.cur_dirs = Vec::new();
        self.cur_is_folder_ext = Vec::new();
        for ((spec, dir), &is_fe) in self
            .all_specs
            .iter()
            .zip(self.all_dirs.iter())
            .zip(self.all_is_folder_ext.iter())
        {
            if spec.applies_to(&self.appid) {
                self.cur_specs.push(spec.clone());
                self.cur_dirs.push(dir.clone());
                self.cur_is_folder_ext.push(is_fe);
            }
        }
        self.selected_ext = prev_id
            .and_then(|id| self.cur_specs.iter().position(|s| s.id() == id))
            .unwrap_or(0);
    }

    /// Remove stored values for variables no longer declared by any loaded
    /// module — across the global config, every profile, and every game. Cleans
    /// up after extension updates (removed or renamed variables).
    fn config_cleanup(&mut self) {
        // (author, name, variable) tuples any loaded module still declares.
        let mut valid: std::collections::HashSet<(String, String, String)> =
            std::collections::HashSet::new();
        for spec in &self.all_specs {
            for field in spec.ui.values().flatten() {
                valid.insert((
                    spec.meta.author.clone(),
                    spec.meta.name.clone(),
                    field.variable.clone(),
                ));
            }
        }

        // Global config.
        cleanup_modules(&mut self.global_config.modules, &valid);
        let _ = self.paths.save_global_config(&self.global_config);

        // Every profile on disk.
        for name in self.all_presets.clone() {
            if let Ok(Some(mut p)) = self.paths.load_preset(&name) {
                cleanup_modules(&mut p.modules, &valid);
                let _ = self.paths.save_preset(&p);
            }
        }

        // Every game on disk.
        for (appid, _) in self.games.clone() {
            if let Ok(Some(mut g)) = self.paths.load_game(&appid) {
                cleanup_modules(&mut g.config.modules.authors, &valid);
                let _ = self.paths.save_game(&g);
            }
        }

        // Mirror into the in-memory state so the tree updates immediately.
        cleanup_modules(&mut self.game_config.config.modules.authors, &valid);
        if let Some(p) = self.preset.as_mut() {
            cleanup_modules(&mut p.modules, &valid);
        }
        if let Some(p) = self.editing_preset_buf.as_mut() {
            cleanup_modules(&mut p.modules, &valid);
        }
    }

    /// Reload all extensions from disk into the running editor (hot-reload).
    fn reload_extensions(&mut self) {
        let Ok(exts) = context::load_extensions(&self.paths) else {
            return;
        };
        self.all_specs = exts.iter().map(|e| e.spec.clone()).collect();
        self.all_dirs = exts.iter().map(|e| e.rel_dir.clone()).collect();
        self.all_is_folder_ext = exts.iter().map(|e| e.is_folder_ext).collect();
        self.rebuild_cur_specs();
    }

    /// Reload the on-disk configuration into the running editor: global config,
    /// general settings, the profile/game lists, the current game's config +
    /// preset, and the profile being edited. Discards uncommitted text edits.
    fn reload_configs(&mut self) {
        self.global_config = self.paths.load_global_config().unwrap_or_default();
        self.general_config = self.paths.load_general().unwrap_or_default();
        self.refresh_presets();
        self.refresh_games();
        let appid = self.appid.clone();
        let name = self.game_config.game.name.clone();
        self.switch_game(&appid, &name);
        if let NavSel::Profile(pname) = self.nav_sel.clone() {
            self.editing_preset_buf = self.paths.load_preset(&pname).ok().flatten();
        }
    }

    /// Scope tint for the layer currently being edited (Game blue / Profile green
    /// / Global red). Used to color values set at that layer.
    fn editing_scope_color(&self) -> Color32 {
        match &self.nav_sel {
            NavSel::GlobalSettings => COL_GLOBAL,
            NavSel::Profile(_) => COL_PROFILE,
            _ => COL_GAME,
        }
    }

    /// Resolution used for the extension editor (reflects whichever layer is being edited).
    fn resolve_for_editing(&self) -> Resolution {
        let global = Some(&self.global_config);
        match &self.nav_sel {
            NavSel::GeneralSettings | NavSel::Game(_) => {
                resolve::resolve(&self.cur_specs, Some(&self.game_config), self.preset.as_ref(), global)
            }
            NavSel::GlobalSettings => {
                let fake = preset_as_fake_game(&self.global_config);
                resolve::resolve(&self.cur_specs, Some(&fake), None, None)
            }
            NavSel::Profile(_) => match &self.editing_preset_buf {
                Some(p) => {
                    let parent = p.parent.as_ref()
                        .map(|n| collect_parent_chain(&self.paths, n));
                    let fake = preset_as_fake_game(p);
                    resolve::resolve(&self.cur_specs, Some(&fake), parent.as_ref(), global)
                }
                None => resolve::resolve(&self.cur_specs, None, None, global),
            },
        }
    }

    /// Resolution always for the current game — used for the launch command preview.
    fn resolve_for_game(&self) -> Resolution {
        // If the profile being edited is the one applied to this game, use the
        // live (unsaved) edit buffer so the preview reflects in-progress edits.
        let direct = match (&self.editing_preset_buf, &self.preset) {
            (Some(buf), Some(applied)) if buf.name == applied.name => Some(buf as &Preset),
            _ => self.preset.as_ref(),
        };
        // Merge parent chain under the direct preset (parent = lower priority).
        let merged: Option<Preset> = direct.and_then(|p| {
            let pname = p.parent.as_ref()?;
            let mut base = collect_parent_chain(&self.paths, pname);
            merge_modules(&mut base.modules, &p.modules);
            Some(base)
        });
        let effective = merged.as_ref().map(|p| p as &Preset).or(direct);
        resolve::resolve(
            &self.cur_specs,
            Some(&self.game_config),
            effective,
            Some(&self.global_config),
        )
    }

    fn persist(&self) {
        let result = match &self.nav_sel {
            NavSel::GlobalSettings => self.paths.save_global_config(&self.global_config),
            NavSel::Profile(_) => match &self.editing_preset_buf {
                Some(p) => self.paths.save_preset(p),
                None => return,
            },
            NavSel::GeneralSettings | NavSel::Game(_) => self.paths.save_game(&self.game_config),
        };
        if let Err(e) = result {
            eprintln!("ritz: failed to save: {e:#}");
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.ui(ctx);
    }
}

impl GuiApp {
    /// Render the editor into the given context (panels). Usable both as the whole
    /// window and embedded under another app's top bar.
    pub fn ui(&mut self, ctx: &egui::Context) {
        // Ctrl+R: hot-reload extensions, then the configs, from disk.
        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::R)) {
            self.reload_extensions();
            self.reload_configs();
        }

        let edit_resolution = self.resolve_for_editing();
        let game_resolution = self.resolve_for_game();
        let mut changed = false;

        self.render_title_bar(ctx);

        egui::SidePanel::left("nav")
            .exact_width(280.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(theme::PANEL))
            .show(ctx, |ui| {
                self.render_nav_panel(ui);
            });

        if !matches!(self.nav_sel, NavSel::GeneralSettings) {
        egui::SidePanel::left("ext_list")
            .exact_width(280.0)
            .resizable(false)
            .frame(egui::Frame::none().fill(theme::PANEL))
            .show(ctx, |ui| {
            egui::TopBottomPanel::top("ext_header")
                .show_separator_line(false)
                .frame(egui::Frame::none()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin { left: 16.0, right: 16.0, top: 14.0, bottom: 8.0 }))
                .show_inside(ui, |ui| {
                    ui.label(theme::header_label("Modules"));
                });
            // Toggles + Open Folder pinned to the bottom footer band.
            egui::TopBottomPanel::bottom("group_toggle")
                .exact_height(198.0)
                .show_separator_line(true)
                .frame(egui::Frame::none()
                    .fill(theme::PANEL2)
                    .inner_margin(egui::Margin::same(14.0)))
                .show_inside(ui, |ui| {
                    self.render_modules_footer(ui);
                });
            egui::CentralPanel::default()
                .frame(egui::Frame::none()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin { left: 8.0, right: 18.0, top: 0.0, bottom: 0.0 }))
                .show_inside(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .drag_to_scroll(self.general_config.touch_mode)
                        .show(ui, |ui| {
                            ui.add_space(4.0);
                            self.render_ext_tree(ui);
                        });
                });
        });
        } // end if !GeneralSettings

        let dynamic_preview = self.general_config.dynamic_preview;
        {
            let mut panel = egui::TopBottomPanel::bottom("preview")
                .show_separator_line(true)
                .frame(egui::Frame::none()
                    .fill(theme::PANEL2)
                    .inner_margin(egui::Margin::same(16.0)));
            if !dynamic_preview {
                panel = panel.exact_height(198.0);
            }
            panel.show(ctx, |ui| {
                // General Settings has no launch command — show repo link + credits
                // in the same box instead of the preview.
                if matches!(self.nav_sel, NavSel::GeneralSettings) {
                    self.render_about(ui);
                    return;
                }
                ui.label(theme::header_label("Launch command preview"));
                ui.add_space(8.0);
                let preview_res = if matches!(self.nav_sel, NavSel::Profile(_)) {
                    &edit_resolution
                } else {
                    &game_resolution
                };
                let preview =
                    context::assemble_launch(&self.cur_specs, preview_res, &self.game_command)
                        .map(|lc| lc.to_string())
                        .unwrap_or_else(|e| format!("<error: {e}>"));
                egui::Frame::none()
                    .fill(theme::FIELD)
                    .stroke(egui::Stroke::new(1.0, theme::BORDER))
                    .rounding(egui::Rounding::same(8.0))
                    .inner_margin(egui::Margin::symmetric(13.0, 11.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        if !dynamic_preview {
                            // Fill the remaining footer height; the panel's 16px bottom
                            // margin becomes the gap below, matching the left/right insets.
                            let box_h = ui.available_height();
                            ui.set_min_height((box_h - 22.0).max(0.0));
                        }
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, dynamic_preview])
                            .show(ui, |ui| {
                                ui.add(egui::Label::new(command_job(&preview)).wrap());
                            });
                    });
            });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if matches!(self.nav_sel, NavSel::GeneralSettings) {
                if self.render_general_settings_panel(ui) {
                    changed = true;
                }
                return;
            }

            let edit_ctx_label: Option<String> = match &self.nav_sel {
                NavSel::GlobalSettings => {
                    Some("Editing Global Settings — applies to all games".to_string())
                }
                NavSel::Profile(name) => Some(format!("Editing Profile: {name}")),
                NavSel::Game(_) => {
                    Some(format!("Editing Game: {}", self.game_config.game.name))
                }
                NavSel::GeneralSettings => None,
            };

            let Some(spec) = self.cur_specs.get(self.selected_ext).cloned() else {
                ui.label("No extensions apply to this game.");
                return;
            };

            // Detail header: title + version/author + description, bottom border.
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading(&spec.meta.name);
                ui.add_space(6.0);
                ui.label(egui::RichText::new(format!("v{}", spec.meta.version)).color(theme::FAINT));
                ui.label(egui::RichText::new(format!("by {}", spec.meta.author)).color(theme::FAINT));
            });
            if let Some(label) = edit_ctx_label {
                ui.add_space(2.0);
                ui.label(egui::RichText::new(label).color(theme::ACCENT).small());
            }
            if let Some(desc) = &spec.meta.description {
                ui.add_space(4.0);
                ui.label(egui::RichText::new(desc).color(theme::DIM));
            }
            ui.add_space(10.0);
            ui.separator();

            let ext_id = spec.id();
            let ext_res = edit_resolution.exts.get(&ext_id);

            egui::ScrollArea::vertical()
                // Fill the panel (don't shrink to content) so the scrollbar sits at
                // the far right while the content stays capped/left-aligned.
                .auto_shrink([false, false])
                .drag_to_scroll(self.general_config.touch_mode)
                .show(ui, |ui| {
                ui.vertical(|ui| {
                    // Fill the pane (less an 18px scrollbar gutter), but cap at ~743px
                    // unless "Use full UI width" is on. Using `min(..)` lets it scale
                    // DOWN on narrow windows instead of overflowing.
                    let avail = (ui.available_width() - 18.0).max(300.0);
                    let max_w = if self.general_config.full_width { avail } else { avail.min(743.0) };
                    ui.set_max_width(max_w);
                    if spec.backend.as_deref() == Some("custom-env") {
                        if self.render_custom_env_backend(ui, &spec) {
                            changed = true;
                        }
                    } else if spec.backend.as_deref() == Some("custom-args") {
                        if self.render_custom_args_backend(ui, &spec) {
                            changed = true;
                        }
                    } else {
                        let mut any_section = false;
                        for (section, fields) in &spec.ui {
                            let visible: Vec<&UiField> =
                                fields.iter().filter(|f| field_visible(f, ext_res)).collect();
                            if visible.is_empty() {
                                continue;
                            }
                            any_section = true;
                            ui.add_space(12.0);
                            ui.horizontal(|ui| {
                                ui.label(theme::section_label(section));
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    scope_legend(ui);
                                });
                            });
                            ui.add_space(6.0);
                            for field in visible {
                                let Some(res) = ext_res.and_then(|e| e.fields.get(&field.variable)) else {
                                    continue;
                                };
                                if self.render_field(ui, &spec, field, res) {
                                    changed = true;
                                }
                            }
                        }
                        if any_section {
                            ui.add_space(10.0);
                            ui.label(egui::RichText::new(
                                "Colours mark where each value resolves from — Global, Profile, or this Game. \
                                 Left-click a checkbox to toggle, right-click to reset to inherited.",
                            ).color(theme::FAINT).small());
                        }
                    }
                });
            });
        });

        if self.render_confirm_dialog(ctx) {
            changed = true;
        }

        if self.poll_detect(ctx) {
            changed = true;
        }

        if changed {
            self.persist();
        }
    }

    /// Render the pending-confirmation modal, if any. Returns true if a
    /// destructive action was carried out.
    fn render_confirm_dialog(&mut self, ctx: &egui::Context) -> bool {
        let Some(action) = self.confirm.clone() else {
            return false;
        };
        let (title, msg) = match &action {
            ConfirmAction::DeleteGame(_) => (
                "Delete Game",
                format!("Delete \"{}\"? This cannot be undone.", self.game_config.game.name),
            ),
            ConfirmAction::DeleteProfile(name) => (
                "Delete Profile",
                format!("Delete profile \"{name}\"? This cannot be undone."),
            ),
            ConfirmAction::ClearSettings => (
                "Clear Settings",
                "Reset every override in the current scope to inherited? This cannot be undone."
                    .to_string(),
            ),
            ConfirmAction::ReExportResources => (
                "Re-Export Modules and Plugins",
                "Do you really want to re-export the bundled modules and plugins? \
                 Any local edits to them will be lost."
                    .to_string(),
            ),
            ConfirmAction::ConfigCleanup => (
                "Config Cleanup",
                "Remove stored values for variables that no longer exist in any \
                 module? This cleans every game, profile, and the global config."
                    .to_string(),
            ),
        };

        // Modal backdrop: dim everything and swallow clicks meant for the panels
        // (which live on the Background layer) so only the dialog is interactive.
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("modal_backdrop"))
            .order(egui::Order::Middle)
            .fixed_pos(egui::Pos2::ZERO)
            .interactable(true)
            .show(ctx, |ui| {
                ui.painter().rect_filled(screen, 0.0, egui::Color32::from_black_alpha(160));
                ui.allocate_response(screen.size(), egui::Sense::click_and_drag());
            });

        let mut confirmed = false;
        let mut cancelled = false;
        egui::Window::new(title)
            .order(egui::Order::Foreground)
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(320.0);
                ui.add_space(4.0);
                ui.label(msg);
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.add(theme::danger_button("Confirm")).clicked() {
                        confirmed = true;
                    }
                    if ui.add(theme::secondary_button("Cancel")).clicked() {
                        cancelled = true;
                    }
                });
            });

        if confirmed {
            match action {
                ConfirmAction::DeleteGame(appid) => self.delete_game(&appid),
                ConfirmAction::DeleteProfile(name) => self.delete_profile(&name),
                ConfirmAction::ClearSettings => self.clear_current_settings(),
                ConfirmAction::ReExportResources => {
                    let _ = crate::resources::reexport(&self.paths);
                    self.reload_extensions();
                }
                ConfirmAction::ConfigCleanup => self.config_cleanup(),
            }
            self.confirm = None;
            return true;
        }
        if cancelled {
            self.confirm = None;
        }
        false
    }
}

impl GuiApp {
    /// The Graphite title bar: logo + wordmark, breadcrumb chip, and (in launch
    /// mode) the Cancel/Launch action pair.
    fn render_title_bar(&mut self, ctx: &egui::Context) {
        let frame = egui::Frame::none()
            .fill(theme::HEAD)
            .inner_margin(egui::Margin { left: 7.0, right: 18.0, top: 0.0, bottom: 0.0 });
        egui::TopBottomPanel::top("titlebar")
            .exact_height(61.0)
            .frame(frame)
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    let (logo, _) =
                        ui.allocate_exact_size(egui::vec2(48.0, 48.0), egui::Sense::hover());
                    if let Some(tex) = &self.logo {
                        ui.painter().image(
                            tex.id(),
                            logo,
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                            Color32::WHITE,
                        );
                    } else {
                        paint_logo(ui.painter(), logo);
                    }
                    ui.add_space(-3.5);
                    ui.label(
                        egui::RichText::new("Ritz").color(theme::ACCENT).font(
                            egui::FontId::new(28.8, egui::FontFamily::Name("bold".into())),
                        ),
                    );
                    ui.add_space(10.0);
                    let (div, _) =
                        ui.allocate_exact_size(egui::vec2(1.0, 18.0), egui::Sense::hover());
                    ui.painter().rect_filled(div, 0.0, theme::BORDER);
                    ui.add_space(10.0);
                    breadcrumb_chip(ui, &self.game_config.game.name);

                    if self.launch_mode {
                        let sep = icon_sep(self.general_config.mono_ui);
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                if ui.add(theme::primary_button(format!("\u{f04b}{sep}Launch Game"))).clicked()
                                {
                                    *self.outcome.lock().unwrap() = EditOutcome::Continue;
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                                ui.add_space(8.0);
                                if ui.add(theme::danger_button("Cancel Launch")).clicked() {
                                    *self.outcome.lock().unwrap() = EditOutcome::Cancel;
                                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                            },
                        );
                    }
                });
            });
    }

    /// The modules-column footer band: tree toggles + Open Folder.
    fn render_modules_footer(&mut self, ui: &mut egui::Ui) {
        styled_checkbox(ui, &mut self.group_by_author, "Group by Author");
        ui.add_space(6.0);
        styled_checkbox(ui, &mut self.show_inheritance, "Show Inheritance");

        let sep = icon_sep(self.general_config.mono_ui);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            // bottom_up: first added sits at the bottom, so Clear Settings ends up
            // below Open Folder.
            let w = ui.available_width();
            if ui.add_sized([w, 30.0], theme::danger_button("Clear Settings")).clicked() {
                self.confirm = Some(ConfirmAction::ClearSettings);
            }
            if ui
                .add_sized([w, 30.0], theme::secondary_button(format!("\u{f07b}{sep}Open Folder")))
                .clicked()
            {
                let _ = Command::new("xdg-open").arg(self.paths.games_dir()).spawn();
            }
        });
    }

    fn render_field(
        &mut self,
        ui: &mut egui::Ui,
        spec: &Extension,
        field: &UiField,
        res: &resolve::ResolvedField,
    ) -> bool {
        let state = tri_state(res);
        // A value set at the layer being edited resolves as `Provenance::Game`
        // (the layer is loaded as a fake game), so color it by the actual editing
        // scope — Game blue, Profile green, Global red — not always blue.
        let scope = match res.provenance {
            resolve::Provenance::Game => self.editing_scope_color(),
            p => theme::scope_color(p),
        };
        let mut changed = false;

        // A list field renders as its own growing slot card, not the one-row layout.
        if field.field_type == FieldType::MultiString {
            return self.render_multi_string_field(ui, field, scope);
        }

        // Row card: scope-tinted background, 8px rounding, with a 3px left bar.
        let tint = Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16);
        let inner = egui::Frame::none()
            .fill(tint)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin { left: 12.0, right: 5.0, top: 0.0, bottom: 0.0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    // Fixed row height; all content vertically centered, so the
                    // backdrop doesn't grow when taller controls appear.
                    ui.set_min_height(39.0);
                    ui.spacing_mut().item_spacing.x = 8.0;
                    let label = field.name.clone().unwrap_or_else(|| field.variable.clone());
                    let hover = field.description.clone().unwrap_or_default();
                    let left = ui.cursor().min.x;
                    if let Some(new) =
                        scope_checkbox(ui, &label, &hover, state, res.var.truthy, scope)
                    {
                        self.apply_tri(field, res, new);
                        changed = true;
                    }
                    // Reserve a fixed 260px for the checkbox+label; the control fills the rest.
                    let used = ui.cursor().min.x - left;
                    ui.add_space((260.0 - used).max(0.0));

                    // Always draw the editor for non-toggle fields so the value is
                    // visible; when not editable at this scope (Disabled/inherited)
                    // it's shown greyed-out and read-only, displaying the effective
                    // (inherited) value. Right-to-left so controls are right-bound.
                    if field.field_type != FieldType::Toggle {
                        let editable = state == Tri::Enabled;
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_enabled_ui(editable, |ui| {
                                changed |= self.render_value_editor(ui, spec, field, res, scope, editable);
                            });
                        });
                    }
                });
            });

        // Left scope bar: clip to the left 3px and draw the full rounded card
        // outline, so the bar follows the backdrop's corner curvature exactly.
        let r = inner.response.rect;
        let bar_clip = egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + 3.0, r.max.y));
        ui.painter()
            .with_clip_rect(bar_clip)
            .rect_filled(r, egui::Rounding::same(8.0), scope);
        ui.add_space(8.0);

        changed
    }

    /// Render a `multi_string` field: a scope-tinted card with the label and a
    /// growing list of entry rows (text box + delete) plus a `+ Add` button. The
    /// list is stored as a JSON array at the current scope.
    fn render_multi_string_field(&mut self, ui: &mut egui::Ui, field: &UiField, scope: Color32) -> bool {
        let Some(spec) = self.cur_specs.get(self.selected_ext) else { return false };
        let author = spec.meta.author.clone();
        let name = spec.meta.name.clone();
        let var = field.variable.clone();
        let label = field.name.clone().unwrap_or_else(|| var.clone());
        let hover = field.description.clone().unwrap_or_default();

        // Per scope+field working copy of the list, seeded from the stored array.
        let scope_tag = match &self.nav_sel {
            NavSel::GlobalSettings => "global".to_string(),
            NavSel::Profile(n) => format!("profile:{n}"),
            NavSel::Game(a) => format!("game:{a}"),
            NavSel::GeneralSettings => "general".to_string(),
        };
        let key = format!("{scope_tag}::{author}::{name}::{var}");
        let stored: Vec<String> = self
            .current_scope_value(&author, &name, &var)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let mut entries = self.multi_edit.remove(&key).unwrap_or(stored.clone());

        let tint = Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16);
        let row_h = ui.spacing().interact_size.y;
        let btn_w = row_h + ui.spacing().item_spacing.x;
        let mut to_delete: Option<usize> = None;

        let inner = egui::Frame::none()
            .fill(tint)
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin { left: 12.0, right: 8.0, top: 8.0, bottom: 8.0 })
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                let lab = ui.label(egui::RichText::new(&label).color(theme::TEXT).strong());
                if !hover.is_empty() {
                    lab.on_hover_text(&hover);
                }
                ui.add_space(4.0);
                let w = (ui.available_width() - btn_w - 4.0).max(40.0);
                for (i, entry) in entries.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        ui.add(
                            egui::TextEdit::singleline(entry)
                                .desired_width(w)
                                .hint_text(egui::RichText::new("command").color(theme::FAINT)),
                        );
                        if icon_button(ui, row_h, "\u{f467}", theme::COL_GLOBAL).clicked() {
                            to_delete = Some(i);
                        }
                    });
                }
                if ui.button("+ Add").clicked() {
                    entries.push(String::new());
                }
            });

        if let Some(i) = to_delete {
            entries.remove(i);
        }

        // Persist the non-empty entries when they differ from what's stored.
        let cleaned: Vec<String> = entries.iter().filter(|s| !s.trim().is_empty()).cloned().collect();
        let mut changed = false;
        if cleaned != stored {
            if cleaned.is_empty() {
                self.unset_current(field);
            } else {
                self.set_current(field, json!(cleaned));
            }
            changed = true;
        }

        // Left scope bar to match the other field cards.
        let r = inner.response.rect;
        let bar_clip = egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + 3.0, r.max.y));
        ui.painter()
            .with_clip_rect(bar_clip)
            .rect_filled(r, egui::Rounding::same(8.0), scope);
        ui.add_space(8.0);

        self.multi_edit.insert(key, entries);
        changed
    }

    /// The raw stored value for a variable at the scope currently being edited.
    fn current_scope_value(&self, author: &str, name: &str, var: &str) -> Option<&Value> {
        match &self.nav_sel {
            NavSel::GlobalSettings => self.global_config.get_value(author, name, var),
            NavSel::Profile(_) => self.editing_preset_buf.as_ref()?.get_value(author, name, var),
            _ => self.game_config.get_value(author, name, var),
        }
    }

    /// Apply a new tri-state to the current field's stored value.
    fn apply_tri(&mut self, field: &UiField, res: &resolve::ResolvedField, new: Tri) {
        match new {
            Tri::Unset => self.unset_current(field),
            Tri::Disabled => {
                let val = if field.field_type == FieldType::Toggle {
                    json!(false)
                } else {
                    Value::Null
                };
                self.set_current(field, val);
            }
            Tri::Enabled => {
                let val = match field.field_type {
                    FieldType::Toggle => json!(true),
                    FieldType::Selection => {
                        let cur = if res.var.value.is_empty() {
                            option_values(field)
                                .first()
                                .map(|(v, _)| v.clone())
                                .unwrap_or_default()
                        } else {
                            res.var.value.clone()
                        };
                        json!(cur)
                    }
                    FieldType::Integer => {
                        let v: f64 = res.var.value.parse().unwrap_or(number_range(field).0);
                        num_value(v, true)
                    }
                    FieldType::Float => {
                        let v: f64 = res.var.value.parse().unwrap_or(number_range(field).0);
                        num_value(v, false)
                    }
                    FieldType::String => json!(res.var.value.clone()),
                    // Edited via its own slot card, never through the tri-state checkbox.
                    FieldType::MultiString => json!([] as [String; 0]),
                };
                self.set_current(field, val);
            }
        }
    }

    /// Value editor for a non-toggle field. When `editable` is false the widgets
    /// are drawn read-only (the caller has disabled the ui) and just display the
    /// effective value, so no persistent edit buffer is touched.
    fn render_value_editor(
        &mut self,
        ui: &mut egui::Ui,
        spec: &Extension,
        field: &UiField,
        res: &resolve::ResolvedField,
        scope: Color32,
        editable: bool,
    ) -> bool {
        let mut changed = false;
        match field.field_type {
            FieldType::Selection => {
                let vals = option_values(field);
                let current = res.var.value.clone();
                let cur_label = vals
                    .iter()
                    .find(|(v, _)| *v == current)
                    .map(|(_, l)| l.clone())
                    .unwrap_or_else(|| current.clone());
                // egui draws the ComboBox button ~4px taller than the height it
                // reserves for layout (downward), so cross-axis centering lands it
                // low. Place it in an explicit rect instead, vertically centered in
                // the row band and right-aligned to the band edge (the row card's
                // 5px right margin gives the 5px gap to the card's visual edge).
                let band = ui.max_rect();
                let h = ui.spacing().interact_size.y + 4.0;
                let left = band.left() + 8.0;
                let right = band.right();
                // -2: the ComboBox button draws ~2px below the ui's top, so nudge
                // the placement up to land it centered in the band.
                let rect = egui::Rect::from_min_size(
                    egui::pos2(left, band.center().y - h / 2.0 - 2.0),
                    egui::vec2(right - left, h),
                );
                let mut picked = None;
                ui.allocate_new_ui(egui::UiBuilder::new().max_rect(rect), |ui| {
                    egui::ComboBox::from_id_salt(&field.variable)
                        .width(rect.width())
                        .selected_text(cur_label)
                        .show_ui(ui, |ui| {
                            for (value, label) in &vals {
                                if ui.selectable_label(*value == current, label).clicked() {
                                    picked = Some(value.clone());
                                }
                            }
                        });
                });
                if let Some(v) = picked {
                    self.set_current(field, json!(v));
                    changed = true;
                }
            }
            FieldType::Integer | FieldType::Float => {
                let integer = field.field_type == FieldType::Integer;
                let (min, max, step) = number_range(field);
                let v: f64 = res.var.value.parse().unwrap_or(min);
                if let Some(nv) = scope_slider(ui, v, min, max, step, integer, scope) {
                    self.set_current(field, num_value(nv, integer));
                    changed = true;
                }
            }
            FieldType::String => {
                // Right-to-left: the Detect button (if any) pins to the right, then
                // the text field fills the remaining width to its left.
                if editable && field.variable == "window_class" {
                    let remaining = self.detect.as_ref().and_then(|d| {
                        matches!(*d.result.lock().unwrap(), DetectStatus::Waiting).then(|| {
                            (3.0 - d.start.elapsed().as_secs_f32()).ceil().max(0.0) as u64
                        })
                    });
                    if let Some(secs) = remaining {
                        ui.add_enabled(false, egui::Button::new(format!("Detecting… {secs}")));
                    } else if ui
                        .button("Detect")
                        .on_hover_text("Focus the game window within 3 seconds")
                        .clicked()
                    {
                        self.start_detect(spec, field);
                    }
                }

                let tw = (ui.available_width() - 4.0).max(40.0);
                if editable {
                    let key = format!("{}::{}", spec.id(), field.variable);
                    let edited = {
                        let buf = self
                            .text_buffers
                            .entry(key)
                            .or_insert_with(|| res.var.value.clone());
                        ui.add(egui::TextEdit::singleline(buf).desired_width(tw))
                            .changed()
                            .then(|| buf.clone())
                    };
                    if let Some(v) = edited {
                        self.set_current(field, json!(v));
                        changed = true;
                    }
                } else {
                    // Read-only: show the effective value without a persistent buffer.
                    let mut shown = res.var.value.clone();
                    ui.add(egui::TextEdit::singleline(&mut shown).desired_width(tw));
                }
            }
            // Rendered by their own dedicated paths, not the inline value editor.
            FieldType::Toggle | FieldType::MultiString => {}
        }
        changed
    }

    /// Set a value for a field on the *currently active edit target*.
    fn set_current(&mut self, field: &UiField, value: Value) {
        let Some(spec) = self.cur_specs.get(self.selected_ext) else { return };
        let (a, n) = (spec.meta.author.clone(), spec.meta.name.clone());
        match &self.nav_sel.clone() {
            NavSel::GlobalSettings => {
                self.global_config.set_value(&a, &n, &field.variable, value);
            }
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf {
                    p.set_value(&a, &n, &field.variable, value);
                }
            }
            NavSel::Game(_) | NavSel::GeneralSettings => {
                self.game_config.set_value(&a, &n, &field.variable, value);
            }
        }
    }

    /// Begin a 3-second window-class detection for the given field.
    fn start_detect(&mut self, spec: &Extension, field: &UiField) {
        let result = Arc::new(Mutex::new(DetectStatus::Waiting));
        let handle = result.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            *handle.lock().unwrap() = DetectStatus::Done(detect_active_window_class());
        });
        self.detect = Some(Detect {
            result,
            start: Instant::now(),
            ext_id: spec.id(),
            author: spec.meta.author.clone(),
            name: spec.meta.name.clone(),
            var: field.variable.clone(),
        });
    }

    /// Apply a finished detection (if any). Returns true if the config changed.
    fn poll_detect(&mut self, ctx: &egui::Context) -> bool {
        let done = match &self.detect {
            Some(d) => match d.result.lock().unwrap().clone() {
                DetectStatus::Waiting => {
                    ctx.request_repaint_after(Duration::from_millis(200));
                    return false;
                }
                DetectStatus::Done(class) => Some((
                    d.ext_id.clone(),
                    d.author.clone(),
                    d.name.clone(),
                    d.var.clone(),
                    class,
                )),
            },
            None => None,
        };
        let Some((ext_id, author, name, var, class)) = done else {
            return false;
        };
        self.detect = None;
        // Redraw so the text box reflects the new value immediately.
        ctx.request_repaint();
        if let Some(c) = class {
            self.game_config.set_value(&author, &name, &var, json!(c.clone()));
            self.text_buffers.insert(format!("{ext_id}::{var}"), c);
            return true;
        }
        false
    }

    /// Remove an override for a field on the *currently active edit target* (reset to inherit).
    fn unset_current(&mut self, field: &UiField) {
        let Some(spec) = self.cur_specs.get(self.selected_ext) else { return };
        let (a, n) = (spec.meta.author.clone(), spec.meta.name.clone());
        self.text_buffers.remove(&format!("{}::{}", spec.id(), field.variable));
        match &self.nav_sel.clone() {
            NavSel::GlobalSettings => {
                self.global_config.unset_value(&a, &n, &field.variable);
            }
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf {
                    p.unset_value(&a, &n, &field.variable);
                }
            }
            NavSel::Game(_) | NavSel::GeneralSettings => {
                self.game_config.unset_value(&a, &n, &field.variable);
            }
        }
    }

    fn set_raw_var(&mut self, author: &str, name: &str, var: &str, value: Value) {
        match &self.nav_sel.clone() {
            NavSel::GlobalSettings => self.global_config.set_value(author, name, var, value),
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf { p.set_value(author, name, var, value); }
            }
            NavSel::Game(_) | NavSel::GeneralSettings => self.game_config.set_value(author, name, var, value),
        }
    }

    fn unset_raw_var(&mut self, author: &str, name: &str, var: &str) {
        match &self.nav_sel.clone() {
            NavSel::GlobalSettings => self.global_config.unset_value(author, name, var),
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf { p.unset_value(author, name, var); }
            }
            NavSel::Game(_) | NavSel::GeneralSettings => self.game_config.unset_value(author, name, var),
        }
    }

    fn render_custom_env_backend(&mut self, ui: &mut egui::Ui, spec: &Extension) -> bool {
        let mut changed = false;
        changed |= self.render_custom_env_section(ui, spec, "env_",      "Environment Variables");
        changed |= self.render_custom_env_section(ui, spec, "game_env_", "Game Environment Variables");
        changed
    }

    fn render_custom_env_section(
        &mut self,
        ui: &mut egui::Ui,
        spec: &Extension,
        prefix: &str,
        section_label: &str,
    ) -> bool {
        const MAX_SLOTS: usize = 16;
        let author  = spec.meta.author.clone();
        let name    = spec.meta.name.clone();
        let ext_id  = spec.id();

        // Collect active (slot, env_name, env_value) from the current edit layer.
        let active: Vec<(usize, String, String)> = {
            let modules: &AuthorsMap = match &self.nav_sel {
                NavSel::GlobalSettings => &self.global_config.modules,
                NavSel::Profile(_) => match self.editing_preset_buf.as_ref() {
                    Some(p) => &p.modules,
                    None    => return false,
                },
                _ => &self.game_config.config.modules.authors,
            };
            let vars = modules.get(&author).and_then(|e| e.get(&name));
            (1..=MAX_SLOTS).filter_map(|i| {
                let n = vars
                    .and_then(|m| m.get(&format!("{prefix}{i}_name")))
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)?;
                let v = vars
                    .and_then(|m| m.get(&format!("{prefix}{i}_value")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                Some((i, n, v))
            }).collect()
        };

        let used: std::collections::HashSet<usize> = active.iter().map(|(i, _, _)| *i).collect();

        // Slots with a live buffer but not yet committed to config (just added, focus not lost yet).
        let pending: Vec<usize> = (1..=MAX_SLOTS)
            .filter(|i| !used.contains(i)
                && self.text_buffers.contains_key(&format!("{ext_id}::{prefix}{i}")))
            .collect();

        ui.add_space(6.0);
        ui.label(egui::RichText::new(section_label).heading().size(15.0));
        ui.separator();

        let mut changed = false;
        let mut to_delete: Option<usize> = None;
        let mut to_cancel: Option<usize> = None;
        let mut commits: Vec<(usize, String)> = Vec::new();

        let row_h  = ui.spacing().interact_size.y;
        let btn_w  = row_h + ui.spacing().item_spacing.x;
        // Compute the field width ONCE: querying ui.available_width() per row makes
        // it creep wider with each row, so a fixed width keeps every slot identical.
        let w = (ui.available_width() - btn_w - 4.0).max(40.0);

        for (slot, cur_name, cur_value) in &active {
            let buf_key = format!("{ext_id}::{prefix}{slot}");
            let resp = {
                let buf = self.text_buffers.entry(buf_key.clone())
                    .or_insert_with(|| format!("{}={}", cur_name, cur_value));
                let mut r = None;
                ui.horizontal(|ui| {
                    r = Some(ui.add(egui::TextEdit::singleline(buf).desired_width(w).hint_text(egui::RichText::new("NAME=value").color(theme::FAINT))));
                    if icon_button(ui, row_h, "\u{f467}", theme::COL_GLOBAL).clicked() { to_delete = Some(*slot); }
                });
                r.unwrap()
            };
            if resp.lost_focus() {
                if let Some(b) = self.text_buffers.get(&buf_key) { commits.push((*slot, b.clone())); }
            }
        }

        for slot in &pending {
            let buf_key = format!("{ext_id}::{prefix}{slot}");
            let resp = {
                let buf = self.text_buffers.entry(buf_key.clone()).or_default();
                let mut r = None;
                ui.horizontal(|ui| {
                    r = Some(ui.add(egui::TextEdit::singleline(buf).desired_width(w).hint_text(egui::RichText::new("NAME=value").color(theme::FAINT))));
                    if icon_button(ui, row_h, "\u{f467}", theme::COL_GLOBAL).clicked() { to_cancel = Some(*slot); }
                });
                r.unwrap()
            };
            if resp.lost_focus() {
                if let Some(b) = self.text_buffers.get(&buf_key) { commits.push((*slot, b.clone())); }
            }
        }

        for (slot, buf) in commits {
            let buf_key = format!("{ext_id}::{prefix}{slot}");
            let (n_part, v_part) = buf.find('=')
                .map(|i| (buf[..i].trim().to_string(), buf[i+1..].to_string()))
                .unwrap_or_else(|| (buf.trim().to_string(), String::new()));
            if n_part.is_empty() {
                self.text_buffers.remove(&buf_key);
                self.unset_raw_var(&author, &name, &format!("{prefix}{slot}_name"));
                self.unset_raw_var(&author, &name, &format!("{prefix}{slot}_value"));
            } else {
                self.set_raw_var(&author, &name, &format!("{prefix}{slot}_name"), json!(n_part));
                self.set_raw_var(&author, &name, &format!("{prefix}{slot}_value"), json!(v_part));
            }
            changed = true;
        }

        if let Some(slot) = to_delete {
            self.text_buffers.remove(&format!("{ext_id}::{prefix}{slot}"));
            self.unset_raw_var(&author, &name, &format!("{prefix}{slot}_name"));
            self.unset_raw_var(&author, &name, &format!("{prefix}{slot}_value"));
            changed = true;
        }
        if let Some(slot) = to_cancel {
            self.text_buffers.remove(&format!("{ext_id}::{prefix}{slot}"));
        }

        let total = used.len() + pending.len();
        if total < MAX_SLOTS {
            ui.add_space(4.0);
            if ui.button("+ Add").clicked() {
                if let Some(next) = (1..=MAX_SLOTS).find(|i| {
                    !used.contains(i) && !self.text_buffers.contains_key(&format!("{ext_id}::{prefix}{i}"))
                }) {
                    self.text_buffers.insert(format!("{ext_id}::{prefix}{next}"), String::new());
                }
            }
        }

        changed
    }

    /// Value-only slot UI for the `custom-args` backend — a list of game launch
    /// arguments with add/remove rows (mirrors `render_custom_env_section`, but
    /// each slot stores the raw `arg_N` string directly, no `NAME=value` split).
    fn render_custom_args_backend(&mut self, ui: &mut egui::Ui, spec: &Extension) -> bool {
        const MAX_SLOTS: usize = 16;
        const PREFIX: &str = "arg_";
        let author = spec.meta.author.clone();
        let name = spec.meta.name.clone();
        let ext_id = spec.id();

        // Active slots: arg_N with a non-empty stored value, on the current layer.
        let active: Vec<(usize, String)> = {
            let modules: &AuthorsMap = match &self.nav_sel {
                NavSel::GlobalSettings => &self.global_config.modules,
                NavSel::Profile(_) => match self.editing_preset_buf.as_ref() {
                    Some(p) => &p.modules,
                    None => return false,
                },
                _ => &self.game_config.config.modules.authors,
            };
            let vars = modules.get(&author).and_then(|e| e.get(&name));
            (1..=MAX_SLOTS)
                .filter_map(|i| {
                    let v = vars
                        .and_then(|m| m.get(&format!("{PREFIX}{i}")))
                        .and_then(|v| v.as_str())
                        .filter(|s| !s.is_empty())
                        .map(str::to_string)?;
                    Some((i, v))
                })
                .collect()
        };

        let used: std::collections::HashSet<usize> = active.iter().map(|(i, _)| *i).collect();
        // Slots with a live buffer but not yet committed (just added).
        let pending: Vec<usize> = (1..=MAX_SLOTS)
            .filter(|i| {
                !used.contains(i) && self.text_buffers.contains_key(&format!("{ext_id}::{PREFIX}{i}"))
            })
            .collect();

        ui.add_space(6.0);
        ui.label(egui::RichText::new("Launch Arguments").heading().size(15.0));
        ui.separator();

        let mut changed = false;
        let mut to_delete: Option<usize> = None;
        let mut to_cancel: Option<usize> = None;
        let mut commits: Vec<(usize, String)> = Vec::new();

        let row_h = ui.spacing().interact_size.y;
        let btn_w = row_h + ui.spacing().item_spacing.x;
        // Compute the field width ONCE: querying ui.available_width() per row makes
        // it creep wider with each row, so a fixed width keeps every slot identical.
        let w = (ui.available_width() - btn_w - 4.0).max(40.0);

        let mut render_slot = |ui: &mut egui::Ui, buffers: &mut HashMap<String, String>, slot: usize, init: Option<&str>, cancel: bool| {
            let buf_key = format!("{ext_id}::{PREFIX}{slot}");
            let resp = {
                let buf = buffers
                    .entry(buf_key.clone())
                    .or_insert_with(|| init.unwrap_or("").to_string());
                let mut r = None;
                ui.horizontal(|ui| {
                    r = Some(ui.add(
                        egui::TextEdit::singleline(buf)
                            .desired_width(w)
                            .hint_text(egui::RichText::new("argument").color(theme::FAINT)),
                    ));
                    if icon_button(ui, row_h, "\u{f467}", theme::COL_GLOBAL).clicked() {
                        if cancel {
                            to_cancel = Some(slot);
                        } else {
                            to_delete = Some(slot);
                        }
                    }
                });
                r.unwrap()
            };
            if resp.lost_focus() {
                if let Some(b) = buffers.get(&buf_key) {
                    commits.push((slot, b.clone()));
                }
            }
        };

        for (slot, cur) in &active {
            render_slot(ui, &mut self.text_buffers, *slot, Some(cur), false);
        }
        for slot in &pending {
            render_slot(ui, &mut self.text_buffers, *slot, None, true);
        }

        for (slot, buf) in commits {
            let buf_key = format!("{ext_id}::{PREFIX}{slot}");
            let val = buf.trim().to_string();
            if val.is_empty() {
                self.text_buffers.remove(&buf_key);
                self.unset_raw_var(&author, &name, &format!("{PREFIX}{slot}"));
            } else {
                self.set_raw_var(&author, &name, &format!("{PREFIX}{slot}"), json!(val));
            }
            changed = true;
        }

        if let Some(slot) = to_delete {
            self.text_buffers.remove(&format!("{ext_id}::{PREFIX}{slot}"));
            self.unset_raw_var(&author, &name, &format!("{PREFIX}{slot}"));
            changed = true;
        }
        if let Some(slot) = to_cancel {
            self.text_buffers.remove(&format!("{ext_id}::{PREFIX}{slot}"));
        }

        let total = used.len() + pending.len();
        if total < MAX_SLOTS {
            ui.add_space(4.0);
            if ui.button("+ Add").clicked() {
                if let Some(next) = (1..=MAX_SLOTS).find(|i| {
                    !used.contains(i) && !self.text_buffers.contains_key(&format!("{ext_id}::{PREFIX}{i}"))
                }) {
                    self.text_buffers.insert(format!("{ext_id}::{PREFIX}{next}"), String::new());
                }
            }
        }

        changed
    }

    /// Render general app settings (splash timeout, default profile) in the central panel.
    /// Returns true if anything changed.
    fn render_general_settings_panel(&mut self, ui: &mut egui::Ui) -> bool {
        let mut changed = false;
        ui.heading("General Settings");
        ui.separator();
        ui.add_space(8.0);

        egui::ScrollArea::vertical()
            // Fill the panel (don't shrink to content) so the scrollbar sits at
            // the far right while the content stays capped/left-aligned.
            .auto_shrink([false, false])
            .drag_to_scroll(self.general_config.touch_mode)
            .show(ui, |ui| {
        ui.vertical(|ui| {
        // Cap the row width like the module panel: fill the pane (less an 18px
        // gutter), but cap at ~743px unless "Use full UI width" is on.
        let avail = (ui.available_width() - 18.0).max(300.0);
        let max_w = if self.general_config.full_width { avail } else { avail.min(743.0) };
        ui.set_max_width(max_w);

        // Splash timeout
        changed |= settings_value_row(
            ui,
            "Splash timeout (s)",
            "Seconds the launch splash counts down before launching.",
            |ui| {
                let mut v = self.general_config.splash_timeout_secs as f32;
                ui.spacing_mut().slider_width = 170.0;
                if ui.add(egui::Slider::new(&mut v, 1.0_f32..=30.0).integer()).changed() {
                    self.general_config.splash_timeout_secs = v as u64;
                    true
                } else {
                    false
                }
            },
        );

        // Default profile
        let presets = self.all_presets.clone();
        let cur = self.general_config.default_preset.clone().unwrap_or_default();
        let cur_label = if cur.is_empty() { "None".to_string() } else { cur.clone() };
        changed |= settings_value_row(
            ui,
            "Default profile",
            "Profile applied to games that don't select their own.",
            |ui| {
                let mut new_preset: Option<Option<String>> = None;
                egui::ComboBox::from_id_salt("general_default_preset_main")
                    .selected_text(cur_label)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(cur.is_empty(), "None").clicked() {
                            new_preset = Some(None);
                        }
                        for p in &presets {
                            if ui.selectable_label(cur == *p, p).clicked() {
                                new_preset = Some(Some(p.clone()));
                            }
                        }
                    });
                if let Some(p) = new_preset {
                    self.general_config.default_preset = p.clone();
                    self.default_preset = p;
                    true
                } else {
                    false
                }
            },
        );

        // Editor closing action
        changed |= settings_value_row(
            ui,
            "Editor closing action",
            "What closing the editor window does when it was opened to gate a launch.",
            |ui| {
                let launches = self.general_config.editor_close_launches;
                let cur_label = if launches { "Launch Game" } else { "Cancel Launch" };
                let mut ch = false;
                egui::ComboBox::from_id_salt("general_editor_close_action")
                    .selected_text(cur_label)
                    .show_ui(ui, |ui| {
                        if ui.selectable_label(launches, "Launch Game").clicked() && !launches {
                            self.general_config.editor_close_launches = true;
                            // Apply live: the window-close default follows the setting.
                            *self.outcome.lock().unwrap() = EditOutcome::Continue;
                            ch = true;
                        }
                        if ui.selectable_label(!launches, "Cancel Launch").clicked() && launches {
                            self.general_config.editor_close_launches = false;
                            *self.outcome.lock().unwrap() = EditOutcome::Cancel;
                            ch = true;
                        }
                    });
                ch
            },
        );

        // Boolean toggles
        let mut mono = self.general_config.mono_ui;
        if settings_toggle_row(
            ui,
            "Monospace UI font",
            "Render the UI in a monospaced font — if you prefer a more techy look.",
            &mut mono,
        ) {
            self.general_config.mono_ui = mono;
            crate::fonts::install(ui.ctx(), mono);
            changed = true;
        }

        let mut touch = self.general_config.touch_mode;
        if settings_toggle_row(
            ui,
            "Touch Mode",
            "Drag content to scroll (instead of only the scrollbar). Handy on touchscreens.",
            &mut touch,
        ) {
            self.general_config.touch_mode = touch;
            changed = true;
        }

        let mut full = self.general_config.full_width;
        if settings_toggle_row(
            ui,
            "Use full UI width",
            "Let the settings rows stretch across the whole pane instead of capping their width.",
            &mut full,
        ) {
            self.general_config.full_width = full;
            changed = true;
        }

        let mut dyn_prev = self.general_config.dynamic_preview;
        if settings_toggle_row(
            ui,
            "Dynamic Preview Size",
            "Auto-size the launch command preview to its content instead of a fixed height.",
            &mut dyn_prev,
        ) {
            self.general_config.dynamic_preview = dyn_prev;
            changed = true;
        }

        // Reload: [Reload Extensions] [Reload Configs]
        settings_value_row(ui, "Reload Actions", "", |ui| {
            if ui
                .add(theme::secondary_button("Reload Configs"))
                .on_hover_text("Re-read the current configuration from disk (part of Ctrl+R).")
                .clicked()
            {
                self.reload_configs();
            }
            if ui
                .add(theme::secondary_button("Reload Extensions"))
                .on_hover_text("Reload all modules from disk into this editor (part of Ctrl+R).")
                .clicked()
            {
                self.reload_extensions();
            }
        });

        // Maintenance: [Re-Export Modules and Plugins] [Clean Up Configs] — these
        // overwrite/strip files, so they use the red (destructive) button style.
        // right_to_left places the first-added button rightmost, so add in
        // reverse of the desired reading order.
        settings_value_row(ui, "Maintenance Actions", "", |ui| {
            if ui
                .add(theme::danger_button("Clean Up Configs"))
                .on_hover_text(
                    "Remove stored values for variables that no longer exist in any module \
                     (e.g. after an extension update).",
                )
                .clicked()
            {
                self.confirm = Some(ConfirmAction::ConfigCleanup);
            }
            if ui
                .add(theme::danger_button("Re-Export Modules and Plugins"))
                .on_hover_text(
                    "Restore the bundled modules and plugins, overwriting the files on disk.",
                )
                .clicked()
            {
                self.confirm = Some(ConfirmAction::ReExportResources);
            }
        });
        }); // end width-capped column
        }); // end scroll area

        if changed {
            let _ = self.paths.save_general(&self.general_config);
        }
        changed
    }

    /// Repo link + project credits, shown in the bottom box on General Settings.
    fn render_about(&self, ui: &mut egui::Ui) {
        ui.hyperlink_to(
            egui::RichText::new("\u{f09b}  ritz-game-launcher on GitHub")
                .color(theme::ACCENT)
                .size(15.0),
            "https://github.com/Ritze03/ritz-game-launcher",
        );
        ui.add_space(10.0);
        ui.label(theme::header_label("Credits"));
        ui.add_space(2.0);
        // Tighter rows for the credit list.
        ui.spacing_mut().item_spacing.y = 1.0;
        let credit = |ui: &mut egui::Ui, name: &str, url: &str, what: &str| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 5.0;
                ui.hyperlink_to(
                    egui::RichText::new(format!("\u{f09b}  {name} on GitHub"))
                        .color(theme::ACCENT)
                        .size(12.5),
                    url,
                );
                ui.label(egui::RichText::new(what).color(theme::FAINT).size(12.5));
            });
        };
        credit(ui, "geist-font", "https://github.com/vercel/geist-font", "— the UI font");
        credit(ui, "nerd-fonts", "https://github.com/ryanoasis/nerd-fonts", "— the icons");
        credit(ui, "hyprvibr", "https://github.com/devcexx/hyprvibr", "— their work");
    }

    fn refresh_presets(&mut self) {
        self.all_presets = self.paths.list_presets();
        self.preset_pins = self
            .all_presets
            .iter()
            .filter_map(|name| {
                self.paths
                    .load_preset(name)
                    .ok()
                    .flatten()
                    .and_then(|p| p.pin.map(|id| (name.clone(), id)))
            })
            .collect();
    }

    /// Set (or clear) the pin slot of the currently-edited profile, persist, and
    /// refresh the pin cache.
    fn set_parent_preset(&mut self, name: &str, parent: Option<String>) {
        if let Some(buf) = &mut self.editing_preset_buf {
            if buf.name == name {
                buf.parent = parent;
                let _ = self.paths.save_preset(buf);
            }
        }
    }

    fn set_current_preset_pin(&mut self, pin: Option<u8>) {
        if let Some(p) = &mut self.editing_preset_buf {
            p.pin = pin;
        }
        self.persist();
        self.refresh_presets();
    }

    /// Assign slot `new_id` to the currently-edited profile `name`. If another
    /// profile already holds `new_id`, the two swap slots.
    fn assign_pin(&mut self, name: &str, new_id: u8) {
        let old = self.editing_preset_buf.as_ref().and_then(|p| p.pin);
        let holder = self
            .preset_pins
            .iter()
            .find(|(n, id)| **id == new_id && n.as_str() != name)
            .map(|(n, _)| n.clone());
        if let Some(other) = holder {
            self.write_preset_pin(&other, old);
        }
        if let Some(p) = &mut self.editing_preset_buf {
            p.pin = Some(new_id);
        }
        self.persist();
        self.refresh_presets();
    }

    /// Set a preset file's pin slot directly (used for the swap partner).
    fn write_preset_pin(&self, name: &str, pin: Option<u8>) {
        if let Ok(Some(mut p)) = self.paths.load_preset(name) {
            p.pin = pin;
            let _ = self.paths.save_preset(&p);
        }
    }

    fn refresh_games(&mut self) {
        let mut games = Vec::new();
        if let Ok(entries) = std::fs::read_dir(self.paths.games_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                if let Ok(s) = std::fs::read_to_string(&path) {
                    if let Ok(gc) = serde_json::from_str::<GameConfig>(&s) {
                        games.push((gc.game.appid, gc.game.name));
                    }
                }
            }
        }
        games.sort();
        self.games = games;
    }

    fn rename_preset(&mut self, old_name: &str, new_name: &str) {
        if old_name == new_name || new_name.is_empty() {
            return;
        }
        if let Ok(Some(mut p)) = self.paths.load_preset(old_name) {
            p.name = new_name.to_string();
            let _ = self.paths.save_preset(&p);
            let _ = self.paths.delete_preset(old_name);
        }
        // Update any saved game config that references the old preset name.
        let game_files: Vec<_> = self.games.iter().map(|(id, _)| id.clone()).collect();
        for appid in game_files {
            if let Ok(Some(mut gc)) = self.paths.load_game(&appid) {
                if gc.config.modules.preset.as_deref() == Some(old_name) {
                    gc.config.modules.preset = Some(new_name.to_string());
                    let _ = self.paths.save_game(&gc);
                    if self.appid == appid {
                        self.game_config.config.modules.preset = Some(new_name.to_string());
                    }
                }
            }
        }
        if self.general_config.default_preset.as_deref() == Some(old_name) {
            self.general_config.default_preset = Some(new_name.to_string());
            let _ = self.paths.save_general(&self.general_config);
            self.default_preset = Some(new_name.to_string());
        }
        self.refresh_presets();
        self.editing_preset_buf = self.paths.load_preset(new_name).ok().flatten();
        self.nav_sel = NavSel::Profile(new_name.to_string());
        self.nav_name_buf = new_name.to_string();
    }
}

// ---- Navigator panel -------------------------------------------------------

impl GuiApp {
    fn render_nav_panel(&mut self, ui: &mut egui::Ui) {
        // Always show the bottom band (it's empty for General/Global Settings).
        egui::TopBottomPanel::bottom("nav_settings")
            .exact_height(198.0)
            .show_separator_line(true)
            .frame(egui::Frame::none()
                .fill(theme::PANEL2)
                .inner_margin(egui::Margin::same(14.0)))
            .show_inside(ui, |ui| {
                self.render_nav_settings(ui);
            });
        egui::TopBottomPanel::top("nav_header")
            .show_separator_line(false)
            .frame(egui::Frame::none()
                .fill(theme::PANEL)
                .inner_margin(egui::Margin { left: 16.0, right: 16.0, top: 14.0, bottom: 8.0 }))
            .show_inside(ui, |ui| {
                ui.label(theme::header_label("Profiles / Games"));
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::none()
                .fill(theme::PANEL)
                .inner_margin(egui::Margin { left: 8.0, right: 18.0, top: 0.0, bottom: 0.0 }))
            .show_inside(ui, |ui| {
                egui::ScrollArea::vertical()
                    .drag_to_scroll(self.general_config.touch_mode)
                    .show(ui, |ui| {
                        ui.add_space(4.0);
                        self.render_nav_tree(ui);
                    });
            });
    }

    fn render_nav_tree(&mut self, ui: &mut egui::Ui) {
        // Collect any nav action to apply after rendering (avoids borrow conflicts).
        enum NavAction {
            SelectGeneral,
            SelectGlobal,
            SelectProfile(String),
            SelectGame(String, String), // appid, name
            StartCreateProfile,
            StartCreateGame,
        }
        let mut action: Option<NavAction> = None;

        let sep = icon_sep(self.general_config.mono_ui);
        let gap = if self.general_config.mono_ui { 8.0 } else { 7.0 };
        let is_general = self.nav_sel == NavSel::GeneralSettings;
        if full_selectable(ui, is_general, egui::RichText::new(format!("\u{f013}{sep}General Settings")).color(theme::FAINT)).clicked() {
            action = Some(NavAction::SelectGeneral);
        }
        let is_global = self.nav_sel == NavSel::GlobalSettings;
        if full_selectable(ui, is_global, egui::RichText::new(format!("\u{f0ac}{sep}Global Settings")).color(COL_GLOBAL)).clicked() {
            action = Some(NavAction::SelectGlobal);
        }
        ui.add_space(2.0);

        // Profiles — highlight the assigned profile only when a Game is selected.
        let active_preset = match &self.nav_sel {
            NavSel::Game(_) => self.game_config.config.modules.preset.clone(),
            _ => None,
        };
        // Collect the parent chain as depth → arrow count.
        // Profile selected: direct parent=1, grandparent=2, …
        // Game selected:    active profile=1 (via is_active), its parent=2, …
        let profile_parents: std::collections::HashMap<String, usize> = match &self.nav_sel {
            NavSel::Profile(_) => {
                let mut depths = std::collections::HashMap::new();
                let mut cur = self.editing_preset_buf.as_ref().and_then(|p| p.parent.clone());
                let mut seen = std::collections::HashSet::new();
                let mut depth = 1usize;
                while let Some(pname) = cur {
                    if !seen.insert(pname.clone()) { break; }
                    depths.insert(pname.clone(), depth);
                    cur = self.paths.load_preset(&pname).ok().flatten().and_then(|p| p.parent);
                    depth += 1;
                }
                depths
            }
            NavSel::Game(_) => {
                let mut depths = std::collections::HashMap::new();
                if let Some(ref preset_name) = active_preset {
                    let mut cur = self.paths.load_preset(preset_name).ok().flatten().and_then(|p| p.parent);
                    let mut seen = std::collections::HashSet::new();
                    seen.insert(preset_name.clone());
                    let mut depth = 2usize;
                    while let Some(pname) = cur {
                        if !seen.insert(pname.clone()) { break; }
                        depths.insert(pname.clone(), depth);
                        cur = self.paths.load_preset(&pname).ok().flatten().and_then(|p| p.parent);
                        depth += 1;
                    }
                }
                depths
            }
            _ => std::collections::HashMap::new(),
        };
        // Ordered: pinned profiles (by slot id, with " [id]" suffix) first, then
        // the rest in list order.
        let presets = self.all_presets.clone();
        let mut pinned: Vec<(u8, String)> = presets
            .iter()
            .filter_map(|n| self.preset_pins.get(n).map(|id| (*id, n.clone())))
            .collect();
        pinned.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        // (name, display) — pinned show "<id> - <name>" and come first.
        // (name, pin id) — pinned first, shown as "[id] name" with a gray id.
        let ordered: Vec<(String, Option<u8>)> = pinned
            .iter()
            .map(|(id, n)| (n.clone(), Some(*id)))
            .chain(
                presets
                    .iter()
                    .filter(|n| !self.preset_pins.contains_key(*n))
                    .map(|n| (n.clone(), None)),
            )
            .collect();
        let id_col = egui::Color32::from_rgb(0x59, 0x5E, 0x66);
        egui::CollapsingHeader::new(egui::RichText::new("Profiles").color(COL_PROFILE))
            .default_open(true)
            .show(ui, |ui| {
                for (name, pin) in &ordered {
                    let is_sel = self.nav_sel == NavSel::Profile(name.clone());
                    let is_active = active_preset.as_deref() == Some(name.as_str());
                    let parent_depth = profile_parents.get(name.as_str()).copied();
                    let arrow_count = parent_depth.unwrap_or(if is_active { 1 } else { 0 });
                    let label: egui::WidgetText = if arrow_count > 0 {
                        let font_id = ui.style().text_styles[&egui::TextStyle::Body].clone();
                        let mut job = LayoutJob::default();
                        for i in 0..arrow_count {
                            if i > 0 {
                                job.append(" ", 0.0, TextFormat { font_id: font_id.clone(), color: theme::TEXT, ..Default::default() });
                            }
                            job.append(ICON_INHERIT, 0.0, TextFormat { font_id: font_id.clone(), color: COL_PROFILE, ..Default::default() });
                        }
                        job.append(name, gap, TextFormat { font_id, color: theme::TEXT, ..Default::default() });
                        job.into()
                    } else {
                        name.as_str().into()
                    };
                    let resp = full_selectable(ui, is_sel, label);
                    if resp.clicked() {
                        action = Some(NavAction::SelectProfile(name.clone()));
                    }
                    // Right-bound pin id.
                    if let Some(id) = pin {
                        let font = egui::TextStyle::Body.resolve(ui.style());
                        let r = resp.rect;
                        ui.painter().text(
                            egui::pos2(r.right() - 8.0, r.center().y),
                            egui::Align2::RIGHT_CENTER,
                            format!("[{id}]"),
                            font,
                            id_col,
                        );
                    }
                }
                if full_selectable(ui, false, egui::RichText::new(format!("\u{2b}{sep}Add profile")).color(theme::FAINT)).clicked() {
                    action = Some(NavAction::StartCreateProfile);
                }
            });
        ui.add_space(2.0);

        // Games
        let games = self.games.clone();
        egui::CollapsingHeader::new(egui::RichText::new("Games").color(COL_GAME))
            .default_open(true)
            .show(ui, |ui| {
                for (appid, name) in &games {
                    let is_sel = self.nav_sel == NavSel::Game(appid.clone());
                    if full_selectable(ui, is_sel, name.as_str()).clicked() && !is_sel {
                        action = Some(NavAction::SelectGame(appid.clone(), name.clone()));
                    }
                }
                if full_selectable(ui, false, egui::RichText::new(format!("\u{2b}{sep}Add game")).color(theme::FAINT)).clicked() {
                    action = Some(NavAction::StartCreateGame);
                }
            });

        // Apply action
        match action {
            None => {}
            Some(NavAction::SelectGeneral) => {
                self.nav_sel = NavSel::GeneralSettings;
                self.creating_profile = false;
                self.duplicating_preset = None;
                self.creating_game = false;
                self.text_buffers.clear();
            }
            Some(NavAction::SelectGlobal) => {
                self.nav_sel = NavSel::GlobalSettings;
                self.creating_profile = false;
                self.duplicating_preset = None;
                self.creating_game = false;
                self.text_buffers.clear();
            }
            Some(NavAction::SelectProfile(name)) => {
                self.editing_preset_buf = self.paths.load_preset(&name).ok().flatten();
                self.nav_name_buf = name.clone();
                self.nav_sel = NavSel::Profile(name);
                self.creating_profile = false;
                self.duplicating_preset = None;
                self.creating_game = false;
                self.text_buffers.clear();
            }
            Some(NavAction::SelectGame(appid, name)) => {
                self.switch_game(&appid, &name);
                self.nav_sel = NavSel::Game(appid);
                self.nav_name_buf = self.game_config.game.name.clone();
                self.editing_preset_buf = None;
                self.creating_profile = false;
                self.duplicating_preset = None;
                self.creating_game = false;
            }
            Some(NavAction::StartCreateProfile) => {
                self.creating_profile = true;
                self.creating_game = false;
                self.nav_name_buf = String::new();
                self.focus_nav_name = true;
            }
            Some(NavAction::StartCreateGame) => {
                self.creating_game = true;
                self.creating_profile = false;
                self.nav_name_buf = String::new();
                self.nav_appid_buf = String::new();
                self.focus_nav_name = true;
            }
        }
    }

    /// Reload the nav-panel buffers for the currently-selected entry (used after
    /// cancelling a create, when the buffers were cleared for the new entry).
    fn reselect_current(&mut self) {
        match self.nav_sel.clone() {
            NavSel::Profile(name) => {
                self.editing_preset_buf = self.paths.load_preset(&name).ok().flatten();
                self.nav_name_buf = name;
            }
            NavSel::Game(appid) => {
                self.nav_name_buf = self.game_config.game.name.clone();
                self.nav_appid_buf = appid;
            }
            NavSel::GeneralSettings | NavSel::GlobalSettings => {}
        }
    }

    fn render_nav_settings(&mut self, ui: &mut egui::Ui) {
        if self.creating_profile {
            let label = if self.duplicating_preset.is_some() { "Duplicate profile name:" } else { "New profile name:" };
            ui.label(label);
            let resp = ui.text_edit_singleline(&mut self.nav_name_buf);
            if self.focus_nav_name {
                resp.request_focus();
                self.focus_nav_name = false;
            }
            ui.horizontal(|ui| {
                let name = self.nav_name_buf.trim().to_string();
                if ui.button("Create").clicked() && !name.is_empty() {
                    let preset = if let Some(src) = self.duplicating_preset.take() {
                        Preset { name: name.clone(), modules: src.modules, pin: None, parent: None }
                    } else {
                        Preset { name: name.clone(), ..Default::default() }
                    };
                    let _ = self.paths.save_preset(&preset);
                    self.refresh_presets();
                    self.editing_preset_buf = Some(preset);
                    self.nav_sel = NavSel::Profile(name.clone());
                    self.nav_name_buf = name;
                    self.creating_profile = false;
                    self.text_buffers.clear();
                }
                if ui.button("Cancel").clicked() {
                    self.duplicating_preset = None;
                    self.creating_profile = false;
                    self.reselect_current();
                }
            });
            return;
        }

        if self.creating_game {
            ui.label("Game name:");
            let resp = ui.text_edit_singleline(&mut self.nav_name_buf);
            if self.focus_nav_name {
                resp.request_focus();
                self.focus_nav_name = false;
            }
            ui.label("AppID:");
            ui.text_edit_singleline(&mut self.nav_appid_buf);
            ui.horizontal(|ui| {
                let name = self.nav_name_buf.trim().to_string();
                let appid = self.nav_appid_buf.trim().to_string();
                if ui.button("Create").clicked() && !name.is_empty() && !appid.is_empty() {
                    let mut gc = GameConfig::new(appid.clone(), name.clone());
                    // Apply the configured default profile to the new game.
                    gc.config.modules.preset = self.general_config.default_preset.clone();
                    let _ = self.paths.save_game(&gc);
                    self.refresh_games();
                    self.switch_game(&appid, &name);
                    self.nav_sel = NavSel::Game(appid);
                    self.nav_name_buf = self.game_config.game.name.clone();
                    self.creating_game = false;
                }
                if ui.button("Cancel").clicked() {
                    self.creating_game = false;
                    self.reselect_current();
                }
            });
            return;
        }

        match self.nav_sel.clone() {
            NavSel::GeneralSettings | NavSel::GlobalSettings => {
                // Settings for these live in the central panel.
            }

            NavSel::Profile(name) => {
                ui.horizontal(|ui| {
                    footer_label(ui, "Name");
                    let w = ui.available_width();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.nav_name_buf).desired_width(w),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let new_name = self.nav_name_buf.trim().to_string();
                        if !new_name.is_empty() && new_name != name {
                            self.rename_preset(&name, &new_name);
                        }
                    }
                });

                // Pin to top (up to 10), with a 1–10 slot id.
                let cur_pin = self.editing_preset_buf.as_ref().and_then(|p| p.pin);
                ui.horizontal(|ui| {
                    footer_label(ui, "Pin");
                    let mut pinned = cur_pin.is_some();
                    if styled_checkbox(ui, &mut pinned, "").clicked() {
                        if pinned {
                            let used: std::collections::HashSet<u8> =
                                self.preset_pins.values().copied().collect();
                            if let Some(id) = (1..=10).find(|id| !used.contains(id)) {
                                self.set_current_preset_pin(Some(id));
                            }
                        } else {
                            self.set_current_preset_pin(None);
                        }
                    }
                    if let Some(cur) = cur_pin {
                        ui.add_space(8.0);
                        // All slots selectable; picking a taken one swaps IDs.
                        let mut new_id: Option<u8> = None;
                        egui::ComboBox::from_id_salt("profile_pin_id")
                            .selected_text(cur.to_string())
                            .width(52.0)
                            .show_ui(ui, |ui| {
                                for id in 1..=10u8 {
                                    if ui.selectable_label(id == cur, id.to_string()).clicked() {
                                        new_id = Some(id);
                                    }
                                }
                            });
                        if let Some(id) = new_id {
                            if id != cur {
                                self.assign_pin(&name, id);
                            }
                        }
                    }
                });

                // Parent profile — any profile except self; cycles allowed but shown in red.
                let cur_parent = self.editing_preset_buf.as_ref().and_then(|p| p.parent.clone());
                let sep = icon_sep(self.general_config.mono_ui);
                ui.horizontal(|ui| {
                    footer_label(ui, "Parent");
                    let display = cur_parent.as_deref().unwrap_or("None");
                    egui::ComboBox::from_id_salt("profile_parent_id")
                        .selected_text(display)
                        .width(ui.available_width())
                        .show_ui(ui, |ui| {
                            let is_none = cur_parent.is_none();
                            if ui.selectable_label(is_none, "None").clicked() && !is_none {
                                self.set_parent_preset(&name, None);
                            }
                            for pname in self.all_presets.clone() {
                                if pname == name { continue; }
                                let selected = cur_parent.as_deref() == Some(&pname);
                                let is_cyclic = chain_would_have_cycle(&self.paths, &name, &pname);
                                if is_cyclic {
                                    let row_h = ui.spacing().interact_size.y;
                                    let (rect, _) = ui.allocate_exact_size(
                                        egui::vec2(ui.available_width(), row_h),
                                        egui::Sense::hover(),
                                    );
                                    if ui.is_rect_visible(rect) {
                                        let c = theme::COL_GLOBAL;
                                        let fill = egui::Color32::from_rgb(0x3f, 0x21, 0x25);
                                        ui.painter().rect(rect, ui.visuals().widgets.inactive.rounding, fill, egui::Stroke::new(1.0, c));
                                        ui.painter().text(
                                            rect.left_center() + egui::vec2(ui.spacing().button_padding.x, 0.0),
                                            egui::Align2::LEFT_CENTER,
                                            format!("\u{ea6c}{sep}{pname}"),
                                            egui::TextStyle::Body.resolve(ui.style()),
                                            c,
                                        );
                                    }
                                } else if ui.selectable_label(selected, pname.as_str()).clicked() && !selected {
                                    self.set_parent_preset(&name, Some(pname));
                                }
                            }
                        });
                });

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    if ui
                        .add_sized([ui.available_width(), 30.0], theme::danger_button("Delete Profile"))
                        .clicked()
                    {
                        self.confirm = Some(ConfirmAction::DeleteProfile(name.clone()));
                    }
                    if ui
                        .add_sized([ui.available_width(), 30.0], egui::Button::new("Duplicate Profile"))
                        .clicked()
                    {
                        self.duplicating_preset = self.paths.load_preset(&name).ok().flatten();
                        self.creating_profile = true;
                        self.nav_name_buf = format!("{} Copy", name);
                        self.focus_nav_name = true;
                    }
                });
            }

            NavSel::Game(appid) => {
                // AppID — editable; pressing Enter renames the games/<appid>.json file.
                ui.horizontal(|ui| {
                    footer_label(ui, "AppID");
                    let w = ui.available_width();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.nav_appid_buf).desired_width(w),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let new_appid = self.nav_appid_buf.trim().to_string();
                        self.rename_game_appid(&appid, &new_appid);
                    }
                });
                // Name — Enter commits.
                ui.horizontal(|ui| {
                    footer_label(ui, "Name");
                    let w = ui.available_width();
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.nav_name_buf).desired_width(w),
                    );
                    if resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        let n = self.nav_name_buf.trim().to_string();
                        if !n.is_empty() && n != self.game_config.game.name {
                            self.game_config.game.name = n;
                            self.persist();
                            self.refresh_games();
                        }
                    }
                });

                let presets = self.all_presets.clone();
                let cur = self.game_config.config.modules.preset.clone().unwrap_or_default();
                let cur_label = if cur.is_empty() { "None".to_string() } else { cur.clone() };
                ui.horizontal(|ui| {
                    footer_label(ui, "Profile");
                    let w = ui.available_width();
                    let mut new_preset: Option<Option<String>> = None;
                    egui::ComboBox::from_id_salt("game_preset_sel")
                        .width(w)
                        .selected_text(cur_label)
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(cur.is_empty(), "None").clicked() {
                                new_preset = Some(None);
                            }
                            for p in &presets {
                                if ui.selectable_label(cur == *p, p).clicked() {
                                    new_preset = Some(Some(p.clone()));
                                }
                            }
                        });
                    if let Some(p) = new_preset {
                        self.game_config.config.modules.preset = p.clone();
                        self.preset = p
                            .as_deref()
                            .and_then(|n| self.paths.load_preset(n).ok().flatten());
                        self.persist();
                    }
                });

                ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
                    if ui
                        .add_sized([ui.available_width(), 30.0], theme::danger_button("Delete Game"))
                        .clicked()
                    {
                        self.confirm = Some(ConfirmAction::DeleteGame(appid.clone()));
                    }
                });
            }
        }
    }

    /// Rename a game's AppID: rewrite its config under the new filename, drop the
    /// old file, and re-point the UI. No-op on empty/unchanged/colliding ids.
    fn rename_game_appid(&mut self, old: &str, new: &str) {
        if new.is_empty() || new == old || self.games.iter().any(|(a, _)| a == new) {
            self.nav_appid_buf = old.to_string();
            return;
        }
        self.game_config.game.appid = new.to_string();
        let _ = self.paths.save_game(&self.game_config);
        let _ = self.paths.delete_game(old);
        let name = self.game_config.game.name.clone();
        self.refresh_games();
        self.switch_game(new, &name);
        self.nav_sel = NavSel::Game(new.to_string());
    }

    /// Delete a game and switch to the next available one (or a blank scratch).
    fn delete_game(&mut self, appid: &str) {
        let _ = self.paths.delete_game(appid);
        self.refresh_games();
        if let Some((new_appid, new_name)) = self.games.first().cloned() {
            self.switch_game(&new_appid, &new_name);
            self.nav_sel = NavSel::Game(new_appid);
            self.nav_name_buf = self.game_config.game.name.clone();
        } else {
            self.appid = "0".to_string();
            self.game_config = GameConfig::new("0", "Scratch");
            self.nav_sel = NavSel::GlobalSettings;
            self.nav_name_buf = String::new();
        }
    }

    /// Delete a profile and return to Global Settings. Any game referencing it
    /// falls back to no profile.
    fn delete_profile(&mut self, name: &str) {
        let _ = self.paths.delete_preset(name);
        if self.game_config.config.modules.preset.as_deref() == Some(name) {
            self.game_config.config.modules.preset = None;
            self.preset = None;
            self.persist();
        }
        self.refresh_presets();
        self.nav_sel = NavSel::GlobalSettings;
    }

    /// Clear all variable overrides in the current edit scope (game / profile /
    /// global), keeping the game's assigned profile.
    fn clear_current_settings(&mut self) {
        match &self.nav_sel {
            NavSel::GlobalSettings => {
                self.global_config.modules.clear();
                let _ = self.paths.save_global_config(&self.global_config);
            }
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf {
                    p.modules.clear();
                    let _ = self.paths.save_preset(p);
                }
            }
            NavSel::Game(_) | NavSel::GeneralSettings => {
                self.game_config.config.modules.authors.clear();
                self.persist();
            }
        }
        self.text_buffers.clear();
    }
}

/// Convert a Preset into a fake GameConfig so it can be passed as the "game" layer
/// to `resolve::resolve`, making its values show as Provenance::Game in the GUI.
fn preset_as_fake_game(preset: &Preset) -> GameConfig {
    let mut gc = GameConfig::new("__stub__", &preset.name);
    gc.config.modules.authors = preset.modules.clone();
    gc
}

/// One level of the module tree: named subfolders plus extensions at this level.
#[derive(Default)]
struct TreeNode {
    children: BTreeMap<String, TreeNode>,
    leaves: Vec<usize>,
}

impl TreeNode {
    fn insert(&mut self, comps: &[String], idx: usize) {
        match comps.split_first() {
            None => self.leaves.push(idx),
            Some((head, rest)) => self
                .children
                .entry(head.clone())
                .or_default()
                .insert(rest, idx),
        }
    }
}

impl GuiApp {
    /// Render the left-panel module tree. In folder mode it mirrors the nested
    /// `extensions/` layout; in author mode it groups by extension Author. In
    /// both modes the built-in (backend) modules are collected under a single
    /// "Built-In" node shown last.
    fn render_ext_tree(&mut self, ui: &mut egui::Ui) {
        // Build per-extension icon lists: each entry is (icon_glyph, color).
        // Inheritance from a lower layer → ICON_INHERIT; edit at current scope → ICON_EDIT.
        let resolution = self.resolve_for_editing();
        let mono = self.general_config.mono_ui;
        let nav_sel = &self.nav_sel;
        let global_modules = &self.global_config.modules;
        let game_profile_modules = self.preset.as_ref().map(|p| &p.modules);
        let editing_modules = self.editing_preset_buf.as_ref().map(|p| &p.modules);
        let game_modules = &self.game_config.config.modules.authors;
        // When editing a profile that has a parent, compute the merged parent chain
        // once so we can show inheritance indicators for the parent's values.
        let profile_parent_preset: Option<Preset> = if let NavSel::Profile(_) = &self.nav_sel {
            self.editing_preset_buf.as_ref()
                .and_then(|p| p.parent.as_ref())
                .map(|pname| collect_parent_chain(&self.paths, pname))
        } else {
            None
        };

        let icon_lists: Vec<Vec<(&'static str, Color32)>> = if !self.show_inheritance {
            vec![Vec::new(); self.cur_specs.len()]
        } else { self.cur_specs.iter().map(|spec| {
            // Variables the module still declares — a stored value for a removed
            // variable (e.g. an old `xkb_de`) must not count as "configured".
            let declared: std::collections::HashSet<&str> =
                spec.ui.values().flatten().map(|f| f.variable.as_str()).collect();
            // Fields whose Requires gate is currently closed don't contribute to
            // the launch command, so stored values behind them aren't "edited".
            let field_by_var: std::collections::HashMap<&str, &UiField> =
                spec.ui.values().flatten().map(|f| (f.variable.as_str(), f)).collect();
            let ext_res = resolution.exts.get(&spec.id());
            let has_in = |modules: &AuthorsMap| -> bool {
                modules.get(&spec.meta.author)
                    .and_then(|e| e.get(&spec.meta.name))
                    .map(|vars| vars.keys().any(|k| {
                        declared.contains(k.as_str())
                            && field_by_var.get(k.as_str())
                                .map_or(true, |f| field_visible(f, ext_res))
                    }))
                    .unwrap_or(false)
            };
            let mut icons: Vec<(&'static str, Color32)> = Vec::new();
            match nav_sel {
                NavSel::Game(_) => {
                    if has_in(global_modules)                                  { icons.push((ICON_INHERIT, COL_GLOBAL)); }
                    if game_profile_modules.map(|m| has_in(m)).unwrap_or(false) { icons.push((ICON_INHERIT, COL_PROFILE)); }
                    if has_in(game_modules)                                    { icons.push((ICON_EDIT,    COL_GAME)); }
                }
                NavSel::Profile(_) => {
                    if has_in(global_modules)                                              { icons.push((ICON_INHERIT, COL_GLOBAL)); }
                    if profile_parent_preset.as_ref().map(|p| has_in(&p.modules)).unwrap_or(false) { icons.push((ICON_INHERIT, COL_PROFILE)); }
                    if editing_modules.map(|m| has_in(m)).unwrap_or(false)                 { icons.push((ICON_EDIT,    COL_PROFILE)); }
                }
                NavSel::GlobalSettings => {
                    if has_in(global_modules) { icons.push((ICON_EDIT, COL_GLOBAL)); }
                }
                NavSel::GeneralSettings => {}
            }
            icons
        }).collect() };

        let mut builtins: Vec<usize> = Vec::new();
        let mut others: Vec<usize> = Vec::new();
        for (i, spec) in self.cur_specs.iter().enumerate() {
            if spec.backend.is_some() {
                builtins.push(i);
            } else {
                others.push(i);
            }
        }

        if self.group_by_author {
            let mut by_author: BTreeMap<String, Vec<usize>> = BTreeMap::new();
            for &i in &others {
                by_author
                    .entry(self.cur_specs[i].meta.author.clone())
                    .or_default()
                    .push(i);
            }
            for (author, items) in &by_author {
                egui::CollapsingHeader::new(egui::RichText::new(author).color(theme::ACCENT).strong())
                    .default_open(true)
                    .show(ui, |ui| {
                        for &i in items {
                            leaf(ui, i, &self.cur_specs, &icon_lists, &mut self.selected_ext, mono);
                        }
                    });
            }
        } else {
            let mut root = TreeNode::default();
            for &i in &others {
                let comps: Vec<String> = self.cur_dirs[i]
                    .iter()
                    .map(|c| c.to_string_lossy().into_owned())
                    .collect();
                root.insert(&comps, i);
            }
            render_node(ui, &root, &self.cur_specs, &self.cur_is_folder_ext, &icon_lists, &mut self.selected_ext, mono);
        }

        if !builtins.is_empty() {
            egui::CollapsingHeader::new(egui::RichText::new("Built-In").color(theme::ACCENT).strong())
                .default_open(true)
                .show(ui, |ui| {
                    for &i in &builtins {
                        leaf(ui, i, &self.cur_specs, &icon_lists, &mut self.selected_ext, mono);
                    }
                });
        }
    }
}

/// Render a tree node: subfolders (sorted) first, then extensions at this level.
/// A child folder whose single leaf has `is_folder_ext` set is collapsed into
/// the leaf directly (no CollapsingHeader wrapper).
fn render_node(ui: &mut egui::Ui, node: &TreeNode, specs: &[Extension], is_folder_ext: &[bool], icon_lists: &[Vec<(&'static str, Color32)>], selected: &mut usize, mono: bool) {
    let mut leaves: Vec<usize> = node.leaves.clone();
    for (name, child) in &node.children {
        // ponytail: skip the subfolder header when the folder IS the extension
        if child.children.is_empty() && child.leaves.len() == 1 && is_folder_ext.get(child.leaves[0]).copied().unwrap_or(false) {
            leaves.push(child.leaves[0]);
        } else {
            egui::CollapsingHeader::new(egui::RichText::new(name).color(theme::ACCENT).strong())
                .default_open(true)
                .show(ui, |ui| render_node(ui, child, specs, is_folder_ext, icon_lists, selected, mono));
        }
    }
    leaves.sort_by(|&a, &b| specs[a].meta.name.cmp(&specs[b].meta.name));
    for &i in &leaves {
        leaf(ui, i, specs, icon_lists, selected, mono);
    }
}

/// A fixed-width, faint, left-aligned footer label so the inputs/dropdown to its
/// right all start at the same x and fill to the same right edge. Allocates an
/// *exact* box (not content-sized) so every row's label column is identical.
fn footer_label(ui: &mut egui::Ui, text: &str) {
    const LABEL_W: f32 = 52.0;
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(LABEL_W, ui.spacing().interact_size.y), egui::Sense::hover());
    ui.painter().text(
        egui::pos2(rect.left(), rect.center().y),
        egui::Align2::LEFT_CENTER,
        text,
        egui::TextStyle::Body.resolve(ui.style()),
        theme::FAINT,
    );
}

/// Separator between a leading Nerd-Font icon and its text label. The mono space
/// glyph is wide, so one suffices; the proportional space is narrow, so use more.
fn icon_sep(mono: bool) -> &'static str {
    if mono {
        " "
    } else {
        "   "
    }
}

/// Build a monospace layout for the launch command, highlighting every
/// `%command%` token in the accent color.
pub(crate) fn command_job(cmd: &str) -> LayoutJob {
    let font = egui::FontId::monospace(11.5);
    let mut job = LayoutJob::default();
    let mut rest = cmd;
    while let Some(pos) = rest.find("%command%") {
        if pos > 0 {
            job.append(&rest[..pos], 0.0, TextFormat { font_id: font.clone(), color: theme::TEXT, ..Default::default() });
        }
        job.append("%command%", 0.0, TextFormat { font_id: font.clone(), color: theme::ACCENT, ..Default::default() });
        rest = &rest[pos + "%command%".len()..];
    }
    if !rest.is_empty() {
        job.append(rest, 0.0, TextFormat { font_id: font, color: theme::TEXT, ..Default::default() });
    }
    job
}

/// Decode the bundled logo PNG and upload it as a texture (painted sloth is the
/// fallback). Downsampled to ~64px for the small title-bar logo.
fn load_logo(ctx: &egui::Context) -> Option<egui::TextureHandle> {
    crate::image::load_logo_texture(ctx, crate::resources::logo_bytes(), 64, "ritz-logo")
}

/// Paint the stylized sloth logo mark inside `rect` (accent face + dark eye
/// patches + accent eye dots + a small nose). Fallback when the PNG fails.
fn paint_logo(p: &egui::Painter, rect: egui::Rect) {
    let c = rect.center();
    let r = rect.width().min(rect.height()) / 2.0;
    p.circle_filled(c, r, theme::ACCENT);
    let eye = egui::vec2(r * 0.42, -r * 0.12);
    let patch_r = r * 0.34;
    let dot_r = r * 0.13;
    for sx in [-1.0_f32, 1.0] {
        let center = c + egui::vec2(sx * eye.x, eye.y);
        p.circle_filled(center, patch_r, theme::HEAD);
        p.circle_filled(center, dot_r, theme::ACCENT);
    }
    p.circle_filled(c + egui::vec2(0.0, r * 0.4), r * 0.1, theme::HEAD);
}

/// A rounded "pill" chip (used for the title-bar breadcrumb) with the current
/// selection tint.
fn breadcrumb_chip(ui: &mut egui::Ui, text: &str) {
    let galley = ui.painter().layout_no_wrap(
        text.to_string(),
        egui::FontId::proportional(12.5),
        theme::TEXT,
    );
    let pad = egui::vec2(12.0, 4.0);
    let size = galley.size() + pad * 2.0;
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect(
        rect,
        egui::Rounding::same(rect.height() / 2.0),
        theme::SEL,
        egui::Stroke::new(1.0, theme::SELBD),
    );
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos, galley, theme::TEXT);
}

/// A selectable row that fills the full available width (left-aligned), so the
/// hover/selection highlight spans the whole column instead of hugging the text.
fn full_selectable(
    ui: &mut egui::Ui,
    selected: bool,
    text: impl Into<egui::WidgetText>,
) -> egui::Response {
    ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
        ui.selectable_label(selected, text)
    })
    .inner
}

/// A single selectable extension leaf.
/// Colored icons prefix the name: ICON_INHERIT for inherited layers, ICON_EDIT for
/// the current editing scope. The name itself stays the default UI color.
fn leaf(ui: &mut egui::Ui, i: usize, specs: &[Extension], icon_lists: &[Vec<(&'static str, Color32)>], selected: &mut usize, mono: bool) {
    let spec = &specs[i];
    // Gap (points) between a leading icon and following text — font-independent.
    let gap = if mono { 8.0 } else { 7.0 };
    let name = spec.meta.name.clone();
    // Configurable (backend) modules get a cog, painted right-bound below.
    let has_cog = spec.backend.is_some();

    let icons = icon_lists.get(i).map(|v| v.as_slice()).unwrap_or(&[]);
    let resp = if icons.is_empty() {
        full_selectable(ui, *selected == i, name)
    } else {
        let font_id = ui.style().text_styles[&egui::TextStyle::Body].clone();
        let mut job = LayoutJob::default();
        for (idx, (icon, color)) in icons.iter().enumerate() {
            let lead = if idx == 0 { 0.0 } else { gap };
            job.append(icon, lead, TextFormat { font_id: font_id.clone(), color: *color, ..Default::default() });
        }
        // Explicit white (not PLACEHOLDER): a selected selectable resolves PLACEHOLDER
        // to the selection stroke color, which would tint the name blue.
        job.append(&name, gap, TextFormat { font_id, color: theme::TEXT, ..Default::default() });
        full_selectable(ui, *selected == i, job)
    };
    if resp.clicked() {
        *selected = i;
    }

    if has_cog {
        let font = egui::TextStyle::Body.resolve(ui.style());
        let r = resp.rect;
        ui.painter().text(
            egui::pos2(r.right() - 11.0, r.center().y),
            egui::Align2::RIGHT_CENTER,
            "\u{f013}",
            font,
            theme::FAINT,
        );
    }
}

/// The scope swatches (● Default · ● Global · ● Profile · ● Game). Designed to
/// be called inside a right-to-left layout so it reads left→right.
fn scope_legend(ui: &mut egui::Ui) {
    for (label, col) in [
        ("Game", COL_GAME),
        ("Profile", COL_PROFILE),
        ("Global", COL_GLOBAL),
        ("Default", theme::COL_DEFAULT),
    ] {
        ui.label(egui::RichText::new(format!("\u{25cf} {label}")).color(col).small());
    }
}

/// A scope-colored slider with a drag/type spinner. Must be called inside a
/// right-to-left layout: the spinner anchors to the right (growing leftward for
/// long values) and the track fills the remaining width, so the row stays a
/// fixed width regardless of the value's digit count.
fn scope_slider(
    ui: &mut egui::Ui,
    value: f64,
    min: f64,
    max: f64,
    step: f64,
    integer: bool,
    scope: Color32,
) -> Option<f64> {
    let snap = |mut nv: f64| {
        if step > 0.0 {
            nv = (nv / step).round() * step;
        }
        if integer {
            nv = nv.round();
        }
        nv.clamp(min, max)
    };

    let mut out = None;

    // Spinner first → rightmost in the right-to-left layout.
    let mut v = value;
    let speed = if integer { 1.0 } else { step.max(0.01) };
    let mut dv = egui::DragValue::new(&mut v).range(min..=max).speed(speed);
    dv = if integer { dv.fixed_decimals(0) } else { dv.max_decimals(2) };
    // Fixed width (~5 digits) so it doesn't grow when entering edit mode (egui's
    // edit field would otherwise expand to fill the available width).
    let h = ui.spacing().interact_size.y;
    if ui.add_sized([64.0, h], dv).changed() {
        out = Some(snap(v));
    }

    // Gap between the spinner and the track.
    ui.add_space(10.0);
    // Track fills the remaining width to the left of the spinner.
    let track_w = ui.available_width().max(40.0);
    let (rect, resp) =
        ui.allocate_exact_size(egui::vec2(track_w, 18.0), egui::Sense::click_and_drag());
    if let Some(pos) = resp.interact_pointer_pos() {
        let t = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0) as f64;
        let nv = snap(min + t * (max - min));
        if nv != value {
            out = Some(nv);
        }
    }

    if ui.is_rect_visible(rect) {
        let shown = out.unwrap_or(value);
        let cy = rect.center().y;
        let track = egui::Rect::from_min_max(
            egui::pos2(rect.left(), cy - 3.0),
            egui::pos2(rect.right(), cy + 3.0),
        );
        let t = if max > min { ((shown - min) / (max - min)).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let fill_x = rect.left() + t * rect.width();
        let painter = ui.painter();
        painter.rect_filled(track, egui::Rounding::same(3.0), theme::BORDER);
        painter.rect_filled(
            egui::Rect::from_min_max(track.min, egui::pos2(fill_x, track.max.y)),
            egui::Rounding::same(3.0),
            scope,
        );
        painter.circle_filled(egui::pos2(fill_x, cy), 7.5, scope);
        painter.circle_stroke(egui::pos2(fill_x, cy), 7.5, egui::Stroke::new(2.5, theme::PANEL));
    }

    out
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tri {
    Unset,
    Enabled,
    Disabled,
}

fn tri_state(res: &resolve::ResolvedField) -> Tri {
    if res.provenance != Provenance::Game {
        Tri::Unset
    } else if res.enabled {
        Tri::Enabled
    } else {
        Tri::Disabled
    }
}

/// Paint an 18×18, 6px-rounded checkbox into `rect`: `color` fill + white check
/// when on, faint bordered box when off; both dimmed when `faded` (inherited).
fn paint_scope_box(painter: &egui::Painter, rect: egui::Rect, on: bool, faded: bool, color: Color32) {
    let round = egui::Rounding::same(6.0);
    let box_rect = rect.shrink(1.0);
    if on {
        let fill = if faded { color.gamma_multiply(0.35) } else { color };
        painter.rect_filled(box_rect, round, fill);
        let col = if faded { Color32::WHITE.gamma_multiply(0.6) } else { Color32::WHITE };
        let galley =
            painter.layout_no_wrap("\u{f00c}".to_owned(), egui::FontId::proportional(11.0), col);
        let ink_center = galley
            .rows
            .first()
            .and_then(|row| row.glyphs.first())
            .map(|g| g.pos.to_vec2() + g.uv_rect.offset + g.uv_rect.size * 0.5)
            .unwrap_or_else(|| galley.size() * 0.5);
        painter.galley(rect.center() - ink_center, galley, col);
    } else {
        // Inset the stroke by half its width so its outer edge sits exactly on
        // box_rect (draws inward only) — matching the filled box's size.
        painter.rect(box_rect.shrink(0.75), round, Color32::TRANSPARENT, egui::Stroke::new(1.5, theme::CHECK_OUTLINE));
    }
}

/// A square icon button that paints the glyph centered on its *ink* rect (Nerd
/// Font glyphs have asymmetric side bearings, so the default advance-box
/// centering looks off). Uses the standard widget background/hover.
fn icon_button(ui: &mut egui::Ui, size: f32, glyph: &str, color: Color32) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let visuals = *ui.style().interact(&resp);
        let painter = ui.painter();
        painter.rect(rect, visuals.rounding, visuals.weak_bg_fill, visuals.bg_stroke);
        let galley =
            painter.layout_no_wrap(glyph.to_owned(), egui::FontId::proportional(13.0), color);
        let ink_center = galley
            .rows
            .first()
            .and_then(|row| row.glyphs.first())
            .map(|g| g.pos.to_vec2() + g.uv_rect.offset + g.uv_rect.size * 0.5)
            .unwrap_or_else(|| galley.size() * 0.5);
        painter.galley(rect.center() - ink_center, galley, color);
    }
    resp
}

/// Lay out and sense a single clickable `[box] label` row: the whole rect (box +
/// gap + label) is one click target with a subtle hover background. `paint_box`
/// draws the 18×18 box. Returns the row's response.
fn checkbox_row(
    ui: &mut egui::Ui,
    label: &str,
    paint_box: impl FnOnce(&egui::Painter, egui::Rect),
) -> egui::Response {
    const BOX: f32 = 18.0;
    const GAP: f32 = 7.0;
    let font = egui::TextStyle::Body.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, theme::TEXT);
    let size = egui::vec2(BOX + GAP + galley.size().x, BOX.max(galley.size().y));
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if resp.hovered() {
            painter.rect_filled(rect.expand2(egui::vec2(4.0, 2.0)), egui::Rounding::same(6.0), theme::HOV);
        }
        let box_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.center().y - BOX / 2.0),
            egui::Vec2::splat(BOX),
        );
        paint_box(painter, box_rect);
        let tp = egui::pos2(box_rect.right() + GAP, rect.center().y - galley.size().y / 2.0);
        painter.galley(tp, galley, theme::TEXT);
    }
    resp
}

/// A plain on/off checkbox styled like the scope-row checkboxes. Box and label
/// form one clickable rect. Toggles `checked` on click; returns the response so
/// callers can attach a tooltip or check `.clicked()`.
/// Drop stored variables not present in `valid` (keyed by author/name/variable),
/// then prune any name/author maps left empty.
fn cleanup_modules(
    modules: &mut AuthorsMap,
    valid: &std::collections::HashSet<(String, String, String)>,
) {
    modules.retain(|author, names| {
        names.retain(|name, vars| {
            vars.retain(|var, _| {
                valid.contains(&(author.clone(), name.clone(), var.clone()))
            });
            !vars.is_empty()
        });
        !names.is_empty()
    });
}

/// The module-config row card, always in the neutral "Default" gray scheme:
/// gray-tinted backdrop, 8px rounding, fixed 39px height, 3px left scope bar.
/// Runs `content` inside the row and returns its value.
fn settings_card<R>(ui: &mut egui::Ui, content: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let scope = theme::COL_DEFAULT;
    let tint = Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16);
    let inner = egui::Frame::none()
        .fill(tint)
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin { left: 12.0, right: 11.0, top: 0.0, bottom: 0.0 })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.set_min_height(39.0);
                ui.spacing_mut().item_spacing.x = 8.0;
                content(ui)
            })
            .inner
        });
    let r = inner.response.rect;
    let bar_clip = egui::Rect::from_min_max(r.min, egui::pos2(r.min.x + 3.0, r.max.y));
    ui.painter()
        .with_clip_rect(bar_clip)
        .rect_filled(r, egui::Rounding::same(8.0), scope);
    ui.add_space(8.0);
    inner.inner
}

/// A settings row: `label` (with optional `hover`) on the left, `control`
/// right-bound — mirroring a module's non-toggle field row.
fn settings_value_row<R>(
    ui: &mut egui::Ui,
    label: &str,
    hover: &str,
    control: impl FnOnce(&mut egui::Ui) -> R,
) -> R {
    settings_card(ui, |ui| {
        let left = ui.cursor().min.x;
        let resp = ui.label(egui::RichText::new(label).color(theme::TEXT));
        if !hover.is_empty() {
            resp.on_hover_text(hover);
        }
        let used = ui.cursor().min.x - left;
        ui.add_space((260.0 - used).max(0.0));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), control)
            .inner
    })
}

/// A boolean settings row styled like a module Toggle: a gray scope-checkbox +
/// label filling the card. Toggles `value` and returns true if clicked.
fn settings_toggle_row(ui: &mut egui::Ui, label: &str, hover: &str, value: &mut bool) -> bool {
    let on = *value;
    let clicked = settings_card(ui, |ui| {
        let mut resp =
            checkbox_row(ui, label, |p, r| paint_scope_box(p, r, on, false, theme::COL_DEFAULT));
        if !hover.is_empty() {
            resp = resp.on_hover_text(hover);
        }
        resp.clicked()
    });
    if clicked {
        *value = !*value;
    }
    clicked
}

fn styled_checkbox(ui: &mut egui::Ui, checked: &mut bool, label: &str) -> egui::Response {
    let on = *checked;
    let resp = checkbox_row(ui, label, |p, r| paint_scope_box(p, r, on, false, COL_PROFILE));
    if resp.clicked() {
        *checked = !*checked;
    }
    resp
}

/// A mockup-style checkbox over the tri-state model, with the label as part of
/// the click target:
/// - **left-click** toggles Enabled ⇄ Disabled (flipping from the current
///   effective on/off, so the first click on an inherited value sets the opposite)
/// - **right-click** resets to Unset (inherit from a lower scope)
fn scope_checkbox(
    ui: &mut egui::Ui,
    label: &str,
    hover: &str,
    state: Tri,
    inherited_on: bool,
    scope: Color32,
) -> Option<Tri> {
    let on = match state {
        Tri::Enabled => true,
        Tri::Disabled => false,
        Tri::Unset => inherited_on,
    };
    let faded = state == Tri::Unset;
    let mut resp = checkbox_row(ui, label, |p, r| paint_scope_box(p, r, on, faded, scope));
    if !hover.is_empty() {
        resp = resp.on_hover_text(hover);
    }

    if resp.secondary_clicked() {
        Some(Tri::Unset)
    } else if resp.clicked() {
        Some(if on { Tri::Disabled } else { Tri::Enabled })
    } else {
        None
    }
}

/// Query the focused window's class via `hyprctl activewindow -j`. Prefers
/// `initialClass` (what the hypr-monctl plugin matches on), falling back to `class`.
fn detect_active_window_class() -> Option<String> {
    let out = Command::new("hyprctl")
        .args(["activewindow", "-j"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let v: Value = serde_json::from_slice(&out.stdout).ok()?;
    let pick = |key: &str| {
        v.get(key)
            .and_then(|x| x.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    };
    pick("initialClass").or_else(|| pick("class"))
}

fn num_value(v: f64, integer: bool) -> Value {
    if integer {
        json!(v as i64)
    } else {
        json!((v * 1000.0).round() / 1000.0)
    }
}

fn number_range(field: &UiField) -> (f64, f64, f64) {
    match &field.options {
        Some(OptionsSpec::Range { min, max, step }) => (*min, *max, step.unwrap_or(1.0)),
        _ => (0.0, 100.0, 1.0),
    }
}

fn option_values(field: &UiField) -> Vec<(String, String)> {
    let vals = match &field.options {
        Some(OptionsSpec::List(v)) => v.clone(),
        _ => Vec::new(),
    };
    let labels = field.display_options.clone();
    vals.iter()
        .enumerate()
        .map(|(i, v)| {
            let label = labels
                .as_ref()
                .and_then(|l| l.get(i))
                .cloned()
                .unwrap_or_else(|| v.clone());
            (v.clone(), label)
        })
        .collect()
}

fn field_visible(field: &UiField, ext_res: Option<&resolve::ExtResolution>) -> bool {
    condition::eval_opt(field.requires.as_deref(), &|n| {
        ext_res
            .and_then(|e| e.fields.get(n))
            .map(|f| f.var.truthy)
            .unwrap_or(false)
    })
    .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn cleanup_drops_undeclared_vars_and_prunes_empties() {
        // Ritze/Core has a live `kbd_layout` and a stale `xkb_de`; the whole
        // Other/Gone module is gone.
        let mut modules: AuthorsMap = serde_json::from_value(json!({
            "Ritze": { "Core": { "kbd_layout": "de", "xkb_de": true } },
            "Other": { "Gone": { "removed": true } }
        }))
        .unwrap();

        let valid: std::collections::HashSet<(String, String, String)> =
            [("Ritze", "Core", "kbd_layout")]
                .iter()
                .map(|(a, n, var)| (a.to_string(), n.to_string(), var.to_string()))
                .collect();

        cleanup_modules(&mut modules, &valid);

        // Stale var dropped, live one kept, the dead module pruned entirely.
        assert!(!modules["Ritze"]["Core"].contains_key("xkb_de"));
        assert_eq!(modules["Ritze"]["Core"]["kbd_layout"], json!("de"));
        assert!(!modules.contains_key("Other"));
    }
}
