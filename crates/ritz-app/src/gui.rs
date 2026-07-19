//! The settings manager GUI: renders each extension's UI tree dynamically,
//! honors `Requires` visibility, shows tri-state inheritance (default / preset /
//! game override) with reset-to-inherit, and previews the live launch command.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use egui::text::{LayoutJob, TextFormat};
use egui::Color32;
use ritz_core::condition;
use ritz_core::config::{self as core_config, AuthorsMap, GameConfig, GeneralConfig, InheritanceDisplayMode, Paths, Preset};
use ritz_core::extension::{self as core_extension, ExtensionLoadError};
use ritz_core::resolve::{self, Provenance, Resolution};
use ritz_core::schema::{
    apply_var_renames, ArgSpec, EnvBuilderEntry, EnvOp, EnvVarSpec, Extension, FieldType,
    OptionsSpec, UiField, WrapperBuilderEntry, WrapperSpec,
};
use indexmap::IndexMap;
use serde_json::{json, Value};

use crate::context::{self, chain_would_have_cycle, collect_parent_chain, collect_parent_presets, merge_modules, AppContext};
use crate::icon_center::IconCenterCache;

use crate::theme::{self, COL_GAME, COL_GLOBAL, COL_PROFILE, ICON_DIRTY, ICON_EDIT, ICON_INHERIT};

/// Width of the app's left column, in points.
///
/// Named because three places have to agree on it: the `SidePanel::left("nav")`
/// that *is* IDE Mode's column, IDE Mode's 50/50 split (which sizes itself from
/// "everything to the right of the nav"), and Config Mode's
/// `SidePanel::left("ext_list")`. A literal in any of them would let them drift.
///
/// *Why `ext_list` shares it (issue #15):* an earlier note here called `ext_list`
/// "a separate column with its own reasons" and left it on its own `280.0`
/// literal. That was wrong — the two are the same affordance, the left column of
/// their respective modes, and are meant to stay the same width, so nothing
/// should let a tweak to one silently desync the other.
const NAV_W: f32 = 280.0;

/// Height of IDE Mode's module-header band, in points.
///
/// Derived, not eyeballed — term by term, top to bottom:
///
/// | term   | what it is                                                       |
/// |--------|------------------------------------------------------------------|
/// | `8.0`  | the frame's top inner margin; see *Why 8/12 and not 14/10 (unequal on purpose)* |
/// | `27.0` | the name/version/author + button row — **button-driven**, not `interact_size.y`; see [`IDE_HEADER_ROW_H`] |
/// | `7.0`  | `item_spacing.y` (`theme.rs`) — egui's gap between those two rows |
/// | `17.0` | [`IDE_HEADER_DESC_H`], the one-line description slot             |
/// | `11.0` | the frame's bottom inner margin; see *Why 8/12 and not 14/10 (unequal on purpose)* |
///
/// Sum: `8 + 27 + 7 + 17 + 11 = 70.0`.
///
/// *Why the band holds no status lines* (2026-07-19, issue #26, revised same
/// day): for one commit (`7f6109c`) it did — the draft's status messages were
/// hoisted out of the editor body into a fixed slot here, sized for the
/// four-line worst case, which put `IDE_HEADER_H` at `134.0`. That was rejected
/// on sight: *"It's way too large."* Only the first status line is ever normally
/// present, so three of the four reserved rows — ~42pt, a third of the band —
/// were permanently empty. Shrinking the reservation to one line and letting the
/// band grow was considered and dropped in favour of the better answer the user
/// pointed at: *"We could move it into the diagnostic view?"*
///
/// **So the status lines now render in the diagnostics band**
/// ([`render_ide_diagnostics_band`]) instead. That band is already
/// `exact_height(198.0)`, always visible, and already contains a *scrolling*
/// list — so any number of messages costs zero height and nothing reflows, which
/// is strictly better than either the fixed reservation (dead space) or a
/// growing band (reflow mid-keystroke). It is also where they belong on the
/// merits: a `validate` error, a section-name collision and an unparseable
/// `Requires` **are** diagnostics; they only ever sat in this band because
/// `7f6109c` put them here. See [`ide_diagnostic_entries`] for the assembled
/// list and [`DiagSeverity`] for the info/warning/error vocabulary that carries
/// them.
///
/// This band is therefore back to exactly what it was before `7f6109c` — the
/// name/version/author row and the description — at the same `70.0` it has
/// always been, so nothing downstream reflows either way.
///
/// The old term-by-term sum,
/// `14 + 23 + 7 + 16 + 10`, also summed to 70 on paper — but the `23` and `16`
/// terms were merely the *documented* derivation, not what egui actually laid
/// out at runtime. The heading row is button-driven (see [`IDE_HEADER_ROW_H`])
/// and really measured `27`, four points more than the doc claimed; the
/// description slot really was allocated at the old `IDE_HEADER_DESC_H` (`16`),
/// so that term *was* load-bearing, just one point short of its own galley. Real
/// layout therefore came out to `14 + 27 + 7 + 16 + 10 = 74`, four points past
/// `exact_height`'s 70 — see the 2026-07-19 fix note below for what absorbed the
/// difference.
///
/// *Why two rows and not one* (2026-07-19): the one-row band (`14 + 23 + 10 = 47`)
/// read as a cramped strip next to Config mode's module header. That header
/// (`GuiApp::render_module_detail_header`) has **no** fixed height — it is
/// natural-sized — but its structure is fixed: `6` space, the same button-driven
/// heading row, an edit-context line, the description, `10` space, a separator.
/// This band reproduces it minus the two parts it does not own: the edit-context
/// line (IDE Mode has no edit scope to name — the tree shows the selection) and
/// the trailing separator (the panel's own `show_separator_line` draws that).
/// What is left is heading row + description, which is the 70pt here.
///
/// *Why 14 and not 6, and why the left margin also became 14* (2026-07-19):
/// same-day fix to Config mode's module-detail header found its top inset (the
/// `CentralPanel`'s 8px default margin + a 6px `add_space`) came out to 14
/// against an unrelated 8px left margin — visibly lopsided, corner-unsquare
/// (`GuiApp::render_module_detail_header`'s own doc comment has the fix).
/// Rather than reproduce that exact 8+6 split here too, this band's frame
/// margin folded the two into one literal, `top: 14.0, left: 14.0`. That literal
/// is what this same-day fix below corrects — read on.
///
/// *Why 8/12 and not 14/10 (unequal on purpose)* (2026-07-19): real layout
/// (`74`, previous paragraph) overran `exact_height` (`70`) by 4pt. egui clips
/// the frame *fill* to `exact_height` but returns the frame's full, larger rect
/// and advances the cursor from there — so nothing painted the resulting
/// `70..74` strip, and the framebuffer clear colour showed through beneath the
/// separator hline as a black bar. Fixing the row to its real `27.0` and the
/// description slot to its real `17.0` — correct on their own terms, see
/// [`IDE_HEADER_ROW_H`] and [`IDE_HEADER_DESC_H`] — raises the honest sum to
/// `14 + 27 + 7 + 17 + 10 = 75`: *worse* by one more point than the bug, because
/// the old `16` slot was under-sized too. The margins had to absorb all 5 of
/// those points (`75 − 70`) to keep `IDE_HEADER_H` at 70 and avoid reflowing the
/// editor/preview columns: top `14 → 8` (−6), bottom `10 → 11` (+1), net −5.
/// Check the arithmetic: `8 + 27 + 7 + 17 + 11 = 70`. ✓
///
/// The margins are **not** made equal (8 ≠ 12) because the visual gap is not the
/// margin number — it also depends on font ascent/cap-height above the heading
/// and ascent below the last text row:
/// `top_ink = top_margin + (row_h − heading_galley)/2 + (ascent₁₉ − cap₁₉)`,
/// `bottom_ink = bottom_margin + (slot_h − ink_bottom_of_last_row)`. Measured,
/// these come out equal (within 0.4pt: top 14.61, left 14.72, bottom ≈15) only
/// when the bottom margin runs ~3pt over the top one. **Do not "simplify" these
/// back to equal numbers** — that reintroduces the top/bottom asymmetry this fix
/// removed. The bottom margin still lands close to Config's `add_space(10.0)`
/// gap between its description and separator, which is the number this term
/// originally tried to reproduce.
///
/// *Why the bottom margin is 11 and not 12* (2026-07-19, issue #26): `7f6109c`
/// took it to `12` for one commit, because the band's last text row was then an
/// 11pt `Small` status line rather than the 13pt `Body` description, and the two
/// sit differently inside their slots — measured headlessly against the real
/// bundled font (same harness as [`IDE_HEADER_ROW_H`]), a cap glyph's ink bottom
/// is `13.0` in the description's `17.0` slot (4pt of slack below it) but `11.0`
/// in a status line's `14.0` slot (3pt), so the margin gave a point back to keep
/// the optical distance the same. With the status lines gone to the diagnostics
/// band the description is the last row again, so the extra point goes back too.
/// **The measurement is kept here on purpose:** if anything 11pt-sized is ever
/// added as the band's last row, this is the point that has to move again.
///
/// *Why `exact_height` and not auto-sizing:* the band spans the editor **and**
/// preview columns, so any height change here reflows half the window — the
/// hazard that ultimately sent the status lines to the diagnostics band rather
/// than letting this band grow. Pinning it also keeps the layout still on the
/// frames where
/// [`GuiApp::editor_header_info`] returns `None` (a module switch, before
/// `ensure_draft` catches up) — an auto-sized band would collapse to nothing and
/// snap back, which reads as a flicker.
const IDE_HEADER_H: f32 = IDE_HEADER_MARGIN.top
    + IDE_HEADER_ROW_H
    + 7.0
    + IDE_HEADER_DESC_H
    + IDE_HEADER_MARGIN.bottom;

/// The `ide_module_header` panel frame's inner margin.
///
/// *Why a shared constant and not a literal at the panel* (2026-07-19, issue
/// #26): the top and bottom terms are two of the seven that make up
/// [`IDE_HEADER_H`], and `IDE_HEADER_H` is what an `exact_height` panel is sized
/// from — so a margin edit that does not reach the sum silently reintroduces the
/// black bar of `1142dd8`. Written as literals in both places, that is a
/// one-character mistake with no compile error and no test failure (verified:
/// changing the panel's `bottom` alone left every test green). Derived from one
/// constant, the sum cannot fall out of step with the frame it describes, and
/// `ide_header_content_is_exactly_ide_header_h` measures the real thing rather
/// than a copy of it.
///
/// See [`IDE_HEADER_H`]'s *Why 8/12 and not 14/10 (unequal on purpose)* for why
/// top and bottom are deliberately different numbers, and why `left` is 14 while
/// `right` is 16.
const IDE_HEADER_MARGIN: egui::Margin = egui::Margin {
    left: 14.0,
    right: 16.0,
    top: 8.0,
    bottom: 11.0,
};

/// Height of the header band's first row (name/version/author + action buttons),
/// in points.
///
/// **Button-driven, not `interact_size.y` (`theme.rs`'s `23.0`)** — the row is
/// `max(interact_size.y, galley.y + 2*button_padding.y)`
/// (`egui::Button`'s own sizing, `egui-0.29.1/src/widgets/button.rs`), and the
/// Fork button renders on every path through this row (including bundled
/// modules in IDE mode — see [`render_editor_header_row`]'s doc comment), so the
/// button term always applies. With this crate's `theme.rs` numbers —
/// `button_padding = (9.0, 5.0)` and the `Button`/`Body` text style at 13pt
/// Geist Mono, whose galley measures `17.0` (`13.0 * 1.3` row height, rounded up)
/// — that's `max(23.0, 17.0 + 10.0) = 27.0`. `interact_size.y` never wins here.
/// Independently confirmed 2026-07-19 with a headless `egui::Context` run against
/// the real bundled font and `theme::apply`: `secondary_button("Fork")` measured
/// exactly `27.0`pt tall, and the Body galley measured `16.9`pt (→ 17.0).
///
/// *Why this needs its own named constant:* every earlier derivation of
/// [`IDE_HEADER_H`] assumed `23.0` from `interact_size.y` alone, missing that a
/// `Button`'s own padded galley can win the `max`. That silent 4pt gap is what
/// caused the header content to lay out taller than the `exact_height` band
/// clipped its fill to — a black bar where nothing painted the difference. Naming
/// the number stops the next person from re-deriving `interact_size.y` and
/// reintroducing the bug.
const IDE_HEADER_ROW_H: f32 = 27.0;

/// Height of the header band's description slot, in points.
///
/// One `Body` (13pt) text row. Geist Mono's row height is `size * 1.3`, so
/// `13.0 * 1.3 = 16.9`, rounded up to a whole point: `17.0` — **not** `16.0`; the
/// slot used to be a point shorter than its own text (confirmed 2026-07-19 with
/// the same headless font measurement noted on [`IDE_HEADER_ROW_H`]: the Body
/// galley measures `16.9`pt). The slot is **always** allocated at exactly this
/// height — see [`render_editor_header_description`] for why that is the whole
/// point of it.
const IDE_HEADER_DESC_H: f32 = 17.0;

#[derive(Debug, Clone, PartialEq)]
enum NavSel {
    GeneralSettings, // splash timeout, default preset — shown in central panel
    GlobalSettings,  // extension-variable global overrides — shown in central panel (ext editor)
    Profile(String),
    Game(String), // appid
}

/// Which top-level area of the window is showing.
///
/// *Why this is a separate axis and not another [`NavSel`] variant:* `NavSel`
/// answers "which config scope am I editing", `Mode` answers "which shape is the
/// window in". Folding IDE Mode into `NavSel` would force every exhaustive
/// `match self.nav_sel` in this file to grow an arm that has nothing to say
/// about scopes. The two are fully orthogonal (S4b): entering or leaving IDE
/// Mode never touches `nav_sel` at all — "which module has focus" is carried by
/// [`GuiApp::focused_module`] instead, a plain field nothing else aliases. Pre-S4b
/// `Mode::Ide` ran with the now-deleted `nav_sel == NavSel::ModuleEditor(_)` as
/// its own "which module is open" carrier, which is what made the two axes
/// collide in the first place.
///
/// Deliberately **not persisted** — IDE Mode is a workbench you enter for a
/// session, not a preference. Reopening the settings window always lands in
/// `Config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// The classic three-column config editor (nav | modules | fields).
    Config,
    /// The module-authoring workbench (module tree | manifest editor | preview).
    Ide,
}

/// Where a module-field write lands: the real config scope `nav_sel` names, or
/// the IDE preview's throw-away scratch config.
///
/// *Why an orthogonal flag and not a `NavSel` variant* (S5a, corrected S4b): even
/// before S4b deleted `NavSel::ModuleEditor`, `nav_sel` could not distinguish
/// "editor writing config" from "preview writing scratch" — both IDE columns ran
/// under the *same* `nav_sel` value. Since S4b, `nav_sel` isn't touched by IDE
/// Mode at all (see [`Mode`]'s doc comment), so it has even less to say about
/// which column is rendering. Only a second, independent axis can tell editor
/// writes from preview writes apart — exactly the same reasoning that made
/// [`Mode`] a separate axis from [`NavSel`].
///
/// Set to [`WriteTarget::Preview`] for the duration of one
/// `render_module_settings_body` call and nothing else; see
/// [`GuiApp::with_preview_writes`], which is the only place that flips it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteTarget {
    /// Write to whichever config store `nav_sel` names (global / profile / game).
    Scope,
    /// Write to `GuiApp::preview_config`, which is never persisted.
    Preview,
}

/// A click on one of the three rows in the nav's GENERAL category box. Collected
/// during render and applied afterwards (the render closure already holds
/// `&mut self`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NavCategory {
    GeneralSettings,
    Ide,
    GamesProfiles,
}

/// What happens *after* the user confirms a [`ConfirmAction::DiscardEdits`]
/// prompt — i.e. what they were trying to do when the unsaved-work guard stopped
/// them.
///
/// *Why one payload enum and not three `ConfirmAction` variants* (2026-07-19,
/// issue #33): all three render the identical dialog — same title ("Discard
/// Changes"), same message listing the affected modules, same buttons — and
/// differ only in the follow-up. Three variants would triple the message arm in
/// [`GuiApp::render_confirm_dialog`] to carry one word of state, and the user's
/// acceptance criterion for the window-close case was explicitly *"the same
/// discard dialogue that I get when I would switch to profiles"*, so a bespoke
/// second prompt was never an option.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiscardThen {
    /// The editor's own Close button (Close doubling as Discard). Confirm runs
    /// [`GuiApp::discard_module`], dropping the *focused* draft only and landing
    /// on the remembered view exactly as a clean Close would.
    CloseEditor,
    /// A category tab (Profiles / Settings / IDE Mode) was clicked. Confirm
    /// applies that click and drops exactly the drafts the destination itself
    /// drops — the focused one for `GamesProfiles`, none for the other two
    /// (issue #35). It used to `clear()` the whole map regardless.
    Nav(NavCategory),
    /// The whole window is closing — the OS close (X / Alt+F4 / the window
    /// manager) or a launch-mode title-bar button. Confirm drops every draft,
    /// records `EditOutcome` (so "Launch Game" still launches), and closes for
    /// real via [`GuiApp::pending_close`].
    ExitApp(EditOutcome),
}

/// A destructive action awaiting confirmation in a small dialog.
#[derive(Debug, Clone)]
enum ConfirmAction {
    DeleteGame(String),    // appid
    DeleteProfile(String), // preset name
    ClearSettings,
    ReExportResources,
    ConfigCleanup,
    /// Delete a user-authored module manifest. `label` is `Author::Name` for the
    /// prompt; `manifest` is the file to remove.
    DeleteModule { manifest: PathBuf, label: String },
    /// Leave the open module editor — or the whole window — while some draft
    /// holds unsaved work.
    ///
    /// `then` is **what the user was trying to do** when the prompt was raised,
    /// so Confirm completes it instead of merely discarding. See
    /// [`DiscardThen`] for the three destinations and why they share one variant.
    DiscardEdits { then: DiscardThen },
    /// Save was pressed while the open draft holds a **staged identity change**
    /// (Author / Name / an existing field's `Variable`) in `ModuleDraft::identity`.
    ///
    /// *Why this prompt exists — and why it is no longer protective (2026-07-19):*
    /// its ancestor guarded real data loss — Save used to write the body under the
    /// old identity and then drop the draft, taking the staged rename with it.
    /// That is fixed: [`GuiApp::save_module`] now leaves a staged identity exactly
    /// where it is. The prompt came back at the user's explicit request as an
    /// **advisory** one: "it should notify me, you got to first save your rename
    /// and then you can save your changes". With the identity edit living outside
    /// `snapshot()`, a Save that silently ignores it looks — from the seat of
    /// someone who just typed a new name — like the rename went out with it. The
    /// prompt keeps the user oriented about which of their two pending edits this
    /// button actually commits.
    ///
    /// **Carries no payload.** The one thing the arm branches on,
    /// `ModuleDraft::identity_error`, is refreshed on the draft every frame and is
    /// therefore read *live* at render and at confirm time. A captured copy could
    /// only ever go stale, and stale captures are a known hazard here (same
    /// reasoning as `DiscardEdits`, which reads its module names live).
    SaveWithPendingRename,
}

/// The Fork / Create-new module dialog. Both share the same Author/Name form;
/// Fork additionally copies the parent's saved settings and carries its lineage.
struct ModuleDialog {
    kind: DialogKind,
    author: String,
    name: String,
    /// Fork only: snapshot the parent's stored config into the fork's namespace.
    copy_settings: bool,
}

enum DialogKind {
    /// Fork the module with this `Extension::id()`.
    Fork { parent_id: String },
    /// Create a brand-new module from an empty template.
    Create,
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
    /// Absolute manifest path for each entry in `all_specs`, index-aligned. Used
    /// by the module editor to write edits back and decide editability.
    all_manifests: Vec<PathBuf>,
    all_is_folder_ext: Vec<bool>,
    /// Non-fatal load problems (bad manifests / duplicate identities) shown as a
    /// banner atop the module tree. Refreshed on every hot-reload.
    extension_errors: Vec<ExtensionLoadError>,
    /// The modules the **current screen** lists, in tree order. Game screens
    /// (and the editor, which resolves against the ambient game) get `all_specs`
    /// filtered by `Extension::applies_to(appid)`; the Global Profile and Profile
    /// screens get all of them — see [`GuiApp::cur_specs_filter`].
    cur_specs: Vec<Extension>,
    /// Tree folder for each entry in `cur_specs`, index-aligned.
    cur_dirs: Vec<PathBuf>,
    cur_is_folder_ext: Vec<bool>,
    /// The filter `cur_specs` was last built with (`Some(appid)` / `None` = no
    /// filter). Compared against [`GuiApp::cur_specs_filter`] once per frame so a
    /// screen change rebuilds the list exactly once, instead of every frame.
    cur_specs_filter: Option<String>,
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
    /// Which top-level area is showing (see [`Mode`]). Orthogonal to `nav_sel`;
    /// in-memory only, never written to any config file.
    mode: Mode,
    /// The module IDE Mode currently has focus on, if any (S4b).
    ///
    /// *Why a plain field and not the deleted `NavSel::ModuleEditor(_)`:* that
    /// variant made "which module is focused" and "which config scope am I
    /// editing" the same piece of state, so entering/leaving the editor had to
    /// stomp `nav_sel` and remember a prior value to restore (`editor_return`).
    /// This field carries focus on its own axis — `select_nav_category`,
    /// `focus_module` and friends never touch `nav_sel` at all, so there is
    /// nothing to restore on the way out: `nav_sel` was simply never disturbed.
    /// [`GuiApp::focused_module`] (the accessor, distinct from this field of the
    /// same name conceptually) is still the single reader everything else uses.
    focused_module: Option<String>,
    /// Which store module-field writes land in *right now* — see [`WriteTarget`].
    /// Always [`WriteTarget::Scope`] except inside the one
    /// `render_module_settings_body` call the IDE preview column makes.
    write_target: WriteTarget,
    /// The IDE preview's scratch game layer. A real [`GameConfig`] handed to the
    /// resolver as the game scope, exactly like `preset_as_fake_game` does for
    /// profiles — so `render_field` / `current_scope_value` / `apply_tri` work
    /// verbatim against it. **Never written to disk**: nothing maps it to a
    /// `save_game`, and `persist()` has no arm that can reach it.
    ///
    /// Seeded empty (`__preview__` / "None") so the preview resolves against
    /// nothing by default; [`GuiApp::set_preview_game`] swaps in a *clone* of a
    /// real game's config when the preview game selector picks one.
    preview_config: GameConfig,
    /// Profile layer that goes under `preview_config`, pre-merged with its parent
    /// chain. Cached by [`GuiApp::set_preview_game`] rather than re-read from disk
    /// inside the render path — resolution runs every frame, preset loads are file
    /// reads.
    preview_preset: Option<Preset>,
    /// The same profile layer **unmerged**, one entry per preset, index 0 = the
    /// game's own profile, 1 = its parent, 2 = grandparent, … — the shape
    /// `render_module_settings_body` needs to pick an inheritance-shading depth.
    ///
    /// *Why a second copy of what `preview_preset` already holds* (issue #8):
    /// `preview_preset` is pre-*merged*, which is exactly what the resolver wants
    /// and exactly what the depth badge cannot use — merging is what destroys the
    /// "which preset in the chain set this" information the badge exists to show.
    /// Cached here for the same reason `preview_preset` is: resolution and render
    /// both run every frame, and rebuilding this means one `load_preset` per link.
    preview_preset_chain: Vec<Preset>,
    /// AppID of the game the IDE preview resolves against, or `None` for the empty
    /// scratch layer. **Deliberately not persisted and not in `GeneralConfig`**:
    /// the user asked twice that this never be remembered, so every app start
    /// begins at None. Only the selector reads it back.
    preview_game: Option<String>,
    /// Index into `all_specs` of the module the IDE column has selected. Kept
    /// separate from `selected_ext` (which indexes `cur_specs`) because the IDE
    /// tree browses the *unfiltered* module set — the two lists have different
    /// lengths and orderings, so one index cannot serve both.
    ide_selected: usize,
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
    /// Every in-memory module draft, keyed by **manifest path**.
    ///
    /// *Why a map and not a single `Option<ModuleDraft>` (S4a, 2026-07-19):* the
    /// user's requirement is to edit several modules and browse others without
    /// losing unsaved work. One slot means every module switch silently destroyed
    /// the draft (the old frame-start drop guard did it without a word).
    ///
    /// *Why keyed by manifest `PathBuf` and not by `Extension::id()`:*
    /// [`GuiApp::perform_rename`] rewrites the manifest **in place** — the file is
    /// never renamed, and `id()` is derived from the JSON meta — so the path is
    /// stable across a rename while the id is not. Keying on the path makes
    /// re-keying a non-event; the id is carried as a plain field on the draft for
    /// display and lookup.
    ///
    /// A draft whose module has vanished from `all_specs` (deleted outside the
    /// app, or a manifest that stopped validating across a Ctrl+R) is **kept**,
    /// never dropped — losing unsaved work is the worse failure. Such orphans are
    /// surfaced by [`GuiApp::orphan_draft_names`].
    drafts: IndexMap<PathBuf, ModuleDraft>,
    /// Manifest paths of the drafts holding **unsaved work** this frame —
    /// [`ModuleDraft::has_unsaved_work`], not bare `dirty()` — recomputed once per
    /// frame by [`GuiApp::refresh_unsaved_drafts`].
    ///
    /// *Why unsaved-work and not dirty* (2026-07-19, issue #34): everything
    /// downstream of this set answers the user's question "will I lose
    /// something?" — the discard prompts, the modules they name, the nav lock, the
    /// tree's per-row markers. A staged-but-uncommitted rename is something to
    /// lose, so it belongs in the set. Crucially, [`GuiApp::apply_discard_edits`]
    /// destroys whole drafts (`drafts.clear()` / `shift_remove`), staged identity
    /// included, so a dirty-based set would let the confirm dialog name strictly
    /// fewer modules than confirming it destroys. The set and the destruction now
    /// describe the same thing.
    ///
    /// *Why cached and not asked per call:* [`ModuleDraft::dirty`] clones the
    /// whole draft, rebuilds an `IndexMap` and serializes it. It was already
    /// running ~4–6× per frame for a single draft; per-row dirty markers in the
    /// module tree would make that N× per frame with N drafts open. Computing the
    /// set once at the top of the frame and reading it everywhere else is cheaper
    /// than today even for one draft.
    ///
    /// **Frame-scoped.** Written in exactly one place (the top of [`GuiApp::ui`],
    /// right after `ensure_draft`) and read by everything downstream in the same
    /// frame. Anything that mutates a draft outside the render path must call
    /// `refresh_unsaved_drafts` itself.
    unsaved_drafts: std::collections::HashSet<PathBuf>,
    /// Set once a window close has been **approved** — either because there was
    /// no unsaved work to lose, or because the user confirmed the discard prompt
    /// ([`DiscardThen::ExitApp`]). Two jobs, both required (2026-07-19, issue
    /// #33):
    ///
    /// 1. **Re-issue the close.** The guard answers the OS close request with
    ///    `ViewportCommand::CancelClose`, which eframe honours by *not* setting
    ///    its `close` flag ([`eframe`]'s `EpiIntegration::update`). Nothing will
    ///    close the window afterwards unless we ask again, so the tail of
    ///    [`GuiApp::ui`] sends `ViewportCommand::Close` on every frame this is
    ///    set. That command round-trips through the backend as a
    ///    `ViewportEvent::Close`, arriving as `close_requested()` on the *next*
    ///    frame — a close genuinely cannot happen within the frame that asks.
    /// 2. **Let that next frame through.** The guard checks this flag first, so
    ///    the re-issued close is not caught and cancelled by the very guard that
    ///    raised the prompt (an infinite "cancel, ask, cancel" loop).
    ///
    /// *Why a flag and not a direct `send_viewport_cmd` from the confirm arm:*
    /// [`GuiApp::apply_discard_edits`] takes no `&egui::Context` on purpose — it
    /// is the one part of the confirmation that unit tests drive directly (see
    /// its doc comment). Threading a context through it would make the
    /// close-decision untestable again.
    pending_close: bool,
    /// The open Fork / Create-new module dialog, if any.
    module_dialog: Option<ModuleDialog>,
    /// "Also purge stored config" checkbox state for the pending Delete-module
    /// confirmation.
    delete_module_purge: bool,
    /// Transient message summarising which stored settings a fork snapshot (or a
    /// future rename) couldn't carry over. Shown near the module editor until
    /// dismissed.
    carryover_report: Option<String>,
    /// Measured ink centers for icon glyphs, so every icon sits visually centered
    /// in its cell (see `crate::icon_center`). Lives on the app so the
    /// measurement survives across frames.
    icon_cache: IconCenterCache,
}

/// In-memory editor state for one module manifest. `ext` carries every editable
/// block *except* the UI sections, which live in `sections` as an ordered
/// `Vec` (trivial reorder/rename/remove) and are folded back into an
/// [`Extension`] via [`ModuleDraft::snapshot`] for serialize / validate / preview.
struct ModuleDraft {
    /// The `Extension::id()` this draft edits — matches the on-disk manifest.
    id: String,
    /// Absolute manifest path (write target).
    manifest: PathBuf,
    /// True when the manifest lives in a user-writable (non-bundled) location.
    editable: bool,
    /// Everything but the UI sections (meta, backend, env/wrapper/arg blocks).
    ext: Extension,
    /// UI sections as an ordered list of `(section name, fields)`.
    sections: Vec<(String, Vec<UiField>)>,
    /// The on-disk manifest as a JSON value — the baseline for the dirty check.
    baseline: Value,
    /// UI-field variables present on disk. A field whose `Variable` is in this set
    /// is an *existing* field (rename would orphan config → read-only); a field
    /// whose variable is absent is *newly added* → its `Variable` is editable.
    baseline_vars: std::collections::HashSet<String>,
    /// Name-collision message for the module's **on-disk** identity, fed into the
    /// Save gate. For an existing editable module the on-disk Author/Name never
    /// collide with themselves (excluded by path), so this stays `None`; identity
    /// edits are validated separately via [`ModuleDraft::identity_error`].
    name_error: Option<String>,
    /// Staged identity change (Author / Name / existing-field `Variable`). Held
    /// **out of** [`ModuleDraft::snapshot`] so editing it never marks the draft
    /// dirty and never travels the normal Save path — it is committed only through
    /// the explicit Rename action.
    identity: PendingIdentity,
    /// Validity of the pending identity change, recomputed each frame by
    /// [`GuiApp::refresh_identity_state`]. `Some(msg)` = there IS a pending change
    /// but it cannot be committed (empty/colliding name, or a bad var rename);
    /// `None` = either no pending change or a committable one.
    ///
    /// **Meaningful on the FOCUSED draft only.** `refresh_identity_state` writes
    /// it through `draft_mut()`, so an unfocused draft's value is whatever it held
    /// when it last lost focus and may be arbitrarily stale.
    ///
    /// *Why that is sound, and not the data-integrity bug it looks like*
    /// (2026-07-19, issue #32): the asymmetry is real — the collision check
    /// [`GuiApp::draft_identity_collides`] *reads* every draft's staged identity
    /// while only the focused draft's error is *written*. But every read of this
    /// field goes through `GuiApp::draft()` (the confirm-dialog label and its
    /// blocked-reason branch, the `perform_rename` gate, and
    /// `editor_header_info`), i.e. the focused draft — and `ui()` runs
    /// `refresh_identity_state` at the top of every frame, against all drafts'
    /// current staged identities, before any of them render. An unfocused draft's
    /// identity cannot change while unfocused (only the focused draft has an open
    /// editor), so refocusing recomputes the value before it is next read.
    /// A stale marker is therefore never *observed*: it cannot reach the screen
    /// and it cannot let `perform_rename` commit a colliding rename.
    ///
    /// The invariant this rests on is "**never read this off a draft that is not
    /// focused**". If a future caller needs a background draft's validity — a
    /// per-row error marker in the module tree, say — do not read this field:
    /// either refresh every draft or call `draft_identity_collides` live at that
    /// read. See the regression test `identity_error_is_refreshed_on_refocus`.
    identity_error: Option<String>,
}

/// Staged (Author, Name, per-variable) identity edits for the open module,
/// committed only by the Rename action. Seeded from the on-disk identity when the
/// draft is (re)built; `var_edits` is keyed by each **existing** field's current
/// (on-disk) `Variable` — the stable rename key — with the value being the edited
/// name buffer.
#[derive(Clone, Default)]
struct PendingIdentity {
    author: String,
    name: String,
    /// **Insertion order is the extension's own UI declaration order** — the
    /// order the fields appear on screen — and may be relied on.
    ///
    /// *Why it is pinned* (2026-07-19, issue #31): both seeding sites
    /// ([`GuiApp::ensure_draft`] and the re-base at the tail of
    /// [`GuiApp::perform_rename`]) used to build this by iterating the
    /// `HashSet<String>` of baseline vars, which made the order of an
    /// order-preserving `IndexMap` nondeterministic across runs. Nothing read it
    /// in order *yet* — `changed_var_renames` walks `sections` and looks each key
    /// up — so the only symptom would have been a future UI list or diff report
    /// silently reordering itself between launches, which is close to
    /// unreproducible once it ships. Both sites now seed from an ordered walk of
    /// `spec.ui` and the `HashSet` is only ever asked `contains`.
    var_edits: IndexMap<String, String>,
}

impl PendingIdentity {
    /// The staged Author as every consumer must see it: **trimmed**.
    ///
    /// *Why an accessor and not `.trim()` at each call site* (2026-07-19): the
    /// staged identity is read by the pending predicate, the two collision
    /// checks, the identity validator, the rename commit, the confirm-dialog
    /// label and the `(rename pending)` notes — and a site that forgot to trim
    /// made a *trailing space* count as a rename. That is invisible on screen,
    /// so the module went "unsaved", the discard prompt named it, and the Rename
    /// button lit up, all with nothing to see. One accessor means the next
    /// reader cannot get it wrong.
    ///
    /// **Never trim the buffer in place.** `author`/`name` are live
    /// `TextEdit` buffers and module names legitimately contain interior spaces
    /// ("LSFG VK"). Normalising the buffer every frame deletes the space the
    /// instant it is typed, so the user can never reach the second word. Trim on
    /// *read*, mutate never.
    fn staged_author(&self) -> &str {
        self.author.trim()
    }
    /// The staged Name, trimmed — see [`Self::staged_author`].
    fn staged_name(&self) -> &str {
        self.name.trim()
    }
    /// True when the staged `Variable` buffer for the field currently named
    /// `key` is a real rename (whitespace-only differences are not).
    fn staged_var_changed(var_edits: &IndexMap<String, String>, key: &str) -> bool {
        var_edits.get(key).is_some_and(|b| b.trim() != key)
    }
}

impl ModuleDraft {
    /// Fold `sections` back into `ext.ui` to produce the full [`Extension`].
    /// Section names are folded in **trimmed** (2026-07-19, same
    /// invisible-whitespace class as module identity): `ext.ui` is an
    /// `IndexMap`, so `"General"` and `"General "` are two different sections
    /// that look identical on screen. Trimming here — at the fold, not in the
    /// live `sections` buffer — keeps interior spaces ("Advanced Options")
    /// typable while making the written manifest canonical. Every `Requires`
    /// expression is canonicalised the same way — see [`normalize_requires`].
    fn snapshot(&self) -> Extension {
        let mut e = self.ext.clone();
        e.ui = self
            .sections
            .iter()
            .map(|(k, v)| (k.trim().to_string(), v.clone()))
            .collect();
        normalize_requires(&mut e);
        e
    }
    fn dirty(&self) -> bool {
        serde_json::to_value(self.snapshot()).ok().as_ref() != Some(&self.baseline)
    }
    /// Save is enabled only when the draft is on a writable module, differs from
    /// disk, validates, every `Requires` parses, and there is no name collision.
    ///
    /// *Why the gate stays `dirty()` and not [`Self::has_unsaved_work`]*
    /// (2026-07-19, issue #34): Save writes `snapshot()` to the manifest, and a
    /// rename-only draft's snapshot is byte-identical to disk — pressing Save
    /// would rewrite the file with the same bytes under the *old* identity and
    /// then close the editor, which neither applies the rename nor is what Save
    /// was asked to do. The rename is committed by the Rename button
    /// ([`GuiApp::perform_rename`]), which is gated on the identity being valid
    /// and explicitly allows a clean body. So a rename-only draft reports unsaved
    /// work everywhere the user is asked about losing it, while Save stays greyed
    /// and Rename is the lit affordance.
    ///
    /// Duplicate UI-section names collide when folded into `ext.ui` (an
    /// `IndexMap`), silently dropping a section on Save — block Save instead.
    fn sections_unique(&self) -> bool {
        // Trimmed, to match how `snapshot` folds them: two names differing only
        // by surrounding whitespace collapse into one map key on Save, so they
        // must read as a collision here rather than silently dropping a section.
        let mut seen = std::collections::HashSet::new();
        self.sections.iter().all(|(k, _)| seen.insert(k.trim()))
    }
    fn save_enabled(&self) -> bool {
        let snap = self.snapshot();
        self.editable
            && self.sections_unique()
            && save_gate(
                self.dirty(),
                core_extension::validate(&snap).is_ok(),
                all_requires_parse(&snap),
                self.name_error.is_some(),
            )
    }

    /// Every current field `Variable` in declaration order (baseline + newly
    /// added). Used to detect chained/colliding var renames.
    fn current_field_vars(&self) -> Vec<String> {
        self.sections
            .iter()
            .flat_map(|(_, fields)| fields.iter().map(|f| f.variable.clone()))
            .collect()
    }

    /// The subset of `identity.var_edits` that actually renames an **existing**
    /// (still-present) field's variable, as `old → new`. Removed fields and
    /// unchanged names are excluded; an edited-to-empty name is kept (so the
    /// validator can flag it).
    fn changed_var_renames(&self) -> IndexMap<String, String> {
        let mut out = IndexMap::new();
        for (_, fields) in &self.sections {
            for f in fields {
                if !self.baseline_vars.contains(&f.variable) {
                    continue;
                }
                if let Some(new) = self.identity.var_edits.get(&f.variable) {
                    let new = new.trim();
                    if new != f.variable {
                        out.insert(f.variable.clone(), new.to_string());
                    }
                }
            }
        }
        out
    }

    /// True when this draft holds **any** work that would be lost by dropping it —
    /// body edits, a staged identity change, or both. The single predicate every
    /// "is there unsaved work?" gate asks (2026-07-19, issue #34).
    ///
    /// *Why this exists instead of folding identity into [`Self::snapshot`]:*
    /// `PendingIdentity` is deliberately **outside** the snapshot, so
    /// [`Self::dirty`] cannot see it — that asymmetry is what lets the editor
    /// distinguish "the body changed" (→ Save writes it) from "a rename is staged"
    /// (→ only the Rename path may commit it), and moving identity into the
    /// snapshot would collapse the distinction and break the rename flow. But
    /// *the user* has no such distinction: they typed something and it is not on
    /// disk. So the two notions are unified here, at the predicate layer, and
    /// nowhere else — four gates used to ask bare `dirty()` and each one silently
    /// lost a rename-only draft.
    ///
    /// **Not the Save gate.** [`Self::save_enabled`] stays `dirty()`-based on
    /// purpose — see its doc comment.
    fn has_unsaved_work(&self) -> bool {
        self.dirty() || self.has_pending_identity()
    }

    /// True when the staged identity differs from the on-disk identity.
    ///
    /// Compared **trimmed on both sides** (2026-07-19): the staged side through
    /// [`PendingIdentity::staged_author`], the disk side structurally by
    /// `ritz_core::schema`'s trimming (de)serializer. Whitespace-only
    /// differences are therefore not a pending rename — typing a trailing space
    /// must not make a module report unsaved work, because the user has no way
    /// to see what changed.
    fn has_pending_identity(&self) -> bool {
        self.identity.staged_author() != self.ext.meta.author
            || self.identity.staged_name() != self.ext.meta.name
            || !self.changed_var_renames().is_empty()
    }

    /// Reason the pending identity change cannot be committed, or `None` when
    /// there is no pending change or it is committable. `name_collides` is passed
    /// in because the collision check needs the full loaded set (owned by
    /// [`GuiApp`]).
    fn compute_identity_error(&self, name_collides: bool) -> Option<String> {
        if !self.has_pending_identity() {
            return None;
        }
        if self.identity.staged_author().is_empty() || self.identity.staged_name().is_empty() {
            return Some("Author and Name must not be empty".to_string());
        }
        if name_collides {
            return Some("Author + Name already in use".to_string());
        }
        validate_var_renames(&self.current_field_vars(), &self.changed_var_renames()).err()
    }
}

/// Reject an invalid set of `old → new` variable renames (pure, unit-tested):
///
/// - an empty target name;
/// - a **chained** rename — a new name equal to some *other* field's current
///   variable — which would strand config (do the renames one at a time);
/// - two renames producing the *same* new name.
///
/// `current_vars` is every field's pre-rename variable. `Ok(())` = safe to apply.
fn validate_var_renames(
    current_vars: &[String],
    renames: &IndexMap<String, String>,
) -> std::result::Result<(), String> {
    let mut seen_new: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (old, new) in renames {
        let new = new.trim();
        if new.is_empty() {
            return Err(format!("Variable \"{old}\" cannot be renamed to an empty name"));
        }
        // Chained rename / collision: the new name is already some other field's
        // live variable (a field renamed A→B while B still exists, or a swap).
        if current_vars.iter().any(|v| v == new && v != old) {
            return Err(format!(
                "Renaming \"{old}\" to \"{new}\" collides with an existing variable \u{2014} rename that one separately"
            ));
        }
        if !seen_new.insert(new) {
            return Err(format!("Two variables renamed to the same name \"{new}\""));
        }
    }
    Ok(())
}

/// Pure Save-gate predicate (factored out for testing): Save is allowed only when
/// the draft differs from disk, validates, every `Requires` parses, and no name
/// collision is flagged.
fn save_gate(dirty: bool, valid: bool, requires_all_parse: bool, name_error: bool) -> bool {
    dirty && valid && requires_all_parse && !name_error
}

/// Pure predicate (factored out for testing): must a request to close the whole
/// window stop and ask, or may it close immediately? (2026-07-19, issue #33.)
///
/// `any_unsaved` is `!unsaved_drafts.is_empty()` — the same frame-cached set, and
/// so the same [`ModuleDraft::has_unsaved_work`] predicate, the category-tab
/// guard above uses. **Not bare `dirty()`**: a staged-but-uncommitted rename is
/// unsaved work, `apply_discard_edits` destroys it along with the draft, and
/// closing the window destroys it just as thoroughly as switching to Profiles
/// does. Asking `dirty()` here would reintroduce issue #34's blind spot on the
/// one path where the loss is permanent — the process is gone afterwards.
///
/// `already_approved` is [`GuiApp::pending_close`]: once the user has confirmed
/// the discard (or there was nothing to confirm), the close we re-issue must not
/// be caught by the guard that raised the prompt.
fn close_needs_confirm(any_unsaved: bool, already_approved: bool) -> bool {
    any_unsaved && !already_approved
}

/// The central panel's "there is nothing to show you" text, chosen by mode
/// (issue #4).
///
/// IDE mode reaches the empty state only with an **empty `all_specs`**: its
/// invariant (top of [`GuiApp::ui`]) focuses `all_specs[ide_selected]` whenever
/// anything is loaded, and a focused module renders the editor instead. Config
/// mode reaches it whenever the *game filter* leaves `cur_specs` empty, which is
/// a different fact about a different list — hence two strings, not one.
///
/// *Why a free function:* it makes the choice assertable without driving a render
/// pass, the same seam `save_gate` / `nav_category_drops_draft` use. Copy that
/// only exists inside a `ui.label` call is copy nothing can regression-test.
fn empty_central_message(ide: bool) -> &'static str {
    if ide {
        // Points at `render_ide_nav_footer`'s "+ New Module", which renders
        // whenever `mode == Mode::Ide` regardless of focus or `all_specs` — it is
        // the *only* route to a first module from an empty extensions dir, and
        // therefore why entering IDE mode is deliberately NOT blocked on a
        // non-empty list (the direction issue #4 originally suggested). Refusing
        // entry would close that route and turn a bare screen into a real dead
        // end; naming the state and pointing at the button is what was missing.
        "No modules loaded. Use \u{201c}+ New Module\u{201d} below the tree to create one."
    } else {
        // Config mode's long-standing wording, unchanged.
        "No extensions apply to this game."
    }
}

/// Pure predicate (factored out for testing): would applying this category-tab
/// click take unsaved module edits away from the user?
///
/// `focused_unsaved` is [`GuiApp::focused_draft_unsaved`] — whether the *focused*
/// draft holds unsaved work ([`ModuleDraft::has_unsaved_work`]: body edits **or**
/// a staged rename, issue #34). **Not** `!unsaved_drafts.is_empty()` any more —
/// see the per-arm reasoning below.
///
/// *Why the IDE arm is unconditionally `false` (S4a):* it used to compare the
/// tree's selection against the module under edit, because switching modules
/// replaced the single draft slot and so destroyed the edits. With a keyed map,
/// switching modules destroys nothing — the previous draft simply stops being
/// focused and keeps its edits. There is nothing left to warn about, and a prompt
/// on every module switch would be pure noise on the mode's primary gesture.
///
/// *Why `GeneralSettings` is now `false` too* (2026-07-19, issue #35): the
/// destination drops **nothing**. `GuiApp::select_nav_category(GeneralSettings)`
/// deliberately clears `focused_module` and leaves every draft in the map — its
/// own comment says so. The old arm returned `any_unsaved`, so the prompt fired,
/// and confirming it ran `apply_discard_edits` → `drafts.clear()`. **The
/// confirmation created a loss the destination never required**, with Cancel as
/// the only non-destructive answer. A destination that costs nothing must not ask
/// a question whose "yes" costs everything.
///
/// *Why `GamesProfiles` asks about the focused draft only* (same issue): its
/// destination drops exactly one draft — `select_nav_category` calls
/// `GuiApp::close_focused_draft`, which `shift_remove`s the focused key and
/// leaves every other open draft alone. Asking `any_unsaved` made a background
/// draft (one the user is *not* editing) raise a prompt for a click that would
/// never have touched it. The gate and the destination now describe the same
/// single draft, which is also exactly the set
/// [`GuiApp::discard_names`] puts in the dialog.
fn nav_category_drops_draft(cat: NavCategory, focused_unsaved: bool) -> bool {
    match cat {
        // Drops the focused draft (`close_focused_draft`) and nothing else.
        NavCategory::GamesProfiles => focused_unsaved,
        // Keeps every draft; only `focused_module` is cleared. Nothing to ask.
        NavCategory::GeneralSettings => false,
        // Staying inside IDE mode never costs a draft any more.
        NavCategory::Ide => false,
    }
}

/// True when `(author, name)` is already taken by a loaded module — **Version-blind**
/// (compares Author+Name only, since config keys on those two). `exclude` skips the
/// module being edited/forked, matched by manifest path so a module never collides
/// with itself. `specs` and `manifests` are index-aligned.
fn name_collides(
    specs: &[Extension],
    manifests: &[PathBuf],
    author: &str,
    name: &str,
    exclude: Option<&Path>,
) -> bool {
    specs.iter().zip(manifests.iter()).any(|(s, m)| {
        exclude != Some(m.as_path())
            && s.meta.author == author
            && s.meta.name == name
    })
}

/// Turn a free-text Author/Name into a filesystem-safe slug component: ASCII
/// alphanumerics kept (lowercased), everything else → `_`, collapsed edges
/// trimmed. Empty input falls back to `module` so a filename always has a stem.
fn sanitize_slug(s: &str) -> String {
    let mapped: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let trimmed = mapped.trim_matches('_');
    if trimmed.is_empty() {
        "module".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Given a slug `base` (no extension) and an `exists` predicate over candidate
/// filenames, return the first free `"<base>.json"`, `"<base>-2.json"`,
/// `"<base>-3.json"`, … Factored out (predicate injected) so it's unit-testable
/// without touching the filesystem.
fn uniquify_slug(base: &str, exists: impl Fn(&str) -> bool) -> String {
    let first = format!("{base}.json");
    if !exists(&first) {
        return first;
    }
    let mut n = 2;
    loop {
        let candidate = format!("{base}-{n}.json");
        if !exists(&candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Build the write path for a new module in `dir`: `sanitize(author)__sanitize(name).json`,
/// suffixed `-2`, `-3`, … if the file already exists (so a fork never clobbers an
/// unrelated manifest that happens to slug-collide).
fn unique_slug_path(dir: &Path, author: &str, name: &str) -> PathBuf {
    let base = format!("{}__{}", sanitize_slug(author), sanitize_slug(name));
    let fname = uniquify_slug(&base, |c| dir.join(c).exists());
    dir.join(fname)
}

/// True if every `Requires` expression in the manifest parses (empty = ok).
fn all_requires_parse(ext: &Extension) -> bool {
    let ok = |r: &Option<String>| {
        r.as_deref()
            .is_none_or(|s| condition::parse(s).is_ok())
    };
    ext.ui.values().flatten().all(|f| ok(&f.requires))
        && ext
            .env_vars
            .iter()
            .chain(ext.game_env_vars.iter())
            .all(|e| ok(&e.requires) && e.builder.iter().all(|b| ok(&b.requires)))
        && ext
            .wrappers
            .iter()
            .all(|w| ok(&w.requires) && w.builder.iter().all(|b| ok(&b.requires)))
        && ext.game_launch_args.iter().all(|a| ok(&a.requires))
}

/// Canonicalise every `Requires` expression in `ext` by **trimming** it. Same
/// walk as [`all_requires_parse`].
///
/// *Why it trims but never collapses `Some("")` to `None`* — which is the obvious
/// next step, and is wrong: bundled manifests ship an explicitly empty
/// `"Requires": ""` (`dxvk.json`), so `Some("")` is a real on-disk state. Nulling
/// it here makes `snapshot()` differ from `baseline` for every such module, and
/// [`ModuleDraft::dirty`] is a serialized comparison — so merely *opening* DXVK
/// would mark it unsaved with nothing to see. The two existing tests
/// `rendering_a_draft_without_touching_it_leaves_it_clean` and
/// `an_empty_string_in_an_optional_slot_survives_a_render` both catch it; the
/// same trap `6f47e5a` and the WRITE-BACK GATING rule exist for. Empty and absent
/// mean the same thing to `condition::eval_opt` (always true), so preserving the
/// distinction costs nothing and keeps a round-trip byte-exact.
///
/// *Why this exists* (2026-07-19, issue #29): `requires_edit` decides
/// `None`-vs-`Some` on `s.trim()` but stores the untrimmed `s`, so `"  "`
/// became `None` while `" enabled "` kept its padding. This never changed how
/// the expression *evaluates* — `condition::tokenize` skips whitespace, so
/// `" enabled "` and `"enabled"` parse to the same AST — it is a **data-hygiene**
/// fix. It matters because [`ModuleDraft::dirty`] is a *serialized-value*
/// comparison: two logically identical drafts differing only in `Requires`
/// padding compare unequal, which re-triggers the spurious-dirty class of bug
/// `6f47e5a` fixed, and the padding also lands verbatim in the written manifest.
///
/// *Why here and not in `requires_edit`'s write-back* — which is what issue #29
/// suggested: `val` **is** the live `TextEdit` buffer, rebuilt from the stored
/// `Option<String>` every frame. Trimming on write-back deletes a trailing space
/// the instant it is typed, so the user could never get from `enabled` to
/// `enabled AND !clear` — they would be stuck at `enabledAND`. That is the exact
/// hazard already documented on [`PendingIdentity::staged_author`]: trim on
/// *read*, mutate the buffer never. `snapshot()` is the read boundary — it feeds
/// `dirty()`, the Save gate and the bytes written to disk — so canonicalising
/// there gets the whole benefit with none of the typing regression. Section
/// names are handled the same way, in the same fold.
fn normalize_requires(ext: &mut Extension) {
    let fix = |r: &mut Option<String>| {
        if let Some(s) = r {
            if s.trim().len() != s.len() {
                *s = s.trim().to_string();
            }
        }
    };
    ext.ui.values_mut().flatten().for_each(|f| fix(&mut f.requires));
    for e in ext.env_vars.iter_mut().chain(ext.game_env_vars.iter_mut()) {
        fix(&mut e.requires);
        e.builder.iter_mut().for_each(|b| fix(&mut b.requires));
    }
    for w in ext.wrappers.iter_mut() {
        fix(&mut w.requires);
        w.builder.iter_mut().for_each(|b| fix(&mut b.requires));
    }
    ext.game_launch_args.iter_mut().for_each(|a| fix(&mut a.requires));
}

/// Row-action returned by the reusable `[↑][↓][🗑]` widget; the caller applies the
/// container-correct op.
#[derive(Clone, Copy, PartialEq)]
enum RowAction {
    None,
    Up,
    Down,
    Remove,
}

/// A path-addressed structural edit (add / remove / reorder), collected during
/// render and applied to the draft *after* the render loop so no `Vec`/`IndexMap`
/// is mutated mid-iteration.
enum Deferred {
    Section(usize, RowAction),
    SectionAdd,
    Field(usize, usize, RowAction),
    FieldAdd(usize),
    FieldOpt(usize, usize, usize, RowAction),
    FieldOptAdd(usize, usize),
    Env(bool, usize, RowAction),
    EnvAdd(bool),
    EnvStep(bool, usize, usize, RowAction),
    EnvStepAdd(bool, usize),
    Wrapper(usize, RowAction),
    WrapperAdd,
    WrapperStep(usize, usize, RowAction),
    WrapperStepAdd(usize),
    Arg(usize, RowAction),
    ArgAdd,
}

/// What the editor header requested this frame (Save / Discard / Fork / Delete /
/// Rename).
enum TopAction {
    None,
    Save,
    Fork,
    Delete,
    Rename,
    Close,
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
    /// The navigation selection this detection was started under. If `nav_sel`
    /// has moved on by the time the 3s timer fires, the detection is cancelled
    /// rather than applied — see [`GuiApp::cancel_stale_detect`].
    nav: NavSel,
    /// The write target in force when the detection was *started*.
    ///
    /// *Why this has to be captured* (S5a): `nav_sel` alone does not identify the
    /// store — see [`WriteTarget`] — and `poll_detect` runs at the tail of the
    /// frame, long after `GuiApp::with_preview_writes` has restored
    /// [`WriteTarget::Scope`]. Making the preview's fields editable also makes its
    /// "Detect window class" button live, so without this a Detect started in the
    /// preview column would land its result in the **real** game config: a write
    /// that bypasses all three of the preview's guards, because it happens outside
    /// the render call they wrap.
    target: WriteTarget,
    /// The `Mode` in force when the detection was *started*.
    ///
    /// *Why this has to be captured too, and can't be derived from `target`*
    /// (2026-07-19): `cancel_stale_detect` needs to notice "started in the IDE
    /// preview, but the user has since left IDE mode" even though `nav_sel` alone
    /// doesn't change on every such transition. `target` looks like it should
    /// carry this — `Detect::target` is `Preview` exactly when the detection
    /// started in the preview — but `target` is a snapshot of `write_target`,
    /// which is back to `Scope` for the rest of the frame the instant
    /// `with_preview_writes` returns; comparing the *snapshot* against the
    /// *live* `self.write_target` at `cancel_stale_detect` time would compare
    /// `Preview` against `Scope` on the very next check regardless of whether the
    /// user is still on the preview, cancelling every preview detection almost
    /// immediately. `mode` doesn't have that problem: `self.mode` stays `Ide` for
    /// the whole time the user is looking at IDE mode, not just for the one
    /// render call, so comparing the captured mode against the live one actually
    /// answers "is the user still where this detection was started".
    mode: Mode,
}

impl Detect {
    /// Read the worker thread's slot, treating a **poisoned** mutex as a finished
    /// detection that found nothing (issue #3).
    ///
    /// *Why tolerate poisoning at all rather than `.unwrap()`:* both readers are on
    /// the GUI thread — the countdown in `render_value_editor` and
    /// [`GuiApp::poll_detect`], which runs every frame while a detection is live.
    /// `.unwrap()` there converts *any* panic in the detector thread into a panic
    /// in the UI thread, i.e. a failed `hyprctl` call takes down the whole settings
    /// window and every unsaved draft with it. The user's data is worth more than
    /// the fail-fast signal, and the poisoning is not evidence of shared state
    /// left half-written: the slot holds one enum, and the only writer assigns it
    /// whole.
    ///
    /// *Why `Done(None)` and not `Waiting`:* the poisoned slot still literally
    /// contains `Waiting` (the initial value — the writer never got to store), so
    /// propagating what's inside would leave the countdown stuck at "Detecting… 0"
    /// forever with no way back to the Detect button. `Done(None)` is what a
    /// detection that found no window class already reports, so it flows through
    /// `poll_detect`'s existing "clear `detect`, write nothing" path.
    ///
    /// Note this is belt-and-braces: `start_detect` no longer holds the guard
    /// across anything fallible, so nothing *should* poison it. This is what makes
    /// that structural fix non-load-bearing for UI survival.
    fn status(&self) -> DetectStatus {
        match self.result.lock() {
            Ok(g) => g.clone(),
            Err(_) => DetectStatus::Done(None),
        }
    }

    /// Whether this in-flight detection targets the given module + variable.
    ///
    /// *Why the render site must ask this* (issue #2): the "Detecting… Ns"
    /// countdown that replaces the Detect button used to be gated on nothing but
    /// `field.variable == "window_class"` plus "some detection is in flight". Only
    /// `hypr-monctl` declares `window_class` today, so there was never a second
    /// field to mis-render into — but users drop their own modules into
    /// `~/.config/ritz/extensions/`, and a second declarer would draw hypr-monctl's
    /// countdown on *its* field while the result silently landed on hypr-monctl.
    ///
    /// `target`/`mode` are compared alongside `ext_id`/`var` because the same
    /// module's fields are drawn by two different surfaces — Config mode and the
    /// IDE preview column — for which `write_target`/`mode` are the discriminator,
    /// not `nav_sel` (see [`WriteTarget`] and [`GuiApp::cancel_stale_detect`]).
    fn targets(&self, ext_id: &str, var: &str, target: WriteTarget, mode: Mode) -> bool {
        self.ext_id == ext_id && self.var == var && self.target == target && self.mode == mode
    }
}

impl GuiApp {
    /// Build an editor for a game (does not open a window).
    pub fn new(ctx: &AppContext, appid: &str, name: &str, game_command: Vec<String>) -> GuiApp {
        let all_specs: Vec<Extension> = ctx.extensions.iter().map(|e| e.spec.clone()).collect();
        let all_dirs: Vec<PathBuf> = ctx.extensions.iter().map(|e| e.rel_dir.clone()).collect();
        let all_manifests: Vec<PathBuf> = ctx.extensions.iter().map(|e| e.manifest.clone()).collect();
        let all_is_folder_ext: Vec<bool> = ctx.extensions.iter().map(|e| e.is_folder_ext).collect();
        let extension_errors = ctx.extension_errors.clone();
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
            all_manifests,
            all_is_folder_ext,
            extension_errors,
            cur_specs: Vec::new(),
            cur_dirs: Vec::new(),
            cur_is_folder_ext: Vec::new(),
            cur_specs_filter: None,
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
            mode: Mode::Config,
            focused_module: None,
            write_target: WriteTarget::Scope,
            preview_config: GameConfig::new("__preview__", "None"),
            preview_preset: None,
            preview_preset_chain: Vec::new(),
            preview_game: None,
            ide_selected: 0,
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
            drafts: IndexMap::new(),
            unsaved_drafts: std::collections::HashSet::new(),
            pending_close: false,
            module_dialog: None,
            delete_module_purge: false,
            carryover_report: None,
            icon_cache: IconCenterCache::new(),
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

    /// Point the IDE preview's scratch layer at a real game (`Some(appid)`) or at
    /// nothing (`None`, the default and the state every app start begins in).
    ///
    /// **Seed-from-game, not overlay-on-game.** `preview_config` becomes a *clone*
    /// of the game's stored config, so the preview opens showing that game's real
    /// settings and every edit made in the preview mutates only the clone. It is
    /// never written back — `set_preview_game` is the only thing that ever
    /// assigns `preview_config`, and no code path hands it to `save_game`.
    ///
    /// *Why not `switch_game`, which already does most of this:* `switch_game`
    /// overwrites `appid`, `game_config`, `preset` and rebuilds `cur_specs`. That
    /// would change which game Config mode is editing, and which game would
    /// actually launch, as a side effect of moving a preview dropdown. The two
    /// jobs share shape, not state.
    ///
    /// *Why the preset chain is merged here rather than in the resolver:*
    /// resolution runs every frame; `load_preset` is a file read. Mirrors the
    /// `NavSel::Game(_)` arm of [`Self::resolve_specs_for_editing`].
    fn set_preview_game(&mut self, appid: Option<&str>) {
        match appid {
            None => {
                self.preview_game = None;
                self.preview_config = GameConfig::new("__preview__", "None");
                self.preview_preset = None;
                self.preview_preset_chain = Vec::new();
            }
            Some(id) => {
                self.preview_game = Some(id.to_string());
                // A game with no file yet is a legitimate pick (it exists in
                // `self.games` only if a file exists, but a delete could race a
                // stale list) — fall back to an empty config rather than bailing.
                self.preview_config = self
                    .paths
                    .load_game(id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| GameConfig::new(id, id));
                let preset_name = self
                    .preview_config
                    .config
                    .modules
                    .preset
                    .clone()
                    .or_else(|| self.default_preset.clone());
                let direct = preset_name.and_then(|n| self.paths.load_preset(&n).ok().flatten());
                // The unmerged chain for inheritance shading, cached alongside the
                // merged one — same [direct, parent, grandparent, …] order the
                // `NavSel::Game(_)` arm of `render_module_settings_body` builds.
                self.preview_preset_chain = match &direct {
                    Some(p) => {
                        let mut c = vec![p.clone()];
                        if let Some(pn) = &p.parent {
                            c.extend(collect_parent_presets(&self.paths, pn));
                        }
                        c
                    }
                    None => Vec::new(),
                };
                // Parent chain merges UNDER the direct preset (parent = lower
                // priority) — same order as `resolve_specs_for_editing`.
                self.preview_preset = direct.map(|p| match p.parent.as_ref() {
                    Some(pname) => {
                        let mut base = collect_parent_chain(&self.paths, pname);
                        merge_modules(&mut base.modules, &p.modules);
                        base
                    }
                    None => p,
                });
            }
        }
        // The resolution base just changed underneath every in-progress preview
        // edit buffer, so drop them. Only the `"preview"`-tagged ones: Config
        // mode's buffers describe a scope this selector does not touch. See
        // `buffer_scope_tag` for why the namespaces are separable at all.
        self.text_buffers.retain(|k, _| !k.starts_with("preview::"));
        self.multi_edit.retain(|k, _| !k.starts_with("preview::"));
    }

    /// Which game `cur_specs` is filtered by on the screen currently showing, or
    /// `None` for "list every module".
    ///
    /// *Why the Global Profile and Profile screens take no filter:* those scopes
    /// apply **across** games. `AppIds`-filtering them by whichever game happened
    /// to be selected last is meaningless — the ambient game is not what the
    /// screen is configuring — and it *hides* modules the user has to be able to
    /// set a global/profile value for. The Game screens keep the filter: there the
    /// ambient game is exactly the subject, and listing modules that can never
    /// apply to it would be noise.
    ///
    /// This only ever *widens* the list. `RequiresDesktop`-gated modules are
    /// dropped much earlier — at load time in `context::load_extensions`, shared by
    /// `AppContext::load` and the GUI hot-reload — so they never reach `all_specs`
    /// and dropping the `AppIds` filter cannot resurrect them.
    fn cur_specs_filter(&self) -> Option<String> {
        match &self.nav_sel {
            NavSel::GlobalSettings | NavSel::Profile(_) => None,
            // The ambient game, not `NavSel::Game`'s payload: everything else on
            // these screens (config store, launch preview) resolves through
            // `self.appid`, and a momentary disagreement between the two must not
            // make the module list describe a different game than the fields do.
            NavSel::Game(_) | NavSel::GeneralSettings => {
                Some(self.appid.clone())
            }
        }
    }

    /// Rebuild `cur_specs` (the modules the current screen lists) from
    /// `all_specs`, preserving the selected module by id where possible.
    fn rebuild_cur_specs(&mut self) {
        let prev_id = self.cur_specs.get(self.selected_ext).map(|s| s.id());
        let filter = self.cur_specs_filter();
        self.cur_specs = Vec::new();
        self.cur_dirs = Vec::new();
        self.cur_is_folder_ext = Vec::new();
        for ((spec, dir), &is_fe) in self
            .all_specs
            .iter()
            .zip(self.all_dirs.iter())
            .zip(self.all_is_folder_ext.iter())
        {
            if filter.as_deref().is_none_or(|a| spec.applies_to(a)) {
                self.cur_specs.push(spec.clone());
                self.cur_dirs.push(dir.clone());
                self.cur_is_folder_ext.push(is_fe);
            }
        }
        // `selected_ext` is a raw index into `cur_specs`, so a list that changed
        // length would otherwise leave it pointing at a *different* module (or out
        // of range). Re-find the previously selected module by id; fall back to the
        // first entry only when it is genuinely gone from the new list.
        self.selected_ext = prev_id
            .and_then(|id| self.cur_specs.iter().position(|s| s.id() == id))
            .unwrap_or(0);
        self.cur_specs_filter = filter;
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
        let Ok((exts, errors)) = context::load_extensions(&self.paths) else {
            return;
        };
        // Issue #37. Same treatment `rebuild_cur_specs` gives `selected_ext`, for
        // the same reason: `ide_selected` is a raw index into `all_specs`, and
        // `load_extensions` re-sorts alphabetically by `meta.name` every time. Any
        // reload that adds, removes or renames a module therefore renumbers the
        // rows under the index, and the IDE tree would highlight — and the
        // `Mode::Ide` reopen invariant would reopen — an arbitrary neighbour.
        let prev_ide_id = self.all_specs.get(self.ide_selected).map(|s| s.id());
        self.all_specs = exts.iter().map(|e| e.spec.clone()).collect();
        self.all_dirs = exts.iter().map(|e| e.rel_dir.clone()).collect();
        self.all_manifests = exts.iter().map(|e| e.manifest.clone()).collect();
        self.all_is_folder_ext = exts.iter().map(|e| e.is_folder_ext).collect();
        self.extension_errors = errors;
        self.anchor_ide_selection(prev_ide_id.as_deref());
        self.rebuild_cur_specs();
    }

    /// Re-point [`GuiApp::ide_selected`] at module `id`'s row in the current
    /// `all_specs`, clamping into range when `id` is gone (or `None`).
    ///
    /// *Why "keep the index, clamped" is the right fallback for a vanished id*
    /// rather than snapping to row 0: the id disappears on **delete**, and there
    /// the row that shifted into the old index is precisely the deleted module's
    /// next neighbour — which is where a list selection should land. Row 0 would
    /// throw the user back to the top of the tree from wherever they were working.
    ///
    /// Callers that know the id they want (`perform_fork`, `perform_create`,
    /// `perform_rename` — all three move focus to a module that did not exist under
    /// that id a moment ago) pass it explicitly *after* `reload_extensions`, since
    /// the id `reload_extensions` itself remembers is the pre-operation one.
    fn anchor_ide_selection(&mut self, id: Option<&str>) {
        if let Some(pos) = id.and_then(|id| self.all_specs.iter().position(|s| s.id() == id)) {
            self.ide_selected = pos;
            return;
        }
        self.ide_selected = self.ide_selected.min(self.all_specs.len().saturating_sub(1));
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
    ///
    /// *Why the `write_target == Preview` guard (S4b)* — this is called from
    /// `render_field` for **every** field, including inside the IDE preview
    /// column, where the layer being written is the scratch `preview_config`, not
    /// whatever `nav_sel` happens to still hold. Pre-S4b `nav_sel` was forced to
    /// `NavSel::ModuleEditor(_)` for the whole time the preview rendered, which
    /// fell through to `_ => COL_GAME` here — the preview's "set at this layer"
    /// badge was always blue. Now that IDE Mode leaves `nav_sel` untouched (see
    /// [`Mode`]'s doc comment), `nav_sel` can be `GlobalSettings` or `Profile(_)`
    /// while the preview renders (whichever screen was active before entering IDE
    /// Mode) — without this guard the preview's badge would silently follow that
    /// leftover value instead of staying blue, a real (if subtle) color
    /// regression the refactor would otherwise introduce.
    fn editing_scope_color(&self) -> Color32 {
        if self.write_target == WriteTarget::Preview {
            return COL_GAME;
        }
        match &self.nav_sel {
            NavSel::GlobalSettings => COL_GLOBAL,
            NavSel::Profile(_) => COL_PROFILE,
            _ => COL_GAME,
        }
    }

    /// Resolution used for the extension editor (reflects whichever layer is being edited).
    fn resolve_for_editing(&self) -> Resolution {
        self.resolve_specs_for_editing(&self.cur_specs)
    }

    /// Like [`resolve_for_editing`] but over an explicit spec list.
    ///
    /// *Why this exists (S3b):* every arm below used to say `&self.cur_specs`
    /// literally. That was inert while the only caller resolved `cur_specs`
    /// anyway, but the IDE column's tree and preview browse the **unfiltered**
    /// `all_specs` — and a spec absent from `cur_specs` resolves to no
    /// `ExtResolution` at all, so its badges and its whole preview body would
    /// silently come up empty. Taking the list as a parameter makes the
    /// resolution match the data actually on screen. `resolve_for_editing`
    /// delegates here with `cur_specs`, so Config-mode behaviour is byte-identical.
    fn resolve_specs_for_editing(&self, specs: &[Extension]) -> Resolution {
        let global = Some(&self.global_config);
        match &self.nav_sel {
            NavSel::GeneralSettings => {
                resolve::resolve(specs, Some(&self.game_config), self.preset.as_ref(), global)
            }
            NavSel::Game(_) => {
                let merged: Option<Preset> = self.preset.as_ref().and_then(|p| {
                    let pname = p.parent.as_ref()?;
                    let mut base = collect_parent_chain(&self.paths, pname);
                    merge_modules(&mut base.modules, &p.modules);
                    Some(base)
                });
                let effective = merged.as_ref().map(|p| p as &Preset).or(self.preset.as_ref());
                resolve::resolve(specs, Some(&self.game_config), effective, global)
            }
            NavSel::GlobalSettings => {
                let fake = preset_as_fake_game(&self.global_config);
                resolve::resolve(specs, Some(&fake), None, None)
            }
            NavSel::Profile(_) => match &self.editing_preset_buf {
                Some(p) => {
                    let parent = p.parent.as_ref()
                        .map(|n| collect_parent_chain(&self.paths, n));
                    let fake = preset_as_fake_game(p);
                    resolve::resolve(specs, Some(&fake), parent.as_ref(), global)
                }
                None => resolve::resolve(specs, None, None, global),
            },
        }
    }

    /// Resolution for the IDE preview column and its launch band: the scratch
    /// `preview_config` as the game layer, its cached preset chain under it, and
    /// the real global config at the bottom.
    ///
    /// *Why a separate method rather than another arm in
    /// [`Self::resolve_specs_for_editing`]:* that one dispatches on `nav_sel`,
    /// which (S4b) IDE mode never touches — it stays whatever real scope was
    /// selected before entering IDE mode, the same for both columns — so an arm
    /// there could not tell the manifest editor's resolution from the preview's.
    /// Same reasoning as [`WriteTarget`]. `resolve_specs_for_editing` is left
    /// byte-for-byte alone so Config mode cannot be perturbed.
    ///
    /// *Why the real `global_config` is included* even with no preview game
    /// selected: it is a genuine layer of the launch the user is trying to
    /// predict. Only the game/profile layers are scratch.
    fn resolve_specs_for_preview(&self, specs: &[Extension]) -> Resolution {
        resolve::resolve(
            specs,
            Some(&self.preview_config),
            self.preview_preset.as_ref(),
            Some(&self.global_config),
        )
    }

    /// The parent-preset chain the currently rendering surface shades against —
    /// index 0 = the preset closest to the value, 1 = its parent, and so on.
    /// `render_module_settings_body` searches it for whichever link actually set a
    /// field, and uses that index as the field's inheritance-shading depth.
    ///
    /// **`write_target == Preview` takes its own arm** (S4b): that body renders
    /// both Config mode's fields and the IDE preview column's, and the preview's
    /// `nav_sel` is no longer forced away from `Game`/`Profile` while it renders
    /// (see [`Mode`]'s doc comment) — without this branch a preview opened from
    /// the Global Profile or a Profile screen would pick up *that screen's* chain
    /// and shade against a layer the preview is not resolving through at all. The
    /// preview's own layers are the scratch `preview_config` / `preview_preset`,
    /// not whatever `nav_sel` happens to still hold.
    ///
    /// *Why that arm returns the preview's own chain and not `Vec::new()`* (issue
    /// #8, 2026-07-19): empty was the conservative first cut, and it was wrong in
    /// one real case. With a preview game selected whose profile has parents,
    /// [`Self::resolve_specs_for_preview`] genuinely resolves fields to
    /// `resolve::Provenance::Preset` through that chain, so hard-empty dropped the
    /// depth badge for exactly the values Config mode's Game view badges — the
    /// same value, two screens, two answers. With **no** preview game selected
    /// `preview_preset_chain` is empty and the depth stays `None`, which is still
    /// the right answer: there is no chain to point at.
    ///
    /// *Why a method rather than the inline `match` it used to be:* it is a pure
    /// decision over `write_target`/`nav_sel`/cached state and it is the thing
    /// issue #8 was about, so it should be assertable without standing up an egui
    /// render pass.
    fn field_chain(&self) -> Vec<Preset> {
        if self.write_target == WriteTarget::Preview {
            return self.preview_preset_chain.clone();
        }
        match &self.nav_sel {
            NavSel::Game(_) => {
                let mut c = Vec::new();
                if let Some(p) = &self.preset {
                    c.push(p.clone());
                    if let Some(pn) = &p.parent {
                        c.extend(collect_parent_presets(&self.paths, pn));
                    }
                }
                c
            }
            NavSel::Profile(_) => self
                .editing_preset_buf
                .as_ref()
                .and_then(|p| p.parent.as_ref())
                .map(|pn| collect_parent_presets(&self.paths, pn))
                .unwrap_or_default(),
            _ => Vec::new(),
        }
    }

    /// Seconds left on the "Detecting… Ns" countdown **for this specific field**,
    /// or `None` when the plain Detect button should be drawn instead.
    ///
    /// *Why this is scoped and not just "is a detection in flight"* (issue #2):
    /// see [`Detect::targets`]. The old inline form asked only whether *any*
    /// detection was waiting, so a second module declaring `window_class` — which
    /// any user can drop into `~/.config/ritz/extensions/` — would draw
    /// hypr-monctl's countdown on its own field while the result landed on
    /// hypr-monctl.
    fn detect_countdown_secs(&self, ext_id: &str, var: &str) -> Option<u64> {
        let d = self
            .detect
            .as_ref()
            .filter(|d| d.targets(ext_id, var, self.write_target, self.mode))?;
        matches!(d.status(), DetectStatus::Waiting)
            .then(|| (3.0 - d.start.elapsed().as_secs_f32()).ceil().max(0.0) as u64)
    }

    /// Run `f` with every module-field write redirected to the scratch
    /// `preview_config`, restoring the *previous* [`WriteTarget`] afterwards.
    ///
    /// **Guard #1 of three.** *Why a closure and not a bare
    /// `self.write_target = …` around the call:* the restore lives after `f(self)`
    /// *inside this wrapper*, so no `return` written in the caller's closure body
    /// can skip it — a caller can only return from its own closure, which returns
    /// into this function. `render_module_settings_body` and the `push_id` block
    /// that wraps it both contain early returns, and a bare assignment placed
    /// after the call would be trivially bypassed by adding one more. Leaving
    /// `write_target` stuck on `Preview` would be the worst possible failure: the
    /// *next* Config-mode edit would silently land in the scratch layer and be
    /// discarded.
    ///
    /// *Why save-and-restore rather than a hardcoded reset to `Scope`:* today
    /// there is exactly one call site and it always starts from `Scope`, so
    /// either form produces the same behaviour. But a hardcoded `Scope` restore
    /// is a landmine for a future nested call — `poll_detect` already does its
    /// own save/restore of `write_target` around a `set_scoped` call, and if that
    /// (or any other future caller) ever ran while already inside
    /// `with_preview_writes`, a hardcoded restore would disarm the *outer* call's
    /// `Preview` target the moment the inner one returns, silently sending the
    /// rest of the outer closure's writes to the real config. Saving and
    /// restoring the value that was actually in force on entry — the same
    /// two-line shape `poll_detect` uses — makes this re-entrant-safe by
    /// construction: nesting can only ever narrow the redirected span, never
    /// widen or shorten the enclosing one.
    fn with_preview_writes<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let prev = self.write_target;
        self.write_target = WriteTarget::Preview;
        let out = f(self);
        self.write_target = prev;
        out
    }

    /// The scope prefix for `text_buffers` / `multi_edit` keys.
    ///
    /// *Why this must exist* (S5a): `text_buffers` keys were `"{spec.id()}::{var}"`
    /// with **no** scope tag, and both `multi_edit` scope-tag sites mapped
    /// `NavSel::ModuleEditor(_)` onto `"game:{appid}"`. So the IDE preview and
    /// Config mode's Game view computed **identical** keys for the same
    /// module/variable — two different resolution bases sharing one text buffer.
    /// That was inert only for as long as the preview never wrote; making the
    /// preview interactive fires it. Routing every key site through here gives the
    /// preview its own `"preview"` namespace.
    ///
    /// It also fixes an `egui::Id`: the `String` field arm mints
    /// `egui::Id::new(("text_edit", &key))`, an **absolute** id that `push_id`
    /// does not namespace and that becomes reachable the moment the preview's
    /// fields turn editable. Folding the tag into `key` fixes the id and the
    /// buffer key in one move.
    ///
    /// *Why the `nav_sel` match below never needs an IDE-mode arm (S4b):* the
    /// only caller that renders fields under `Mode::Ide` is the preview column,
    /// and it always renders inside [`Self::with_preview_writes`] — so it always
    /// takes the `Preview` early return above and never reaches this match at
    /// all. The manifest editor itself (the other IDE column) never calls this:
    /// it mutates the open [`ModuleDraft`] directly and has no `text_buffers` /
    /// `multi_edit` keys of its own.
    fn buffer_scope_tag(&self) -> String {
        if self.write_target == WriteTarget::Preview {
            return "preview".to_string();
        }
        match &self.nav_sel {
            NavSel::GlobalSettings => "global".to_string(),
            NavSel::Profile(n) => format!("profile:{n}"),
            NavSel::Game(a) => format!("game:{a}"),
            NavSel::GeneralSettings => "general".to_string(),
        }
    }

    /// The modules that apply to the **ambient game**, whatever screen is showing.
    ///
    /// Identical to `cur_specs` on the Game screens and in the editor. It exists
    /// for the Global Profile / Profile screens, where `cur_specs` is deliberately
    /// unfiltered (see [`Self::cur_specs_filter`]) but the launch-command preview
    /// still is — a launch command is by definition for one concrete game, so
    /// assembling it from modules that can never apply to that game would print a
    /// command that will never be run.
    fn game_specs(&self) -> Vec<Extension> {
        self.all_specs
            .iter()
            .filter(|s| s.applies_to(&self.appid))
            .cloned()
            .collect()
    }

    /// Like the game resolution over an explicit spec list — lets the module
    /// editor resolve a draft-spliced spec set for its live preview without
    /// touching disk or the launch assembler's signature.
    fn resolve_specs_for_game(&self, specs: &[Extension]) -> Resolution {
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
            specs,
            Some(&self.game_config),
            effective,
            Some(&self.global_config),
        )
    }

    /// Manifest path + editability for a module id. A module is editable only when
    /// its manifest is *not* one of the bundled sets (`default/…`, `built-in/…`),
    /// which — although bootstrapped into the user config dir — remain
    /// inspect-only until forked (Phase 3).
    fn module_editability(&self, id: &str) -> Option<(PathBuf, bool)> {
        let idx = self.all_specs.iter().position(|s| s.id() == id)?;
        let rel = &self.all_dirs[idx];
        let top = rel.components().next().and_then(|c| c.as_os_str().to_str());
        let bundled = matches!(top, Some("default") | Some("built-in"));
        Some((self.all_manifests[idx].clone(), !bundled))
    }

    /// The module the editor currently has **focus** on, or `None` when nothing
    /// is focused.
    ///
    /// *Why an accessor over a bare field read:* "which module is focused" is
    /// distinct from "which modules have drafts" — the second is the whole
    /// `drafts` map. Before S4b this read `nav_sel`'s now-deleted
    /// `NavSel::ModuleEditor(id)` variant; that accessor is why S4b's removal of
    /// the variant was a one-place change (swap the body to read
    /// [`GuiApp::focused_module`], the field) rather than a dozen call-site edits.
    fn focused_module(&self) -> Option<&str> {
        self.focused_module.as_deref()
    }

    /// The `drafts` key for module `id`: its manifest path.
    ///
    /// Normally that comes straight from [`Self::module_editability`]. The
    /// fallback scan exists for **orphans** — a draft whose module is no longer in
    /// `all_specs` has no `module_editability` answer, but its draft still knows
    /// the path it was loaded from, and that draft must stay reachable or the
    /// editor would render "no longer available" over work the user can still see
    /// listed as unsaved.
    fn draft_key(&self, id: &str) -> Option<PathBuf> {
        if let Some((manifest, _)) = self.module_editability(id) {
            return Some(manifest);
        }
        self.drafts
            .iter()
            .find(|(_, d)| d.id == id)
            .map(|(k, _)| k.clone())
    }

    /// The `drafts` key of the focused module, if any.
    fn focused_key(&self) -> Option<PathBuf> {
        let id = self.focused_module()?;
        self.draft_key(id)
    }

    /// The focused module's draft, if one is loaded.
    fn draft(&self) -> Option<&ModuleDraft> {
        let key = self.focused_key()?;
        self.drafts.get(&key)
    }

    /// Mutable twin of [`Self::draft`].
    fn draft_mut(&mut self) -> Option<&mut ModuleDraft> {
        let key = self.focused_key()?;
        self.drafts.get_mut(&key)
    }

    /// Recompute [`Self::unsaved_drafts`] for this frame. See that field's doc for
    /// why the set is cached rather than recomputed per query.
    fn refresh_unsaved_drafts(&mut self) {
        self.unsaved_drafts = self
            .drafts
            .iter()
            .filter(|(_, d)| d.has_unsaved_work())
            .map(|(k, _)| k.clone())
            .collect();
    }

    /// `Author::Name` of every draft whose module is no longer loadable from disk.
    ///
    /// These are retained (see the `drafts` field doc) but would otherwise be
    /// invisible: the module tree renders `all_specs`, which by definition no
    /// longer contains them. Surfaced as a banner above the IDE tree so the user
    /// can find the work and Save it back out.
    fn orphan_draft_names(&self) -> Vec<String> {
        self.drafts
            .values()
            .filter(|d| !self.all_specs.iter().any(|s| s.id() == d.id))
            .map(|d| format!("{}::{}", d.ext.meta.author, d.ext.meta.name))
            .collect()
    }

    /// Focus the module editor on `id`. No-op if already focused there.
    ///
    /// *Why this no longer touches `nav_sel` (S4b):* pre-S4b this set
    /// `nav_sel = NavSel::ModuleEditor(id)` and remembered the prior `nav_sel` in
    /// `editor_return` so leaving could restore it. Now that focus lives on its
    /// own field ([`GuiApp::focused_module`]), `nav_sel` is simply never touched
    /// by focusing a module — it stays exactly whatever real config scope was
    /// selected before IDE Mode was entered, with nothing to remember or restore.
    ///
    /// *Why the draft is built here and not left to the next frame's
    /// [`Self::sync_focused_draft`] (issue #5):* both routes that move focus
    /// interactively — a click on an IDE tree row, and entering IDE mode via
    /// `select_nav_category` — run inside `render_nav_panel`, which `GuiApp::ui`
    /// declares *after* its top-of-frame sync but *before* the header band, the
    /// editor column and the preview column. Setting focus alone therefore left
    /// those three rendering against a `focused_module` with no draft behind it
    /// for exactly one frame: `editor_header_info` returns `None`, the editor
    /// column prints "Loading…" and the preview column blanks, all self-healing on
    /// the next frame. Syncing at the point of the focus change removes the window
    /// entirely instead of racing the panel declaration order — the frame-order
    /// comments in `ui()` (which are load-bearing for the *panel rects*) then have
    /// one less thing riding on them.
    fn focus_module(&mut self, id: String) {
        if self.focused_module.as_deref() == Some(id.as_str()) {
            return;
        }
        self.focused_module = Some(id);
        self.sync_focused_draft();
    }

    /// Drop the draft for whichever module currently has focus (if any) and
    /// clear focus. Every *other* open draft is left exactly alone.
    ///
    /// *Why this no longer "exits" anywhere (S4b):* pre-S4b this restored
    /// `nav_sel` to a remembered prior view via `editor_return`/
    /// `editor_exit_target`, because `nav_sel` itself carried "which module is
    /// focused". Since focus moved to its own field that `nav_sel` never touches,
    /// there is no view to return to — `nav_sel` was never disturbed by focusing
    /// a module in the first place, so it already holds the right value.
    fn close_focused_draft(&mut self) {
        if let Some(key) = self.focused_key() {
            self.drafts.shift_remove(&key);
        }
        self.focused_module = None;
        self.refresh_unsaved_drafts();
    }

    /// Bring the focused module's draft and its two derived error states up to
    /// date. No-op when nothing is focused.
    ///
    /// *Why a method rather than three inline calls:* two callers need the whole
    /// trio — [`GuiApp::ui`] once at the top of every frame, and
    /// [`Self::focus_module`] at each focus change (issue #5). The three have to
    /// stay together: a draft whose `name_error`/`identity_error` were not
    /// refreshed alongside it is a draft the Save gate reads stale, so they are one
    /// unit rather than something a caller can half-apply.
    fn sync_focused_draft(&mut self) {
        let Some(id) = self.focused_module().map(str::to_string) else {
            return;
        };
        self.ensure_draft(&id);
        self.refresh_draft_name_error();
        self.refresh_identity_state();
    }

    /// Load a draft for `id` unless one already exists for it (keeping any
    /// in-progress edits).
    ///
    /// **Insert-if-absent, never clear.** Pre-S4a this replaced the single draft
    /// slot on every module switch — which is exactly the data loss S4a exists to
    /// remove. It also dropped the draft outright when the module vanished from
    /// `all_specs`; with a map that would be a *silent* loss of unsaved work, so
    /// an absent spec now leaves the existing (orphaned) draft alone and simply
    /// declines to build a new one. See [`Self::orphan_draft_names`].
    fn ensure_draft(&mut self, id: &str) {
        if self.draft_key(id).is_some_and(|k| self.drafts.contains_key(&k)) {
            return;
        }
        // `module_editability` and the spec lookup share one `all_specs` position
        // search, so either both succeed or the module is gone. Bail without
        // touching the map in the latter case.
        let Some((manifest, editable)) = self.module_editability(id) else {
            return;
        };
        let Some(spec) = self.all_specs.iter().find(|s| s.id() == id).cloned() else {
            return;
        };
        let baseline = serde_json::to_value(&spec).unwrap_or(Value::Null);
        // Declaration order first, set second (issue #31): `baseline_vars` stays a
        // `HashSet` because every consumer only ever asks `contains`, but the
        // *order* `var_edits` is seeded in has to be the order the user sees, so
        // it is taken from this ordered walk rather than from the set.
        let baseline_var_order: Vec<String> =
            spec.ui.values().flatten().map(|f| f.variable.clone()).collect();
        let baseline_vars: std::collections::HashSet<String> =
            baseline_var_order.iter().cloned().collect();
        let sections: Vec<(String, Vec<UiField>)> =
            spec.ui.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut ext = spec;
        ext.ui.clear(); // authoritative UI lives in `sections`
        // Seed the staged identity from the on-disk identity: Author/Name buffers
        // and one var-edit buffer per existing field variable (keyed by its
        // current name), so an unedited draft has no pending change.
        let identity = PendingIdentity {
            author: ext.meta.author.clone(),
            name: ext.meta.name.clone(),
            var_edits: baseline_var_order
                .iter()
                .map(|v| (v.clone(), v.clone()))
                .collect(),
        };
        self.drafts.insert(manifest.clone(), ModuleDraft {
            id: id.to_string(),
            manifest,
            editable,
            ext,
            sections,
            baseline,
            baseline_vars,
            name_error: None,
            identity,
            identity_error: None,
        });
    }

    /// The **only** entry point for the Save button and Ctrl+S. Saves directly
    /// when nothing is staged, and otherwise raises
    /// [`ConfirmAction::SaveWithPendingRename`] instead of writing.
    ///
    /// *Why one shared entry point:* the button and the shortcut diverging was a
    /// real bug once — whatever Save has to say, it must say from both. It also
    /// holds the one thing a keyboard shortcut needs and a button does not: a
    /// guard against acting while a modal is up. Ctrl+S is not swallowed by the
    /// dialog's backdrop (that only eats pointer input), so a user holding it
    /// down would otherwise write under an open confirmation.
    ///
    /// *Why the ask lives here and not in [`Self::save_module`]:* it leaves
    /// `save_module` a plain "write what you're told" primitive, which is what
    /// the prompt's own confirmed body-only arm calls back into.
    ///
    /// The Save gate is checked **first**, so a draft that could not be saved
    /// anyway never raises a dialog — pressing a disabled Save stays a no-op.
    fn request_save_module(&mut self) {
        if self.confirm.is_some() {
            return;
        }
        let Some(draft) = self.draft() else {
            return;
        };
        if !draft.save_enabled() {
            return;
        }
        if draft.has_pending_identity() {
            self.confirm = Some(ConfirmAction::SaveWithPendingRename);
            return;
        }
        self.save_module();
    }

    /// The **only** entry point for Ctrl+R: hot-reload extensions, then configs,
    /// from disk.
    ///
    /// *Why the `confirm` guard* (issue #38): identical reasoning to
    /// [`Self::request_save_module`]'s, one shortcut over. The confirmation
    /// dialog's backdrop eats **pointer** input only, so Ctrl+R fires straight
    /// through an open modal. The two toolbar buttons that run these same reloads
    /// individually *are* blocked by the backdrop and need no guard — which is
    /// exactly the inconsistency this closes.
    ///
    /// *Why it matters even though it cannot corrupt anything:* worth stating
    /// plainly — it can't. `perform_rename` re-checks `identity_error` at
    /// execution time and re-derives its target from the live draft, so a dialog's
    /// captured state is advisory and the whole stale-capture class is already
    /// closed downstream. The cost is data loss of a smaller kind:
    /// `reload_configs` → `switch_game` clears `text_buffers` and `multi_edit`,
    /// and any row that has not yet reached `cleaned` — a freshly added blank
    /// multi_string row, an env pair with an empty or invalid NAME — lives *only*
    /// in `multi_edit`. Reloading under a question the user has not answered yet
    /// drops those without a word.
    fn request_reload(&mut self) {
        if self.confirm.is_some() {
            return;
        }
        self.reload_extensions();
        self.reload_configs();
    }

    /// What the [`ConfirmAction::SaveWithPendingRename`] dialog's affirmative
    /// button runs. Shared by the renderer and the tests so the two cannot drift.
    ///
    /// The branch is on live `identity_error`, the *same* read the dialog labelled
    /// its button from one frame earlier — and the backdrop makes a change in
    /// between impossible:
    /// - **committable** → apply the rename, and *only* the rename. The body edits
    ///   stay pending and the user presses Save themselves, which is precisely the
    ///   "first your rename, then your changes" they asked for. Deliberately **not**
    ///   a rename-then-save combo: that coupling is the thing they rejected.
    /// - **not committable** → [`Self::perform_rename`] would filter itself out and
    ///   silently do nothing, so the affirmative offers the only action that is
    ///   actually available: write the body. Non-destructive now — the unappliable
    ///   rename stays staged for the user to fix.
    fn apply_save_with_pending_rename(&mut self) {
        if self.draft().is_some_and(|d| d.identity_error.is_none()) {
            self.perform_rename();
        } else {
            self.save_module();
        }
    }

    /// Serialize the draft's **body** and write it to its manifest via
    /// `write_atomic`. Refuses to write unless the Save gate is satisfied.
    ///
    /// **Save writes the body and nothing else** (2026-07-19, user's decision):
    /// a staged identity change is neither committed, cleared nor discarded here
    /// — it stays staged, and only [`Self::perform_rename`] commits it. *Why:*
    /// the user asked for the two operations to be genuinely independent
    /// ("rename should actually only do the renaming, and saving should actually
    /// only do the saving"), so both kinds of pending work can be committed in
    /// either order, with two clicks and no coupling gate.
    ///
    /// That independence is what decides the two exit paths below. With a staged
    /// identity still pending, dropping the draft would destroy it (the exact
    /// silent loss the original confirmation prompt existed to prevent), so the
    /// draft is re-based **in place** and the editor stays open on the
    /// still-pending rename. With nothing staged, Save keeps its historical
    /// outcome.
    ///
    /// **Not a UI entry point.** Reached only through
    /// [`Self::request_save_module`], or from the body-only arm of
    /// [`ConfirmAction::SaveWithPendingRename`] when the staged rename cannot be
    /// applied. Keeping it a plain "write what you're told" primitive is what
    /// lets that arm exist at all.
    fn save_module(&mut self) {
        let Some(draft) = self.draft() else {
            return;
        };
        if !draft.save_enabled() {
            return;
        }
        let manifest = draft.manifest.clone();
        let snap = draft.snapshot();
        let json = match serde_json::to_string_pretty(&snap) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ritz: failed to serialize module: {e:#}");
                return;
            }
        };
        if let Err(e) = core_config::write_atomic(&manifest, json.as_bytes()) {
            eprintln!("ritz: failed to save module: {e:#}");
            return;
        }
        if self.draft().is_some_and(ModuleDraft::has_pending_identity) {
            // Body landed, identity still staged: re-base the draft onto what was
            // just written so `dirty()` reads false, and keep everything else —
            // `identity` above all — untouched. Staying in the editor is not a
            // preference, it is the only way the pending rename survives: closing
            // drops the draft.
            let vars: std::collections::HashSet<String> =
                snap.ui.values().flatten().map(|f| f.variable.clone()).collect();
            let baseline = serde_json::to_value(&snap).unwrap_or(Value::Null);
            if let Some(d) = self.drafts.get_mut(&manifest) {
                d.baseline = baseline;
                d.baseline_vars = vars;
            }
            self.reload_extensions();
            self.refresh_unsaved_drafts();
            return;
        }
        // Drop **this** draft and reload from disk, then clear focus so the tree's
        // reopen invariant (or, in Config mode, nothing) puts the user back in the
        // normal view showing the saved ("real") module. Other drafts are
        // untouched — saving one module says nothing about the others.
        self.drafts.shift_remove(&manifest);
        self.reload_extensions();
        self.close_focused_draft();
    }

    /// Discard the **focused** draft only.
    ///
    /// *Why only the focused one (S4a decision):* Discard is a per-module button
    /// sitting in a per-module header. Wiping every open draft from there would be
    /// a destructive action nobody asked for. The whole-map clear is reserved for
    /// the confirmed exit path (`apply_discard_edits(Some(cat))`).
    ///
    /// In IDE mode this **re-seeds from disk and stays put**: the `Mode::Ide`
    /// invariant reopens the same module next frame anyway, so leaving the editor
    /// would be theatre. In Config mode it keeps its old meaning (clear focus,
    /// dropping the draft) — that path is currently unreachable (`focused_module`
    /// is only ever set under `Mode::Ide`), but Config-mode behaviour is not this
    /// stage's to change.
    fn discard_module(&mut self) {
        if self.mode != Mode::Ide {
            self.close_focused_draft();
            return;
        }
        if let Some(key) = self.focused_key() {
            self.drafts.shift_remove(&key);
        }
        if let Some(id) = self.focused_module().map(str::to_string) {
            self.ensure_draft(&id);
        }
        self.refresh_unsaved_drafts();
    }

    /// Carry out a confirmed [`ConfirmAction::DiscardEdits`].
    ///
    /// *Why a method and not an inline match arm* (2026-07-19): this is the whole
    /// payoff of the confirmation — the branch that decides whether the user's
    /// unsaved work is dropped *and* whether the navigation they asked for
    /// happens. Inline in `render_confirm_dialog` it is only reachable through a
    /// live egui frame, so it could never be tested; as a method it is exercised
    /// directly against a real `GuiApp` (see the tests at the bottom of this file).
    fn apply_discard_edits(&mut self, then: DiscardThen) {
        match then {
            // Close acting as Discard: drop the focused draft only.
            DiscardThen::CloseEditor => self.discard_module(),
            // A category tab was clicked and the user approved the discard.
            //
            // *Why this no longer `clear()`s the map* (2026-07-19, issue #35):
            // the destination already drops exactly what it needs — `GamesProfiles`
            // calls `close_focused_draft` (one draft), `GeneralSettings` keeps
            // every draft, `Ide` keeps every draft. Clearing here destroyed drafts
            // no destination ever asked for, including on the `GeneralSettings`
            // path where the confirmation was the *only* thing doing any damage.
            // Delegating to `select_nav_category` makes the confirmed action and
            // the plain (nothing-unsaved) action the same code path, so they
            // cannot drift: Confirm now means "yes, complete the click", and the
            // click costs precisely what `discard_names` said it would.
            DiscardThen::Nav(cat) => {
                self.select_nav_category(cat);
                self.refresh_unsaved_drafts();
            }
            // The window is closing, so every draft goes — the prompt named every
            // *unsaved* one, and the clean ones it did not name are byte-identical
            // to disk (see `discard_names`), so clearing them destroys nothing.
            //
            // The outcome is applied *here*, not when the button was clicked: a
            // launch-mode "Launch Game" that the user then cancelled out of must
            // leave `outcome` exactly as it was, or the next plain X-close would
            // silently launch the game instead of doing what the user's
            // "Editor closing action" setting says.
            DiscardThen::ExitApp(outcome) => {
                self.drafts.clear();
                self.refresh_unsaved_drafts();
                *self.outcome.lock().unwrap() = outcome;
                self.pending_close = true;
            }
        }
    }

    /// Ask to close the whole settings window with `outcome`, guarded.
    ///
    /// **The single funnel for every in-app close.** Both launch-mode title-bar
    /// buttons route through it, and the OS-close interceptor
    /// ([`Self::handle_close_request`]) applies the same predicate. Nothing in
    /// `gui.rs` may send `ViewportCommand::Close` outside the one tail-of-frame
    /// site driven by [`Self::pending_close`] — a second raw send site is exactly
    /// how the Ctrl+S / Ctrl+E / Ctrl+R bypasses of earlier stages happened.
    ///
    /// Takes no `&egui::Context` on purpose, so the decision it makes ("prompt or
    /// close") is unit-testable; the actual viewport command is issued from the
    /// tail of [`Self::ui`]. Callers must have a fresh `unsaved_drafts`.
    fn request_app_close(&mut self, outcome: EditOutcome) {
        if !close_needs_confirm(!self.unsaved_drafts.is_empty(), self.pending_close) {
            *self.outcome.lock().unwrap() = outcome;
            self.pending_close = true;
            return;
        }
        // Re-entrancy: a confirmation is already on screen. Leave it alone rather
        // than overwriting the decision the user is mid-way through making — the
        // modal backdrop means they can still only answer one question at a time,
        // and clobbering (say) a pending "Delete Module" with a discard prompt
        // would silently cancel an action they had already chosen. The close is
        // simply dropped; asking again once the dialog is answered works.
        if self.confirm.is_none() {
            self.confirm =
                Some(ConfirmAction::DiscardEdits { then: DiscardThen::ExitApp(outcome) });
        }
    }

    /// Intercept the **OS** window close — the X button, Alt+F4, or a window
    /// manager close — and stop it if some draft holds unsaved work.
    ///
    /// Also catches the close this app re-issues itself, since eframe routes
    /// `ViewportCommand::Close` back through the same `close_requested()` flag;
    /// [`Self::pending_close`] is what lets an approved close pass straight
    /// through.
    ///
    /// Must run after the frame's [`Self::refresh_unsaved_drafts`] and before the
    /// dialog renders, so the prompt it raises is visible on the same frame the
    /// close was cancelled.
    fn handle_close_request(&mut self, ctx: &egui::Context) {
        if !ctx.input(|i| i.viewport().close_requested()) {
            return;
        }
        if !close_needs_confirm(!self.unsaved_drafts.is_empty(), self.pending_close) {
            // Nothing to lose: let it close. Not making the common case cost a
            // click is the whole reason this is a guard and not a "really quit?"
            // prompt.
            return;
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        // The OS close carries no outcome of its own, so it keeps whatever the
        // "Editor closing action" setting put in `outcome` (`GuiApp::new`).
        let outcome = *self.outcome.lock().unwrap();
        if self.confirm.is_none() {
            self.confirm =
                Some(ConfirmAction::DiscardEdits { then: DiscardThen::ExitApp(outcome) });
        }
    }

    /// `Author::Name` of every module that currently has unsaved edits.
    ///
    /// *Why a `Vec`:* the discard prompt names the modules at risk, and the
    /// wording the user asked for is already plural ("These modules have unsaved
    /// changes: …"). S4a made it list several drafts; neither the dialog text nor
    /// its callers needed rewording.
    ///
    /// Reads the frame's cached [`Self::unsaved_drafts`] rather than calling
    /// `dirty()` again — see that field's doc. Callers outside the render loop
    /// (tests included) must `refresh_unsaved_drafts` first.
    ///
    /// Names come from `ext.meta` (the on-disk identity), not from the staged
    /// `identity` buffers: a half-typed rename is not what the user recognises
    /// the module by.
    fn unsaved_module_names(&self) -> Vec<String> {
        self.drafts
            .iter()
            .filter(|(k, _)| self.unsaved_drafts.contains(*k))
            .map(|(_, d)| format!("{}::{}", d.ext.meta.author, d.ext.meta.name))
            .collect()
    }

    /// Does the **focused** draft hold unsaved work?
    ///
    /// The gate for every gesture whose destination drops only the focused draft
    /// ([`nav_category_drops_draft`]'s `GamesProfiles` arm, the editor's Close
    /// button). Reads the frame's cached [`Self::unsaved_drafts`], like
    /// [`Self::unsaved_module_names`], so callers outside the render loop must
    /// `refresh_unsaved_drafts` first.
    fn focused_draft_unsaved(&self) -> bool {
        self.focused_key()
            .is_some_and(|k| self.unsaved_drafts.contains(&k))
    }

    /// `Author::Name` of exactly the drafts that confirming `then` will destroy —
    /// the list the discard dialog prints (2026-07-19, issue #35).
    ///
    /// *Why this exists at all:* the dialog used to print
    /// [`Self::unsaved_module_names`] — *every* unsaved draft — for all three
    /// destinations, while [`Self::apply_discard_edits`] destroyed a different set
    /// per destination. A prompt that names a set it does not destroy is
    /// misleading; one that destroys a set it did not name is silent data loss.
    /// This function is the single answer both halves now read, so the dialog
    /// cannot promise less (or more) than the destination takes.
    ///
    /// The three arms mirror `apply_discard_edits` exactly:
    /// - `CloseEditor` → `discard_module`, the focused draft only.
    /// - `Nav(cat)` → whatever `select_nav_category(cat)` drops: the focused draft
    ///   for `GamesProfiles`, nothing for `GeneralSettings`/`Ide` (which is why
    ///   `nav_category_drops_draft` never raises the prompt for those two — an
    ///   empty list here would be a dialog with nothing to say).
    /// - `ExitApp` → every unsaved draft; the process is ending, so all of it goes.
    ///
    /// *Why the `ExitApp` arm names only the **unsaved** drafts even though
    /// `apply_discard_edits` clears the whole map:* a clean, unstaged draft is
    /// byte-identical to its manifest and can be rebuilt from disk by reopening
    /// the module, so dropping it destroys nothing the user could notice. Naming
    /// it would pad the prompt with modules that are not at risk. The map-clear is
    /// a cache eviction for those entries and a real loss only for the unsaved
    /// ones — which is precisely the set listed here.
    fn discard_names(&self, then: DiscardThen) -> Vec<String> {
        let focused = || {
            self.focused_key()
                .filter(|k| self.unsaved_drafts.contains(k))
                .and_then(|k| self.drafts.get(&k))
                .map(|d| format!("{}::{}", d.ext.meta.author, d.ext.meta.name))
                .into_iter()
                .collect()
        };
        match then {
            DiscardThen::CloseEditor => focused(),
            DiscardThen::Nav(NavCategory::GamesProfiles) => focused(),
            DiscardThen::Nav(NavCategory::GeneralSettings | NavCategory::Ide) => Vec::new(),
            DiscardThen::ExitApp(_) => self.unsaved_module_names(),
        }
    }

    /// True when some **other open draft** has staged the identity `(author,
    /// name)`, excluding the draft at `exclude` (matched by manifest path).
    ///
    /// *Why this is needed on top of [`name_collides`] (S4a):* `name_collides`
    /// compares against **disk** only. With one draft that was complete — nothing
    /// else could be staging an identity. With several, two dirty drafts can both
    /// stage `(Ritze, Alpha)` and neither sees the other, so both Save gates open
    /// and the second Save silently clobbers the first module's namespace. The
    /// failure is invisible until the user notices a module missing.
    ///
    /// The *pending* identity is what's compared, because that is what Rename
    /// would write. For a draft with no pending change the pending identity equals
    /// the on-disk one, so including it here is a harmless superset of what
    /// `name_collides` already reports.
    fn draft_identity_collides(
        &self,
        author: &str,
        name: &str,
        exclude: Option<&Path>,
    ) -> bool {
        let (author, name) = (author.trim(), name.trim());
        self.drafts.iter().any(|(path, d)| {
            exclude != Some(path.as_path())
                && d.identity.staged_author() == author
                && d.identity.staged_name() == name
        })
    }

    /// Recompute the focused draft's live name-collision flag (Version-blind,
    /// excluding itself by manifest path), against **disk and every other open
    /// draft's staged identity** — see [`Self::draft_identity_collides`].
    fn refresh_draft_name_error(&mut self) {
        let flag = self.draft().map(|d| {
            name_collides(
                &self.all_specs,
                &self.all_manifests,
                &d.ext.meta.author,
                &d.ext.meta.name,
                Some(d.manifest.as_path()),
            ) || self.draft_identity_collides(
                &d.ext.meta.author,
                &d.ext.meta.name,
                Some(d.manifest.as_path()),
            )
        });
        if let (Some(collides), Some(draft)) = (flag, self.draft_mut()) {
            draft.name_error = collides.then(|| "Author + Name already in use".to_string());
        }
    }

    /// Recompute the focused draft's pending-identity validity. The (Author, Name)
    /// collision check is Version-blind and excludes the module itself by manifest
    /// path — same rule as [`Self::refresh_draft_name_error`], but run against the
    /// *pending* (edited) Author/Name rather than the on-disk pair.
    ///
    /// **Focused draft only** — deliberately. Why refreshing every draft is not
    /// needed (and what would make it needed) is on
    /// [`ModuleDraft::identity_error`], issue #32.
    fn refresh_identity_state(&mut self) {
        let collides = self.draft().map(|d| {
            name_collides(
                &self.all_specs,
                &self.all_manifests,
                d.identity.staged_author(),
                d.identity.staged_name(),
                Some(d.manifest.as_path()),
            ) || self.draft_identity_collides(
                d.identity.staged_author(),
                d.identity.staged_name(),
                Some(d.manifest.as_path()),
            )
        });
        if let (Some(collides), Some(draft)) = (collides, self.draft_mut()) {
            draft.identity_error = draft.compute_identity_error(collides);
        }
    }

    /// Open the Fork dialog seeded from the module `id` (its Author, and
    /// "`<name>` (copy)" as the default fork name).
    fn open_fork_dialog(&mut self, id: &str) {
        let Some(spec) = self.all_specs.iter().find(|s| s.id() == id) else {
            return;
        };
        let author = if spec.meta.author.is_empty() {
            "Ritze".to_string()
        } else {
            spec.meta.author.clone()
        };
        let name = format!("{} (copy)", spec.meta.name);
        self.module_dialog = Some(ModuleDialog {
            kind: DialogKind::Fork { parent_id: id.to_string() },
            author,
            name,
            copy_settings: true,
        });
    }

    /// Open the Create-new-module dialog (empty template; no copy-settings option).
    fn open_create_dialog(&mut self) {
        self.module_dialog = Some(ModuleDialog {
            kind: DialogKind::Create,
            author: "Ritze".to_string(),
            name: "New Module".to_string(),
            copy_settings: false,
        });
    }

    /// Deep-copy the parent module, retag it with the new Author/Name + `ForkedFrom`
    /// lineage, write it to the user extensions dir under a unique slug, optionally
    /// snapshot the parent's stored config into the fork's namespace, then reload
    /// and open the fork in the editor.
    fn perform_fork(&mut self, parent_id: &str, author: String, name: String, copy_settings: bool) {
        self.carryover_report = None;
        // Source the **draft** when one is open for the parent, falling back to
        // disk. Pre-S4a this read `all_specs` unconditionally, so forking a module
        // you had just edited produced a fork of the *saved* version and silently
        // dropped every unsaved change (issue #20). Multi-draft makes the right
        // behaviour free: the fork carries the edits AND the original draft keeps
        // them, because nothing has to be evicted to make room.
        let from_draft = self
            .drafts
            .values()
            .find(|d| d.id == parent_id)
            .map(|d| d.snapshot());
        let Some(parent) = from_draft
            .or_else(|| self.all_specs.iter().find(|s| s.id() == parent_id).cloned())
        else {
            return;
        };
        let parent_author = parent.meta.author.clone();
        let parent_name = parent.meta.name.clone();
        let mut ext = parent;
        ext.meta.author = author.clone();
        ext.meta.name = name.clone();
        ext.meta.forked_from = Some(format!("{parent_author}::{parent_name}"));

        let path = unique_slug_path(&self.paths.user_extensions(), &author, &name);
        let json = match serde_json::to_string_pretty(&ext) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ritz: failed to serialize fork: {e:#}");
                return;
            }
        };
        if let Err(e) = core_config::write_atomic(&path, json.as_bytes()) {
            eprintln!("ritz: failed to write fork: {e:#}");
            return;
        }

        if copy_settings {
            match core_config::snapshot_config_to_fork(
                &self.paths,
                (&parent_author, &parent_name),
                (&author, &name),
            ) {
                Ok(report) => self.set_carryover_report(report),
                Err(e) => eprintln!("ritz: fork config snapshot failed: {e:#}"),
            }
        }

        // The parent's draft (if any) is deliberately **left in place** — forking
        // is not a decision to abandon the original's edits. Focus moves to the
        // fork; `ensure_draft` builds its draft next frame. `nav_sel` is
        // untouched (S4b) — Fork only ever runs from inside an already-focused
        // editor, so it has nothing to say about which config scope is active.
        let new_id = ext.id();
        self.reload_extensions();
        self.reload_configs();
        self.focused_module = Some(new_id.clone());
        // Issue #37: the tree must highlight what the editor is editing. The fork
        // is a brand-new row, inserted at its alphabetical position, so the index
        // `reload_extensions` re-anchored (the *parent*'s) is not it.
        self.anchor_ide_selection(Some(&new_id));
    }

    /// Commit the staged identity change (Author / Name / existing-field
    /// `Variable`) for the open editable module. Safety-critical **ordering**:
    ///
    /// 1. compute `from` = current on-disk identity, `to` = new identity, and the
    ///    `old→new` var renames from the changed existing fields;
    /// 2. **scope sweep FIRST** — [`migrate_renamed_module`] moves stored config
    ///    across every scope; on error, abort **without** touching the manifest;
    /// 3. build the new manifest from the **on-disk body** (`ModuleDraft::baseline`)
    ///    with the new Author/Name applied and in-module `{var}`/`Requires`
    ///    references + field `Variable`s rewritten via [`apply_var_renames`];
    /// 4. write the manifest **LAST, in place** (file never renamed; `id()` comes
    ///    from JSON meta) — only after the sweep succeeded.
    ///
    /// Because the sweep is idempotent and the manifest is written last, a crash
    /// between steps 2 and 4 leaves the manifest on the OLD identity, so
    /// re-pressing Rename re-runs cleanly (already-moved scopes no-op).
    ///
    /// **Rename writes the identity and nothing else** (2026-07-19, user's
    /// decision). It used to build the manifest from `snapshot()` — the whole
    /// *in-memory* body — so pressing Rename silently saved the body too. The
    /// user's call: "rename should actually only do the renaming, and saving
    /// should actually only do the saving". Body edits therefore stay pending in
    /// the draft and are still dirty after the rename lands; Save commits them.
    ///
    /// *Why the body comes from `baseline` and not a re-read of the file:*
    /// `baseline` **is** the on-disk state by definition — it is the value
    /// [`ModuleDraft::dirty`] measures against. Re-reading would introduce a
    /// second source of truth that can disagree with it (an external edit, or a
    /// text-vs-serde round-trip difference), and then the post-rename baseline
    /// bookkeeping below would be re-basing onto bytes the dirty check never saw.
    /// One source, so the write and the dirty check cannot drift apart.
    ///
    /// *Why the old `!dirty() || save_enabled()` gate is gone:* it existed purely
    /// because Rename wrote the body — it stopped Rename committing unsaved (and
    /// possibly invalid) body edits behind the user's back. Rename no longer
    /// writes the body, so a dirty or even invalid body is not a reason to refuse
    /// a rename. Renaming with a half-finished field in progress must work.
    fn perform_rename(&mut self) {
        // Snapshot everything we need out of the draft so the immutable borrow is
        // released before we touch `self.paths` / reload.
        let Some((mut ext, manifest, old_author, old_name, new_author, new_name, var_rename)) = self
            .draft()
            .filter(|d| d.editable && d.has_pending_identity() && d.identity_error.is_none())
            .and_then(|d| {
                // The on-disk body. `from` is read off it too (rather than off
                // `d.ext.meta`), so the identity the scope sweep migrates *from*
                // and the body being rewritten are provably the same file state.
                let disk: Extension = serde_json::from_value(d.baseline.clone()).ok()?;
                let (old_author, old_name) = (disk.meta.author.clone(), disk.meta.name.clone());
                Some((
                    disk,
                    d.manifest.clone(),
                    old_author,
                    old_name,
                    d.identity.staged_author().to_string(),
                    d.identity.staged_name().to_string(),
                    d.changed_var_renames(),
                ))
            })
        else {
            return;
        };

        self.carryover_report = None;
        let from = (old_author.as_str(), old_name.as_str());
        let to = (new_author.as_str(), new_name.as_str());

        // Step 2: scope sweep FIRST. On error, abort before the manifest is touched.
        let report =
            match core_config::migrate_renamed_module(&self.paths, from, to, &var_rename) {
                Ok(r) => r,
                Err(e) => {
                    self.carryover_report =
                        Some(format!("Rename aborted: config migration failed: {e}"));
                    return;
                }
            };

        // Step 2b: the IDE preview's scratch layer lives only in memory, so the
        // disk sweep never reaches it — pre-S4a its values silently orphaned under
        // the old identity the moment a rename landed, and the preview column
        // started showing defaults for settings the user could still see in the
        // form. Same remap, same `remove_source: true`; run here, immediately
        // after the sweep, so the in-memory layer and the on-disk ones can never
        // be left describing different identities.
        core_config::remap_one_scope(
            &mut self.preview_config.config.modules.authors,
            from,
            to,
            &var_rename,
            &std::collections::HashSet::new(),
            true,
        );

        // Step 3: build the new manifest — the ON-DISK body (`ext` came from
        // `baseline`) with the new identity and the reference rewrite applied.
        // The draft's in-memory body edits are deliberately NOT part of this.
        ext.meta.author = new_author.clone();
        ext.meta.name = new_name.clone();
        apply_var_renames(&mut ext, &var_rename);

        // Step 4: write the manifest LAST, in place, only after the sweep succeeded.
        let json = match serde_json::to_string_pretty(&ext) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ritz: failed to serialize renamed module: {e:#}");
                return;
            }
        };
        if let Err(e) = core_config::write_atomic(&manifest, json.as_bytes()) {
            eprintln!("ritz: failed to write renamed module: {e:#}");
            return;
        }

        // Step 5: report dropped vars, reload, keep focus on the new id. `nav_sel`
        // is untouched (S4b) — Rename only ever runs from inside an already-focused
        // editor.
        let new_id = ext.id();
        self.set_carryover_report(report);
        self.reload_extensions();
        self.reload_configs();
        self.focused_module = Some(new_id.clone());
        // Issue #37: a rename changes `meta.name`, which is the key
        // `load_extensions` sorts on — so the module moves rows even though the
        // file did not. `reload_extensions` re-anchored on the *old* id, which no
        // longer exists; point the tree at the new one.
        self.anchor_ide_selection(Some(&new_id));
        // Step 6: re-base the draft **in place**, keeping its body edits.
        //
        // Manifest-path keying means the entry does not need re-keying (the file
        // was rewritten in place, never renamed). It used to be dropped and
        // re-seeded from disk via `ensure_draft` — correct only while Rename also
        // wrote the body, since then disk and draft agreed. Now that the body
        // stays pending, re-seeding would silently throw those edits away, so the
        // stale parts (`id`, `baseline`, `baseline_vars`, staged `identity`) are
        // updated field by field instead.
        //
        // The live body is carried across the rename too: its Author/Name become
        // the new pair (they are the on-disk identity `has_pending_identity`
        // compares against) and `apply_var_renames` runs over it, so a renamed
        // variable is renamed in the pending edits as well. Without that, a later
        // Save would write the OLD variable names back and undo the rename in the
        // manifest while the config sweep had already moved the stored values.
        //
        // Post-state, by construction: `has_pending_identity()` false (staged ==
        // on-disk), `dirty()` unchanged from before the rename (body edits vs. an
        // equally-renamed baseline), `has_unsaved_work()` therefore exactly
        // "there are body edits".
        // Ordered walk first, set second — same rule as `ensure_draft` (issue #31).
        let new_var_order: Vec<String> =
            ext.ui.values().flatten().map(|f| f.variable.clone()).collect();
        let new_vars: std::collections::HashSet<String> =
            new_var_order.iter().cloned().collect();
        let new_baseline = serde_json::to_value(&ext).unwrap_or(Value::Null);
        if let Some(d) = self.drafts.get_mut(&manifest) {
            // Rewrite references in the live body. `apply_var_renames` works on an
            // `Extension`, whose `ui` is an `IndexMap` — folding `sections` into
            // one would collapse two identically-named sections (legal in a draft
            // mid-edit, blocked only at the Save gate) and lose one. So the fields
            // travel through a single synthetic section and are split back by the
            // original per-section counts, leaving section names untouched.
            let counts: Vec<usize> = d.sections.iter().map(|(_, f)| f.len()).collect();
            let mut live = d.ext.clone();
            live.ui = std::iter::once((
                String::new(),
                d.sections.iter().flat_map(|(_, f)| f.iter().cloned()).collect::<Vec<_>>(),
            ))
            .collect();
            live.meta.author = new_author.clone();
            live.meta.name = new_name.clone();
            apply_var_renames(&mut live, &var_rename);
            let mut flat = std::mem::take(&mut live.ui)
                .into_iter()
                .next()
                .map(|(_, f)| f)
                .unwrap_or_default()
                .into_iter();
            for ((_, fields), n) in d.sections.iter_mut().zip(counts) {
                *fields = flat.by_ref().take(n).collect();
            }
            d.ext = live; // `ui` emptied by the take above — sections stay authoritative
            d.id = new_id.clone();
            d.baseline = new_baseline;
            d.baseline_vars = new_vars;
            // Re-seed the staged identity from the (new) on-disk identity, so
            // nothing reads as pending. Per-field var buffers are re-seeded lazily
            // by the editor (`or_insert_with`), keyed by each field's current name.
            d.identity = PendingIdentity {
                author: new_author,
                name: new_name,
                var_edits: new_var_order.iter().map(|v| (v.clone(), v.clone())).collect(),
            };
            d.identity_error = None;
        }
        self.refresh_unsaved_drafts();
    }

    /// Write a minimal valid template module (meta + one empty UI section) to the
    /// user extensions dir, then reload and open it in the editor.
    fn perform_create(&mut self, author: String, name: String) {
        self.carryover_report = None;
        let template = json!({
            "Extension": { "Name": name, "Author": author, "Version": "1.0" },
            "UI": { "General": [] }
        });
        let ext: Extension = match serde_json::from_value(template) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("ritz: failed to build template module: {e:#}");
                return;
            }
        };
        if let Err(e) = core_extension::validate(&ext) {
            eprintln!("ritz: template module failed validation: {e:#}");
            return;
        }
        let path = unique_slug_path(&self.paths.user_extensions(), &author, &name);
        let json = match serde_json::to_string_pretty(&ext) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("ritz: failed to serialize template module: {e:#}");
                return;
            }
        };
        if let Err(e) = core_config::write_atomic(&path, json.as_bytes()) {
            eprintln!("ritz: failed to write template module: {e:#}");
            return;
        }
        // Existing drafts are left alone: creating a module says nothing about the
        // ones already open. Focus moves to the new module; `ensure_draft` builds
        // its draft next frame. `nav_sel` is untouched (S4b) — Create is reached
        // only from the IDE nav footer, which only shows while a module is
        // already focused.
        let new_id = ext.id();
        self.reload_extensions();
        self.focused_module = Some(new_id.clone());
        // Issue #37, same as `perform_fork`: a new alphabetical row, so the tree
        // selection has to be moved onto it explicitly.
        self.anchor_ide_selection(Some(&new_id));
    }

    /// Delete a user-authored module's manifest file. When `purge` is set, also
    /// sweep the now-undeclared config across every scope via [`config_cleanup`].
    /// Clears focus and points `nav_sel` at the ambient game.
    fn delete_module(&mut self, manifest: &Path, purge: bool) {
        if let Err(e) = std::fs::remove_file(manifest) {
            eprintln!("ritz: failed to delete module: {e:#}");
            return;
        }
        // Only the deleted module's draft goes: the map is keyed by manifest path,
        // which is exactly what was just unlinked. Every other draft survives.
        self.drafts.shift_remove(manifest);
        self.refresh_unsaved_drafts();
        self.reload_extensions();
        if purge {
            // The deleted module's vars are now undeclared, so cleanup removes
            // exactly them (alongside any other stale values it normally prunes).
            self.config_cleanup();
        }
        // Clear focus so the `Mode::Ide` reopen invariant (top of `GuiApp::ui`)
        // picks up whatever the tree has selected next frame, instead of trying
        // to reopen the now-deleted id. `nav_sel` is deliberately reset to the
        // ambient game here too (S4b): since nothing else in an IDE session
        // touches `nav_sel` any more, this is what decides where the user lands
        // if they leave IDE mode without navigating further — reproducing the
        // pre-S4b outcome, where the same assignment overwrote whatever
        // `editor_return` would otherwise have restored.
        self.focused_module = None;
        self.nav_sel = NavSel::Game(self.appid.clone());
    }

    /// Format a fork/rename dropped-var report into the transient carryover
    /// message (cleared when empty — a clean copy carries everything over).
    fn set_carryover_report(&mut self, report: Vec<(String, Vec<String>)>) {
        if report.is_empty() {
            return;
        }
        let count: usize = report.iter().map(|(_, vars)| vars.len()).sum();
        let mut parts = Vec::new();
        for (scope, vars) in &report {
            for v in vars {
                parts.push(format!("{v} ({scope})"));
            }
        }
        self.carryover_report = Some(format!(
            "{count} setting(s) couldn't be carried over: {}",
            parts.join(", ")
        ));
    }

    /// Save whichever config store `nav_sel` currently names.
    ///
    /// **No-op under `Mode::Ide` (S4b).** There used to be a "config-autosave
    /// interlock" here (`pending_config_write` / `config_autosave_held` /
    /// `flush_config_writes_if_clean`) that withheld this write to disk while a
    /// module draft was dirty, flushing once every draft went clean again. That
    /// machinery only existed to guard against IDE mode's field writes reaching a
    /// real scope — but IDE mode's field writes never do: the manifest editor
    /// mutates the open `ModuleDraft` directly, and the preview column runs
    /// exclusively inside `with_preview_writes` (`WriteTarget::Preview`), so
    /// `changed` from a real config write is already false-or-absent for the
    /// whole time `Mode::Ide` is showing. The one place `changed` *does* come
    /// back `true` under `Mode::Ide` is `render_confirm_dialog`, which returns
    /// `true` unconditionally on Confirm (e.g. `DeleteModule`/`DiscardEdits`) even
    /// though neither touches `game_config`/`global_config`/`editing_preset_buf`.
    /// The interlock used to let that call through harmlessly (re-saving an
    /// unmodified `game_config`); this guard is strictly stronger — it closes the
    /// call off entirely rather than relying on it being harmless.
    fn persist(&self) {
        if self.mode == Mode::Ide {
            return;
        }
        let result = match &self.nav_sel {
            NavSel::GlobalSettings => self.paths.save_global_config(&self.global_config),
            NavSel::Profile(_) => match &self.editing_preset_buf {
                Some(p) => self.paths.save_preset(p),
                None => return,
            },
            NavSel::GeneralSettings | NavSel::Game(_) => {
                self.paths.save_game(&self.game_config)
            }
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
            self.request_reload();
        }

        // *Why there is no longer a frame-start draft-drop guard here (S4a):* it
        // used to destroy the open draft the instant `nav_sel` moved off the
        // editor by ANY route, without a word — which is precisely the data loss
        // this stage removes. Drafts now outlive navigation; the only routes that
        // destroy one are Save, the per-module Discard, Delete, and the confirmed
        // whole-map clear behind `ConfirmAction::DiscardEdits`. `persist()`'s
        // `Mode::Ide` no-op (S4b) is what stands in for the old interlock now —
        // see its doc comment.

        // IDE-mode invariant: a module is ALWAYS focused. `focused_module` is IDE
        // mode's "which module" carrier (S4b; `nav_sel` plays no part in it), so
        // anything that clears it (the editor's Close button, a Save, a Delete)
        // would otherwise drop the central column into Config mode's module-detail
        // view inside an IDE-shaped window. Re-open the tree's current selection
        // instead, which makes Close read as "discard and reload" — the right
        // meaning when there is nowhere to close to. Runs AFTER the draft-drop
        // guard above so Close's discard still happens.
        if self.mode == Mode::Ide && self.focused_module().is_none() {
            if let Some(spec) = self.all_specs.get(self.ide_selected) {
                let id = spec.id();
                self.focus_module(id);
            }
        }

        // Keep `cur_specs` in step with the screen being shown. `nav_sel` moves
        // from a dozen call sites (nav tree, category tabs, editor open/close,
        // delete, fork…), and the module list has to follow all of them — so the
        // check lives here, once per frame, rather than being sprinkled next to
        // every assignment where the next one added would forget it. Comparing the
        // *filter* (not rebuilding unconditionally) keeps it to one rebuild per
        // screen change; `switch_game`/`reload_extensions` still rebuild eagerly
        // because they change the underlying data, not just the filter.
        if self.cur_specs_filter != self.cur_specs_filter() {
            self.rebuild_cur_specs();
        }

        // Make sure the focused module has a draft (insert-if-absent; never
        // touches the other entries), then recompute this frame's dirty set —
        // ONCE, here, for every consumer downstream. See `unsaved_drafts`.
        self.sync_focused_draft();
        self.refresh_unsaved_drafts();
        // Guard the OS window close (X / Alt+F4 / WM) against unsaved drafts.
        // Placed right after the dirty set is rebuilt and well before any panel
        // renders, so the prompt it may raise shows on this very frame — the
        // frame in which the close was cancelled.
        self.handle_close_request(ctx);
        if self.focused_module().is_some() {
            // Ctrl+S: Save when the editor is open and the Save gate is satisfied
            // (same condition as the button); a no-op otherwise. Save exits the
            // editor on success — unless a rename is staged, which it leaves
            // staged and stays put for.
            //
            // Routes through `request_save_module` — the *same* entry point as
            // `TopAction::Save` — so the keyboard can bypass neither its modal
            // guard nor the staged-rename notice. `request_save_module` re-checks
            // the Save gate itself; `save_ok` stays only to skip the input poll.
            let save_ok = self.draft().is_some_and(|d| d.save_enabled());
            if save_ok && ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::S)) {
                self.request_save_module();
                self.refresh_unsaved_drafts();
            }
        }

        let edit_resolution = self.resolve_for_editing();
        // The launch preview's own spec list, kept separate from `cur_specs`:
        // on the Global Profile / Profile screens the latter is unfiltered, but a
        // launch command is always for one game. Equal to `cur_specs` everywhere
        // else, so no other screen changes.
        let game_specs = self.game_specs();
        let game_resolution = self.resolve_specs_for_game(&game_specs);
        let mut changed = false;

        self.render_title_bar(ctx);

        egui::SidePanel::left("nav")
            .exact_width(NAV_W)
            .resizable(false)
            .frame(egui::Frame::none().fill(theme::PANEL))
            .show(ctx, |ui| {
                self.render_nav_panel(ui);
            });
        // The nav panel may have just changed `nav_sel`; drop any detection belonging
        // to the scope we left before anything renders its "Detecting… Ns" countdown.
        // `poll_detect` re-checks at the end of the frame to catch the other nav sites.
        self.cancel_stale_detect();

        // The module-list column exists only in Config mode: IDE mode moves the
        // module tree into the nav column, so a second copy here would be redundant
        // (and would eat the width the editor/preview split needs).
        if self.mode == Mode::Config && !matches!(self.nav_sel, NavSel::GeneralSettings) {
        egui::SidePanel::left("ext_list")
            .exact_width(NAV_W)
            .resizable(false)
            .frame(egui::Frame::none().fill(theme::PANEL))
            .show(ctx, |ui| {
            egui::TopBottomPanel::top("ext_header")
                .show_separator_line(false)
                .frame(egui::Frame::none()
                    .fill(theme::PANEL)
                    .inner_margin(egui::Margin { left: 16.0, right: 16.0, top: 14.0, bottom: 8.0 }))
                .show_inside(ui, |ui| {
                    // Create-module now lives only in IDE Mode's nav footer
                    // (`render_ide_nav_footer`'s "+ New Module" button, which shares
                    // `open_create_dialog` with what used to be here) — this header
                    // used to carry its own "+" affordance too, but IDE Mode has
                    // taken over module creation/editing, so a second entry point
                    // here was redundant. Plain label now, no trailing button.
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
                            self.render_ext_errors_banner(ui);
                            // The MODULES tree is inert (and greyed) while the
                            // module editor is open, so a stray click can't swap
                            // the module out from under an in-progress edit. Exit
                            // is via Close / Save (or Ctrl+S) only.
                            let tree_live = self.drafts.is_empty();
                            ui.add_enabled_ui(tree_live, |ui| {
                                // `render_ext_tree` now takes its data source as
                                // parameters (S3a) so S3b can reuse it against
                                // `all_specs`/`all_dirs`/`all_is_folder_ext` for a
                                // read-only preview column. Clone/copy out of `self`
                                // first — passing `&self.cur_specs`/`&mut
                                // self.selected_ext` directly would borrow `self`
                                // while the method call also needs `&mut self`.
                                let tree_specs = self.cur_specs.clone();
                                let tree_dirs = self.cur_dirs.clone();
                                let tree_is_folder_ext = self.cur_is_folder_ext.clone();
                                let mut tree_selected = self.selected_ext;
                                let show_inheritance = self.show_inheritance;
                                self.render_ext_tree(
                                    ui,
                                    &tree_specs,
                                    &tree_dirs,
                                    &tree_is_folder_ext,
                                    &mut tree_selected,
                                    show_inheritance,
                                    // No dirty markers in Config mode: this tree
                                    // renders `cur_specs`, which has no
                                    // manifest-path alignment to key the set on,
                                    // and the column locks whenever a draft
                                    // exists anyway. Config mode is unchanged.
                                    &[],
                                );
                                self.selected_ext = tree_selected;
                            });
                        });
                });
        });
        } // end if !GeneralSettings

        // The spec list both IDE columns (preview body + launch band) resolve and
        // assemble against: the ambient game's applicable modules, with the module
        // under edit spliced over its entry — or *appended* when it isn't in that
        // list at all. The append matters because the IDE tree browses the
        // unfiltered `all_specs`: without it, opening a module that doesn't apply to
        // this game would show a preview that silently ignores everything you type.
        // Empty (and never used) outside IDE mode, so Config mode is untouched.
        //
        // S5b: the applicability filter follows the **preview** game when one is
        // selected, not the ambient one. With a preview game chosen, the ambient
        // game's filter is the wrong one — modules gated to the previewed game
        // would be missing from the launch string, and modules gated to the ambient
        // game would be in it, both silently. `None` falls back to the ambient
        // game's set (the pre-S5 behaviour).
        //
        // `game_specs`, not `cur_specs`: in IDE mode the two are equal (`nav_sel`
        // is always `ModuleEditor(_)`, whose filter *is* the ambient game), but
        // this list feeds a launch command, and `game_specs` is the list that is
        // game-filtered by definition rather than by which screen is showing.
        let ide_specs: Vec<Extension> = if self.mode == Mode::Ide {
            let mut specs: Vec<Extension> = match self.preview_game.clone() {
                Some(pid) => self
                    .all_specs
                    .iter()
                    .filter(|s| s.applies_to(&pid))
                    .cloned()
                    .collect(),
                None => game_specs.clone(),
            };
            // Splice (replace in place) every **dirty** draft that is already in
            // this list, so the launch string reflects unsaved edits to any open
            // module — not just the focused one.
            //
            // *Why splice-only for the non-focused ones (S4a):* appending a
            // non-focused draft would inject a module that could never run for
            // this game, and `push` puts it at the END of the fold order, which
            // silently changes `lossy_env_overwrites` attribution and wrapper
            // `Priority` ordering. Replacement in place preserves both.
            for (path, d) in &self.drafts {
                if !self.unsaved_drafts.contains(path) {
                    continue;
                }
                if let Some(pos) = specs.iter().position(|s| s.id() == d.id) {
                    specs[pos] = d.snapshot();
                }
            }
            // Only the FOCUSED module may be appended when it isn't in the list at
            // all: the IDE tree browses the unfiltered `all_specs`, so without this
            // opening a module that doesn't apply to this game would show a preview
            // that silently ignores everything you type.
            if let Some(id) = self.focused_module() {
                let snap = self
                    .drafts
                    .values()
                    .find(|d| d.id == id)
                    .map(|d| d.snapshot())
                    .or_else(|| self.all_specs.iter().find(|s| s.id() == id).cloned());
                if let Some(snap) = snap {
                    match specs.iter().position(|s| s.id() == id) {
                        Some(pos) => specs[pos] = snap,
                        None => specs.push(snap),
                    }
                }
            }
            specs
        } else {
            Vec::new()
        };

        // Assemble the launch preview. In the module editor, splice the in-memory
        // draft over its on-disk entry so the preview reflects unsaved edits, and
        // lint for env values one module discards out from under another.
        let (preview, diagnostics): (String, Vec<EnvOverwrite>) = if self.mode == Mode::Ide {
            // S5b: resolve the band through the scratch layer, the same way
            // `render_ide_preview_panel` resolves its form. `resolve_specs_for_game`
            // would assemble against the ambient game's stored config, so the band
            // would ignore both the preview game selection and every value the user
            // set in the preview column — actively misleading, since predicting the
            // launch string is what the band is for.
            let res = self.resolve_specs_for_preview(&ide_specs);
            let text = context::assemble_launch(&ide_specs, &res, &self.game_command)
                .map(|lc| lc.to_string())
                .unwrap_or_else(|e| format!("<error: {e}>"));
            let coll = lossy_env_overwrites(&ide_specs, &res);
            (text, coll)
        } else if self.focused_module().is_some() {
                match self.draft() {
                    Some(draft) => {
                        let mut preview_specs = game_specs.clone();
                        if let Some(pos) =
                            preview_specs.iter().position(|s| s.id() == draft.id)
                        {
                            preview_specs[pos] = draft.snapshot();
                        }
                        let res = self.resolve_specs_for_game(&preview_specs);
                        let text =
                            context::assemble_launch(&preview_specs, &res, &self.game_command)
                                .map(|lc| lc.to_string())
                                .unwrap_or_else(|e| format!("<error: {e}>"));
                        let coll = lossy_env_overwrites(&preview_specs, &res);
                        (text, coll)
                    }
                    None => (String::new(), Vec::new()),
                }
            } else {
                let preview_res = if matches!(self.nav_sel, NavSel::Profile(_)) {
                    &edit_resolution
                } else {
                    &game_resolution
                };
                let text =
                    context::assemble_launch(&game_specs, preview_res, &self.game_command)
                        .map(|lc| lc.to_string())
                        .unwrap_or_else(|e| format!("<error: {e}>"));
                (text, Vec::new())
            };

        let dynamic_preview = self.general_config.dynamic_preview;

        // ── Panel declaration order is load-bearing ──────────────────────────
        // egui hands each panel the rect left over by the panels declared before
        // it (`TopBottomPanel`/`SidePanel::show` both start from
        // `ctx.available_rect()`). Order here is: nav (above, left) →
        // ide_module_header (top, spans what's right of the nav) → ide_preview
        // (right, full remaining height) → ide_editor_band (bottom, in what's
        // left) → CentralPanel (the editor).
        //
        // The header's slot is the load-bearing part: declared AFTER the nav it
        // starts at the nav's right edge instead of the window's, and declared
        // BEFORE the preview it spans the editor AND preview columns rather than
        // just the editor's half. Move it after `render_ide_preview_panel` and it
        // silently shrinks to the editor column — no compile error, just the old
        // cramped layout back.
        let mut ide_action = TopAction::None;
        let mut ide_header: Option<EditorHeaderInfo> = None;
        if self.mode == Mode::Ide {
            // Full-width module header, above both columns.
            //
            // *Why it spans both columns* (2026-07-19): the action cluster
            // (`[Fork] [Delete] [✕] [Save]` + `Rename`) plus the name/version/author
            // heading needs ~500pt, and inside the editor's half that set the
            // column's effective minimum window width — the buttons collided with
            // the module name on anything under ~1300pt wide. Spanning the full
            // width right of the nav roughly halves that floor and decouples the
            // editor column's width from the button row entirely.
            if let Some(id) = self.focused_module().map(str::to_string) {
                ide_header = self.editor_header_info(&id);
                let cache = &mut self.icon_cache;
                let info = ide_header.as_ref();
                egui::TopBottomPanel::top("ide_module_header")
                    .exact_height(IDE_HEADER_H)
                    // Kept: with the band now filled the SAME colour as the
                    // columns under it, this hairline (`noninteractive.bg_stroke`
                    // = `theme::BORDER`) is the only thing marking the edge. A
                    // second, hand-drawn bottom border would just double it.
                    .show_separator_line(true)
                    // *Why `PANEL` and not `PANEL2`* (2026-07-19): `PANEL` is the
                    // fill of the nav column, the editor `CentralPanel` and the
                    // preview panel, so the header reads as the top of one
                    // continuous surface. `PANEL2` (the 198px bands' fill) made it
                    // read as a separate toolbar sitting on top of the editor —
                    // tried, seen, rejected.
                    .frame(egui::Frame::none()
                        .fill(theme::PANEL)
                        // top 8, left 14, right 16, bottom 12 — top and bottom
                        // are deliberately unequal (not a typo), and the numbers
                        // live in [`IDE_HEADER_MARGIN`] because two of them are
                        // terms of [`IDE_HEADER_H`], which sizes this panel. See
                        // that constant's *Why 8/12 and not 14/10 (unequal on
                        // purpose)*: measured ink-to-edge distance, not the
                        // margin number, is what has to match across the
                        // top/left/bottom edges, and the band's last text row is
                        // now an 11pt `Small` status line whose ink sits 1pt
                        // closer to its slot's bottom than the 13pt description's
                        // did — hence 12 where this was 11 before issue #26.
                        .inner_margin(IDE_HEADER_MARGIN))
                    .show(ctx, |ui| {
                        // `None` renders an empty band rather than skipping the
                        // panel: `exact_height` holds the layout still while
                        // `ensure_draft` catches up with a module switch.
                        if let Some(info) = info {
                            // `ide_mode: true` — this whole block only runs
                            // under `self.mode == Mode::Ide` (see the
                            // enclosing `if` a few lines up).
                            ide_action = render_editor_header_row(ui, cache, info, true);
                            // Second row, full band width. *Why below the name row
                            // and not beside it:* the action cluster owns the right
                            // end of row one, so anything sharing that row competes
                            // for the same space and re-creates the collision the
                            // band was built to fix. A row of its own cannot collide.
                            render_editor_header_description(ui, info.description.as_deref());
                            // NO third row. The draft's status messages render in
                            // the diagnostics band below (2026-07-19, issue #26)
                            // — see [`IDE_HEADER_H`]'s *Why the band holds no
                            // status lines*. Adding one here re-opens the choice
                            // between dead space and mid-keystroke reflow that
                            // moving them out of this band is what avoids.
                        }
                    });
            }
            self.render_ide_preview_panel(ctx, &ide_specs, &preview, dynamic_preview);
            // The diagnostics band under the editor column — no longer empty as of
            // 2026-07-19, but still `exact_height(198.0)`: the same band height as
            // the nav footer and the launch band beside it, so all three bottom
            // edges align across the window. It must stay fixed even as content
            // grows, because a variable height here would reflow the editor column
            // above it on every keystroke that changed the warning count. That
            // fixed height plus the scroll area inside it is exactly why the
            // draft's status lines moved in here (2026-07-19, issue #26): however
            // many appear as the user types, the band does not move a pixel.
            let mono = self.general_config.mono_ui;
            egui::TopBottomPanel::bottom("ide_editor_band")
                .exact_height(198.0)
                .show_separator_line(true)
                .frame(egui::Frame::none()
                    .fill(theme::PANEL2)
                    .inner_margin(egui::Margin::same(16.0)))
                .show(ctx, |ui| {
                    // `ide_header` is the SAME snapshot the header band above
                    // rendered from — taken once per frame, before any panel, so
                    // the two surfaces cannot disagree about the draft's state.
                    render_ide_diagnostics_band(ui, ide_header.as_ref(), &diagnostics, mono);
                });
        }

        // Config mode only. *Why skipped entirely rather than emptied:* a
        // zero-content bottom panel still reserves its height across the FULL window
        // width, which would push a dead strip under the IDE editor column on top of
        // the band declared above.
        if self.mode == Mode::Config {
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
                // The "Previewing against: {game}" line that used to sit here is
                // gone (S4a): the preview resolves against the scratch layer, so
                // naming the ambient game contradicted a settled decision.
                // Config mode keeps its lint here, in the launch band. *Why not
                // moved like IDE mode's* (2026-07-19): there is no editor band in
                // Config mode to move it *to* — `ide_editor_band` is declared only
                // under `self.mode == Mode::Ide`. Same corrected detection, same
                // `icon_sep` spacing; only the IDE surface relocates.
                let sep = icon_sep(self.general_config.mono_ui);
                for d in &diagnostics {
                    ui.label(
                        egui::RichText::new(format!("\u{f071}{sep}{}", d.message()))
                            .color(theme::COL_GLOBAL)
                            .small(),
                    );
                }
                ui.add_space(8.0);
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

            if let Some(id) = self.focused_module().map(str::to_string) {
                // IDE mode drew the header row in its own full-width band above
                // and dispatches that click itself, below.
                let header_inline = self.mode == Mode::Config;
                self.render_module_editor(ui, &id, header_inline);
                return;
            }

            // IDE mode with nothing focused is an empty `all_specs` (issue #4);
            // see `empty_central_message` for why the two cases need different
            // copy, and why entering IDE mode is not blocked instead.
            // Reaching this point under `Mode::Ide` means no module is focused,
            // which the invariant only permits with an empty `all_specs` — so
            // `cur_specs` is not consulted at all there: whatever it holds would be
            // a Config-mode answer to an IDE-mode question.
            let ide_no_modules = self.mode == Mode::Ide;
            let spec = if ide_no_modules {
                None
            } else {
                self.cur_specs.get(self.selected_ext).cloned()
            };
            let Some(spec) = spec else {
                ui.label(empty_central_message(ide_no_modules));
                return;
            };

            // S3a: the detail header (name/version/author/description/separator)
            // and the settings body (ScrollArea + section/field loop + legend/hint)
            // are extracted methods so S3b can call the body a second time for a
            // read-only preview column without duplicating this layout.
            //
            // *Why the extra 6px left inset* (2026-07-19): `CentralPanel::default()`
            // (this whole panel's frame, shared with the General Settings and
            // Config-mode module-editor branches above) gives an 8px margin on
            // every side. `render_module_detail_header` then adds its own
            // `add_space(6.0)` above the heading, so the header sat at top = 14
            // (8 + 6) but left = 8 — visibly closer to the left edge than the
            // top, which read as lopsided. This local `Frame` adds 6px on the
            // left ONLY for this branch, bringing left to 14 too, without
            // touching the shared panel margin the other `nav_sel` arms render
            // through (they were never part of this complaint).
            egui::Frame::none()
                .inner_margin(egui::Margin { left: 6.0, right: 0.0, top: 0.0, bottom: 0.0 })
                .show(ui, |ui| {
                    self.render_module_detail_header(ui, &spec);

                    let ext_id = spec.id();
                    let ext_res = edit_resolution.exts.get(&ext_id);
                    let full_width = self.general_config.full_width;
                    // `show_legend: true` — Config mode always wants the legend;
                    // it used to be implied by `!read_only`.
                    if self.render_module_settings_body(ui, &spec, ext_res, full_width, false, true) {
                        changed = true;
                    }
                });
        });

        // IDE mode's header band produced its click before the preview column and
        // the editor body rendered; carry it out only now that both have. Acting on
        // it at the point of the click would drop the draft mid-frame and leave
        // everything downstream rendering a "no longer available" flash against
        // state the header had already invalidated. This lands at the same point in
        // the frame as Config mode's inline dispatch (the tail of
        // `render_module_editor`, one closure up), so `ensure_draft`, the draft-drop
        // guard, the IDE reopen invariant and the `persist()` call below all keep
        // their existing order.
        if let (Some(info), Some(id)) =
            (ide_header.as_ref(), self.focused_module().map(str::to_string))
        {
            let (unsaved, editable) = (info.unsaved, info.editable);
            self.dispatch_top_action(ide_action, &id, unsaved, editable);
        }

        if self.render_confirm_dialog(ctx) {
            changed = true;
        }

        self.render_module_dialog(ctx);

        if self.poll_detect(ctx) {
            changed = true;
        }

        // Re-derive the dirty set at the frame tail: the body that just rendered
        // may have made a draft dirty (or hand-reverted one back to disk), and
        // `unsaved_drafts` must reflect that for the tree's per-row markers and the
        // exit-confirmation gate next frame, not the state from the top of this one.
        self.refresh_unsaved_drafts();
        // `persist()` is a no-op under `Mode::Ide` (S4b) — see its doc comment for
        // why that supersedes the old config-autosave interlock outright, rather
        // than reproducing its hold-while-dirty/flush-when-clean dance.
        if changed {
            self.persist();
        }

        // The only `ViewportCommand::Close` send site in this file. Everything
        // that wants the window shut sets `pending_close` (via
        // `request_app_close` or the confirmed `DiscardThen::ExitApp`) and lands
        // here, so the unsaved-work guard cannot be routed around. Re-sent every
        // frame the flag is set rather than once: the command only takes effect
        // on the following frame, and an idempotent resend is cheaper than
        // reasoning about which single frame is the right one.
        if self.pending_close {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Render the pending-confirmation modal, if any. Returns true if a
    /// destructive action was carried out.
    fn render_confirm_dialog(&mut self, ctx: &egui::Context) -> bool {
        let Some(action) = self.confirm.clone() else {
            return false;
        };
        // `confirm_label` / `destructive` default to the historical "Confirm" in a
        // red danger button; only the pending-rename arm overrides them.
        // *Why an override and not a second dialog:* the modal chrome, backdrop
        // and Cancel wiring are identical — the arm differs in one word on one
        // button, plus whether that button should read as destructive at all
        // (neither "Apply Rename" nor "Save Changes" destroys anything).
        let mut confirm_label = "Confirm";
        let mut destructive = true;
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
            ConfirmAction::DeleteModule { label, .. } => (
                "Delete Module",
                format!("Delete the module \"{label}\"? This removes its manifest file."),
            ),
            ConfirmAction::DiscardEdits { then } => (
                "Discard Changes",
                {
                    // Names are read live rather than captured into the action:
                    // the dialog and the draft can only disagree if the draft
                    // changed under an open modal, and the modal backdrop makes
                    // that impossible. One source of truth, no stale copy.
                    //
                    // `discard_names(then)`, not `unsaved_module_names()`: the
                    // list must be what *this* destination destroys, not every
                    // unsaved draft in the map (issue #35).
                    let names = self.discard_names(*then);
                    // The empty case is unreachable (nothing raises this action
                    // with a clean draft) but must still read as a sentence —
                    // "These modules have unsaved changes: ." would be a visible
                    // bug if any future caller got the gate wrong.
                    if names.is_empty() {
                        "There are unsaved module changes. Discard them and continue?"
                            .to_string()
                    } else {
                        format!(
                            "These modules have unsaved changes: {}.\n\n\
                             Discard them and continue?",
                            names.join(", ")
                        )
                    }
                },
            ),
            ConfirmAction::SaveWithPendingRename => {
                // Both the label and the blocking reason are read **live** off the
                // draft rather than captured into the action — see the variant's
                // doc comment. The backdrop means nothing can move under us.
                let (label, blocked) = self
                    .draft()
                    .map(|d| {
                        (
                            format!(
                                "{}::{}",
                                d.identity.staged_author(),
                                d.identity.staged_name()
                            ),
                            d.identity_error.clone(),
                        )
                    })
                    .unwrap_or_default();
                destructive = false;
                match &blocked {
                    // Committable: point the user at the rename, and do only that.
                    // They press Save again themselves — the two operations stay
                    // independent, which is the whole point of the split.
                    None => {
                        confirm_label = "Apply Rename";
                        (
                            "Rename Pending",
                            format!(
                                "This module has an unapplied rename to \"{label}\".\n\n\
                                 Apply the rename first, then press Save again to \
                                 write your changes. Nothing is lost either way — \
                                 Cancel leaves both the rename and your changes \
                                 pending."
                            ),
                        )
                    }
                    // Not committable: Rename cannot run, so the affirmative offers
                    // the escape instead — saving the body alone, which no longer
                    // costs the staged rename anything.
                    Some(reason) => {
                        confirm_label = "Save Changes";
                        (
                            "Rename Pending",
                            format!(
                                "This module has an unapplied rename to \"{label}\", \
                                 but it cannot be applied yet: {reason}.\n\n\
                                 You can still save your changes now — the rename \
                                 stays staged, ready to apply once you have fixed it."
                            ),
                        )
                    }
                }
            }
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
                if matches!(action, ConfirmAction::DeleteModule { .. }) {
                    ui.add_space(8.0);
                    styled_checkbox(
                        ui,
                        &mut self.delete_module_purge,
                        "Also purge this module's saved settings",
                    );
                }
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let btn = if destructive {
                        theme::danger_button(confirm_label)
                    } else {
                        theme::primary_button(confirm_label)
                    };
                    if ui.add(btn).clicked() {
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
                ConfirmAction::DeleteModule { manifest, .. } => {
                    self.delete_module(&manifest, self.delete_module_purge);
                    self.delete_module_purge = false;
                }
                ConfirmAction::DiscardEdits { then } => self.apply_discard_edits(then),
                ConfirmAction::SaveWithPendingRename => self.apply_save_with_pending_rename(),
            }
            self.confirm = None;
            return true;
        }
        if cancelled {
            self.confirm = None;
        }
        false
    }

    /// Render the Fork / Create-new module dialog, if open. Live-validates the
    /// (Author, Name) pair for uniqueness (Version-blind, whole loaded set) and
    /// disables the confirm button while the pair collides or Name is empty.
    fn render_module_dialog(&mut self, ctx: &egui::Context) {
        let Some(mut dlg) = self.module_dialog.take() else {
            return;
        };

        let author = dlg.author.trim().to_string();
        let name = dlg.name.trim().to_string();
        let name_empty = name.is_empty();
        let author_empty = author.is_empty();
        // Uniqueness is checked over the full loaded set; neither Fork nor Create
        // is editing an existing entry, so nothing is excluded.
        let collides = !name_empty
            && name_collides(&self.all_specs, &self.all_manifests, &author, &name, None);
        let can_confirm = !name_empty && !author_empty && !collides;

        let (title, verb) = match &dlg.kind {
            DialogKind::Fork { .. } => ("Fork Module", "Fork"),
            DialogKind::Create => ("Create Module", "Create"),
        };
        // Hoisted out of the closures below, which can't reach `self`.
        let sep = icon_sep(self.general_config.mono_ui);

        // Modal backdrop (mirrors the confirm dialog).
        let screen = ctx.screen_rect();
        egui::Area::new(egui::Id::new("module_dialog_backdrop"))
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
                ui.set_max_width(360.0);
                ui.add_space(4.0);
                ui.label(egui::RichText::new("Author").color(theme::DIM).small());
                ui.add(egui::TextEdit::singleline(&mut dlg.author).desired_width(f32::INFINITY));
                ui.add_space(6.0);
                ui.label(egui::RichText::new("Name").color(theme::DIM).small());
                ui.add(egui::TextEdit::singleline(&mut dlg.name).desired_width(f32::INFINITY));
                if matches!(dlg.kind, DialogKind::Fork { .. }) {
                    ui.add_space(8.0);
                    styled_checkbox(ui, &mut dlg.copy_settings, "Copy saved settings");
                }

                ui.add_space(8.0);
                let (fb, col) = if name_empty {
                    ("Name is required".to_string(), theme::COL_GLOBAL)
                } else if author_empty {
                    ("Author is required".to_string(), theme::COL_GLOBAL)
                } else if collides {
                    (format!("\u{f00d}{sep}{author} :: {name} already exists"), theme::COL_GLOBAL)
                } else {
                    (format!("\u{f00c}{sep}{author} :: {name} is available"), theme::COL_PROFILE)
                };
                ui.label(egui::RichText::new(fb).color(col).small());

                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(can_confirm, theme::primary_button(verb)).clicked() {
                        confirmed = true;
                    }
                    if ui.add(theme::secondary_button("Cancel")).clicked() {
                        cancelled = true;
                    }
                });
            });

        if confirmed {
            match dlg.kind {
                DialogKind::Fork { parent_id } => {
                    self.perform_fork(&parent_id, author, name, dlg.copy_settings)
                }
                DialogKind::Create => self.perform_create(author, name),
            }
            return; // dialog consumed
        }
        if !cancelled {
            // Not dismissed — keep it open (with the edited buffers) next frame.
            self.module_dialog = Some(dlg);
        }
    }
}

/// Everything the editor's header row and status lines need, snapshotted **out
/// of** [`ModuleDraft`] once per frame.
///
/// *Why a snapshot struct and not a `&ModuleDraft`:* in IDE mode the header row
/// and the editor body render inside **different panel closures**, and the body
/// holds a `&mut` borrow of the focused draft for its whole scope. Passing owned, plain data
/// to the header means neither closure has to borrow the draft while the other
/// runs, which is what lets the header move to its own full-width panel at all.
/// Every field is cheap (a few `String`s + `bool`s); the two `Option<String>`
/// diagnostics are the only allocations that aren't already being made.
struct EditorHeaderInfo {
    name: String,
    version: String,
    author: String,
    /// `Extension::meta.description`, verbatim and un-truncated. Truncation is a
    /// render-time concern (see [`render_editor_header_description`]) — the full
    /// string has to survive to here so the hover tooltip can show it.
    description: Option<String>,
    editable: bool,
    /// The **body** differs from disk (`ModuleDraft::dirty`). Feeds the Save
    /// gate's explanation, not the "is anything unsaved?" question — see
    /// [`EditorHeaderInfo::unsaved`].
    dirty: bool,
    /// Anything at all is unsaved: `dirty` **or** a staged identity change
    /// ([`ModuleDraft::has_unsaved_work`], issue #34). What the state line and
    /// Close's discard prompt read; `dirty` alone let a rename-only draft claim
    /// "All changes saved" and be closed without a word.
    unsaved: bool,
    save_on: bool,
    has_identity: bool,
    identity_err: Option<String>,
    sections_unique: bool,
    validate_err: Option<String>,
    req_ok: bool,
}

/// The editor's header row: name / version / author on the left, the action
/// cluster on the right. Returns the [`TopAction`] the user clicked, if any —
/// the caller dispatches it (never this function, see
/// [`GuiApp::dispatch_top_action`]).
///
/// Free function taking `cache` directly rather than a `&mut self` method: the
/// IDE-mode call site renders this inside a panel closure that already holds
/// `&mut self`, and the only thing it needs from `self` is the icon cache.
///
/// **Invariant: this row must stay constant-height, at [`IDE_HEADER_ROW_H`]
/// (`27.0`, button-driven — see that constant's doc comment for why it is 27
/// and not the `interact_size.y` value of 23 an earlier version of this file
/// assumed).** IDE mode renders it in a fixed-height panel spanning both
/// columns, so anything that changed height here would reflow the editor *and*
/// the preview column. The `editable`-gated Delete / Rename buttons are safe:
/// `editable` is fixed per module, and they grow the row horizontally, never
/// vertically. The `ide_mode`-gated Save / Close below are safe for the same
/// reason: `ide_mode` is fixed for the whole session (it's `self.mode`), so a
/// given call site's row width never changes frame to frame.
///
/// `ide_mode` distinguishes the two call sites, which want different
/// behaviour for a **bundled** (non-editable) module (2026-07-19):
///
/// - **Config mode** (`GuiApp::render_module_editor`'s `header_inline` call):
///   keeps Save and Close always — Save disabled with a "Fork to edit"
///   tooltip (the established way Config teaches the fork gesture); useful
///   regardless of editability. This whole call site is currently unreachable
///   (see `GuiApp::focused_module`'s doc), kept alive only by this shared
///   function.
/// - **IDE mode** (the `ide_module_header` panel): hides both for a bundled
///   module, and labels the Close button **Discard**. Save can never be
///   enabled there (nothing to save — the fields
///   render disabled). Discard is not "leave the editor": IDE mode's
///   invariant that a module is ALWAYS focused (see the guard right after the
///   doc comment on `Mode::Ide`, and `GuiApp::dispatch_top_action`'s
///   `TopAction::Close` arm) reopens whatever the tree has selected the next
///   frame, so in IDE mode Close only ever discards-and-reloads the current
///   selection. On a bundled module there is nothing to discard, so the
///   reload is a no-op — the button did nothing observable, which is exactly
///   why it is hidden here rather than left disabled.
fn render_editor_header_row(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    info: &EditorHeaderInfo,
    ide_mode: bool,
) -> TopAction {
    let mut action = TopAction::None;
    ui.horizontal(|ui| {
        ui.heading(&info.name);
        ui.add_space(6.0);
        ui.label(egui::RichText::new(format!("v{}", info.version)).color(theme::FAINT));
        ui.label(egui::RichText::new(format!("by {}", info.author)).color(theme::FAINT));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Right-to-left layout: added first == drawn rightmost, so this
            // reads [Fork] [Delete] [Close] [Save] on screen — Close and Save
            // only when the module is editable OR this is Config mode; see
            // the `ide_mode` doc comment above for why the two modes differ.
            if info.editable || !ide_mode {
                let mut save = ui.add_enabled(info.save_on, theme::primary_button("Save"));
                if !info.editable {
                    save = save.on_hover_text("Fork to edit");
                }
                if save.clicked() {
                    action = TopAction::Save;
                }
                // Same `TopAction::Close` on both sides — only the wording
                // differs, because only the *meaning* differs (2026-07-19).
                //
                // *Why IDE mode says Discard:* `TopAction::Close` calls
                // `GuiApp::discard_module`, but the `Mode::Ide` invariant at
                // the top of `GuiApp::ui` ("a module is ALWAYS focused") reopens
                // the tree's selected module on the very next frame. So in IDE
                // mode this button can never mean "leave" — it can only mean
                // "throw away my edits and reload the draft from disk". Labelling
                // it Close there promised an exit that structurally cannot
                // happen. Config mode keeps Close (currently unreachable, see
                // above); its `discard_module` branch drops the focused draft
                // and clears focus via `GuiApp::close_focused_draft` (S4b) —
                // there is no "remembered view" to return to any more, because
                // focusing a module never touched `nav_sel` in the first place.
                //
                // Behaviour is untouched on both paths, including the dirty-draft
                // confirmation `dispatch_top_action` puts in front of it.
                let (close_label, close_hover) = if ide_mode {
                    ("Discard", "Discard unsaved edits and reload this module")
                } else {
                    ("Close", "Leave the editor (discards unsaved edits)")
                };
                if icon_button(ui, cache, "\u{f00d}", close_label, IconBtn::Secondary, true)
                    .on_hover_text(close_hover)
                    .clicked()
                {
                    action = TopAction::Close;
                }
            }
            if info.editable
                && ui
                    .add(theme::danger_button("Delete"))
                    .on_hover_text("Delete this user module")
                    .clicked()
            {
                action = TopAction::Delete;
            }
            // Fork works on any module (bundled or user); Delete only on
            // user-authored (editable) modules.
            if ui
                .add(theme::secondary_button("Fork"))
                .on_hover_text("Create an editable copy of this module")
                .clicked()
            {
                action = TopAction::Fork;
            }
            // Rename / Apply identity changes: commits the staged Author /
            // Name / Variable edits via the scope sweep + manifest rewrite.
            // Enabled only for an editable module with a *valid* pending
            // identity change (never the normal Save/autosave path).
            if info.editable {
                let rename = ui
                    .add_enabled(
                        info.has_identity && info.identity_err.is_none(),
                        theme::secondary_button("Rename"),
                    )
                    .on_hover_text(
                        "Apply the staged Author / Name / Variable changes \u{2014} migrates saved settings across all scopes",
                    );
                if rename.clicked() {
                    action = TopAction::Rename;
                }
            }
        });
    });
    action
}

/// The header band's second row: the module's description, on exactly one line.
///
/// **Invariant: this occupies [`IDE_HEADER_DESC_H`] points, unconditionally.**
/// [`IDE_HEADER_H`] budgets for it, and the band is `exact_height`, so a slot
/// that grew would paint over the columns beneath it. Three things could have
/// made it vary, and each is closed off here:
///
/// 1. **A module with no description** (`meta.description` is `Option`) — the
///    rect is allocated *before* the text is even looked at, so the empty case
///    reserves the identical space and the band does not twitch as you walk the
///    module tree.
/// 2. **A long description wrapping to two lines** — the layout job is capped at
///    `max_rows = 1` with an ellipsis overflow character, so it elides instead of
///    wrapping. This is why the text is painted from a hand-built `LayoutJob`
///    rather than handed to `ui.label`: a `Label` sizes the `Ui` from its galley,
///    and the galley is exactly the thing that must not be allowed to.
/// 3. **A narrow window** — `max_width` follows the rect, so narrowing the window
///    moves the ellipsis leftward and changes nothing else. Wrapping is the only
///    way width could have become height, and (2) removed it.
///
/// The galley is *painted*, not allocated, and centred in the slot: the slot is
/// sized *to* the galley — [`IDE_HEADER_DESC_H`] (`17.0`) is the actual measured
/// height of a 13pt Body row (Geist Mono row height is `size * 1.3`; `13.0 *
/// 1.3 = 16.9`, rounded up), so nothing bleeds. (It briefly did, by exactly
/// 1.0pt into the frame's bottom margin — invisible, clipped by the panel —
/// while `IDE_HEADER_DESC_H` was `16.0`; fixed 2026-07-19, see [`IDE_HEADER_H`]'s
/// doc comment.)
///
/// Elided text keeps the full string on hover, so nothing is actually lost. The
/// tooltip is attached only when the text *was* elided — a tooltip that merely
/// repeats what you can already read is noise.
///
/// Colour is [`theme::DIM`], matching how `GuiApp::render_module_detail_header`
/// renders the same field in Config mode.
fn render_editor_header_description(ui: &mut egui::Ui, desc: Option<&str>) {
    // Reserve the slot FIRST — before any early return — so every path through
    // this function costs the same height. See invariant (1) above.
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), IDE_HEADER_DESC_H),
        egui::Sense::hover(),
    );
    let Some(text) = desc.map(str::trim).filter(|t| !t.is_empty()) else {
        return;
    };
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.to_owned(),
        egui::TextStyle::Body.resolve(ui.style()),
        theme::DIM,
    );
    job.wrap.max_width = rect.width();
    job.wrap.max_rows = 1;
    job.wrap.overflow_character = Some('\u{2026}');
    let galley = ui.fonts(|f| f.layout_job(job));
    let elided = galley.elided;
    let y = rect.center().y - galley.size().y / 2.0;
    ui.painter()
        .galley(egui::pos2(rect.left(), y), galley, theme::DIM);
    if elided {
        resp.on_hover_text(text);
    }
}

/// One status message about the open draft, in both dialects the two surfaces
/// that render it speak.
///
/// *Why an entry carries two presentations* (2026-07-19, issue #26): the same
/// message list feeds Config mode's inline stack under the editor header, which
/// colours by a per-message palette it has always used, and IDE mode's
/// diagnostics band, which colours by [`DiagSeverity`]. Those are genuinely
/// different schemes, not one scheme two ways: Config's state line is green when
/// dirty, grey when clean and red when the module is bundled — three colours for
/// what is a single informational message under the severity vocabulary. Mapping
/// severity back onto Config's colours is therefore impossible without changing
/// Config mode, which is out of scope, and duplicating the `if` ladder to give
/// each surface its own list would let the two drift invisibly (each surface only
/// ever shows one mode's version). Carrying both on the entry keeps *which
/// messages exist* a single decision while leaving *how each surface paints them*
/// to the surface.
struct StatusLine {
    text: String,
    /// The colour Config mode draws this message in — preserved verbatim from
    /// before the severity vocabulary existed, so that surface is byte-for-byte
    /// unchanged.
    config_color: egui::Color32,
    /// How the IDE diagnostics band ranks and paints this message.
    severity: DiagSeverity,
}

/// The status messages for a draft: dirty state first, then any schema /
/// `Requires` / pending-identity problems, in display order.
///
/// **The first entry is unconditional and always [`DiagSeverity::Info`]** — it
/// is the draft's state line, and both surfaces rely on it being there. The
/// diagnostics band pins it to the top of its list in info blue; Config mode
/// keeps it as the first label under the header. Nothing else in the list is
/// ever `Info`, which is what lets the band's tally count "problems" by simply
/// skipping the info entries ([`ide_diagnostic_entries`]).
/// [`status_lines_have_exactly_one_pinned_info_line`] pins both halves of that
/// over every combination of flags that can add a message.
fn editor_status_lines(info: &EditorHeaderInfo) -> Vec<StatusLine> {
    let mut out: Vec<StatusLine> = Vec::new();
    let mut push = |text: String, config_color: egui::Color32, severity: DiagSeverity| {
        out.push(StatusLine { text, config_color, severity });
    };
    // State line. ALWAYS present (greyed when clean), ALWAYS `Info`: it says
    // what the draft *is*, not what is wrong with it — even the bundled/read-only
    // variant, which reports a property of the module rather than a defect the
    // user can fix.
    if !info.editable {
        push(
            "Bundled module \u{2014} read-only. Fork to edit (coming soon).".to_string(),
            theme::COL_GLOBAL,
            DiagSeverity::Info,
        );
    } else if info.dirty {
        push(
            "Unsaved changes \u{2014} Save or Discard to apply.".to_string(),
            theme::COL_PROFILE,
            DiagSeverity::Info,
        );
    } else if info.unsaved {
        // Clean body, staged rename (2026-07-19, issue #34). This arm used to fall
        // through to "All changes saved." because the ladder asked `dirty` only —
        // which the user called out as flatly wrong: they had typed a new name and
        // it was not on disk.
        //
        // *Why its own sentence rather than reusing the line above:* the state line
        // answers "is my work saved", and here the honest answer is "no, and the
        // button that saves it is Rename, not Save". Save is greyed for a clean
        // body (see `ModuleDraft::save_enabled`), so naming Save would point at a
        // dead control.
        //
        // *Why the "Pending identity change" Warning below still fires:* the two
        // say different things. This Info says THAT there is unsaved work; the
        // Warning says WHAT is unfinished about it (staged, settings not migrated).
        // Merging them would either lose the state line in the "body dirty AND
        // rename staged" case or duplicate it in this one.
        push(
            "Unsaved rename \u{2014} Rename or Discard to apply.".to_string(),
            theme::COL_PROFILE,
            DiagSeverity::Info,
        );
    } else {
        push("All changes saved.".to_string(), theme::FAINT, DiagSeverity::Info);
    }
    // Explain *why* Save is greyed for a schema problem. Duplicate section
    // names collapse when folded into the `IndexMap` before `validate` sees
    // them, so that case is caught by `sections_unique` separately.
    //
    // `Error`, not `Warning`, for all three of the next blocks: each one is a
    // condition `Draft::save_enabled` gates on, so each is literally the reason
    // an action is refused, not advice about one that would still work.
    if info.editable && !info.sections_unique {
        push(
            "Cannot save: two UI sections share a name \u{2014} rename one.".to_string(),
            theme::COL_GLOBAL,
            DiagSeverity::Error,
        );
    } else if info.editable {
        if let Some(reason) = &info.validate_err {
            push(format!("Cannot save: {reason}"), theme::COL_GLOBAL, DiagSeverity::Error);
        }
    }
    if info.editable && !info.req_ok {
        push(
            "A Requires expression does not parse.".to_string(),
            theme::COL_GLOBAL,
            DiagSeverity::Error,
        );
    }
    // Pending-identity feedback: why Rename is blocked, or a ready prompt.
    if info.editable && info.has_identity {
        match &info.identity_err {
            // Blocks Rename → error, same rule as the save gates above.
            Some(reason) => push(
                format!("Cannot rename: {reason}"),
                theme::COL_GLOBAL,
                DiagSeverity::Error,
            ),
            // *Why `Warning` and not a second `Info`:* the rename is staged but
            // NOT applied, so the draft's on-disk identity and its edited one
            // disagree until the user acts — an unfinished state that wants
            // attention, which is what a warning is. Keeping it out of `Info`
            // also preserves the "exactly one info entry" rule the band's tally
            // and pinning both lean on.
            None => push(
                "Pending identity change \u{2014} press Rename to migrate saved settings and apply it."
                    .to_string(),
                theme::COL_PROFILE,
                DiagSeverity::Warning,
            ),
        }
    }
    out
}

/// The status lines as the **Config-mode editor body** renders them: a plain
/// stack of `ui.label`s, spaced by egui's `item_spacing.y`, growing the column
/// as messages appear.
///
/// *Why the growth is acceptable here and not in IDE mode* (2026-07-19, issue
/// #26): this stack is inside the editor column's own scroll area, so a line
/// appearing mid-keystroke shifts only the fields underneath it — the behaviour
/// Config mode has always had. IDE mode's header band spans the editor **and**
/// the preview column, so the same growth there would shove half the window
/// down; that mode draws these messages in its diagnostics band instead
/// ([`ide_diagnostic_entries`]), inside a scroll area that absorbs any number of
/// them at no height cost. Both surfaces draw the same messages from
/// [`editor_status_lines`].
///
/// *Why Config mode was left exactly as it was:* it has no reflow problem to
/// solve and no diagnostics band to move anything into, and its per-message
/// palette predates the [`DiagSeverity`] vocabulary. It therefore keeps painting
/// by [`StatusLine::config_color`] — deliberately byte-for-byte what it rendered
/// before issue #26.
fn render_editor_status_lines(ui: &mut egui::Ui, info: &EditorHeaderInfo) {
    for line in editor_status_lines(info) {
        ui.label(
            egui::RichText::new(line.text)
                .color(line.config_color)
                .small(),
        );
    }
}

impl GuiApp {
    /// Snapshot the open draft's header state. `&self` only, so it is safe to
    /// call **before any panel is declared** — which is what IDE mode does, to
    /// have the data ready for a header panel that renders ahead of the editor.
    ///
    /// `None` means "nothing to head": either no draft, or a draft for a
    /// different module (`ensure_draft` has not caught up yet this frame).
    fn editor_header_info(&self, id: &str) -> Option<EditorHeaderInfo> {
        let d = self.draft().filter(|d| d.id == id)?;
        let snap = d.snapshot();
        Some(EditorHeaderInfo {
            name: d.ext.meta.name.clone(),
            version: d.ext.meta.version.clone(),
            author: d.ext.meta.author.clone(),
            description: d.ext.meta.description.clone(),
            editable: d.editable,
            dirty: d.dirty(),
            unsaved: d.has_unsaved_work(),
            save_on: d.save_enabled(),
            has_identity: d.has_pending_identity(),
            identity_err: d.identity_error.clone(),
            sections_unique: d.sections_unique(),
            validate_err: core_extension::validate(&snap).err().map(|e| e.to_string()),
            req_ok: all_requires_parse(&snap),
        })
    }

    /// Carry out the action the header row produced.
    ///
    /// *Why this is its own method:* the header row renders in the editor's own
    /// `Ui` in Config mode but in a separate full-width panel in IDE mode, so
    /// the two paths reach the dispatch from different places. Both must run it
    /// at the same point in the frame — **after** the editor body and the
    /// preview column have rendered. Saving or closing mid-frame drops the
    /// draft, and everything downstream would then render a "no longer
    /// available" flash against state the header already invalidated.
    ///
    /// `unsaved` is [`EditorHeaderInfo::unsaved`], **not** `dirty` (issue #34): a
    /// draft whose only change is a staged rename has work to lose, so Close must
    /// prompt for it too.
    fn dispatch_top_action(&mut self, action: TopAction, id: &str, unsaved: bool, editable: bool) {
        match action {
            // `request_save_module`, not `save_module`: one entry point shared
            // with Ctrl+S, carrying the "not while a modal is up" guard and the
            // staged-rename notice.
            TopAction::Save => self.request_save_module(),
            // Close doubles as Discard: warn before dropping unsaved edits.
            TopAction::Close => {
                if unsaved && editable {
                    // `CloseEditor` — the editor Close has no destination to
                    // complete beyond what `discard_module` already does on
                    // Confirm.
                    self.confirm =
                        Some(ConfirmAction::DiscardEdits { then: DiscardThen::CloseEditor });
                } else {
                    // `discard_module`: S4a made Discard a per-module action that
                    // (in IDE mode) re-seeds the focused draft from disk and stays
                    // put, rather than leaving for some other view. Calling
                    // `GuiApp::close_focused_draft` directly here instead would
                    // clear focus and let the `Mode::Ide` reopen invariant pick
                    // whatever the tree currently has selected — which could be a
                    // *different* module than the one this button was on, not
                    // what the button says.
                    self.discard_module();
                }
            }
            TopAction::Fork => self.open_fork_dialog(id),
            TopAction::Rename => self.perform_rename(),
            TopAction::Delete => {
                if let Some(d) = self.draft() {
                    let label = format!("{}::{}", d.ext.meta.author, d.ext.meta.name);
                    let manifest = d.manifest.clone();
                    self.delete_module_purge = false;
                    self.confirm = Some(ConfirmAction::DeleteModule { manifest, label });
                }
            }
            TopAction::None => {}
        }
    }

    /// Phase 2: editable manifest editor. Edits an in-memory [`ModuleDraft`] with a
    /// live preview and an explicit Save. Bundled modules render the same widgets
    /// but disabled (Save shows a "Fork to edit" tooltip). `id` is the module's
    /// `Extension::id()`; the draft is (re)loaded by [`ensure_draft`] each frame.
    ///
    /// `header_inline` controls whether the name/version/author + action row is
    /// drawn here. Config mode passes `true` (the header belongs to this column).
    /// IDE mode passes `false`: it renders the same row via
    /// [`render_editor_header_row`] in a full-width `TopBottomPanel` spanning the
    /// editor *and* preview columns, and dispatches the action itself. The status
    /// lines follow the header row in Config mode ([`render_editor_status_lines`]);
    /// IDE mode shows the same messages in its diagnostics band instead
    /// ([`ide_diagnostic_entries`]) as of issue #26.
    fn render_module_editor(&mut self, ui: &mut egui::Ui, id: &str, header_inline: bool) {
        let touch = self.general_config.touch_mode;
        // IDE mode always runs full-width: its editor column is already sized by the
        // nav/preview split, so the 743px clamp (built for Config mode's wider,
        // unsplit central panel) would only add a dead gutter.
        let full_width = self.general_config.full_width || self.mode == Mode::Ide;
        let mut action = TopAction::None;

        // The editor body holds a `&mut` borrow of the focused draft for its
        // whole scope, so a second `&mut self.icon_cache` can't be taken
        // alongside it. Move the (map-only, cheap) cache out for the duration and
        // put it back afterwards; the measurements survive across frames.
        let mut cache = std::mem::take(&mut self.icon_cache);
        // Hoisted: `self` is borrowed out from under the closures below.
        let sep = icon_sep(self.general_config.mono_ui);

        // Transient dropped-var report from the last fork snapshot, dismissible.
        if let Some(msg) = self.carryover_report.clone() {
            let mut dismiss = false;
            editor_card(ui, &mut cache, theme::EDIT_L1, "Carry-over report", None, |ui, _cache| {
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!("\u{f071}{sep}{msg}"))
                            .color(theme::COL_GLOBAL)
                            .small(),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.add(theme::secondary_button("Dismiss")).clicked() {
                            dismiss = true;
                        }
                    });
                });
            });
            if dismiss {
                self.carryover_report = None;
            }
        }
        // Header state is snapshotted out of the draft BEFORE the mutable borrow
        // below, so the header row (which in IDE mode renders in a different panel
        // entirely) never has to hold the draft.
        let Some(info) = self.editor_header_info(id) else {
            // Two distinct "nothing to draw" cases, kept apart because they read
            // very differently to the user: the draft is gone for good, or
            // `ensure_draft` simply hasn't caught up with a fresh selection yet.
            let msg = if self.draft().is_none() {
                "This module is no longer available."
            } else {
                "Loading\u{2026}"
            };
            ui.label(egui::RichText::new(msg).color(theme::DIM));
            self.icon_cache = cache;
            return;
        };
        let (unsaved, editable) = (info.unsaved, info.editable);

        ui.add_space(6.0);
        if header_inline {
            // `ide_mode: false` — `header_inline` is only ever `true` when
            // `self.mode == Mode::Config` (see this method's doc comment).
            action = render_editor_header_row(ui, &mut cache, &info, false);
            // Status lines follow the header row, so they render here only on
            // the path that owns that row. IDE mode draws the same messages in
            // its diagnostics band instead (`ide_diagnostic_entries`);
            // rendering them here too would put them on screen twice
            // (2026-07-19, issue #26). Gated on `header_inline` rather than
            // `self.mode` because `header_inline` is what actually says "this
            // column owns the header" — the two agree today, and if a third
            // call site ever disagrees, the header and its status lines should
            // still travel together.
            render_editor_status_lines(ui, &info);
        }
        ui.add_space(10.0);
        ui.separator();

        {
            // Re-borrow for the body only. `editor_header_info` above already
            // established that a draft for `id` exists, so this cannot fail — but
            // handle it rather than unwrap, since a panic here would take the app
            // down mid-frame.
            let Some(draft) = self.draft_mut() else {
                self.icon_cache = cache;
                return;
            };

            let mut deferred: Vec<Deferred> = Vec::new();
            // Explicit, stable scroll id: keeps the body's content Ui id constant
            // regardless of how many status lines render above it, so the focused
            // TextEdit's id never shifts when the dirty/identity banners appear.
            egui::ScrollArea::vertical()
                .id_salt("module_editor_body")
                .auto_shrink([false, false])
                .drag_to_scroll(touch)
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        let max_w = body_max_width(ui, full_width);
                        ui.set_max_width(max_w);
                        ui.add_enabled_ui(editable, |ui| {
                            render_editor_body(ui, &mut cache, draft, &mut deferred);
                        });
                    });
                });
            // Apply structural edits AFTER the render loop (never mid-iteration).
            for d in deferred {
                apply_deferred(draft, d);
            }
        }
        self.icon_cache = cache;
        // Only the inline (Config-mode) header produces an action here; IDE mode's
        // header panel dispatches its own, after the preview column has rendered.
        if header_inline {
            self.dispatch_top_action(action, id, unsaved, editable);
        }
    }
}

/// Width to clamp a central-panel body to: fills the pane less an 18px
/// scrollbar gutter, then caps at ~743px unless "Use full UI width" is on.
/// `min(..)` lets it scale DOWN on narrow windows instead of overflowing.
/// Shared by the module list, the manifest editor, and General Settings —
/// was copy-pasted three times before this extraction.
///
/// Free function, not a `&self` method: `render_module_editor` computes this
/// while the focused draft is borrowed mutably (as `draft`) for the whole
/// surrounding block, so a `&self` call there would conflict with that
/// borrow. Taking `full_width` as a plain `bool` sidesteps it everywhere,
/// not just at that one call site.
///
/// *Why this exists as its own helper:* IDE mode (see
/// `docs/brainstorm/ide-mode.md`, S3) will need the clamp to NOT bind in its
/// wide preview layout — one helper means one place to add that condition
/// later instead of three. `Mode` doesn't exist yet, so that condition is
/// deliberately not added here.
fn body_max_width(ui: &egui::Ui, full_width: bool) -> f32 {
    let avail = (ui.available_width() - 18.0).max(300.0);
    if full_width { avail } else { avail.min(743.0) }
}

// ── Editor column geometry ────────────────────────────────────────────────
// The editor reuses the fixed-column idiom the normal module field rows already
// use (`AppState::render_field`: reserve a constant label column by padding the
// cursor out to a fixed x, then let the control take the remaining width), so
// every row's control starts and ends at the same x. `render_field` reserves
// 260px for its checkbox+label; editor labels are single words, so the editor
// applies the same mechanism with its own narrower constant.

/// Fixed label-column width for an editor row — `render_field`'s 260px reserve
/// at editor scale.
const EDITOR_LABEL_W: f32 = 96.0;
/// Side of the square a glyph-only [`icon_button`] allocates: `interact_size.y`
/// (== `font.size + button_padding.y * 2` at the editor's theme values —
/// `13.0 + 5.0*2 == 23.0`, set in `theme.rs`). Kept as its own constant, rather
/// than re-derived at each call site, so [`ACTION_COL_W`] can reference the same
/// number `icon_button` actually draws at. If `theme.rs` ever changes
/// `interact_size.y`, `button_padding.y`, or the Button font size, update this
/// literal to match (`icon_button_width` computes the live value from
/// `ui.spacing()`, so a mismatch here only affects the const-derived
/// [`ACTION_COL_W`], not the buttons themselves).
const ICON_BTN_SIDE: f32 = 23.0;
/// Fixed width reserved for a row's `[↑][↓][🗑]` cluster. Constant so every
/// cluster in a card pins to the same right edge whatever precedes it. Must
/// cover what the cluster actually draws, or a control sized with this as its
/// `reserve` slides under the leftmost button: three square, glyph-only
/// `icon_button`s ([`ICON_BTN_SIDE`] each) plus two `item_spacing.x` gaps
/// (7.0, `theme.rs`) = `3 * 23.0 + 2 * 7.0` = 83.
const ACTION_COL_W: f32 = 3.0 * ICON_BTN_SIDE + 2.0 * 7.0;
/// Floor for a row control, so a narrow window shrinks but never collapses it.
const MIN_CONTROL_W: f32 = 80.0;
/// Side of the square cell an [`icon_button`] reserves for its glyph.
const ICON_CELL: f32 = 18.0;

/// Width for an editor row's control: what remains of the row after the fixed
/// label column, minus any right-pinned `reserve` (e.g. an action cluster),
/// floored at [`MIN_CONTROL_W`]. Pure — unit-tested.
fn editor_control_width(remaining: f32, reserve: f32) -> f32 {
    (remaining - reserve).max(MIN_CONTROL_W)
}

/// Start an editor row: draw `label`, pad out to the fixed label column, and
/// return the width the row's control should use so it ends flush at the card's
/// right edge (less `reserve`). Every editor row goes through this, which is
/// what makes the textboxes share one left and one right edge.
fn editor_row_label(ui: &mut egui::Ui, label: &str, reserve: f32) -> f32 {
    let left = ui.cursor().min.x;
    field_label(ui, label);
    let used = ui.cursor().min.x - left;
    ui.add_space((EDITOR_LABEL_W - used).max(0.0));
    editor_control_width(ui.available_width(), reserve)
}

/// Which theme role an [`icon_button`] draws its fill / stroke / text from.
#[derive(Clone, Copy, PartialEq)]
enum IconBtn {
    Secondary,
    Danger,
}

/// The single entry point for every editor icon affordance (↑ ↓ 🗑 ✕ ✎ ＋).
///
/// Reserves a fixed square cell for the glyph and paints it through the
/// [`IconCenterCache`] so its *ink* — not its layout box, whose height is the
/// same for every glyph and whose width includes side bearings — sits centered
/// in the cell. A row of different glyphs then reads as one aligned column.
/// `label` may be empty for a glyph-only button.
///
/// Anchors `Align2::LEFT_TOP`: the cache already returns a corrected top-left,
/// so anchoring `CENTER_CENTER` here would double-correct.
fn icon_button(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    icon: &str,
    label: &str,
    style: IconBtn,
    enabled: bool,
) -> egui::Response {
    ui.add_enabled_ui(enabled, |ui| {
        let font = egui::TextStyle::Button.resolve(ui.style());
        let pad = ui.spacing().button_padding;
        let gap = if label.is_empty() { 0.0 } else { 6.0 };
        // Laid out with PLACEHOLDER so `Painter::galley` can substitute the
        // state-dependent color below without a second layout pass.
        let text = (!label.is_empty()).then(|| {
            ui.painter()
                .layout_no_wrap(label.to_owned(), font.clone(), Color32::PLACEHOLDER)
        });
        let text_w = text.as_ref().map_or(0.0, |g| g.size().x);
        // Height must match plain `egui::Button`'s own formula
        // (`interact_size.y.max(galley.size().y + pad.y*2)`, see
        // `egui::widgets::Button::ui`), or a labelled `icon_button` (e.g. "Edit")
        // ends up a different height than a same-row `theme::secondary_button`
        // (e.g. "Fork") — `font.size` is the font's nominal point size, not the
        // laid-out glyph height a real button measures, so using it here for
        // labelled buttons under-measured and let interact_size.y win when the
        // sibling button's actual galley was taller. Icon-only buttons keep the
        // old `font.size` fallback (`text` is `None`), so `ICON_BTN_SIDE`/
        // `ACTION_COL_W` are untouched.
        let text_h = text.as_ref().map_or(font.size, |g| g.size().y);
        let h = ui.spacing().interact_size.y.max(text_h + pad.y * 2.0);
        // Icon-only buttons drop the horizontal padding and allocate a square
        // cell (width == height) instead of `pad.x * 2 + ICON_CELL`, which used
        // to leave them wider than tall. Labelled buttons are unchanged.
        let w = if label.is_empty() { h } else { pad.x * 2.0 + ICON_CELL + gap + text_w };
        let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click());
        if ui.is_rect_visible(rect) {
            let vis = *ui.style().interact(&resp);
            let (fill, stroke, fg) = match style {
                IconBtn::Secondary => (vis.weak_bg_fill, vis.bg_stroke, theme::TEXT),
                IconBtn::Danger => (
                    if resp.hovered() { theme::HOV } else { Color32::TRANSPARENT },
                    egui::Stroke::new(
                        1.0,
                        Color32::from_rgba_unmultiplied(
                            theme::COL_GLOBAL.r(),
                            theme::COL_GLOBAL.g(),
                            theme::COL_GLOBAL.b(),
                            82,
                        ),
                    ),
                    theme::COL_GLOBAL,
                ),
            };
            let fg = if enabled { fg } else { theme::FAINT };
            ui.painter().rect(rect, egui::Rounding::same(8.0), fill, stroke);
            let cell_x = if label.is_empty() { (rect.width() - ICON_CELL) / 2.0 } else { pad.x };
            let cell = egui::Rect::from_min_size(
                egui::pos2(rect.min.x + cell_x, rect.min.y),
                egui::vec2(ICON_CELL, rect.height()),
            );
            let pos = cache.centered_pos(ui, icon, &font, cell.center());
            ui.painter().text(pos, egui::Align2::LEFT_TOP, icon, font, fg);
            if let Some(g) = text {
                let ty = rect.center().y - g.size().y / 2.0;
                ui.painter().galley(egui::pos2(cell.max.x + gap, ty), g, fg);
            }
        }
        resp
    })
    .inner
}

/// A card matching the rounding used elsewhere in the app, holding the editable
/// widgets of one field / block entry.
///
/// The `title` is the FIRST widget inside the frame's inner margin, styled with
/// `theme::section_label` — the border stays a plain continuous 1px stroke, this
/// is deliberately NOT a fieldset/legend notch. An empty `title` skips the
/// header line. `fill` selects the nesting shade (the `theme::EDIT_L*` ramp) so
/// deeper levels read as a visible hierarchy — ritz nests module → block →
/// section/field → builder row, so the shades carry information the border
/// alone can't.
///
/// `actions` (index, length) adds the `[↑][↓][🗑]` cluster to the header row,
/// right-pinned at a fixed width, and returns what the user clicked. `add`
/// receives the icon cache back so nested cards and buttons can use it (one
/// `&mut` borrow, handed down in sequence).
fn editor_card(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    fill: egui::Color32,
    title: &str,
    actions: Option<(usize, usize)>,
    add: impl FnOnce(&mut egui::Ui, &mut IconCenterCache),
) -> RowAction {
    let mut act = RowAction::None;
    egui::Frame::none()
        .fill(fill)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            if !title.is_empty() {
                ui.horizontal(|ui| {
                    ui.label(theme::section_label(title));
                    if let Some((idx, len)) = actions {
                        act = row_actions(ui, cache, idx, len);
                    }
                });
                ui.add_space(4.0);
            }
            add(ui, cache);
        });
    ui.add_space(6.0);
    act
}

/// Width a glyph-only [`icon_button`] occupies: it's square, so this is the
/// same cell height the button allocates. Callers that reserve room for the
/// button before laying out the flexible part of a row need this up front.
fn icon_button_width(ui: &egui::Ui) -> f32 {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let pad = ui.spacing().button_padding;
    ui.spacing().interact_size.y.max(font.size + pad.y * 2.0)
}

/// A short leading label placed before an editor textbox so the user knows what
/// the box is for.
fn field_label(ui: &mut egui::Ui, text: &str) {
    ui.label(egui::RichText::new(text).color(theme::DIM).small());
}

/// Gray placeholder/hint text for an editor textbox (entered text stays the
/// normal foreground; only the hint is dimmed).
fn gray_hint(text: &str) -> egui::RichText {
    egui::RichText::new(text).color(theme::FAINT)
}

/// A `+`-prefixed secondary button used for "add" affordances.
fn add_button(ui: &mut egui::Ui, cache: &mut IconCenterCache, label: &str) -> egui::Response {
    icon_button(ui, cache, "\u{f067}", label, IconBtn::Secondary, true)
}

/// The reusable `[↑][↓][🗑]` row-action widget. Returns the action the user
/// clicked; the caller records a path-addressed [`Deferred`] and applies the
/// container-correct op after the render loop.
///
/// The cluster pins itself to the row's right edge and then allocates exactly
/// [`ACTION_COL_W`], so section rows, field rows and builder rows all put their
/// buttons at the same x and share one vertical center — instead of each row
/// ending wherever its own content happened to run out.
fn row_actions(ui: &mut egui::Ui, cache: &mut IconCenterCache, idx: usize, len: usize) -> RowAction {
    let mut a = RowAction::None;
    ui.add_space((ui.available_width() - ACTION_COL_W).max(0.0));
    let h = ui.spacing().interact_size.y;
    ui.allocate_ui_with_layout(
        egui::vec2(ACTION_COL_W, h),
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            if icon_button(ui, cache, "\u{f1f8}", "", IconBtn::Danger, true)
                .on_hover_text("Remove")
                .clicked()
            {
                a = RowAction::Remove;
            }
            if icon_button(ui, cache, "\u{f063}", "", IconBtn::Secondary, idx + 1 < len)
                .on_hover_text("Move down")
                .clicked()
            {
                a = RowAction::Down;
            }
            if icon_button(ui, cache, "\u{f062}", "", IconBtn::Secondary, idx > 0)
                .on_hover_text("Move up")
                .clicked()
            {
                a = RowAction::Up;
            }
        },
    );
    a
}

// ── WRITE-BACK GATING ───────────────────────────────────────────────────────
//
// **Every optional-text write-back in this editor is gated on
// `Response::changed()`.** (2026-07-19 — the "opening a bundled module marks it
// as edited" bug.)
//
// These editors bind an `Option<String>` to a `TextEdit` through a scratch
// `String`, collapsing `None` into `""` on the way in. Writing the scratch back
// unconditionally therefore re-encodes `Some("")` as `None` on *every frame* —
// including the very first one, before the user has touched anything. A draft is
// created clean (its `baseline` is serialized from the same `Extension` the draft
// holds), so that unprompted normalisation was the entire bug: three shipped
// manifests carry an empty-string `Value`/`Requires` (`amd`, `misc`, `dxvk`), so
// those three — and only those three — grew a dirty marker in the IDE tree and an
// "unsaved changes" prompt the instant they were opened, on modules the user
// cannot even edit. It had nothing to do with which modules had stored config.
//
// *Why gate on `changed()` rather than "only assign if different":* an
// if-different guard would still be lossy the moment the user *does* type,
// because the round trip `Some("") -> "" -> None` is not information-preserving
// in either direction. `changed()` is egui's own statement that this frame's
// value came from the user, which is exactly the precondition for overwriting the
// model. It also means a disabled widget can never write at all —
// `add_enabled_ui(false, ..)` greys the control but does not stop the plain Rust
// assignment after it, which is why read-only modules were affected too.
//
// *Why keep the `""` / absent distinction at all,* rather than declaring them
// equivalent and normalising the manifests: for `Separator` they are genuinely
// different values. `builder.rs` reads an absent separator as `","`
// (`entry.separator.as_deref().unwrap_or(",")`), so silently turning
// `"Separator": ""` into `null` would change the assembled launch command, not
// merely the dirty flag.
//
// Sites: `opt_text_edit`, `requires_edit`, the field `Name` row, and the env
// builder step's `Value` / `Separator` rows.

/// Edit an `Option<String>` as a single line (empty text → `None`) with a leading
/// `label` and a gray `hint` placeholder. `id_salt` gives the box a stable egui id
/// so it never loses focus when banners above it appear/disappear.
fn opt_text_edit(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    label: &str,
    val: &mut Option<String>,
    hint: &str,
) {
    let mut s = val.clone().unwrap_or_default();
    let resp = ui
        .horizontal(|ui| {
            let w = editor_row_label(ui, label, 0.0);
            ui.add(
                egui::TextEdit::singleline(&mut s)
                    .id_salt(id_salt)
                    .hint_text(gray_hint(hint))
                    .desired_width(w),
            )
        })
        .inner;
    // Write back ONLY on a real edit — see WRITE-BACK GATING above.
    if resp.changed() {
        *val = if s.is_empty() { None } else { Some(s) };
    }
}

/// A single-line `Requires` editor with a leading label, live-validated via
/// `condition::parse`. Entered text is the normal foreground and turns red only on
/// a parse error; the hint stays gray. The wordy error is shown once the field is
/// no longer being edited. `id_salt` keeps focus stable across banner reflows.
fn requires_edit(ui: &mut egui::Ui, id_salt: impl std::hash::Hash, val: &mut Option<String>) {
    let mut s = val.clone().unwrap_or_default();
    let parsed = condition::parse(&s);
    let ok = s.trim().is_empty() || parsed.is_ok();
    let color = if ok { theme::TEXT } else { theme::COL_GLOBAL };
    let resp = ui
        .horizontal(|ui| {
            let w = editor_row_label(ui, "Requires", 0.0);
            ui.add(
                egui::TextEdit::singleline(&mut s)
                    .id_salt(id_salt)
                    .hint_text(gray_hint("e.g. enabled AND !clear"))
                    .text_color(color)
                    .desired_width(w),
            )
        })
        .inner;
    // Write back ONLY on a real edit — see WRITE-BACK GATING above. `dxvk.json`
    // ships `"Requires": ""`, which this used to null out on the first frame.
    //
    // `s` is stored UNTRIMMED on purpose (issue #29): `val` is the live `TextEdit`
    // buffer, so trimming here would eat a trailing space as it is typed and make
    // `enabled AND !clear` unreachable. The emptiness test is on `s.trim()` so a
    // whitespace-only entry still clears the field; canonicalisation of the kept
    // value happens at the read boundary instead — see `normalize_requires`.
    if resp.changed() {
        *val = if s.trim().is_empty() { None } else { Some(s) };
    }
    if !ok && (!resp.has_focus() || resp.lost_focus()) {
        if let Err(e) = parsed {
            ui.label(egui::RichText::new(e.to_string()).color(theme::COL_GLOBAL).small());
        }
    }
}

/// Split a stored custom-env row on its FIRST `=` into (name, value); a row with
/// no `=` becomes (whole string, empty value). Pure — the inverse of
/// `format!("{name}={value}")`.
fn parse_env_row(entry: &str) -> (String, String) {
    match entry.split_once('=') {
        Some((n, v)) => (n.to_string(), v.to_string()),
        None => (entry.to_string(), String::new()),
    }
}

/// POSIX env-var name charset: `^[A-Za-z_][A-Za-z0-9_]*$`.
fn is_valid_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Title text for a list-entry card: the entry's own identifying string, or a
/// numbered `kind` placeholder while it's still blank (a card must never show an
/// empty header — the title is what anchors the row-action cluster). Pure.
fn entry_title(value: &str, kind: &str, idx: usize) -> String {
    let v = value.trim();
    if v.is_empty() {
        format!("{kind} {}", idx + 1)
    } else {
        v.to_string()
    }
}

fn field_type_label(t: FieldType) -> &'static str {
    match t {
        FieldType::Toggle => "toggle",
        FieldType::Selection => "selection",
        FieldType::Integer => "integer",
        FieldType::Float => "float",
        FieldType::String => "string",
        FieldType::MultiString => "multi_string",
    }
}

fn env_op_label(op: EnvOp) -> &'static str {
    match op {
        EnvOp::Set => "set",
        EnvOp::Append => "append",
        EnvOp::Unset => "unset",
    }
}

/// Ensure a field's `Options` is a `List`, returning it for editing.
///
/// **Mutating — call this only from a user-initiated action, never from a
/// renderer.** It is a write-back in the sense of WRITE-BACK GATING above: for a
/// Selection field whose `Options` is absent, or is a leftover
/// [`OptionsSpec::Range`] from a `Type` switch, it *replaces* the value with an
/// empty `List`. Doing that while drawing the field would dirty a draft nobody
/// touched — the 2026-07-19 bug class, one instance of which lived at exactly
/// this function's old render call site.
///
/// The only caller is therefore [`Deferred::FieldOptAdd`], which is queued by a
/// click on "Add option": at that point materialising the list *is* the user's
/// edit. The Selection renderer instead reads the list through
/// [`selection_options`] and draws nothing when there is none.
fn ensure_list(options: &mut Option<OptionsSpec>) -> &mut Vec<String> {
    if !matches!(options, Some(OptionsSpec::List(_))) {
        *options = Some(OptionsSpec::List(Vec::new()));
    }
    match options {
        Some(OptionsSpec::List(v)) => v,
        _ => unreachable!(),
    }
}

/// The option rows a Selection field should draw, as an editable slice.
///
/// The non-mutating counterpart to [`ensure_list`], and the only one a renderer
/// may use. When `Options` is absent — or holds a [`OptionsSpec::Range`] a
/// `Type` switch left behind — this yields an **empty slice** instead of
/// installing `Some(List(vec![]))`, so drawing the field cannot change the
/// draft's shape. The list only comes into existence when the user clicks
/// "Add option" ([`Deferred::FieldOptAdd`]).
///
/// The strings themselves are still `&mut`: editing one in place is exactly what
/// the option textboxes do, and that write is user-driven. What this refuses is
/// the *structural* write that used to happen before the first pixel was drawn.
fn selection_options(options: &mut Option<OptionsSpec>) -> &mut [String] {
    match options {
        Some(OptionsSpec::List(v)) => v,
        _ => Default::default(),
    }
}

fn new_field(variable: String) -> UiField {
    UiField {
        name: None,
        description: None,
        field_type: FieldType::Toggle,
        variable,
        default: None,
        options: None,
        display_options: None,
        requires: None,
    }
}

/// The whole editable body: meta, UI sections (with fields), and the four output
/// blocks. Structural edits are pushed to `deferred`; scalar edits mutate in place.
/// The "(rename pending)" note shown under any staged identity edit — module
/// Author, module Name, or a field's Variable.
///
/// Rendered on its own line so it can never shorten the input above it and
/// break the shared control column. Red rather than green: green reads as
/// "settled" everywhere else in the scope colors, and an unapplied rename is
/// the opposite of settled — it is lost if the user walks away without
/// pressing Rename.
fn pending_rename_note(ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        editor_row_label(ui, "", 0.0);
        ui.label(
            egui::RichText::new("(rename pending)")
                .color(theme::COL_GLOBAL)
                .small(),
        );
    });
}

fn render_editor_body(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    draft: &mut ModuleDraft,
    deferred: &mut Vec<Deferred>,
) {
    let ModuleDraft {
        ext,
        sections,
        baseline_vars,
        identity,
        ..
    } = draft;

    // ── Meta ────────────────────────────────────────────────────────────────
    ui.add_space(6.0);
    editor_card(ui, cache, theme::EDIT_L0, "Module", None, |ui, _cache| {
        // Author + Name are STAGED identity edits: they mutate `identity`, never
        // `ext.meta`, so they don't touch the draft snapshot / Save gate and are
        // committed only through the Rename button.
        ui.horizontal(|ui| {
            let w = editor_row_label(ui, "Author", 0.0);
            ui.add(
                egui::TextEdit::singleline(&mut identity.author)
                    .id_salt("meta_author")
                    .desired_width(w),
            );
        });
        // Same note the Variable rows carry, on the same terms: compared
        // **trimmed**, through the same accessor `has_pending_identity` uses, so
        // it appears exactly when the Rename gate agrees. A red note beside a
        // field the gate ignores — which is what a raw comparison produced the
        // moment a trailing space was typed — is a lie about the same state.
        if identity.staged_author() != ext.meta.author {
            pending_rename_note(ui);
        }
        ui.horizontal(|ui| {
            let w = editor_row_label(ui, "Name", 0.0);
            ui.add(
                egui::TextEdit::singleline(&mut identity.name)
                    .id_salt("meta_name")
                    .desired_width(w),
            );
        });
        if identity.staged_name() != ext.meta.name {
            pending_rename_note(ui);
        }
        ui.label(
            egui::RichText::new(
                "Author & Name are applied via Rename (migrates saved settings). Version is fixed.",
            )
            .color(theme::FAINT)
            .small(),
        );
        ui.add_space(6.0);
        opt_text_edit(ui, "meta_desc", "Description", &mut ext.meta.description, "What this module does");
        ui.add_space(4.0);
        opt_text_edit(ui, "meta_backend", "Backend", &mut ext.backend, "advanced, optional");
    });

    // ── UI sections ─────────────────────────────────────────────────────────
    // The block header lives INSIDE the block's own card (it used to sit above
    // the boxes), so every title is enclosed by the border it names.
    ui.add_space(12.0);
    editor_card(ui, cache, theme::EDIT_L0, "UI Sections", None, |ui, cache| {
        let sec_len = sections.len();
        for (si, (name, fields)) in sections.iter_mut().enumerate() {
            let a = editor_card(
                ui,
                cache,
                theme::EDIT_L1,
                &entry_title(name, "Section", si),
                Some((si, sec_len)),
                |ui, cache| {
                    ui.horizontal(|ui| {
                        let w = editor_row_label(ui, "Name", 0.0);
                        ui.add(
                            egui::TextEdit::singleline(name)
                                .id_salt(("sec_name", si))
                                .hint_text(gray_hint("Section name"))
                                .desired_width(w),
                        );
                    });
                    ui.add_space(4.0);
                    let f_len = fields.len();
                    for (fi, field) in fields.iter_mut().enumerate() {
                        render_field_editor(
                            ui,
                            cache,
                            field,
                            si,
                            fi,
                            f_len,
                            baseline_vars,
                            &mut identity.var_edits,
                            deferred,
                        );
                    }
                    if add_button(ui, cache, "Add field").clicked() {
                        deferred.push(Deferred::FieldAdd(si));
                    }
                },
            );
            if a != RowAction::None {
                deferred.push(Deferred::Section(si, a));
            }
        }
        if add_button(ui, cache, "Add section").clicked() {
            deferred.push(Deferred::SectionAdd);
        }
    });

    // ── Output blocks ─────────────────────────────────────────────────────────
    render_env_block_editor(ui, cache, "ENV_VARS", false, &mut ext.env_vars, deferred);
    render_env_block_editor(ui, cache, "GAME_ENV_VARS", true, &mut ext.game_env_vars, deferred);
    render_wrapper_block_editor(ui, cache, &mut ext.wrappers, deferred);
    render_arg_block_editor(ui, cache, &mut ext.game_launch_args, deferred);
}

/// One editable UI-field card. An *existing* field's `Variable` (present on disk)
/// is renamed as a **staged identity edit** — the buffer lives in
/// `var_edits[current-name]`, so `field.variable` itself is untouched (no draft
/// dirtiness) until the Rename action migrates config and rewrites the manifest.
/// A newly added field's `Variable` is edited directly (no config to migrate).
// Allowed, not refactored: the nine are four unrelated concerns (egui plumbing,
// the field, its position, and the rename-staging pair) with no subset that
// forms a meaningful type — bundling them would only rename the plumbing.
#[allow(clippy::too_many_arguments)]
fn render_field_editor(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    field: &mut UiField,
    si: usize,
    fi: usize,
    f_len: usize,
    baseline_vars: &std::collections::HashSet<String>,
    var_edits: &mut IndexMap<String, String>,
    deferred: &mut Vec<Deferred>,
) {
    let title = entry_title(
        field.name.as_deref().unwrap_or(&field.variable),
        "Field",
        fi,
    );
    let a = editor_card(ui, cache, theme::EDIT_L2, &title, Some((fi, f_len)), |ui, cache| {
        ui.horizontal(|ui| {
            let w = editor_row_label(ui, "Name", 0.0);
            let mut name = field.name.clone().unwrap_or_default();
            let resp = ui.add(
                egui::TextEdit::singleline(&mut name)
                    .id_salt(("field_name", si, fi))
                    .hint_text(gray_hint("Field label"))
                    .desired_width(w),
            );
            // Write back ONLY on a real edit — see WRITE-BACK GATING above.
            if resp.changed() {
                field.name = if name.is_empty() { None } else { Some(name) };
            }
        });

        // Variable — for an existing (on-disk) field this edits the STAGED rename
        // buffer (committed via Rename); a newly added field edits it directly.
        let mut pending_rename = false;
        ui.horizontal(|ui| {
            let w = editor_row_label(ui, "Variable", 0.0);
            if baseline_vars.contains(&field.variable) {
                let key = field.variable.clone();
                let buf = var_edits.entry(key.clone()).or_insert_with(|| key.clone());
                ui.add(
                    egui::TextEdit::singleline(buf)
                        .id_salt(("field_var", si, fi))
                        .desired_width(w),
                );
                // `buf`'s borrow of `var_edits` ends with the `ui.add` above; the
                // buffer itself is never normalised in place (that would eat the
                // space mid-typing), only read trimmed.
                pending_rename = PendingIdentity::staged_var_changed(var_edits, &key);
            } else {
                ui.add(
                    egui::TextEdit::singleline(&mut field.variable)
                        .id_salt(("field_var", si, fi))
                        .hint_text(gray_hint("variable_name"))
                        .desired_width(w),
                );
            }
        });
        // Rendered on its own line so the note can never shorten the Variable
        // box and break the shared control column.
        if pending_rename {
            pending_rename_note(ui);
        }

        // Type.
        ui.horizontal(|ui| {
            let w = editor_row_label(ui, "Type", 0.0);
            egui::ComboBox::from_id_salt((si, fi, "type"))
                .width(w)
                .selected_text(field_type_label(field.field_type))
                .show_ui(ui, |ui| {
                    for t in [
                        FieldType::Toggle,
                        FieldType::Selection,
                        FieldType::Integer,
                        FieldType::Float,
                        FieldType::String,
                        FieldType::MultiString,
                    ] {
                        ui.selectable_value(&mut field.field_type, t, field_type_label(t));
                    }
                });
        });

        opt_text_edit(ui, ("field_desc", si, fi), "Description", &mut field.description, "Shown under the field");
        requires_edit(ui, ("field_req", si, fi), &mut field.requires);

        // Type-specific detail.
        match field.field_type {
            FieldType::Selection => {
                // `selection_options`, NOT `ensure_list` — see WRITE-BACK
                // GATING above. Rendering a Selection field must not give it an
                // empty `Options` list it did not have; a field driven by
                // `DisplayOptions` alone (or one just switched over from
                // Integer, still carrying a `Range`) would otherwise be edited
                // by the act of being looked at. No list yet simply means no
                // option rows — "Add option" below is what creates one.
                let opts = selection_options(&mut field.options);
                let ol = opts.len();
                for (oi, opt) in opts.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        // Each option row keeps the same label column and the
                        // same right-pinned action cluster as every other row.
                        let w = editor_row_label(ui, "Option", ACTION_COL_W);
                        ui.add(
                            egui::TextEdit::singleline(opt)
                                .id_salt(("field_opt", si, fi, oi))
                                .hint_text(gray_hint("option"))
                                .desired_width(w),
                        );
                        let a = row_actions(ui, cache, oi, ol);
                        if a != RowAction::None {
                            deferred.push(Deferred::FieldOpt(si, fi, oi, a));
                        }
                    });
                }
                if add_button(ui, cache, "Add option").clicked() {
                    deferred.push(Deferred::FieldOptAdd(si, fi));
                }
            }
            FieldType::Integer | FieldType::Float => {
                let (mut min, mut max, mut step) = number_range(field);
                let mut ch = false;
                ui.horizontal(|ui| {
                    editor_row_label(ui, "Range", 0.0);
                    field_label(ui, "Min");
                    ch |= ui.add(egui::DragValue::new(&mut min)).changed();
                    field_label(ui, "Max");
                    ch |= ui.add(egui::DragValue::new(&mut max)).changed();
                    field_label(ui, "Step");
                    ch |= ui.add(egui::DragValue::new(&mut step)).changed();
                });
                if ch {
                    field.options = Some(OptionsSpec::Range { min, max, step: Some(step) });
                }
            }
            FieldType::Toggle => {
                ui.horizontal(|ui| {
                    editor_row_label(ui, "Default", 0.0);
                    let mut def =
                        field.default.as_ref().and_then(|v| v.as_bool()).unwrap_or(false);
                    if ui.checkbox(&mut def, "On").changed() {
                        field.default = Some(json!(def));
                    }
                });
            }
            FieldType::String | FieldType::MultiString => {}
        }
    });
    if a != RowAction::None {
        deferred.push(Deferred::Field(si, fi, a));
    }
}

/// ENV_VARS / GAME_ENV_VARS editor. `game` selects which block (for deferral).
fn render_env_block_editor(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    title: &str,
    game: bool,
    specs: &mut [EnvVarSpec],
    deferred: &mut Vec<Deferred>,
) {
    ui.add_space(12.0);
    editor_card(ui, cache, theme::EDIT_L0, title, None, |ui, cache| {
        let len = specs.len();
        for (ei, spec) in specs.iter_mut().enumerate() {
            let a = editor_card(
                ui,
                cache,
                theme::EDIT_L1,
                &entry_title(&spec.name, "Variable", ei),
                Some((ei, len)),
                |ui, cache| {
                    ui.horizontal(|ui| {
                        // "Var Name", not "Name": this row names the *environment
                        // variable*, and three other editor rows already say
                        // "Name" for a module / section / field label.
                        let w = editor_row_label(ui, "Var Name", 0.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut spec.name)
                                .id_salt(("env_name", game, ei))
                                .hint_text(gray_hint("VAR_NAME"))
                                .desired_width(w),
                        );
                    });
                    requires_edit(ui, ("env_req", game, ei), &mut spec.requires);
                    let sl = spec.builder.len();
                    for (bi, step) in spec.builder.iter_mut().enumerate() {
                        let sa = editor_card(
                            ui,
                            cache,
                            theme::EDIT_L3,
                            &format!("Step {}", bi + 1),
                            Some((bi, sl)),
                            |ui, _cache| {
                                ui.horizontal(|ui| {
                                    // "Operation", not "Op": next to "Value" and
                                    // "Separator" the abbreviation reads as a
                                    // third noun rather than the verb it is. Same
                                    // width class as "Separator", so the fixed
                                    // label column absorbs it — see
                                    // `every_editor_row_label_fits_the_label_column`.
                                    let w = editor_row_label(ui, "Operation", 0.0);
                                    egui::ComboBox::from_id_salt((game, ei, bi, "op"))
                                        .selected_text(env_op_label(step.op))
                                        .width(w)
                                        .show_ui(ui, |ui| {
                                            for op in [EnvOp::Set, EnvOp::Append, EnvOp::Unset] {
                                                ui.selectable_value(
                                                    &mut step.op,
                                                    op,
                                                    env_op_label(op),
                                                );
                                            }
                                        });
                                });
                                ui.horizontal(|ui| {
                                    let w = editor_row_label(ui, "Value", 0.0);
                                    let mut v = step.value.clone().unwrap_or_default();
                                    let resp = ui.add(
                                        egui::TextEdit::singleline(&mut v)
                                            .id_salt(("env_step_val", game, ei, bi))
                                            .hint_text(gray_hint("value"))
                                            .desired_width(w),
                                    );
                                    // Write back ONLY on a real edit — see
                                    // WRITE-BACK GATING above. `amd.json` and
                                    // `misc.json` both ship `"Value": ""` (a
                                    // `set` that clears the variable), which this
                                    // used to null out on the first frame.
                                    if resp.changed() {
                                        step.value =
                                            if v.is_empty() { None } else { Some(v) };
                                    }
                                });
                                // The separator only means anything for `append`
                                // (set/unset replace or clear), so it only shows
                                // there — one less always-empty row per step.
                                if step.op == EnvOp::Append {
                                    ui.horizontal(|ui| {
                                        let w = editor_row_label(ui, "Separator", 0.0);
                                        let mut sep = step.separator.clone().unwrap_or_default();
                                        let resp = ui.add(
                                            egui::TextEdit::singleline(&mut sep)
                                                .id_salt(("env_step_sep", game, ei, bi))
                                                .hint_text(gray_hint("e.g. :"))
                                                .desired_width(w),
                                        );
                                        // Write back ONLY on a real edit — see
                                        // WRITE-BACK GATING above. Here it also
                                        // guards a semantic difference, not just a
                                        // dirty flag: an absent separator means
                                        // `","` to `builder.rs`, an empty one means
                                        // "join with nothing".
                                        if resp.changed() {
                                            step.separator =
                                                if sep.is_empty() { None } else { Some(sep) };
                                        }
                                    });
                                }
                                requires_edit(
                                    ui,
                                    ("env_step_req", game, ei, bi),
                                    &mut step.requires,
                                );
                            },
                        );
                        if sa != RowAction::None {
                            deferred.push(Deferred::EnvStep(game, ei, bi, sa));
                        }
                    }
                    if add_button(ui, cache, "Add step").clicked() {
                        deferred.push(Deferred::EnvStepAdd(game, ei));
                    }
                },
            );
            if a != RowAction::None {
                deferred.push(Deferred::Env(game, ei, a));
            }
        }
        if add_button(ui, cache, "Add variable").clicked() {
            deferred.push(Deferred::EnvAdd(game));
        }
    });
}

/// WRAPPERS editor: CommandSyntax, an editable Priority (lower = outermost), and
/// per-step option values.
fn render_wrapper_block_editor(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    wrappers: &mut [WrapperSpec],
    deferred: &mut Vec<Deferred>,
) {
    ui.add_space(12.0);
    editor_card(ui, cache, theme::EDIT_L0, "WRAPPERS", None, |ui, cache| {
        let len = wrappers.len();
        for (wi, w) in wrappers.iter_mut().enumerate() {
            let a = editor_card(
                ui,
                cache,
                theme::EDIT_L1,
                &entry_title(&w.command_syntax, "Wrapper", wi),
                Some((wi, len)),
                |ui, cache| {
                    ui.horizontal(|ui| {
                        let cw = editor_row_label(ui, "Command", 0.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut w.command_syntax)
                                .id_salt(("wrap_syntax", wi))
                                .hint_text(gray_hint("gamescope {OPTIONS} --"))
                                .desired_width(cw),
                        );
                    });
                    ui.horizontal(|ui| {
                        editor_row_label(ui, "Priority", 0.0);
                        ui.add(egui::DragValue::new(&mut w.priority));
                        ui.label(
                            egui::RichText::new("lower = outermost").color(theme::FAINT).small(),
                        );
                    });
                    requires_edit(ui, ("wrap_req", wi), &mut w.requires);
                    let sl = w.builder.len();
                    for (bi, step) in w.builder.iter_mut().enumerate() {
                        let sa = editor_card(
                            ui,
                            cache,
                            theme::EDIT_L3,
                            &format!("Option {}", bi + 1),
                            Some((bi, sl)),
                            |ui, _cache| {
                                ui.horizontal(|ui| {
                                    let vw = editor_row_label(ui, "Value", 0.0);
                                    ui.add(
                                        egui::TextEdit::singleline(&mut step.value)
                                            .id_salt(("wrap_step_val", wi, bi))
                                            .hint_text(gray_hint("option"))
                                            .desired_width(vw),
                                    );
                                });
                                requires_edit(ui, ("wrap_step_req", wi, bi), &mut step.requires);
                            },
                        );
                        if sa != RowAction::None {
                            deferred.push(Deferred::WrapperStep(wi, bi, sa));
                        }
                    }
                    if add_button(ui, cache, "Add option").clicked() {
                        deferred.push(Deferred::WrapperStepAdd(wi));
                    }
                },
            );
            if a != RowAction::None {
                deferred.push(Deferred::Wrapper(wi, a));
            }
        }
        if add_button(ui, cache, "Add wrapper").clicked() {
            deferred.push(Deferred::WrapperAdd);
        }
    });
}

/// GAME_LAUNCH_ARGS editor: one Value + Requires per arg.
fn render_arg_block_editor(
    ui: &mut egui::Ui,
    cache: &mut IconCenterCache,
    args: &mut [ArgSpec],
    deferred: &mut Vec<Deferred>,
) {
    ui.add_space(12.0);
    editor_card(ui, cache, theme::EDIT_L0, "GAME_LAUNCH_ARGS", None, |ui, cache| {
        let len = args.len();
        for (ai, arg) in args.iter_mut().enumerate() {
            let a = editor_card(
                ui,
                cache,
                theme::EDIT_L1,
                &entry_title(&arg.value, "Argument", ai),
                Some((ai, len)),
                |ui, _cache| {
                    ui.horizontal(|ui| {
                        let w = editor_row_label(ui, "Value", 0.0);
                        ui.add(
                            egui::TextEdit::singleline(&mut arg.value)
                                .id_salt(("arg_val", ai))
                                .hint_text(gray_hint("--argument"))
                                .desired_width(w),
                        );
                    });
                    requires_edit(ui, ("arg_req", ai), &mut arg.requires);
                },
            );
            if a != RowAction::None {
                deferred.push(Deferred::Arg(ai, a));
            }
        }
        if add_button(ui, cache, "Add argument").clicked() {
            deferred.push(Deferred::ArgAdd);
        }
    });
}

/// Apply a `RowAction` to a `Vec` (reorder via `swap`, remove via `remove`).
fn apply_row<T>(v: &mut Vec<T>, idx: usize, a: RowAction) {
    match a {
        RowAction::Up => {
            if idx > 0 && idx < v.len() {
                v.swap(idx - 1, idx);
            }
        }
        RowAction::Down => {
            if idx + 1 < v.len() {
                v.swap(idx, idx + 1);
            }
        }
        RowAction::Remove => {
            if idx < v.len() {
                v.remove(idx);
            }
        }
        RowAction::None => {}
    }
}

/// A `base` name made unique against `used` by appending `_2`, `_3`, …
fn unique_string(used: &std::collections::HashSet<String>, base: &str) -> String {
    if !used.contains(base) {
        return base.to_string();
    }
    let mut n = 2;
    loop {
        let cand = format!("{base}_{n}");
        if !used.contains(&cand) {
            return cand;
        }
        n += 1;
    }
}

fn env_block(ext: &mut Extension, game: bool) -> &mut Vec<EnvVarSpec> {
    if game {
        &mut ext.game_env_vars
    } else {
        &mut ext.env_vars
    }
}

/// Apply one path-addressed structural edit to the draft. Applied after the
/// render loop so the containers are never mutated mid-iteration.
fn apply_deferred(draft: &mut ModuleDraft, d: Deferred) {
    match d {
        Deferred::Section(i, a) => apply_row(&mut draft.sections, i, a),
        Deferred::SectionAdd => {
            let used: std::collections::HashSet<String> =
                draft.sections.iter().map(|(k, _)| k.clone()).collect();
            draft
                .sections
                .push((unique_string(&used, "New Section"), Vec::new()));
        }
        Deferred::Field(si, fi, a) => {
            if let Some((_, fs)) = draft.sections.get_mut(si) {
                apply_row(fs, fi, a);
            }
        }
        Deferred::FieldAdd(si) => {
            let used: std::collections::HashSet<String> = draft
                .sections
                .iter()
                .flat_map(|(_, fs)| fs.iter().map(|f| f.variable.clone()))
                .collect();
            let var = unique_string(&used, "new_var");
            if let Some((_, fs)) = draft.sections.get_mut(si) {
                fs.push(new_field(var));
            }
        }
        Deferred::FieldOpt(si, fi, oi, a) => {
            if let Some((_, fs)) = draft.sections.get_mut(si) {
                if let Some(f) = fs.get_mut(fi) {
                    if let Some(OptionsSpec::List(v)) = &mut f.options {
                        apply_row(v, oi, a);
                    }
                }
            }
        }
        Deferred::FieldOptAdd(si, fi) => {
            if let Some((_, fs)) = draft.sections.get_mut(si) {
                if let Some(f) = fs.get_mut(fi) {
                    ensure_list(&mut f.options).push(String::new());
                }
            }
        }
        Deferred::Env(game, i, a) => apply_row(env_block(&mut draft.ext, game), i, a),
        Deferred::EnvAdd(game) => env_block(&mut draft.ext, game).push(EnvVarSpec {
            name: "NEW_VAR".to_string(),
            requires: None,
            builder: Vec::new(),
        }),
        Deferred::EnvStep(game, ei, bi, a) => {
            if let Some(e) = env_block(&mut draft.ext, game).get_mut(ei) {
                apply_row(&mut e.builder, bi, a);
            }
        }
        Deferred::EnvStepAdd(game, ei) => {
            if let Some(e) = env_block(&mut draft.ext, game).get_mut(ei) {
                e.builder.push(EnvBuilderEntry {
                    requires: None,
                    op: EnvOp::Set,
                    value: Some(String::new()),
                    separator: None,
                });
            }
        }
        Deferred::Wrapper(i, a) => apply_row(&mut draft.ext.wrappers, i, a),
        Deferred::WrapperAdd => draft.ext.wrappers.push(WrapperSpec {
            command_syntax: String::new(),
            requires: None,
            priority: 0,
            builder: Vec::new(),
        }),
        Deferred::WrapperStep(wi, bi, a) => {
            if let Some(w) = draft.ext.wrappers.get_mut(wi) {
                apply_row(&mut w.builder, bi, a);
            }
        }
        Deferred::WrapperStepAdd(wi) => {
            if let Some(w) = draft.ext.wrappers.get_mut(wi) {
                w.builder.push(WrapperBuilderEntry {
                    requires: None,
                    value: String::new(),
                });
            }
        }
        Deferred::Arg(i, a) => apply_row(&mut draft.ext.game_launch_args, i, a),
        Deferred::ArgAdd => draft.ext.game_launch_args.push(ArgSpec {
            requires: None,
            value: String::new(),
        }),
    }
}

/// One env var where a module's `Set` or `Unset` throws away a value an
/// **earlier, different** module already contributed to the same var.
///
/// *Which combinations actually lose data* (2026-07-19) — derived by reading
/// `ritz_core::builder::build_env_block`, which folds one `EnvAccum` per var name
/// across every module in declaration order, then every `Builder` step in
/// declaration order:
///
/// | earlier steps | later step | accumulator effect | lossy? |
/// |---|---|---|---|
/// | `Set`    | `Set`    | `accum.value = value` — the old value is gone | **yes** |
/// | `Set`    | `Append` | `accum.value = old + sep + new` | no |
/// | `Append` | `Append` | `accum.value = old + sep + new` | no |
/// | `Append` | `Set`    | `accum.value = value` — the appended text is gone | **yes** |
/// | `Set`/`Append` | `Unset` | `accum.value.clear()`, `unset = true` | **yes** |
/// | `Unset`  | `Set`/`Append` | writes into an accumulator already cleared | no |
///
/// Two deliberate judgement calls behind that table:
///
/// - **`Append` then `Set` warns, `Set` then `Append` does not.** The pair is
///   lossy *only in one order*, and `build_env_block` folds in module declaration
///   order, so the order the lint sees is the order that will actually run — the
///   asymmetry is real, not an artefact. Warning on the harmless order is the bug
///   this replaced; staying silent while a `Set` swallows another module's append
///   would be the worse failure, so the lossy order still warns.
/// - **`Unset` then `Set` does not warn.** An `Unset` carries no value, so nothing
///   is *lost* when a later write lands on top of it — only an intent is
///   overridden. The user's report is specifically about data loss, and a lint
///   that also fired on overridden intent would be back to crying wolf.
///
/// Loss is only reported **across modules**. A single module's own steps are an
/// authored sequence (`Set` a base, `Append` extras, `Unset` under some
/// condition); its author chose that order and does not need warning about it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct EnvOverwrite {
    /// Env var name, after interpolation — what the fold actually keys on.
    var: String,
    /// Display names of the modules whose contribution is discarded, in fold order.
    victims: Vec<String>,
    /// Display name of the module whose step discards it.
    culprit: String,
    /// `true` when the discarding op was `Unset` (the var ends up gone entirely,
    /// not merely rewritten) — the two cases read differently to a user.
    by_unset: bool,
}

impl EnvOverwrite {
    /// The one-line diagnostic text. Kept off the layout code so the future
    /// diagnostics panel can reuse it verbatim.
    fn message(&self) -> String {
        let victims = self.victims.join(", ");
        if self.by_unset {
            format!("{} unsets {} after {} set it — that value is lost.",
                self.culprit, self.var, victims)
        } else {
            format!("{} overwrites {} — the value from {} is lost.",
                self.culprit, self.var, victims)
        }
    }
}

/// UI-side lint over the launch preview: env vars where one module's `Set` or
/// `Unset` discards a value an earlier module already contributed. Sorted by var
/// name, then by the module doing the discarding.
///
/// *Why this is no longer "two modules both `Set`"* (2026-07-19): the previous
/// `set_set_collisions` recovered the per-module attribution that the shared fold
/// throws away by re-running `context::assemble_launch` on each module **in
/// isolation** and looking for `EnvAction::Set`. That cannot work, because
/// `ritz_core::builder::EnvAction` has only `Set(String)` and `Unset` variants — `build_env_block`
/// collapses every op into one accumulator and emits `EnvAction::Set(final)` for
/// anything that ended up holding a value at all. A module whose only op is
/// `Append` has nothing to append *to* when assembled alone, so it came back as
/// `Set(its_own_value)` and was counted as a setter. **Every `Append` looked like
/// a `Set`**, so two modules appending to one var — the correct, lossless way to
/// share it — raised a data-loss warning. Reported by the user against
/// `RADV_PERFTEST`.
///
/// So this no longer asks the assembler what happened. It walks the specs in the
/// same order `build_env_block` folds them and models the accumulator directly,
/// which is the only way to see the op *kind* and the *order* — both of which the
/// assembled `LaunchCommand` has already erased.
///
/// *Why it stays in `ritz-app` and not `ritz-core`:* it needs nothing that isn't
/// already public (`Resolution::var_store`, `VarStore::interpolate`/`lookup_fn`,
/// `condition::eval_opt`), and it is a UI lint over a preview — an opinion about
/// what to warn a module author about, not a rule the launcher enforces. Core
/// stays the source of truth for what the fold *does*; this file owns what the
/// editor *says about it*.
fn lossy_env_overwrites(specs: &[Extension], res: &Resolution) -> Vec<EnvOverwrite> {
    let mut out: Vec<EnvOverwrite> = Vec::new();
    // ENV_VARS and GAME_ENV_VARS get one `build_env_block` call each, so they get
    // one accumulator map each: the same name in both blocks is two independent
    // variables and never collides. (The old lint chained the two blocks together
    // and would have cross-reported them — a second, quieter false positive.)
    let blocks: [fn(&Extension) -> &[EnvVarSpec]; 2] =
        [|e| &e.env_vars, |e| &e.game_env_vars];
    for select in blocks {
        // var name → modules (id, display name) whose text is in the accumulator
        // right now, in fold order. Empty = nothing there to lose.
        let mut live: IndexMap<String, Vec<(String, String)>> = IndexMap::new();
        for spec in specs {
            let id = spec.id();
            let name = spec.meta.name.clone();
            let vars = res.var_store(&id);
            let lookup = vars.lookup_fn();
            // An unparseable `Requires` is treated as "does not apply", matching
            // what the user sees: `build_env_block` propagates the parse error and
            // `assemble_launch` fails outright, so nothing from this spec lands.
            for ev in select(spec) {
                if !condition::eval_opt(ev.requires.as_deref(), &lookup).unwrap_or(false) {
                    continue;
                }
                let var = vars.interpolate(&ev.name);
                if var.is_empty() {
                    // `build_env_block` skips empty names before touching the map.
                    continue;
                }
                for step in &ev.builder {
                    if !condition::eval_opt(step.requires.as_deref(), &lookup).unwrap_or(false) {
                        continue;
                    }
                    let holders = live.entry(var.clone()).or_default();
                    match step.op {
                        // Concatenates onto whatever is there; loses nothing, and
                        // adds this module to the set of contributors a later
                        // `Set`/`Unset` would destroy.
                        EnvOp::Append => {
                            if !holders.iter().any(|(hid, _)| *hid == id) {
                                holders.push((id.clone(), name.clone()));
                            }
                        }
                        // Both replace the accumulated value outright. Anything a
                        // *different* module put there is gone.
                        EnvOp::Set | EnvOp::Unset => {
                            let victims: Vec<String> = holders
                                .iter()
                                .filter(|(hid, _)| *hid != id)
                                .map(|(_, hname)| hname.clone())
                                .collect();
                            if !victims.is_empty() {
                                out.push(EnvOverwrite {
                                    var: var.clone(),
                                    victims,
                                    culprit: name.clone(),
                                    by_unset: step.op == EnvOp::Unset,
                                });
                            }
                            // After a `Set` this module owns the value alone; after
                            // an `Unset` there is no value at all, so a later write
                            // destroys nothing and must not warn.
                            holders.clear();
                            if step.op == EnvOp::Set {
                                holders.push((id.clone(), name.clone()));
                            }
                        }
                    }
                }
            }
        }
    }
    out.sort_by(|a, b| a.var.cmp(&b.var).then_with(|| a.culprit.cmp(&b.culprit)));
    out.dedup();
    out
}

/// How serious one entry in the IDE diagnostics band is.
///
/// *Why a three-level vocabulary* (2026-07-19, issue #26): the band used to hold
/// exactly one kind of thing — env-var overwrite warnings — so severity could be
/// implicit in the one colour and one icon it drew. It now also holds the open
/// draft's status messages, which span "here is what this draft is" (info),
/// "something is staged but unfinished" (warning) and "this is why Save is
/// greyed out" (error). Naming the three makes the band's ordering, its tally
/// and its empty state all derive from one axis instead of three ad-hoc `if`s,
/// and gives the ok/warning/error tally the heading row was built as a growth
/// point for something real to count.
///
/// *Why `Warning` and `Error` share [`theme::COL_GLOBAL`]:* `theme.rs` has no
/// amber. Its palette is scope colours (global red / profile green / game blue)
/// plus chrome, and `COL_GLOBAL` already doubles as the danger colour
/// (`docs/ui/STYLING-GUIDE.md`), which is what the band's warnings have always
/// been drawn in. Inventing a `Color32` literal here would violate the styling
/// guide's top-level rule, so the two ranks are distinguished by **icon** — a
/// triangle for a warning, a filled circle for an error — and share the colour.
/// **A dedicated `COL_WARN` (amber) / `COL_ERROR` (red) pair in `theme.rs` is
/// the right fix** and would let this `color()` split cleanly; it is deliberately
/// not done here because adding palette tokens is a theme decision, not a
/// diagnostics-band one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DiagSeverity {
    /// State, not a problem: the draft's dirty/clean/read-only line. Always
    /// exactly one, always pinned to the top of the list, and never counted by
    /// the tally.
    Info,
    /// Something is unfinished or lossy but no action is being refused: an
    /// env-var overwrite, a staged-but-unapplied rename.
    Warning,
    /// The reason an action is refused — every `Draft::save_enabled` gate, plus a
    /// blocked Rename.
    Error,
}

impl DiagSeverity {
    /// The text colour for this rank. Info is the accent blue; see the type's
    /// doc comment for why the other two are the same red.
    fn color(self) -> egui::Color32 {
        match self {
            DiagSeverity::Info => theme::ACCENT,
            DiagSeverity::Warning | DiagSeverity::Error => theme::COL_GLOBAL,
        }
    }

    /// The leading Nerd Font glyph. This is the *only* thing separating a
    /// warning from an error visually, so the three must stay distinct shapes:
    /// circled `i`, triangle `!`, circled `!`.
    fn icon(self) -> &'static str {
        match self {
            DiagSeverity::Info => "\u{f05a}",
            // Unchanged from before issue #26 — the env-overwrite warnings this
            // band already drew used this glyph, and they are still warnings.
            DiagSeverity::Warning => "\u{f071}",
            DiagSeverity::Error => "\u{f06a}",
        }
    }
}

/// One rendered line of the IDE diagnostics band.
struct DiagEntry {
    severity: DiagSeverity,
    text: String,
}

/// Everything the diagnostics band shows, in display order: the open draft's
/// status messages first, then the launch-preview env-overwrite warnings.
///
/// *Why the draft's messages come first* (2026-07-19, issue #26): they are about
/// the thing the user is typing into right now, while the env-overwrite warnings
/// are about how the whole module set composes at launch. The first entry —
/// always [`DiagSeverity::Info`], always the state line, guaranteed by
/// [`editor_status_lines`] — is therefore pinned at the very top of the list, in
/// info blue, which is what the user asked for: it is the answer to "is my work
/// saved", and that question should not move down the list as errors appear
/// above it.
///
/// The rest keeps source order rather than sorting by severity. Sorting would
/// scramble [`editor_status_lines`]' deliberate sequence (state → why Save is
/// blocked → why Rename is blocked), and with the whole list visible in a scroll
/// area there is no truncation for a sort to protect against.
///
/// `info` is `None` when no draft has resolved yet (a module switch, before
/// `ensure_draft` catches up); the band then shows only the env warnings, which
/// are computed independently of the draft.
fn ide_diagnostic_entries(
    info: Option<&EditorHeaderInfo>,
    diagnostics: &[EnvOverwrite],
) -> Vec<DiagEntry> {
    let mut out: Vec<DiagEntry> = Vec::new();
    if let Some(info) = info {
        for line in editor_status_lines(info) {
            out.push(DiagEntry { severity: line.severity, text: line.text });
        }
    }
    for d in diagnostics {
        out.push(DiagEntry { severity: DiagSeverity::Warning, text: d.message() });
    }
    out
}

/// Contents of `ide_editor_band` — the diagnostics band under the editor column.
///
/// Deliberately shaped as a *down payment* on the full diagnostics panel the IDE
/// plan calls for (ok / warning / error counts plus a list), not as something that
/// has to be torn out to build it:
///
/// - The header is a **row**, not a bare label: the title sits left, a right-aligned
///   tally sits opposite it. As of issue #26 the tally has two severities in it;
///   adding an ok count is appending to that row, and nothing below moves.
/// - The list is a **scroll area inside the framed box**, matching the launch band
///   beside it pixel for pixel (same `FIELD` fill, `BORDER` stroke, 8px rounding,
///   13/11 margins). The two 198px bands are peers and have to read as peers; more
///   diagnostics simply scroll rather than growing the band.
///
/// *Why the draft's status messages render here* (2026-07-19, issue #26): see
/// [`IDE_HEADER_H`]'s *Why the band holds no status lines*. The short version is
/// that this band was already fixed-height, always visible and scrolling, so it
/// absorbs them for free — and most of them are diagnostics in the first place.
/// [`ide_diagnostic_entries`] assembles the combined list.
///
/// *Why the tally counts warnings and errors but not info* (2026-07-19, issue
/// #26): exactly one `Info` entry is always present (the state line), so counting
/// it would mean the band never reads below "1" and a clean draft would claim to
/// have an issue. The tally answers "how much is wrong", and the info line is by
/// definition not wrong. Errors are named first and warnings second, because that
/// is the order the user has to deal with them in.
///
/// *Why an explicit "No issues" line rather than an empty box:* the band is a fixed
/// 198px and always visible, so "nothing wrong" and "lint didn't run" would look
/// identical if it rendered blank — the one state a diagnostics surface most needs
/// to distinguish. It is drawn in `FAINT`, not a success green: the quiet absence of
/// a problem is not an achievement to celebrate, and a coloured line would pull the
/// eye to the emptiest thing on screen. It is keyed to the *problem* count, not the
/// entry count, so it still appears under the pinned info line — a clean draft
/// reads "All changes saved." then "No issues.", which is the clean state stated
/// twice over rather than an empty box under a lone blue line.
fn render_ide_diagnostics_band(
    ui: &mut egui::Ui,
    info: Option<&EditorHeaderInfo>,
    diagnostics: &[EnvOverwrite],
    mono: bool,
) {
    let sep = icon_sep(mono);
    let entries = ide_diagnostic_entries(info, diagnostics);
    let count = |s: DiagSeverity| entries.iter().filter(|e| e.severity == s).count();
    let (errors, warnings) = (count(DiagSeverity::Error), count(DiagSeverity::Warning));
    ui.horizontal(|ui| {
        ui.label(theme::header_label("Diagnostics"));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            // Zero counts are omitted rather than shown as "0 errors": the row is
            // a status summary, not a table, and "3 warnings" alone is quieter and
            // faster to read than "0 errors \u{b7} 3 warnings".
            let parts: Vec<String> = [(errors, "error"), (warnings, "warning")]
                .iter()
                .filter(|(n, _)| *n > 0)
                .map(|(n, word)| format!("{n} {word}{}", if *n == 1 { "" } else { "s" }))
                .collect();
            if !parts.is_empty() {
                ui.label(
                    egui::RichText::new(parts.join(" \u{b7} "))
                        .color(theme::COL_GLOBAL)
                        .size(11.0)
                        .strong(),
                );
            }
        });
    });
    ui.add_space(8.0);
    egui::Frame::none()
        .fill(theme::FIELD)
        .stroke(egui::Stroke::new(1.0, theme::BORDER))
        .rounding(egui::Rounding::same(8.0))
        .inner_margin(egui::Margin::symmetric(13.0, 11.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            // Fill the rest of the fixed band, less this frame's own 11+11 vertical
            // inner margin — the same arithmetic the launch band uses so the two
            // boxes end on the same baseline. Unconditional here: unlike the launch
            // band there is no `dynamic_preview` variant, because the band's height
            // is not allowed to move.
            let box_h = ui.available_height();
            ui.set_min_height((box_h - 22.0).max(0.0));
            egui::ScrollArea::vertical()
                .id_salt("ide_diag_scroll")
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    for e in &entries {
                        ui.label(
                            egui::RichText::new(format!(
                                "{}{sep}{}",
                                e.severity.icon(),
                                e.text
                            ))
                            .color(e.severity.color())
                            .small(),
                        );
                    }
                    // Keyed to problems, not entries — see this function's doc
                    // comment. With no draft resolved and no warnings there are
                    // no entries at all, and this is the only line in the box.
                    if errors + warnings == 0 {
                        ui.label(
                            egui::RichText::new(format!("\u{f00c}{sep}No issues."))
                                .color(theme::FAINT)
                                .small(),
                        );
                    }
                });
        });
}

impl GuiApp {
    /// What the title-bar chip (`breadcrumb_chip`) should show right now, one
    /// branch per mode:
    ///
    /// - **Config mode, `GeneralSettings` selected** → the literal `"Settings"`.
    /// - **Config mode, anything else** (a game, a profile, `GlobalSettings`) →
    ///   the ambient game's name — unchanged from before this method existed,
    ///   `GlobalSettings` included: it lives under the "Profiles / Games"
    ///   category box tab (see `render_nav_category_box`'s `is_games`), so it
    ///   gets that tab's chip content, not `"Settings"`.
    /// - **IDE mode** → the *module* currently open, by its declared `Name`
    ///   (`EditorHeaderInfo::name`, what the header row and the module tree
    ///   both show) — not `Extension::id()`, which nobody but the config files
    ///   ever sees.
    ///
    /// *Why `"IDE Mode"` and not the previous game name or an empty chip* when
    /// no module has resolved yet: `GuiApp::update` sets `focused_module`
    /// and then calls `ensure_draft(&id)` several lines *before*
    /// `render_title_bar` runs later the same frame, so on every ordinary frame
    /// the draft is already synced to `id` by the time this method runs and
    /// `editor_header_info` returns `Some`. This fallback is only live when
    /// `all_specs` is empty (no modules loaded at all — nothing to be "the
    /// current module") or focus briefly isn't set yet inside IDE mode. A fixed
    /// string never flickers between two values on a module switch (there is no
    /// switch to observe — normal switches never take this branch) and is never
    /// empty, so the chip never changes size because it went blank.
    fn title_chip_text(&self) -> String {
        match self.mode {
            Mode::Ide => match self.focused_module() {
                Some(id) => self
                    .editor_header_info(id)
                    .map(|info| info.name)
                    .unwrap_or_else(|| "IDE Mode".to_string()),
                None => "IDE Mode".to_string(),
            },
            Mode::Config if matches!(self.nav_sel, NavSel::GeneralSettings) => {
                "Settings".to_string()
            }
            Mode::Config => self.game_config.game.name.clone(),
        }
    }

    /// The Graphite title bar: logo + wordmark, breadcrumb chip, and (in launch
    /// mode) the Cancel/Launch action pair.
    fn render_title_bar(&mut self, ctx: &egui::Context) {
        // Computed before the panel closure: `editor_header_info` only needs
        // `&self`, but borrowing it from inside the closure below (which also
        // touches `self.logo`/`self.outcome`/…) would fight the closure's own
        // borrow of `self`. Same reasoning as `ide_header` further down in
        // `update` — read first, use the plain `String` inside the closure.
        let chip_text = self.title_chip_text();
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
                    breadcrumb_chip(ui, &chip_text);

                    if self.launch_mode {
                        let sep = icon_sep(self.general_config.mono_ui);
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                // Both buttons close the window, so both go
                                // through the same unsaved-work guard the X
                                // button does — these are the *more* frequently
                                // clicked close path, and leaving them raw would
                                // be a hole straight through the fix. The outcome
                                // is passed along rather than applied here, so a
                                // cancelled prompt leaves `outcome` untouched.
                                if ui.add(theme::primary_button(format!("\u{f04b}{sep}Launch Game"))).clicked()
                                {
                                    self.request_app_close(EditOutcome::Continue);
                                }
                                ui.add_space(8.0);
                                if ui.add(theme::danger_button("Cancel Launch")).clicked() {
                                    self.request_app_close(EditOutcome::Cancel);
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
                .on_hover_text("Open the user extensions folder")
                .clicked()
            {
                // `user_extensions()` — this button sits under the *module* tree,
                // so it opens where module manifests live. It used to open
                // `games_dir()`, which had nothing to do with the column it was in.
                let _ = Command::new("xdg-open").arg(self.paths.user_extensions()).spawn();
            }
        });
    }

    /// IDE mode's right-hand column: a read-only WYSIWYG render of the module
    /// under edit, over a nested launch-command band.
    ///
    /// *Why the launch band is nested inside this side panel* rather than being a
    /// full-width bottom panel: it belongs to the preview, not to the editor — the
    /// space under the editor column is the reserved diagnostics band. The nesting
    /// idiom (`TopBottomPanel::bottom(..).show_inside(..)`) is the same one
    /// `group_toggle` and `nav_settings` already use. Nested panels obey
    /// declaration order exactly like top-level ones, so the band is declared
    /// **before** the inner `CentralPanel` or it would be laid out over the body.
    fn render_ide_preview_panel(
        &mut self,
        ctx: &egui::Context,
        ide_specs: &[Extension],
        preview: &str,
        dynamic_preview: bool,
    ) {
        let Some(id) = self.focused_module().map(str::to_string) else {
            return;
        };
        // Resolve over the SAME spliced list the launch band assembled from, so the
        // two halves of the column can never disagree. Resolving over `cur_specs`
        // (what `resolve_for_editing` hardwired before S3b) would return `None` for
        // any module that doesn't apply to the ambient game and blank the body.
        //
        // S5a: `resolve_specs_for_preview`, not `..._for_editing` — the body must
        // show the scratch layer it now writes into, and the launch band a few
        // lines down resolves the same way. Using the editing resolution here would
        // make the form show the ambient game's values while the launch string
        // showed the scratch ones.
        let resolution = self.resolve_specs_for_preview(ide_specs);
        let spec = ide_specs.iter().find(|s| s.id() == id).cloned();
        let touch = self.general_config.touch_mode;
        // Hoisted: `self` is re-borrowed inside the panel closures below.
        let sep = icon_sep(self.general_config.mono_ui);
        // ── Column split ────────────────────────────────────────────────────
        // A STATIC 50/50: editor and preview each get exactly half of everything
        // right of the fixed `NAV_W` nav column. Recomputed from the live
        // `screen_rect` every frame, so the halves track a window resize instead
        // of holding a width captured at first paint.
        //
        // *Why fixed and not a clamped drag range:* the splitter could be dragged
        // far enough left that the preview slid **behind** the nav column and
        // vanished. Clamping would paper over that; removing the handle removes
        // the failure mode outright, and the user asked for a static half-and-half
        // to begin with. The old `PREVIEW_MIN_W`/`EDITOR_MIN_W`/`width_range`
        // machinery only existed to bound that drag, so it is gone rather than
        // left inert — half a window is comfortably above either column's floor
        // on any display this app is usable on.
        //
        // *Not exactly half, by one pixel:* `SidePanel` paints its separator line
        // out of the space **outside** `exact_width`, so the editor's central
        // panel ends up ~1px narrower than the preview. Absorbing that into the
        // computation would make the two columns provably unequal in the other
        // direction; a 1px asymmetry from a hairline is the smaller lie.
        let split_w = ((ctx.screen_rect().width() - NAV_W) * 0.5).max(0.0);

        // Id deliberately bumped from "ide_preview" to "ide_preview_split" back
        // when the width was persisted. It stays: `exact_width` + `resizable(false)`
        // ignores stored width entirely, so the stale-memory hazard that motivated
        // the rename is now moot, but renaming again would only churn ids. (The
        // `push_id("ide_preview")` widget namespace below is unrelated and
        // intentionally left alone.)
        egui::SidePanel::right("ide_preview_split")
            .resizable(false)
            .exact_width(split_w)
            .frame(egui::Frame::none().fill(theme::PANEL))
            .show(ctx, |ui| {
                let mut band = egui::TopBottomPanel::bottom("ide_launch_band")
                    .show_separator_line(true)
                    .frame(egui::Frame::none()
                        .fill(theme::PANEL2)
                        .inner_margin(egui::Margin::same(16.0)));
                if !dynamic_preview {
                    band = band.exact_height(198.0);
                }
                band.show_inside(ui, |ui| {
                    ui.label(theme::header_label("Launch command preview"));
                    // No "Previewing against: {game}" line here, unlike Config mode.
                    // IDE Mode resolves against the scratch layer, whose game
                    // binding is now shown — and *chosen* — by the nav footer's
                    // "Preview against" selector (S5b). A second, non-interactive
                    // statement of the same fact three columns away would just be a
                    // thing that can go stale.
                    // No env-collision lint here any more (2026-07-19). It moved to
                    // `ide_editor_band` — the band under the *editor* column, which
                    // is the diagnostics surface. Keeping a copy here would put the
                    // same warning on screen twice, in the panel that is supposed to
                    // show what the launch *is*, not what is wrong with it.
                    ui.add_space(8.0);
                    egui::Frame::none()
                        .fill(theme::FIELD)
                        .stroke(egui::Stroke::new(1.0, theme::BORDER))
                        .rounding(egui::Rounding::same(8.0))
                        .inner_margin(egui::Margin::symmetric(13.0, 11.0))
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            if !dynamic_preview {
                                let box_h = ui.available_height();
                                ui.set_min_height((box_h - 22.0).max(0.0));
                            }
                            egui::ScrollArea::vertical()
                                .id_salt("ide_launch_scroll")
                                .auto_shrink([false, dynamic_preview])
                                .drag_to_scroll(touch)
                                .show(ui, |ui| {
                                    ui.add(egui::Label::new(command_job(preview)).wrap());
                                });
                        });
                });
                egui::CentralPanel::default()
                    .frame(egui::Frame::none()
                        .fill(theme::PANEL)
                        .inner_margin(egui::Margin { left: 16.0, right: 8.0, top: 14.0, bottom: 0.0 }))
                    .show_inside(ui, |ui| {
                        // ── Id namespace ────────────────────────────────────────
                        // The editor column and this one render the same module in
                        // the same frame. `render_module_settings_body` mints
                        // position-derived auto-ids (and `ComboBox::from_id_salt`
                        // salted by variable name ALONE) — identical in both columns,
                        // so without a namespace the two would fight over widget
                        // state. `push_id` must wrap the WHOLE body, not just the
                        // combo: the multi_string and env-pair renderers auto-id
                        // their TextEdits, "+ Add" buttons and icon_buttons too.
                        // This is the only `push_id` in the file; S4 replaces the
                        // per-widget salts properly.
                        ui.push_id("ide_preview", |ui| {
                            ui.label(theme::header_label("Preview"));
                            let Some(spec) = spec.as_ref() else {
                                ui.add_space(8.0);
                                ui.label(
                                    egui::RichText::new(
                                        "This module isn't loaded — nothing to preview.",
                                    )
                                    .color(theme::DIM),
                                );
                                return;
                            };
                            ui.add_space(2.0);
                            ui.label(
                                egui::RichText::new(format!(
                                    "{} v{} by {}",
                                    spec.meta.name, spec.meta.version, spec.meta.author
                                ))
                                .color(theme::FAINT)
                                .small(),
                            );
                            ui.add_space(8.0);
                            ui.separator();
                            let Some(ext_res) = resolution.exts.get(&spec.id()) else {
                                // Explicit notice rather than an empty panel: a blank
                                // column reads as "this module has no settings", which
                                // is a very different (and wrong) message.
                                ui.add_space(8.0);
                                ui.label(
                                    egui::RichText::new(format!(
                                        "\u{f071}{sep}Couldn't resolve this module — \
                                         preview unavailable.",
                                    ))
                                    .color(theme::COL_GLOBAL),
                                );
                                return;
                            };
                            // ── The interactive preview, and its three guards ──
                            //
                            // `full_width = true`: the 743px clamp exists for Config
                            // mode's narrow column and would leave a dead gutter here.
                            // `read_only = false` (S5a): the fields are now live —
                            // but every write they make is redirected into the
                            // scratch `preview_config`, which nothing persists.
                            // `show_legend`: only with a real preview game selected,
                            // because only then do the scope colours describe layers
                            // that are actually participating.
                            //
                            // Guard #3: snapshot the REAL game config — the whole
                            // `GameConfig`, not just its module map — across the
                            // call and assert it is untouched. This repurposes the
                            // old `debug_assert!(!changed)` — which could not
                            // survive an interactive preview — into an assertion of
                            // the property that actually matters. It checks the
                            // destination, not the messenger, so it stays
                            // meaningful no matter how the write path is
                            // refactored.
                            //
                            // *Why the whole struct and not just `.config.modules`*
                            // (2026-07-19): the assertion message claims "nothing
                            // reached the real game config", which is broader than
                            // module writes alone — `game_config.game.name`,
                            // `game_config.game.appid` and `config.general` all
                            // live outside `.config.modules` and would sail through
                            // a modules-only snapshot untouched by this check.
                            // `GameConfig` is already `Serialize` (derived in
                            // `ritz-core`), so this costs nothing but a wider
                            // `to_value` call.
                            let before =
                                serde_json::to_value(&self.game_config).unwrap_or(Value::Null);
                            let show_legend = self.preview_game.is_some();
                            // Guard #1: `with_preview_writes` restores the caller's
                            // prior `WriteTarget` on every path out — see its doc.
                            let changed = self.with_preview_writes(|s| {
                                s.render_module_settings_body(
                                    ui,
                                    spec,
                                    Some(ext_res),
                                    true,
                                    false,
                                    show_legend,
                                )
                            });
                            let after =
                                serde_json::to_value(&self.game_config).unwrap_or(Value::Null);
                            debug_assert_eq!(
                                before, after,
                                "IDE preview edit reached the real game config"
                            );
                            // Guard #2: DROP `changed` on the floor, deliberately.
                            // Outside IDE mode this bool feeds `ui()`'s `changed`,
                            // which calls `persist()`. `persist()` is itself a
                            // no-op under `Mode::Ide` (S4b) — but letting `changed`
                            // escape this closure would still be wrong to rely on:
                            // nothing outside the scratch layer changed, so there
                            // is nothing to report upward regardless of what
                            // `persist()` does with it.
                            let _ = changed;
                        });
                    });
            });
    }

    /// Detail header for the selected module in the central panel: the
    /// name/version/author heading row, the "Editing Game/Profile/Global: X" context
    /// label, the description, and a trailing separator. Split out of the old inline
    /// `ui()` block (S3a) so `render_module_settings_body` below can eventually be
    /// called twice — this header only ever needs to render once per selected
    /// module, which is why it stays a separate method rather than folding into the
    /// body.
    ///
    /// *Why no "Edit" button here:* this header used to carry an "Edit" icon button
    /// opening the manifest editor. IDE Mode's tab now owns that job, so
    /// a second entry point in the read-only Config-mode detail view was redundant
    /// — removed along with the `open_inspector` deferred flag it existed to serve.
    fn render_module_detail_header(&mut self, ui: &mut egui::Ui, spec: &Extension) {
        let edit_ctx_label: Option<String> = match &self.nav_sel {
            NavSel::GlobalSettings => {
                Some("Editing Global Profile — applies to all games".to_string())
            }
            NavSel::Profile(name) => Some(format!("Editing Profile: {name}")),
            NavSel::Game(_) => {
                Some(format!("Editing Game: {}", self.game_config.game.name))
            }
            NavSel::GeneralSettings => None,
        };
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
    }

    /// Settings body for the selected module: the field-editor `ScrollArea`
    /// (section header + per-field `render_field` loop), the scope legend, and the
    /// trailing colour-hint footer. Returns whether any field changed (mirrors the
    /// old inline `changed` bool this replaced).
    ///
    /// `read_only` forces every field to render non-editable. It is threaded
    /// through to `render_field`/`render_value_editor` rather than relying on
    /// `ui.add_enabled_ui`; see `render_value_editor`'s doc comment for why.
    ///
    /// `show_legend` gates the scope legend and the trailing colour-hint footer.
    ///
    /// *Why that is its own parameter and not `!read_only`* (S5a): it used to ride
    /// on `!read_only`, which was fine while the only read-only caller was the
    /// inert IDE preview. Making that preview writable flips `read_only` to
    /// `false`, and the legend would have come back **silently** — with colours
    /// that lie, because with no preview game selected the palette describes
    /// layers that aren't participating. Config mode passes `true`; the preview
    /// passes `preview_game.is_some()`, so the colours appear exactly when they
    /// are truthful.
    fn render_module_settings_body(
        &mut self,
        ui: &mut egui::Ui,
        spec: &Extension,
        ext_res: Option<&resolve::ExtResolution>,
        full_width: bool,
        read_only: bool,
        show_legend: bool,
    ) -> bool {
        let mut changed = false;
        egui::ScrollArea::vertical()
            // Fill the panel (don't shrink to content) so the scrollbar sits at
            // the far right while the content stays capped/left-aligned.
            .auto_shrink([false, false])
            .drag_to_scroll(self.general_config.touch_mode)
            .show(ui, |ui| {
            ui.vertical(|ui| {
                let max_w = body_max_width(ui, full_width);
                ui.set_max_width(max_w);
                // Custom-env/-game-env/-args modules render through this same
                // generic section/field loop — `render_field` dispatches their
                // single `multi_string` field (Variable `env`/`game_env`/`args`)
                // to the right widget by backend string, see `render_field`.
                {
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
                            if show_legend {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    scope_legend(ui);
                                });
                            }
                        });
                        ui.add_space(6.0);
                        let field_chain: Vec<Preset> = self.field_chain();
                        for field in visible {
                            let Some(res) = ext_res.and_then(|e| e.fields.get(&field.variable)) else {
                                continue;
                            };
                            let preset_depth: Option<usize> =
                                if res.provenance == resolve::Provenance::Preset
                                    && self.general_config.show_field_inheritance
                                    && !field_chain.is_empty()
                                {
                                    field_chain.iter().enumerate()
                                        .find(|(_, p)| p.modules
                                            .get(&spec.meta.author)
                                            .and_then(|e| e.get(&spec.meta.name))
                                            .and_then(|vars| vars.get(&field.variable))
                                            .is_some())
                                        .map(|(i, _)| i)
                                } else { None };
                            if self.render_field(ui, spec, field, res, preset_depth, read_only) {
                                changed = true;
                            }
                        }
                    }
                    if any_section && show_legend {
                        ui.add_space(10.0);
                        ui.label(egui::RichText::new(
                            "Colours mark where each value resolves from — Global, Profile, or this Game. \
                             Left-click a checkbox to toggle, right-click to reset to inherited.",
                        ).color(theme::FAINT).small());
                    }
                }
            });
        });
        changed
    }

    fn render_field(
        &mut self,
        ui: &mut egui::Ui,
        spec: &Extension,
        field: &UiField,
        res: &resolve::ResolvedField,
        preset_depth: Option<usize>,
        read_only: bool,
    ) -> bool {
        let state = tri_state(res);
        // A value set at the layer being edited resolves as `Provenance::Game`
        // (the layer is loaded as a fake game), so color it by the actual editing
        // scope — Game blue, Profile green, Global red — not always blue.
        let scope = match res.provenance {
            resolve::Provenance::Game => self.editing_scope_color(),
            resolve::Provenance::Preset
                if self.general_config.show_field_inheritance
                    && matches!(self.general_config.inheritance_display, InheritanceDisplayMode::Color)
                    && preset_depth.is_some() =>
            {
                profile_depth_color(preset_depth.unwrap())
            }
            p => theme::scope_color(p),
        };
        const NUMS: [&str; 6] = ["1", "2", "3", "4", "5", "5+"];
        let badge: Option<(&str, Color32)> =
            if self.general_config.show_field_inheritance
                && matches!(self.general_config.inheritance_display, InheritanceDisplayMode::Numbers)
                && res.provenance == resolve::Provenance::Preset
            {
                preset_depth.map(|d| (NUMS[d.min(5)], COL_PROFILE))
            } else {
                None
            };
        let mut changed = false;

        // A list field renders as its own growing slot card, not the one-row layout.
        // The custom-env / custom-game-env modules' one `multi_string` field (keyed
        // by backend + Variable name, not by manifest identity) gets the two-column
        // Name|Value widget instead of the plain single-column one; custom-args
        // keeps the plain widget (one literal argument per row).
        if field.field_type == FieldType::MultiString {
            let pair_var = match spec.backend.as_deref() {
                Some("custom-env") => Some("env"),
                Some("custom-game-env") => Some("game_env"),
                _ => None,
            };
            if pair_var == Some(field.variable.as_str()) {
                return self.render_env_pair_field(ui, spec, field, scope, read_only);
            }
            return self.render_multi_string_field(ui, spec, field, scope, read_only);
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
                    // `read_only` goes *into* `scope_checkbox` rather than gating
                    // its result here: the row must still paint (WYSIWYG preview)
                    // but must be allocated non-interactively, so no click can ever
                    // reach `apply_tri` — which writes config and seeds
                    // `text_buffers`. This is the one write path shared by *every*
                    // field type, Toggle included.
                    let value = TriValue { state, inherited_on: res.var.truthy };
                    if let Some(new) =
                        scope_checkbox(ui, &label, &hover, value, scope, badge, read_only)
                    {
                        self.apply_tri(spec, field, res, new);
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
                        // `read_only` forces this to false regardless of resolution
                        // state — see `render_value_editor` for why that has to be an
                        // explicit bool rather than relying on `ui.is_enabled()`.
                        let editable = !read_only && state == Tri::Enabled;
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            ui.add_enabled_ui(editable, |ui| {
                                changed |= self.render_value_editor(ui, spec, field, res, scope, editable, read_only);
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
    ///
    /// `read_only` renders the identical card — same label, same rows, same
    /// buttons — but with every widget non-interactive and *no* shared state
    /// touched: the working copy is seeded straight from the stored array instead
    /// of taking (and re-inserting) `self.multi_edit`, and the persist block is
    /// skipped, so neither `set_current` nor `unset_current` (which removes from
    /// `text_buffers`) can run. Returns `false` unconditionally when read-only.
    fn render_multi_string_field(&mut self, ui: &mut egui::Ui, spec: &Extension, field: &UiField, scope: Color32, read_only: bool) -> bool {
        let author = spec.meta.author.clone();
        let name = spec.meta.name.clone();
        let var = field.variable.clone();
        let label = field.name.clone().unwrap_or_else(|| var.clone());
        let hover = field.description.clone().unwrap_or_default();

        // Per scope+field working copy of the list, seeded from the stored array.
        // Tag via `buffer_scope_tag` so the IDE preview gets its own namespace —
        // see that method for the collision this closes.
        let key = format!("{}::{author}::{name}::{var}", self.buffer_scope_tag());
        let stored: Vec<String> = self
            .current_scope_value(&author, &name, &var)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        // Read-only takes the working copy by *reference* (clone, no `remove`) so
        // the live editor's in-progress buffer is still what the preview shows,
        // without the preview owning or consuming it.
        let mut entries = if read_only {
            self.multi_edit.get(&key).cloned().unwrap_or_else(|| stored.clone())
        } else {
            self.multi_edit.remove(&key).unwrap_or(stored.clone())
        };

        let tint = Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16);
        let btn_w = icon_button_width(ui) + ui.spacing().item_spacing.x;
        let cache = &mut self.icon_cache;
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
                                .interactive(!read_only)
                                .hint_text(egui::RichText::new("command").color(theme::FAINT)),
                        );
                        if icon_button(ui, cache, "\u{f467}", "", IconBtn::Danger, !read_only).clicked() {
                            to_delete = Some(i);
                        }
                    });
                }
                if ui.add_enabled(!read_only, egui::Button::new("+ Add")).clicked() {
                    entries.push(String::new());
                }
            });

        if let Some(i) = to_delete {
            entries.remove(i);
        }

        // Persist the non-empty entries when they differ from what's stored.
        let cleaned: Vec<String> = entries.iter().filter(|s| !s.trim().is_empty()).cloned().collect();
        let mut changed = false;
        if !read_only && cleaned != stored {
            if cleaned.is_empty() {
                self.unset_current(spec, field);
            } else {
                self.set_current(spec, field, json!(cleaned));
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

        if !read_only {
            self.multi_edit.insert(key, entries);
        }
        changed
    }

    /// Render a two-column Name | Value list for the custom-env / custom-game-env
    /// backends' single `multi_string` field. Storage is unchanged — still the
    /// field's `multi_string` list — but each entry is displayed split into a Name
    /// and a Value box and reassembled as `"{name}={value}"` (see [`parse_env_row`]).
    /// The Name half is live-validated to the POSIX env charset
    /// ([`is_valid_env_name`]): an invalid name gets a red outline, and a row whose
    /// name is empty/invalid is dropped when persisting, so a bad pair can never
    /// reach a stored env var. The Value half is unrestricted (may contain `=`).
    ///
    /// `read_only` behaves exactly as in [`Self::render_multi_string_field`]: same
    /// card, non-interactive widgets, `multi_edit` read but never taken or
    /// re-inserted, and the persist block skipped so no `set_current` /
    /// `unset_current` can fire. Returns `false` unconditionally when read-only.
    fn render_env_pair_field(&mut self, ui: &mut egui::Ui, spec: &Extension, field: &UiField, scope: Color32, read_only: bool) -> bool {
        let author = spec.meta.author.clone();
        let name = spec.meta.name.clone();
        let var = field.variable.clone();
        let label = field.name.clone().unwrap_or_else(|| var.clone());
        let hover = field.description.clone().unwrap_or_default();

        // See `render_multi_string_field` / `buffer_scope_tag`.
        let key = format!("{}::{author}::{name}::{var}", self.buffer_scope_tag());
        let stored: Vec<String> = self
            .current_scope_value(&author, &name, &var)
            .and_then(|v| v.as_array())
            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
            .unwrap_or_default();
        // See `render_multi_string_field`: read-only clones the working copy
        // instead of taking it, so the preview never consumes the live buffer.
        let mut entries = if read_only {
            self.multi_edit.get(&key).cloned().unwrap_or_else(|| stored.clone())
        } else {
            self.multi_edit.remove(&key).unwrap_or(stored.clone())
        };

        let tint = Color32::from_rgba_unmultiplied(scope.r(), scope.g(), scope.b(), 16);
        let btn_w = icon_button_width(ui) + ui.spacing().item_spacing.x;
        let cache = &mut self.icon_cache;
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
                // Name gets ~1/3 of the row, Value the rest.
                let total_w = (ui.available_width() - btn_w - 8.0).max(80.0);
                let name_w = (total_w * 0.32).max(60.0);
                let value_w = (total_w - name_w).max(40.0);
                for (i, entry) in entries.iter_mut().enumerate() {
                    let (mut n, mut v) = parse_env_row(entry);
                    let name_ok = n.is_empty() || is_valid_env_name(&n);
                    ui.horizontal(|ui| {
                        let name_resp = ui.add(
                            egui::TextEdit::singleline(&mut n)
                                .desired_width(name_w)
                                .interactive(!read_only)
                                .hint_text(egui::RichText::new("NAME").color(theme::FAINT)),
                        );
                        if !name_ok {
                            ui.painter().rect_stroke(
                                name_resp.rect,
                                ui.visuals().widgets.inactive.rounding,
                                egui::Stroke::new(1.5, theme::COL_GLOBAL),
                            );
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut v)
                                .desired_width(value_w)
                                .interactive(!read_only)
                                .hint_text(egui::RichText::new("value").color(theme::FAINT)),
                        );
                        if icon_button(ui, cache, "\u{f467}", "", IconBtn::Danger, !read_only).clicked() {
                            to_delete = Some(i);
                        }
                    });
                    *entry = format!("{n}={v}");
                }
                if ui.add_enabled(!read_only, egui::Button::new("+ Add")).clicked() {
                    entries.push(String::new());
                }
            });

        if let Some(i) = to_delete {
            entries.remove(i);
        }

        // Persist only rows with a valid, non-empty name — an invalid/empty name
        // never reaches the stored list, so it can't produce a broken env var.
        let cleaned: Vec<String> = entries
            .iter()
            .filter_map(|e| {
                let (n, v) = parse_env_row(e);
                let n = n.trim();
                (!n.is_empty() && is_valid_env_name(n)).then(|| format!("{n}={v}"))
            })
            .collect();
        let mut changed = false;
        if !read_only && cleaned != stored {
            if cleaned.is_empty() {
                self.unset_current(spec, field);
            } else {
                self.set_current(spec, field, json!(cleaned));
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

        if !read_only {
            self.multi_edit.insert(key, entries);
        }
        changed
    }

    /// The raw stored value for a variable at the scope currently being edited.
    ///
    /// The read twin of [`Self::set_scoped`]'s routing, and it has to branch on
    /// `write_target` first for the same reason: while the preview column renders,
    /// "what is stored here" means "what is in the scratch layer". Reading
    /// `game_config` instead would make a multi_string card compare its edited
    /// list against the *real* game's stored list and persist the difference.
    fn current_scope_value(&self, author: &str, name: &str, var: &str) -> Option<&Value> {
        if self.write_target == WriteTarget::Preview {
            return self.preview_config.get_value(author, name, var);
        }
        match &self.nav_sel {
            NavSel::GlobalSettings => self.global_config.get_value(author, name, var),
            NavSel::Profile(_) => self.editing_preset_buf.as_ref()?.get_value(author, name, var),
            _ => self.game_config.get_value(author, name, var),
        }
    }

    /// Apply a new tri-state to `spec`'s field. Pure dispatcher — it holds no
    /// identity of its own, it only forwards the caller's `spec` to the writers.
    fn apply_tri(&mut self, spec: &Extension, field: &UiField, res: &resolve::ResolvedField, new: Tri) {
        match new {
            Tri::Unset => self.unset_current(spec, field),
            Tri::Disabled => {
                let val = if field.field_type == FieldType::Toggle {
                    json!(false)
                } else {
                    Value::Null
                };
                self.set_current(spec, field, val);
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
                self.set_current(spec, field, val);
            }
        }
    }

    /// Value editor for a non-toggle field. When `editable` is false the widgets
    /// are drawn read-only (the caller has disabled the ui) and just display the
    /// effective value, so no persistent edit buffer is touched.
    ///
    /// `read_only` is threaded down separately from `editable` (rather than
    /// trusting the caller's `editable` alone) as a deliberate belt-and-suspenders:
    /// the `String` arm below branches on the `editable` *parameter*, not
    /// `ui.is_enabled()` — wrapping the call in `ui.add_enabled_ui(false, …)` at the
    /// call site greys out the widget but does NOT stop this arm from writing into
    /// `self.text_buffers` or minting the `("text_edit", key)` `egui::Id`. Re-deriving
    /// `editable` from `read_only` here means a future caller can never reintroduce a
    /// read-only leak by passing a stale `editable: true` alongside `read_only: true`.
    // Allowed, not refactored: `editable` and `read_only` are the obvious pair to
    // merge, but the doc comment above explains they are deliberately kept
    // separate so this fn can re-derive `editable` and defend against a stale
    // `true`. Collapsing them to satisfy the lint would delete that safeguard.
    #[allow(clippy::too_many_arguments)]
    fn render_value_editor(
        &mut self,
        ui: &mut egui::Ui,
        spec: &Extension,
        field: &UiField,
        res: &resolve::ResolvedField,
        scope: Color32,
        editable: bool,
        read_only: bool,
    ) -> bool {
        let editable = editable && !read_only;
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
                // The ComboBox is deliberately still *drawn* when read-only (it is a
                // WYSIWYG preview); `editable` is what stops the pick from writing.
                // A disabled `Ui` already makes `picked` unreachable, so this guard
                // is a no-op for `read_only == false` — it exists so the parameter,
                // not `ui.is_enabled()`, is what guarantees inertness.
                if let Some(v) = picked.filter(|_| editable) {
                    self.set_current(spec, field, json!(v));
                    changed = true;
                }
            }
            FieldType::Integer | FieldType::Float => {
                let integer = field.field_type == FieldType::Integer;
                let (min, max, step) = number_range(field);
                let v: f64 = res.var.value.parse().unwrap_or(min);
                // Same reasoning as the Selection arm: the slider/spinner is drawn
                // either way, `editable` is the gate on the write.
                if let Some(nv) = scope_slider(ui, v, min, max, step, integer, scope)
                    .filter(|_| editable)
                {
                    self.set_current(spec, field, num_value(nv, integer));
                    changed = true;
                }
            }
            FieldType::String => {
                // Right-to-left: the Detect button (if any) pins to the right, then
                // the text field fills the remaining width to its left.
                if editable && field.variable == "window_class" {
                    // Scoped to *this* module's field, not "any detection is in
                    // flight and this variable happens to be named window_class" —
                    // see `detect_countdown_secs`.
                    let remaining = self.detect_countdown_secs(&spec.id(), &field.variable);
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
                    // Scope-tagged: the tag is what keeps the IDE preview's buffer
                    // AND its absolute `("text_edit", key)` `egui::Id` distinct from
                    // Config mode's for the same module/variable — `push_id` does
                    // not namespace an explicitly-minted Id. See `buffer_scope_tag`.
                    let key = format!("{}::{}::{}", self.buffer_scope_tag(), spec.id(), field.variable);
                    let id = egui::Id::new(("text_edit", &key));
                    // Sync the buffer from the resolved value whenever the field isn't
                    // actively focused. This prevents stale values after a profile switch
                    // (the nav action and resolve_for_editing run in the wrong order within
                    // one frame, so without this the buffer gets populated from a stale
                    // resolution and persists until the user clicks the profile again).
                    if !ui.ctx().memory(|m| m.has_focus(id)) {
                        self.text_buffers.insert(key.clone(), res.var.value.clone());
                    }
                    let edited = {
                        let buf = self.text_buffers.entry(key).or_insert_with(|| res.var.value.clone());
                        ui.add(egui::TextEdit::singleline(buf).desired_width(tw).id(id))
                            .changed()
                            .then(|| buf.clone())
                    };
                    if let Some(v) = edited {
                        self.set_current(spec, field, json!(v));
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

    /// Write `value` for `author::name::var` into whichever scope is currently being
    /// edited. The single place that maps `nav_sel` onto a config store for writes —
    /// every writer must route through here, or it silently lands in the wrong scope
    /// (see `poll_detect`, which used to write `game_config` unconditionally).
    fn set_scoped(&mut self, a: &str, n: &str, var: &str, value: Value) {
        // Checked BEFORE the `nav_sel` match, not as an arm inside it — see
        // [`WriteTarget`] for why `nav_sel` structurally cannot carry this
        // distinction. This is guard #1 of the three that keep a preview toggle
        // off the user's disk: the write never reaches `game_config` at all, so
        // `persist()` (which is itself a no-op under `Mode::Ide`, S4b) has
        // nothing new to write even if it fires.
        if self.write_target == WriteTarget::Preview {
            self.preview_config.set_value(a, n, var, value);
            return;
        }
        match &self.nav_sel.clone() {
            NavSel::GlobalSettings => {
                self.global_config.set_value(a, n, var, value);
            }
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf {
                    p.set_value(a, n, var, value);
                }
            }
            NavSel::Game(_) => {
                self.game_config.set_value(a, n, var, value);
            }
            // Unreachable, on two legs — both are needed, the render one alone is not
            // enough:
            //   1. Render path: no module field is ever rendered while General
            //      Settings is selected (the central panel returns right after
            //      `render_general_settings_panel`, and the module-list panel is
            //      skipped for this selection), so no `set_current` caller runs.
            //   2. Async path: `poll_detect` runs every frame for *every* `nav_sel`,
            //      outside both of those gates, and is the one other caller. It is
            //      barred by `cancel_stale_detect`, which drops a detection whose
            //      `nav_sel` (or `mode`, S4b) has changed — and a detection can only
            //      ever be started from `render_field`, i.e. never under General
            //      Settings to begin with.
            // *Why a no-op and not the game arm it used to share:* routing a module
            // write here into `games/<appid>.json` would be a wrong-scope write with
            // no visible symptom, and there is no honest scope to pick — General
            // Settings' own controls write `general_config` (saved inside
            // `render_general_settings_panel`), never module values. `persist()` maps
            // this selection to `save_game`, which looks like evidence to the contrary
            // but is only a harmless re-save of an unchanged game config.
            NavSel::GeneralSettings => {
                debug_assert!(false, "module write with General Settings selected: {a}::{n}::{var}");
            }
        }
    }

    /// Remove a stored value for `author::name::var` from whichever scope is
    /// currently being edited — the unset twin of [`Self::set_scoped`], and the
    /// only place `nav_sel` maps onto a config store for removals.
    fn unset_scoped(&mut self, a: &str, n: &str, var: &str) {
        // Same pre-match preview branch as [`Self::set_scoped`] — see there.
        if self.write_target == WriteTarget::Preview {
            self.preview_config.unset_value(a, n, var);
            return;
        }
        match &self.nav_sel.clone() {
            NavSel::GlobalSettings => {
                self.global_config.unset_value(a, n, var);
            }
            NavSel::Profile(_) => {
                if let Some(p) = &mut self.editing_preset_buf {
                    p.unset_value(a, n, var);
                }
            }
            NavSel::Game(_) => {
                self.game_config.unset_value(a, n, var);
            }
            // Unreachable for the same reason as in `set_scoped` — see the
            // rationale there.
            NavSel::GeneralSettings => {
                debug_assert!(false, "module unset with General Settings selected: {a}::{n}::{var}");
            }
        }
    }

    /// Set a value for `spec`'s field on the *currently active edit target*.
    ///
    /// The module identity comes from the caller's `spec`, never from
    /// `cur_specs[selected_ext]`: a frame that renders more than one module's
    /// fields (e.g. a module next to a preview of another) would otherwise write
    /// every edit into whichever module the global selection index happens to
    /// point at.
    fn set_current(&mut self, spec: &Extension, field: &UiField, value: Value) {
        let (a, n) = (spec.meta.author.clone(), spec.meta.name.clone());
        self.set_scoped(&a, &n, &field.variable, value);
    }

    /// Begin a 3-second window-class detection for the given field.
    fn start_detect(&mut self, spec: &Extension, field: &UiField) {
        let result = Arc::new(Mutex::new(DetectStatus::Waiting));
        let handle = result.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(3));
            // Compute FIRST, lock SECOND (issue #3). Holding the guard across
            // `detect_active_window_class()` — which spawns `hyprctl` and parses
            // its JSON — means a panic in there unwinds *with the guard held* and
            // poisons the mutex. The GUI thread reads the same mutex every frame,
            // so a poisoned lock is one `.unwrap()` away from taking the settings
            // window down with it. Two statements make that structurally
            // impossible: nothing fallible runs inside the guard's lifetime.
            let detected = detect_active_window_class();
            if let Ok(mut slot) = handle.lock() {
                *slot = DetectStatus::Done(detected);
            }
        });
        self.detect = Some(Detect {
            result,
            start: Instant::now(),
            ext_id: spec.id(),
            author: spec.meta.author.clone(),
            name: spec.meta.name.clone(),
            var: field.variable.clone(),
            nav: self.nav_sel.clone(),
            target: self.write_target,
            mode: self.mode,
        });
    }

    /// Drop a pending detection whose scope the user has since navigated away from.
    ///
    /// `poll_detect` is an async completion writer decoupled from rendering: it runs
    /// every frame for every `nav_sel`, so without this a detection started under one
    /// scope could land its `set_scoped` write under another. *Why cancel rather than
    /// write to the scope it started in:* the snapshotted target may no longer exist —
    /// `editing_preset_buf` gets swapped on a profile switch and `game_config` gets
    /// replaced on a game switch — so "apply later" would resurrect exactly the
    /// wrong-scope write this guard exists to prevent. Cancelling also matches intent:
    /// the user navigated away from the field they were configuring.
    ///
    /// Comparing against a snapshot here (rather than clearing `detect` at each nav
    /// site) is deliberate: it cannot be defeated by a future `nav_sel` assignment
    /// that forgets to clear, of which there are a dozen-odd across this file.
    ///
    /// *Why an extra `mode` check alongside `nav`* (2026-07-19, QC on `fd726f7`;
    /// widened to a full `d.mode != self.mode` comparison in S4b — see below):
    /// a detection started in the IDE preview (`write_target == Preview` at
    /// start) and one started in Config mode (`write_target == Scope`
    /// throughout) are told apart by the orthogonal [`Mode`] axis, not by
    /// `nav_sel` — pre-S4b `nav_sel` was forced to `NavSel::ModuleEditor(_)` for
    /// the whole time IDE mode showed, which happened to make *entering or
    /// leaving* IDE mode always change `nav_sel` too, so the `nav` comparison
    /// alone caught most transitions by accident. Tried comparing `d.target`
    /// against the *live* `self.write_target` instead of adding a second field —
    /// rejected: `write_target` is `Scope` for almost the entire frame and only
    /// flips to `Preview` for the single `with_preview_writes` render call, so by
    /// the time this method runs again (next frame, or even later in the same
    /// frame via `poll_detect`) the live value is back to `Scope` regardless of
    /// whether the user is still on the preview — that comparison would cancel a
    /// freshly-started preview detection almost immediately, not just a stale
    /// one. `mode`, captured once at `start_detect` and compared against the
    /// live `self.mode`, doesn't have that problem: `self.mode` holds `Ide` for
    /// the user's whole time in IDE mode, not just for one render call.
    ///
    /// *Why S4b widened this from `d.mode == Ide && self.mode != Ide` to a full
    /// `d.mode != self.mode`:* the one-directional form relied on `nav_sel`
    /// covering the *other* direction (Config → IDE) — since entering IDE mode
    /// used to force `nav_sel` to `NavSel::ModuleEditor(_)`, `d.nav != self.nav_sel`
    /// caught that transition on its own. S4b made entering/leaving IDE mode
    /// (`select_nav_category(NavCategory::Ide)`, `GuiApp::focus_module`) stop
    /// touching `nav_sel` at all — focus lives on `GuiApp::focused_module`
    /// instead — so a detection started in Config mode while `nav_sel` was, say,
    /// `Game("42")` and left running while the user switches into IDE mode (which
    /// doesn't touch `nav_sel`) would otherwise see `d.nav == self.nav_sel` *and*
    /// the old one-directional `mode` term false, and go uncancelled — 3 seconds
    /// later it would write into `game_config` (and, until `persist()`'s own
    /// `Mode::Ide` guard suppresses the save, feed `changed`) while the user is
    /// looking at an unrelated module's manifest editor. The full symmetric
    /// comparison closes both directions the same way the `nav` comparison
    /// always has for same-mode navigation.
    fn cancel_stale_detect(&mut self) {
        let stale = self
            .detect
            .as_ref()
            .is_some_and(|d| d.nav != self.nav_sel || d.mode != self.mode);
        if stale {
            self.detect = None;
        }
    }

    /// Apply a finished detection (if any). Returns true if the config changed.
    fn poll_detect(&mut self, ctx: &egui::Context) -> bool {
        // Must precede the `Waiting` early return below, or an in-flight detection
        // would never be re-examined for a scope change until it completed.
        self.cancel_stale_detect();
        let done = match &self.detect {
            Some(d) => match d.status() {
                DetectStatus::Waiting => {
                    ctx.request_repaint_after(Duration::from_millis(200));
                    return false;
                }
                DetectStatus::Done(class) => Some((
                    d.ext_id.clone(),
                    d.author.clone(),
                    d.name.clone(),
                    d.var.clone(),
                    d.target,
                    class,
                )),
            },
            None => None,
        };
        let Some((ext_id, author, name, var, target, class)) = done else {
            return false;
        };
        self.detect = None;
        // Redraw so the text box reflects the new value immediately.
        ctx.request_repaint();
        if let Some(c) = class {
            // Scope-aware: detecting while editing a Profile must write the profile,
            // not the ambient game. Writing `game_config` here meant the value never
            // reached the edited scope (persist() saves by scope) while the text
            // buffer below still showed it — a silent wrong-scope write.
            //
            // The same argument extends to the write *target* (S5a): restore the
            // one the detection was started under for the duration of the write, so
            // a Detect begun in the interactive preview lands in the scratch layer
            // and under the `"preview"` buffer tag — not in the real game config,
            // which is where it would otherwise go, since `poll_detect` runs at the
            // frame tail with `write_target` already back to `Scope`. Restored
            // immediately, so nothing downstream sees a flipped flag.
            let prev = self.write_target;
            self.write_target = target;
            self.set_scoped(&author, &name, &var, json!(c.clone()));
            // Same tagged key shape as the `String` arm of `render_value_editor`,
            // or the detected value would be written under a key nothing reads.
            self.text_buffers
                .insert(format!("{}::{ext_id}::{var}", self.buffer_scope_tag()), c);
            self.write_target = prev;
            // Report "changed" only for a real scope. This bool feeds `ui()`'s
            // `changed`, which calls `persist()`. A preview detection changed
            // nothing persistable, so saying so would be wrong regardless of
            // `persist()`'s own `Mode::Ide` no-op (S4b) — `ctx.request_repaint()`
            // above already gets the new value on screen. Same reasoning as
            // guard #2 at the preview render site.
            return target == WriteTarget::Scope;
        }
        false
    }

    /// Remove an override for `spec`'s field on the *currently active edit target*
    /// (reset to inherit). Identity — including the `text_buffers` key — comes from
    /// the caller's `spec`, for the reason given on [`Self::set_current`].
    fn unset_current(&mut self, spec: &Extension, field: &UiField) {
        let (a, n) = (spec.meta.author.clone(), spec.meta.name.clone());
        // Tagged to match what `render_value_editor` inserted — an untagged remove
        // would miss, leaving a stale buffer that re-shows the value it just unset.
        self.text_buffers
            .remove(&format!("{}::{}::{}", self.buffer_scope_tag(), spec.id(), field.variable));
        self.unset_scoped(&a, &n, &field.variable);
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
        // Cap the row width like the module panel.
        let max_w = body_max_width(ui, self.general_config.full_width);
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

        {
            let prev = self.general_config.inheritance_display;
            settings_value_row(ui, "Inheritance Display Mode", "How profile chain depth is shown in the nav and module trees.", |ui| {
                egui::ComboBox::from_id_salt("inheritance_display_mode")
                    .selected_text(match self.general_config.inheritance_display {
                        InheritanceDisplayMode::Color => "Color",
                        InheritanceDisplayMode::Numbers => "Numbers",
                        InheritanceDisplayMode::ArrowsOnly => "Arrows Only",
                    })
                    .width(ui.available_width())
                    .show_ui(ui, |ui| {
                        for (label, val) in [
                            ("Color", InheritanceDisplayMode::Color),
                            ("Numbers", InheritanceDisplayMode::Numbers),
                            ("Arrows Only", InheritanceDisplayMode::ArrowsOnly),
                        ] {
                            if ui.selectable_label(self.general_config.inheritance_display == val, label).clicked() {
                                self.general_config.inheritance_display = val;
                            }
                        }
                    });
            });
            if self.general_config.inheritance_display != prev { changed = true; }
        }

        let mut show_fi = self.general_config.show_field_inheritance;
        if settings_toggle_row(
            ui,
            "Show Field Inheritance",
            "Tint or label inherited fields in the module settings panel by chain depth.",
            &mut show_fi,
        ) {
            self.general_config.show_field_inheritance = show_fi;
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
        // These two used to hardcode a double space, which is the mono gap in
        // proportional clothing — too tight in the sans font and too loose in
        // mono. `icon_sep` is the one place that decision lives.
        let sep = icon_sep(self.general_config.mono_ui);
        ui.hyperlink_to(
            egui::RichText::new(format!("\u{f09b}{sep}ritz-game-launcher on GitHub"))
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
                    egui::RichText::new(format!("\u{f09b}{sep}{name} on GitHub"))
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

    /// Re-point **every** stored reference to profile `old_name`: to `Some(new)`
    /// on a rename, or to "no profile" on a delete.
    ///
    /// *Why one function serving both callers* (2026-07-19, issue #36): the sweep
    /// used to live inline in [`Self::rename_preset`], and
    /// [`Self::delete_profile`] had only a one-line version of it that fixed the
    /// **ambient** game (`self.game_config`) and nothing else — while its doc
    /// comment claimed "any game referencing it falls back to no profile". So
    /// deleting profile `P` while viewing game Y left `games/X.json` still holding
    /// `"Preset": "P"`. That degrades gracefully at first (`load_preset` returns
    /// `Ok(None)`), but it is a **resurrection by name** bug: create an unrelated
    /// new profile also called `P` and game X silently re-attaches to it,
    /// inheriting settings nobody assigned. `config_cleanup` cannot catch this —
    /// it sweeps undeclared module *variables*, not profile references. Rename and
    /// delete are the same operation over the same reference graph, so they get
    /// one implementation that cannot drift apart.
    ///
    /// Three kinds of reference exist, and all three resurrect:
    /// 1. **`games/*.json` → `Preset`** — the assignment the issue reported.
    /// 2. **`profiles/*.json` → `Parent`** — a profile's parent link
    ///    (`crates/ritz-core/src/config.rs:Preset::parent`), which forms the extra
    ///    inheritance layer under it. A dangling `Parent` is *worse* than a
    ///    dangling `Preset`, because the resurrected name silently injects a whole
    ///    inherited layer under an existing profile. The old inline sweep did not
    ///    cover this for renames either — that was a live bug on both paths.
    /// 3. **`general.json` → `DefaultPreset`** — the fallback profile for games
    ///    with none assigned. Already handled for rename; now for delete too.
    ///
    /// *Why `Paths::list_games` and not `self.games`:* `self.games` is a cached
    /// display list refreshed only on create/delete/rename and IDE-mode entry, so
    /// a game written since the last refresh would have been skipped — silently
    /// leaving exactly the dangling reference this exists to prevent. The
    /// directory is the authority.
    ///
    /// In-memory mirrors of the same facts (`self.game_config`, `self.preset`,
    /// `self.default_preset`) are updated alongside the files, so the open view
    /// does not keep showing a profile that no longer exists.
    fn retarget_preset_references(&mut self, old_name: &str, new_name: Option<&str>) {
        let new = new_name.map(str::to_string);

        // 1. Every game config on disk that names it.
        for appid in self.paths.list_games() {
            if let Ok(Some(mut gc)) = self.paths.load_game(&appid) {
                if gc.config.modules.preset.as_deref() == Some(old_name) {
                    gc.config.modules.preset = new.clone();
                    let _ = self.paths.save_game(&gc);
                }
            }
        }
        // The ambient game is edited in memory and autosaved from there, so fixing
        // only its file would be undone by the next `persist()`.
        if self.game_config.config.modules.preset.as_deref() == Some(old_name) {
            self.game_config.config.modules.preset = new.clone();
            let _ = self.paths.save_game(&self.game_config);
            if new.is_none() {
                self.preset = None;
            }
        }

        // 2. Every profile that names it as `Parent`.
        for name in self.paths.list_presets() {
            if let Ok(Some(mut p)) = self.paths.load_preset(&name) {
                if p.parent.as_deref() == Some(old_name) {
                    p.parent = new.clone();
                    let _ = self.paths.save_preset(&p);
                    // Keep the open profile editor's buffer in step, or the next
                    // autosave would write the stale parent straight back.
                    if let Some(buf) = &mut self.editing_preset_buf {
                        if buf.name == name {
                            buf.parent = new.clone();
                        }
                    }
                }
            }
        }

        // 3. The general-config default.
        if self.general_config.default_preset.as_deref() == Some(old_name) {
            self.general_config.default_preset = new.clone();
            let _ = self.paths.save_general(&self.general_config);
            self.default_preset = new;
        }
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
        // Re-point every stored reference (games, profile `Parent`s, the general
        // default) — shared with `delete_profile`, see `retarget_preset_references`.
        self.retarget_preset_references(old_name, Some(new_name));
        self.refresh_presets();
        self.editing_preset_buf = self.paths.load_preset(new_name).ok().flatten();
        self.nav_sel = NavSel::Profile(new_name.to_string());
        self.nav_name_buf = new_name.to_string();
    }
}

// ---- Navigator panel -------------------------------------------------------

impl GuiApp {
    fn render_nav_panel(&mut self, ui: &mut egui::Ui) {
        // Deferred intents collected inside the panel closures, applied after them.
        // *Why deferred:* every closure below already holds `&mut self`, so the
        // handlers (which also need `&mut self`) can't run inline. Same pattern as
        // the `open_create` bool in `ui()`'s `ext_list` block.
        let mut nav_action: Option<NavCategory> = None;
        let mut ide_open: Option<String> = None;
        let mut open_create = false;
        let mode = self.mode;

        // Always show the bottom band (it's empty for General Settings/Global Profile).
        egui::TopBottomPanel::bottom("nav_settings")
            .exact_height(198.0)
            .show_separator_line(true)
            .frame(egui::Frame::none()
                .fill(theme::PANEL2)
                .inner_margin(egui::Margin::same(14.0)))
            .show_inside(ui, |ui| {
                match mode {
                    Mode::Config => self.render_nav_settings(ui),
                    Mode::Ide => open_create = self.render_ide_nav_footer(ui),
                }
            });
        egui::TopBottomPanel::top("nav_header")
            .show_separator_line(false)
            .frame(egui::Frame::none()
                .fill(theme::PANEL)
                .inner_margin(egui::Margin { left: 16.0, right: 16.0, top: 14.0, bottom: 8.0 }))
            .show_inside(ui, |ui| {
                nav_action = self.render_nav_category_box(ui);
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
                        match mode {
                            // General Settings owns the whole nav body: the list
                            // below the GENERAL box is EMPTY. *Why:* the tree
                            // lists config *scopes* (Global Profile / Profiles /
                            // Games) and General Settings is not one — it edits
                            // app-wide preferences. Drawing the scope tree under
                            // it would offer a selection that the panel on the
                            // right isn't showing.
                            Mode::Config
                                if matches!(self.nav_sel, NavSel::GeneralSettings) => {}
                            Mode::Config => {
                                // The left nav retargets the editor's live preview, so it
                                // stays usable with a *clean* draft. Once the draft is
                                // dirty it locks: navigating away drops the draft, and a
                                // stray click must not silently discard unsaved edits.
                                let nav_live = self.unsaved_drafts.is_empty();
                                ui.add_enabled_ui(nav_live, |ui| {
                                    self.render_nav_tree(ui);
                                });
                            }
                            Mode::Ide => ide_open = self.render_ide_module_tree(ui),
                        }
                    });
            });

        if let Some(cat) = nav_action {
            // The category tabs are the one route out of an open module editor
            // that nothing else gates: the Config-mode nav tree and MODULES tree
            // both lock while a draft is open, and Close already prompts — but the
            // tabs sit *above* both trees and used to switch instantly, dropping a
            // dirty draft on the floor (the frame-start guard in `ui()` discards it
            // without a word). Prompt instead, carrying the destination so Confirm
            // completes the click and Cancel leaves the editor exactly as it was.
            //
            // Gated on the *focused* draft's unsaved work, not on "a draft
            // exists" and (since issue #35) not on "anything anywhere is
            // unsaved": a clean draft is byte-identical to disk, so dropping it
            // loses nothing, and a background draft this click would never touch
            // is not a reason to ask. See `nav_category_drops_draft`.
            if nav_category_drops_draft(cat, self.focused_draft_unsaved()) {
                self.confirm =
                    Some(ConfirmAction::DiscardEdits { then: DiscardThen::Nav(cat) });
            } else {
                self.select_nav_category(cat);
            }
        }
        if let Some(id) = ide_open {
            self.focus_module(id);
        }
        if open_create {
            self.open_create_dialog();
        }
    }

    /// The bordered category **tab bar** that sits above the nav tree: the three
    /// top-level destinations as three buttons side by side. Replaces the old
    /// single `header_label("Profiles / Games")` — the destinations are now named
    /// on screen, so what the tree below is showing is never a guess.
    ///
    /// *Why a horizontal tab strip and not the earlier stacked rows:* three
    /// full-width rows plus an uppercase `GENERAL` caption spent four lines of
    /// vertical space on navigation that the module tree needs, and read as a
    /// list of *items* rather than as a mode switch. One row of tabs is the
    /// conventional shape for "pick one of N views" and costs one line. The
    /// caption is gone with it (the tabs label themselves); the border stays,
    /// which is what still groups the three as one control.
    ///
    /// Border idiom copied from [`editor_card`] rather than reusing it:
    /// `editor_card` is specialised for the manifest editor (it takes an
    /// `IconCenterCache` and returns a `RowAction`), neither of which applies here.
    fn render_nav_category_box(&mut self, ui: &mut egui::Ui) -> Option<NavCategory> {
        let mut action = None;
        // Selection is a function of BOTH axes: `mode` picks IDE vs. the two config
        // destinations, `nav_sel` disambiguates those two.
        // Both flags are derived from live state every frame, never cached — so
        // ANY code path that moves `mode`/`nav_sel` (Close, the IDE tree, the exit
        // interlock in `ui()`) is followed by the box automatically. There is no
        // second copy of "which category is selected" that could drift.
        let is_ide = self.mode == Mode::Ide;
        let is_general = !is_ide && matches!(self.nav_sel, NavSel::GeneralSettings);
        let is_games = !is_ide && !is_general;
        let mono = self.general_config.mono_ui;
        egui::Frame::none()
            .fill(theme::PANEL2)
            .stroke(egui::Stroke::new(1.0, theme::BORDER))
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::symmetric(8.0, 8.0))
            .show(ui, |ui| {
                let avail = ui.available_width();
                ui.set_width(avail);
                // Equal thirds of whatever the frame leaves, minus the two gutters
                // between them — computed, not a constant, so the tabs stay equal
                // if the nav width or the frame margins ever move.
                let tab_w = ((avail - 2.0 * TAB_GAP) / 3.0).max(0.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = TAB_GAP;
                    // Order: Profiles · IDE Mode · Settings. Profiles first because
                    // it's the app's default/startup destination (see
                    // `GuiApp::new`'s `nav_sel` init) — landing on the leftmost tab
                    // matches "first tab is the default" convention even though the
                    // default is actually chosen by name, not position.
                    // Per-tab color, applied only while that tab is selected (see
                    // `nav_category_tab`'s doc comment): Profiles gets `COL_PROFILE`,
                    // the same green the settings tree already uses for the Profile
                    // scope, so the tab visually ties back to that meaning rather
                    // than borrowing the unrelated brand accent. It reads acceptably
                    // here — it's noticeably darker than `ACCENT` against `PANEL2`,
                    // which if anything helps the three tabs stay distinguishable at
                    // a glance.
                    if nav_category_tab(ui, tab_w, is_games, "\u{f4ff}", "Profiles", mono, COL_PROFILE).clicked() {
                        action = Some(NavCategory::GamesProfiles);
                    }
                    // `\u{f044}` is `theme::ICON_EDIT`, the same pencil the module
                    // detail header uses for its Edit affordance. Reused on purpose:
                    // both mean "author this thing". It replaced `\u{eef4}`, whose
                    // ink is too fine to read at 11px. IDE Mode keeps the brand
                    // accent — it's the app's primary authoring surface, and this is
                    // the same blue the rest of the chrome already treats as "live".
                    if nav_category_tab(ui, tab_w, is_ide, theme::ICON_EDIT, "IDE Mode", mono, theme::ACCENT).clicked() {
                        action = Some(NavCategory::Ide);
                    }
                    // Cog reused verbatim from the old General Settings tree row.
                    // `theme::DIM` (not `COL_DEFAULT`): `COL_DEFAULT` already carries
                    // a specific, unrelated meaning in the scope palette ("no
                    // override anywhere" — see `docs/ui/STYLING-GUIDE.md`), and
                    // reusing it here for tab identity would be a confusing double
                    // duty. `DIM` reads as deliberately neutral rather than
                    // disabled, and — unlike `FAINT`, which the *unselected* icon
                    // already renders in — it's visibly brighter, so selecting this
                    // tab still reads as a state change instead of "no color at
                    // all".
                    if nav_category_tab(ui, tab_w, is_general, "\u{f013}", "Settings", mono, theme::DIM).clicked() {
                        action = Some(NavCategory::GeneralSettings);
                    }
                });
            });
        ui.add_space(2.0);
        action
    }

    /// Apply a click on the GENERAL category box.
    ///
    /// *`nav_sel` is untouched by IDE Mode entry/exit as of S4b* — see [`Mode`]'s
    /// doc comment. Focus is [`GuiApp::focused_module`]'s job; `nav_sel` here is
    /// purely about which of the two *Config*-mode destinations (General
    /// Settings vs. the ambient game/profile/global tree) a click should land on,
    /// and it is only ever assigned in the two Config-mode arms below.
    fn select_nav_category(&mut self, cat: NavCategory) {
        match cat {
            NavCategory::GeneralSettings => {
                self.mode = Mode::Config;
                // Clear focus so a later re-entry into IDE mode re-opens through
                // the normal invariant rather than finding a stale focus. The
                // draft itself is deliberately left in `drafts` — pre-S4b,
                // overwriting `nav_sel` here had exactly this "unfocus but don't
                // drop" effect as a side effect of focus being read *from*
                // `nav_sel`; this reproduces it explicitly now that the two are
                // separate.
                self.focused_module = None;
                self.nav_sel = NavSel::GeneralSettings;
            }
            NavCategory::GamesProfiles => {
                self.mode = Mode::Config;
                // Coming back from IDE mode with a module focused: drop that ONE
                // draft (clean — dropping it loses nothing; dirty means the user
                // already confirmed the discard, since `render_nav_panel` raises
                // `ConfirmAction::DiscardEdits` before this method is ever called
                // in that case) and clear focus. Every other open draft is left
                // exactly alone. `nav_sel` needs no restoring — S4b never moved it
                // away from a real config scope in the first place.
                if self.focused_module.is_some() {
                    self.close_focused_draft();
                }
                // Coming back from General Settings, `nav_sel` is still
                // `GeneralSettings` — which is the box's *other* category. Left
                // alone, the box would snap straight back to "General Settings"
                // (`is_general` is derived from `nav_sel`) and the tree would
                // stay empty, making the click look dead. Land on the ambient
                // game instead.
                if matches!(self.nav_sel, NavSel::GeneralSettings) {
                    self.nav_sel = NavSel::Game(self.appid.clone());
                    self.nav_name_buf = self.game_config.game.name.clone();
                }
            }
            NavCategory::Ide => {
                self.mode = Mode::Ide;
                // The preview game selector lists `self.games`, which is otherwise
                // only refreshed on create/delete/rename. Re-read here so entering
                // IDE mode always offers the current set — the list is a directory
                // scan, run once per mode entry, not per frame.
                self.refresh_games();
                // IDE mode is only coherent with a module focused — `focused_module`
                // being `Some` is the invariant every IDE column relies on.
                // `nav_sel` is deliberately left untouched (S4b): it still names
                // whichever real config scope was selected before entering IDE
                // mode, and nothing about focusing a module needs to change that.
                if let Some(spec) = self.all_specs.get(self.ide_selected) {
                    self.focus_module(spec.id());
                }
            }
        }
    }

    /// The IDE column's module tree: the load-error banner, then the whole
    /// (unfiltered) module set. Returns the id to open when the selection moved.
    ///
    /// *Why no `add_enabled_ui` gate here, unlike the Config-mode nav:* in IDE mode
    /// this tree IS the primary navigation. Locking it while a draft is dirty —
    /// which is what Config mode does — would mean you can never leave the module
    /// you just typed into. The plan flags this exact shape as a dead end; per-row
    /// dirty markers replace locking in S4.
    fn render_ide_module_tree(&mut self, ui: &mut egui::Ui) -> Option<String> {
        // Rehomed from the `ext_list` block, which IDE mode drops entirely. Losing
        // it in an *authoring* mode would be the worst outcome of this restructure:
        // you'd write broken JSON and get silence.
        self.render_ext_errors_banner(ui);
        self.render_orphan_draft_banner(ui);

        // Clone out of `self` first — `render_ext_tree` takes `&mut self`, so it
        // can't also borrow `self.all_specs` / `&mut self.ide_selected`.
        let specs = self.all_specs.clone();
        let dirs = self.all_dirs.clone();
        let is_folder_ext = self.all_is_folder_ext.clone();
        // Per-row unsaved-edit flags, index-aligned with `specs`. `all_manifests`
        // is index-aligned with `all_specs` by construction (`reload_extensions`
        // builds all four vectors from one `load_extensions` result), which is
        // what makes the manifest-keyed dirty set usable here at all.
        let dirty_flags: Vec<bool> = self
            .all_manifests
            .iter()
            .map(|m| self.unsaved_drafts.contains(m))
            .collect();
        let before = self.ide_selected.min(specs.len().saturating_sub(1));
        let mut selected = before;
        // `show_inheritance = false`: IDE mode edits manifests, not config scopes,
        // so inheritance/edit badges have no scope to describe.
        self.render_ext_tree(ui, &specs, &dirs, &is_folder_ext, &mut selected, false, &dirty_flags);
        self.ide_selected = selected;
        // `leaf` writes `selected` directly and never calls `focus_module`, so the
        // tree click has to be turned into a focus change here, after the render.
        if selected != before {
            specs.get(selected).map(|s| s.id())
        } else {
            None
        }
    }

    /// IDE-mode replacement for [`render_nav_settings`] in the nav's bottom band.
    /// Returns true when "New Module" was clicked.
    ///
    /// Deliberately omits two controls the Config-mode module footer has:
    /// **Show Inheritance** (no scope in IDE mode, so the badges it toggles are
    /// meaningless) and **Clear Settings** (it clears stored *config*; IDE Mode
    /// edits manifests and must never touch config).
    fn render_ide_nav_footer(&mut self, ui: &mut egui::Ui) -> bool {
        let mut open_create = false;
        styled_checkbox(ui, &mut self.group_by_author, "Group by Author");

        // ── Preview game selector (S5b) ─────────────────────────────────────
        // *Why here and not under the tab bar:* the user placed it in the nav
        // footer, in the space above Open Folder / New Module. It sits in the
        // top-down flow above the `bottom_up` button block below, which pins the
        // two buttons to the band's floor regardless of what precedes them.
        //
        // Deferred intent (`Option<Option<String>>`: outer = "a pick happened",
        // inner = the appid or None): `set_preview_game` needs `&mut self` and
        // this whole footer already runs inside a panel closure holding one. Same
        // pattern as `open_create` right here and `nav_action` in
        // `render_nav_panel`.
        let mut preview_pick: Option<Option<String>> = None;
        let cur_label = match &self.preview_game {
            None => "None".to_string(),
            Some(id) => self
                .games
                .iter()
                .find(|(a, _)| a == id)
                .map(|(_, n)| n.clone())
                // A game deleted out from under the selector still has a live
                // scratch config; name it by appid rather than showing "None",
                // which would misdescribe what the launch band is resolving.
                .unwrap_or_else(|| id.clone()),
        };
        ui.add_space(6.0);
        ui.label(
            egui::RichText::new("Preview against")
                .color(theme::FAINT)
                .small(),
        );
        ui.add_space(2.0);
        egui::ComboBox::from_id_salt("ide_preview_game")
            .width(ui.available_width())
            .selected_text(cur_label)
            .show_ui(ui, |ui| {
                // None first and default — the preview resolves against nothing
                // unless the user opts in. Never remembered across app starts:
                // there is deliberately no `GeneralConfig` field behind this.
                if ui.selectable_label(self.preview_game.is_none(), "None").clicked() {
                    preview_pick = Some(None);
                }
                for (appid, name) in &self.games {
                    let sel = self.preview_game.as_deref() == Some(appid.as_str());
                    if ui.selectable_label(sel, name).clicked() {
                        preview_pick = Some(Some(appid.clone()));
                    }
                }
            });
        if let Some(pick) = preview_pick {
            self.set_preview_game(pick.as_deref());
        }

        let sep = icon_sep(self.general_config.mono_ui);
        ui.with_layout(egui::Layout::bottom_up(egui::Align::Center), |ui| {
            // bottom_up: first added sits at the bottom, so Open Folder ends up
            // below New Module.
            let w = ui.available_width();
            if ui
                .add_sized([w, 30.0], theme::secondary_button(format!("\u{f07b}{sep}Open Folder")))
                .on_hover_text("Open the user extensions folder")
                .clicked()
            {
                // `user_extensions()` — IDE Mode edits module *manifests*, which
                // live in `~/.config/ritz/extensions/`. The Config-mode module
                // footer's identical-looking button opens the same folder
                // (`render_modules_footer`): both sit under a module tree, so both
                // mean the same thing.
                let _ = Command::new("xdg-open").arg(self.paths.user_extensions()).spawn();
            }
            if ui
                .add_sized([w, 30.0], theme::primary_button(format!("\u{f067}{sep}New Module")))
                .on_hover_text("Create a new custom module")
                .clicked()
            {
                open_create = true;
            }
        });
        open_create
    }

    /// Drop every in-flight nav-transient state — the single definition of what
    /// "leaving a half-finished nav interaction" means.
    ///
    /// *Why one method:* the three `NavAction::Select*` arms each used to repeat
    /// these four lines, so adding a nav-transient field meant remembering to
    /// reset it in N places. `SelectGame` looked like it differed (no
    /// `text_buffers.clear()`) but doesn't: it calls `switch_game`, which clears
    /// the buffers itself — so folding the clear in here is behaviour-preserving.
    fn reset_nav_transients(&mut self) {
        self.creating_profile = false;
        self.duplicating_preset = None;
        self.creating_game = false;
        self.text_buffers.clear();
    }

    fn render_nav_tree(&mut self, ui: &mut egui::Ui) {
        // Collect any nav action to apply after rendering (avoids borrow conflicts).
        enum NavAction {
            SelectGlobal,
            SelectProfile(String),
            SelectGame(String, String), // appid, name
            StartCreateProfile,
            StartCreateGame,
        }
        let mut action: Option<NavAction> = None;

        let sep = icon_sep(self.general_config.mono_ui);
        let gap = if self.general_config.mono_ui { 8.0 } else { 7.0 };
        // No General Settings row here any more — it lives *only* in the GENERAL
        // category box above the tree (`render_nav_category_box`).
        // *Why removed:* with the box present the row was a second, competing
        // control for the same destination — selecting it left the box showing
        // "Games / Profiles" while the panel showed General Settings, and picking
        // any profile/game from the tree silently flipped the box's category. One
        // destination, one control.
        let is_global = self.nav_sel == NavSel::GlobalSettings;
        if full_selectable(ui, is_global, egui::RichText::new(format!("\u{f0ac}{sep}Global Profile")).color(COL_GLOBAL)).clicked() {
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
        let id_col = theme::PIN_ID;
        egui::CollapsingHeader::new(egui::RichText::new("Profiles").color(COL_PROFILE))
            .default_open(true)
            .show(ui, |ui| {
                // Total number of profiles that carry an indicator in this context.
                // Used by Numbers mode to decide arrow vs. numeric labels.
                let chain_total: usize =
                    (if active_preset.is_some() { 1 } else { 0 }) + profile_parents.len();
                for (name, pin) in &ordered {
                    let is_sel = self.nav_sel == NavSel::Profile(name.clone());
                    let is_active = active_preset.as_deref() == Some(name.as_str());
                    let parent_depth = profile_parents.get(name.as_str()).copied();
                    let show_indicator = is_active || parent_depth.is_some();
                    // color_depth: 0 = full COL_PROFILE, +1 per step deeper in the chain.
                    // profile_parents stores 1-based depth; subtract 1 to get color index.
                    let color_depth = parent_depth.map(|d| d - 1).unwrap_or(0);
                    let label: egui::WidgetText = if show_indicator {
                        let font_id = ui.style().text_styles[&egui::TextStyle::Body].clone();
                        let mut job = LayoutJob::default();
                        match self.general_config.inheritance_display {
                            InheritanceDisplayMode::Color => {
                                job.append(ICON_INHERIT, 0.0, TextFormat { font_id: font_id.clone(), color: profile_depth_color(color_depth), ..Default::default() });
                            }
                            InheritanceDisplayMode::Numbers => {
                                const NUMS: [&str; 6] = ["1", "2", "3", "4", "5", "5+"];
                                if chain_total == 1 {
                                    // Single profile in chain → plain arrow, no number needed.
                                    job.append(ICON_INHERIT, 0.0, TextFormat { font_id: font_id.clone(), color: COL_PROFILE, ..Default::default() });
                                } else {
                                    // Depth number: is_active = 1, chain entry at stored depth d = d.
                                    let number = parent_depth.unwrap_or(1);
                                    job.append(NUMS[(number - 1).min(5)], 0.0, TextFormat { font_id: font_id.clone(), color: COL_PROFILE, ..Default::default() });
                                }
                            }
                            InheritanceDisplayMode::ArrowsOnly => {
                                // N plain arrows: is_active = 1, chain entry at depth d = d arrows.
                                let arrow_count = parent_depth.unwrap_or(1);
                                for i in 0..arrow_count {
                                    if i > 0 {
                                        job.append(" ", 0.0, TextFormat { font_id: font_id.clone(), color: theme::TEXT, ..Default::default() });
                                    }
                                    job.append(ICON_INHERIT, 0.0, TextFormat { font_id: font_id.clone(), color: COL_PROFILE, ..Default::default() });
                                }
                            }
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
                        // Monospaced so `[1]` and `[10]` occupy the same width and the
                        // column of ids stays scannable — a proportional digit shifts
                        // the label's left edge per profile. Body's *size* is kept and
                        // only the family swapped, so this reads as the same label, not
                        // a smaller one (TextStyle::Monospace is 12pt against Body's 13).
                        let font = egui::FontId::new(
                            egui::TextStyle::Body.resolve(ui.style()).size,
                            egui::FontFamily::Monospace,
                        );
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
            Some(NavAction::SelectGlobal) => {
                self.nav_sel = NavSel::GlobalSettings;
                self.reset_nav_transients();
            }
            Some(NavAction::SelectProfile(name)) => {
                self.editing_preset_buf = self.paths.load_preset(&name).ok().flatten();
                self.nav_name_buf = name.clone();
                self.nav_sel = NavSel::Profile(name);
                self.reset_nav_transients();
            }
            Some(NavAction::SelectGame(appid, name)) => {
                self.switch_game(&appid, &name);
                self.nav_sel = NavSel::Game(appid);
                self.nav_name_buf = self.game_config.game.name.clone();
                self.editing_preset_buf = None;
                self.reset_nav_transients();
            }
            // The two StartCreate* arms deliberately do *not* call
            // `reset_nav_transients`: they *enter* a nav-transient state rather
            // than leave one, and `StartCreateProfile` in particular must keep
            // `duplicating_preset`, which the "Duplicate profile" context-menu
            // item sets just before raising this same creating_profile flag.
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

    /// Delete a profile and return to the Global Profile view (`NavSel::GlobalSettings`).
    ///
    /// **Every** stored reference to it is swept, not just the ambient game's:
    /// each `games/*.json` `Preset`, each `profiles/*.json` `Parent`, and the
    /// general-config `DefaultPreset` all fall back to "none". See
    /// [`Self::retarget_preset_references`] for why (issue #36 — resurrection by
    /// name) and why the sweep is shared with [`Self::rename_preset`].
    fn delete_profile(&mut self, name: &str) {
        let _ = self.paths.delete_preset(name);
        // Drop the editor buffer *before* the sweep: it belongs to the profile
        // just deleted, and `persist()` under `NavSel::Profile(name)` would
        // `save_preset` it straight back — re-creating the file we just removed.
        if matches!(&self.nav_sel, NavSel::Profile(p) if p == name) {
            self.editing_preset_buf = None;
        }
        self.retarget_preset_references(name, None);
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
    /// Warning banner atop the module tree when some manifests failed to load or
    /// collide. Lazily built (nothing rendered when there are no errors); the
    /// per-error `Display` lines live inside a collapsible so the tree stays
    /// uncluttered.
    fn render_ext_errors_banner(&self, ui: &mut egui::Ui) {
        if self.extension_errors.is_empty() {
            return;
        }
        // A `Dup` entry means the module DID load (it's a config-namespace
        // collision, not a parse failure) — count the two kinds separately so
        // the header doesn't claim duplicates "failed to load".
        let (failed, dup) = self.extension_errors.iter().fold((0usize, 0usize), |(f, d), e| {
            match e {
                ExtensionLoadError::Parse { .. } => (f + 1, d),
                ExtensionLoadError::Dup { .. } => (f, d + 1),
            }
        });
        let header_text = match (failed, dup) {
            (0, d) => format!("\u{26A0} {d} duplicate module(s)"),
            (f, 0) => format!("\u{26A0} {f} module(s) failed to load"),
            (f, d) => format!("\u{26A0} {f} failed to load, {d} duplicate(s)"),
        };
        let header = egui::RichText::new(header_text).color(theme::COL_GLOBAL).strong();
        egui::CollapsingHeader::new(header)
            .id_salt("ext_load_errors")
            .default_open(false)
            .show(ui, |ui| {
                for err in &self.extension_errors {
                    ui.label(
                        egui::RichText::new(err.to_string())
                            .color(theme::DIM)
                            .small(),
                    );
                }
            });
        ui.add_space(6.0);
    }

    /// Banner listing drafts whose module is no longer on disk (S4a).
    ///
    /// *Why retained-and-surfaced rather than dropped:* `ensure_draft` used to
    /// delete a draft the moment its spec left `all_specs` — an external `rm`, or
    /// a Ctrl+R after the manifest stopped validating. With one draft that was
    /// merely rude; with a map it would be a *silent* loss of unsaved work, since
    /// nothing on screen said the draft had existed. The draft therefore survives,
    /// stays editable (Save writes it back to its remembered manifest path,
    /// recreating the file), and is named here — the module tree renders
    /// `all_specs`, which by definition can no longer show it a row.
    fn render_orphan_draft_banner(&self, ui: &mut egui::Ui) {
        let names = self.orphan_draft_names();
        if names.is_empty() {
            return;
        }
        let header = egui::RichText::new(format!(
            "\u{26A0} {} unsaved draft(s) with no module on disk",
            names.len()
        ))
        .color(theme::COL_GLOBAL)
        .strong();
        egui::CollapsingHeader::new(header)
            .id_salt("orphan_drafts")
            .default_open(true)
            .show(ui, |ui| {
                for n in &names {
                    ui.label(egui::RichText::new(n).color(theme::DIM).small());
                }
                ui.label(
                    egui::RichText::new(
                        "Their manifests were deleted or stopped loading. \
                         Open one and Save to write it back.",
                    )
                    .color(theme::FAINT)
                    .small(),
                );
            });
        ui.add_space(6.0);
    }

    /// Render the left-panel module tree. In folder mode it mirrors the nested
    /// `extensions/` layout; in author mode it groups by extension Author. In
    /// both modes the built-in (backend) modules are collected under a single
    /// "Built-In" node shown last.
    ///
    /// Takes its data source as explicit parameters (`specs`/`dirs`/
    /// `is_folder_ext`, index-aligned) rather than always reading `self.cur_*` —
    /// S3b will pass `all_specs`/`all_dirs`/`all_is_folder_ext` here for the
    /// unfiltered preview column; this stage's one call site still passes the
    /// `cur_*` fields, so behaviour is unchanged.
    ///
    /// `specs`/`dirs`/`is_folder_ext`/`selected` can't be `&self.cur_specs`/
    /// `&mut self.selected_ext` etc. at the call site — that would borrow `self`
    /// immutably (or mutably, for `selected`) while also needing `&mut self` for
    /// the method receiver. The call site clones the `cur_*` Vecs and copies
    /// `selected_ext` into locals first (the same "clone the spec" pattern already
    /// used elsewhere in this file for the same reason), passes those in, then
    /// writes `selected_ext` back after the call.
    /// `dirty_flags` is index-aligned with `specs`: `true` marks a module with
    /// unsaved manifest edits (S4a). Config mode passes `&[]` — its tree has no
    /// manifest alignment to key the set on and locks while any draft exists.
    ///
    /// *Why `allow(too_many_arguments)`:* all eight are the tree's data source,
    /// already index-aligned by contract with each other. Bundling them into a
    /// struct would move the alignment invariant from "the call site builds these
    /// together" to "somebody remembered to fill the struct consistently", which
    /// is the weaker guarantee — and both call sites build every vector in one
    /// place already.
    #[allow(clippy::too_many_arguments)]
    fn render_ext_tree(
        &mut self,
        ui: &mut egui::Ui,
        specs: &[Extension],
        dirs: &[PathBuf],
        is_folder_ext: &[bool],
        selected: &mut usize,
        show_inheritance: bool,
        dirty_flags: &[bool],
    ) {
        // Build per-extension icon lists: each entry is (icon_glyph, color).
        // Inheritance from a lower layer → ICON_INHERIT; edit at current scope → ICON_EDIT.
        // Resolve against the list actually being rendered, NOT `cur_specs`: the IDE
        // column passes `all_specs`, and a spec missing from the resolution gets no
        // badges at all (see `resolve_specs_for_editing`).
        let resolution = self.resolve_specs_for_editing(specs);
        let mono = self.general_config.mono_ui;
        let nav_sel = &self.nav_sel;
        let global_modules = &self.global_config.modules;
        let editing_modules = self.editing_preset_buf.as_ref().map(|p| &p.modules);
        let game_modules = &self.game_config.config.modules.authors;
        // Individual (un-merged) parent presets for depth-shaded arrows.
        // Profile context: [direct_parent, grandparent, …]
        let profile_parent_chain: Vec<Preset> = if let NavSel::Profile(_) = &self.nav_sel {
            self.editing_preset_buf.as_ref()
                .and_then(|p| p.parent.as_ref())
                .map(|pname| collect_parent_presets(&self.paths, pname))
                .unwrap_or_default()
        } else { Vec::new() };
        // Game context: [direct_preset, its_parent, grandparent, …]
        //
        // *Why this can't over-read in IDE mode (S4b):* the IDE tree always calls
        // this with `show_inheritance = false`, so `icon_lists` short-circuits to
        // empty below and neither this chain nor `profile_parent_chain` above nor
        // the icon `match` further down is ever consulted for it — dead
        // computation, not dead code with a visible effect. `nav_sel` no longer
        // being forced to a module-editor value while IDE mode renders (see
        // `Mode`'s doc comment) therefore can't leak into the tree's badges.
        let game_preset_chain: Vec<Preset> = if matches!(self.nav_sel, NavSel::Game(_)) {
            let mut chain = Vec::new();
            if let Some(p) = &self.preset {
                chain.push(p.clone());
                if let Some(pname) = &p.parent {
                    chain.extend(collect_parent_presets(&self.paths, pname));
                }
            }
            chain
        } else { Vec::new() };

        let display_mode = self.general_config.inheritance_display;
        let mut icon_lists: Vec<Vec<(&'static str, Color32)>> = if !show_inheritance {
            vec![Vec::new(); specs.len()]
        } else { specs.iter().map(|spec| {
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
                                .is_none_or(|f| field_visible(f, ext_res))
                    }))
                    .unwrap_or(false)
            };
            const DEPTH_NUM: [&str; 6] = ["1", "2", "3", "4", "5", "5+"];
            let push_chain_icons = |icons: &mut Vec<(&'static str, Color32)>, chain: &[Preset]| {
                match display_mode {
                    InheritanceDisplayMode::Color => {
                        for (i, p) in chain.iter().enumerate() {
                            if has_in(&p.modules) { icons.push((ICON_INHERIT, profile_depth_color(i))); }
                        }
                    }
                    InheritanceDisplayMode::Numbers => {
                        let contributing: Vec<usize> = chain.iter().enumerate()
                            .filter(|(_, p)| has_in(&p.modules))
                            .map(|(i, _)| i)
                            .collect();
                        if chain.len() <= 1 {
                            // Single-profile chain → plain arrow.
                            if !contributing.is_empty() { icons.push((ICON_INHERIT, COL_PROFILE)); }
                        } else {
                            // Multi-profile chain → always show numbers, no arrows.
                            for i in contributing { icons.push((DEPTH_NUM[i.min(5)], COL_PROFILE)); }
                        }
                    }
                    InheritanceDisplayMode::ArrowsOnly => {
                        if chain.iter().any(|p| has_in(&p.modules)) { icons.push((ICON_INHERIT, COL_PROFILE)); }
                    }
                }
            };
            let mut icons: Vec<(&'static str, Color32)> = Vec::new();
            match nav_sel {
                NavSel::Game(_) => {
                    if has_in(global_modules) { icons.push((ICON_INHERIT, COL_GLOBAL)); }
                    push_chain_icons(&mut icons, &game_preset_chain);
                    if has_in(game_modules) { icons.push((ICON_EDIT, COL_GAME)); }
                }
                NavSel::Profile(_) => {
                    if has_in(global_modules) { icons.push((ICON_INHERIT, COL_GLOBAL)); }
                    push_chain_icons(&mut icons, &profile_parent_chain);
                    if editing_modules.map(has_in).unwrap_or(false) { icons.push((ICON_EDIT, COL_PROFILE)); }
                }
                NavSel::GlobalSettings => {
                    if has_in(global_modules) { icons.push((ICON_EDIT, COL_GLOBAL)); }
                }
                NavSel::GeneralSettings => {}
            }
            icons
        }).collect() };

        // Unsaved-edit marker (S4a). Prepended into the SAME `icon_lists` the
        // inheritance badges use, so it goes through `leaf`'s `LayoutJob` path
        // with the correct inter-glyph gaps and the name still rendered in
        // `theme::TEXT`.
        //
        // *Why not appended to the label string:* a glyph baked into the label
        // would change what the row's text IS — it would flow through
        // `full_selectable`'s layout as ordinary text (no independent colour), and
        // the label is what the user reads as the module's name. The glyph column
        // already exists for exactly this kind of per-row status mark.
        //
        // `icon_lists` is empty in IDE mode (`show_inheritance = false` there), so
        // in practice this is the only glyph on the row; the prepend order is
        // still explicit so a future IDE badge can't push the dirty mark out of
        // the leading position.
        for (i, list) in icon_lists.iter_mut().enumerate() {
            if dirty_flags.get(i).copied().unwrap_or(false) {
                list.insert(0, (ICON_DIRTY, theme::ACCENT));
            }
        }

        let mut builtins: Vec<usize> = Vec::new();
        let mut others: Vec<usize> = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
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
                    .entry(specs[i].meta.author.clone())
                    .or_default()
                    .push(i);
            }
            for (author, items) in &by_author {
                egui::CollapsingHeader::new(egui::RichText::new(author).color(theme::ACCENT).strong())
                    .default_open(true)
                    .show(ui, |ui| {
                        for &i in items {
                            leaf(ui, &mut self.icon_cache, i, specs, &icon_lists, selected, mono);
                        }
                    });
            }
        } else {
            let mut root = TreeNode::default();
            for &i in &others {
                let comps: Vec<String> = dirs[i]
                    .iter()
                    .map(|c| c.to_string_lossy().into_owned())
                    .collect();
                root.insert(&comps, i);
            }
            let ctx = TreeCtx { specs, is_folder_ext, icon_lists: &icon_lists, mono };
            render_node(ui, &mut self.icon_cache, &root, &ctx, selected);
        }

        if !builtins.is_empty() {
            egui::CollapsingHeader::new(egui::RichText::new("Built-In").color(theme::ACCENT).strong())
                .default_open(true)
                .show(ui, |ui| {
                    for &i in &builtins {
                        leaf(ui, &mut self.icon_cache, i, specs, &icon_lists, selected, mono);
                    }
                });
        }
    }
}

/// The part of [`render_node`]'s input that never changes as it recurses.
///
/// *Why a struct:* these four were four separate parameters threaded unchanged
/// through every recursive call, which is what pushed `render_node` over
/// clippy's argument limit. Bundling the invariants (rather than silencing the
/// lint) leaves only what actually varies per node — the node itself and the
/// selection — in the signature, and makes the recursive call read as
/// "same context, next node".
struct TreeCtx<'a> {
    specs: &'a [Extension],
    is_folder_ext: &'a [bool],
    icon_lists: &'a [Vec<(&'static str, Color32)>],
    mono: bool,
}

/// Render a tree node: subfolders (sorted) first, then extensions at this level.
/// A child folder whose single leaf has `is_folder_ext` set is collapsed into
/// the leaf directly (no CollapsingHeader wrapper).
fn render_node(ui: &mut egui::Ui, cache: &mut IconCenterCache, node: &TreeNode, ctx: &TreeCtx, selected: &mut usize) {
    let mut leaves: Vec<usize> = node.leaves.clone();
    for (name, child) in &node.children {
        // ponytail: skip the subfolder header when the folder IS the extension
        if child.children.is_empty() && child.leaves.len() == 1 && ctx.is_folder_ext.get(child.leaves[0]).copied().unwrap_or(false) {
            leaves.push(child.leaves[0]);
        } else {
            egui::CollapsingHeader::new(egui::RichText::new(name).color(theme::ACCENT).strong())
                .default_open(true)
                .show(ui, |ui| render_node(ui, cache, child, ctx, selected));
        }
    }
    leaves.sort_by(|&a, &b| ctx.specs[a].meta.name.cmp(&ctx.specs[b].meta.name));
    for &i in &leaves {
        leaf(ui, cache, i, ctx.specs, ctx.icon_lists, selected, ctx.mono);
    }
}

/// A fixed-width, faint, left-aligned footer label so the inputs/dropdown to its
/// right all start at the same x and fill to the same right edge. Allocates an
/// *exact* box (not content-sized) so every row's label column is identical.
/// COL_PROFILE (#6CC551) with HSV V decreased by 0.10 per depth level (max 5 steps).
/// depth 0 = full COL_PROFILE, depth 1 = 1-step dimmer, …, depth 5+ = fixed floor.
fn profile_depth_color(depth: usize) -> Color32 {
    let steps = depth.min(5) as f32;
    let h = 106.0_f32 + steps * 20.0;
    let (v, s) = (0.773_f32 - steps * 0.10, 0.588_f32);
    let c = v * s;
    let h6 = h / 60.0;
    let x = c * (1.0 - (h6 % 2.0 - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h6 as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color32::from_rgb(
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

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
/// Cap on the chip's *text* width, in points, before it elides with `…`.
///
/// The chip is now mode-aware (`GuiApp::title_chip_text`) and can show a
/// module's authored `Name` rather than just a game title, so it needed a
/// bound it never had before: unbounded, a long enough name would widen the
/// chip enough to crowd the title bar's right-aligned Launch/Cancel cluster
/// (`GuiApp::render_title_bar`, the `launch_mode` branch, which shares this
/// same row). 260px comfortably fits every bundled game/module name today
/// with room to spare; only pathological cases ever reach the ellipsis.
const BREADCRUMB_MAX_TEXT_W: f32 = 260.0;

/// The title bar's rounded name/status chip. Sizes to its text (up to
/// [`BREADCRUMB_MAX_TEXT_W`], beyond which it elides with `…` and the full
/// text becomes a hover tooltip) plus fixed padding — never a fixed width, so
/// short text (e.g. `"Settings"`) doesn't leave a stub of empty chip.
fn breadcrumb_chip(ui: &mut egui::Ui, text: &str) {
    // Same eliding-`LayoutJob` idiom `render_editor_header_description` uses
    // for the editor header's description line: a `Label`/`layout_no_wrap`
    // would let the galley (and so the chip) grow without bound, which is
    // exactly what capping it here prevents.
    let mut job = egui::text::LayoutJob::simple_singleline(
        text.to_owned(),
        egui::FontId::proportional(12.5),
        theme::TEXT,
    );
    job.wrap.max_width = BREADCRUMB_MAX_TEXT_W;
    job.wrap.max_rows = 1;
    job.wrap.overflow_character = Some('\u{2026}');
    let galley = ui.fonts(|f| f.layout_job(job));
    let elided = galley.elided;
    let pad = egui::vec2(12.0, 4.0);
    let size = galley.size() + pad * 2.0;
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::hover());
    ui.painter().rect(
        rect,
        egui::Rounding::same(rect.height() / 2.0),
        theme::SEL,
        egui::Stroke::new(1.0, theme::SELBD),
    );
    let pos = rect.center() - galley.size() / 2.0;
    ui.painter().galley(pos, galley, theme::TEXT);
    // The chip has never been clickable (`Sense::hover` only, in every mode) —
    // when eliding, a tooltip is the only way to still see the full text.
    if elided {
        resp.on_hover_text(text);
    }
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

/// Gutter between two adjacent category tabs, in points. Tighter than the
/// global `item_spacing.x` (7px) — see [`nav_category_tab`] for the width
/// arithmetic this number is part of.
const TAB_GAP: f32 = 4.0;

/// Text size for a category tab's glyph + label, in points.
///
/// **This number is measured, not chosen.** The nav column is a fixed
/// [`NAV_W`] 280px; the header frame's 16px side margins and the category
/// frame's 8px inner margins leave **232px** for three tabs, i.e. `(232 −
/// 2×`[`TAB_GAP`]`) / 3 ≈ 74.7px` each. Worst case is `mono_ui` (the default),
/// where Geist Mono makes every glyph 0.6em wide and all three labels are
/// exactly 8 characters:
///
/// | size | glyph + gap + label (mono) | fits 74.7px? |
/// |------|----------------------------|--------------|
/// | 13px (Body)  | 8 + 8 + 64 = **80px** | no — clips |
/// | 12px         | 7 + 7 + 56 = **70px** | only just — 4.7px total slack |
/// | 11px (Small) | 6 + 7 + 48 = **61px** | yes — 13.7px slack |
///
/// So 11px, which is also an existing step of the type scale
/// (`TextStyle::Small`) rather than an invented size. *Consequence worth
/// knowing:* the fit is driven by the **label length**, not by this size — any
/// label longer than 8 characters clips again, so the three labels cannot grow
/// without revisiting this table.
///
/// *Why not icon-only with tooltips,* which would fit at full 13px: a tooltip
/// is not discoverable, and top-level navigation is exactly where a first-time
/// reader should not have to hover to find out where a button goes.
const TAB_TEXT_SIZE: f32 = 11.0;

/// One tab of the nav's category bar: `glyph` + `label`, centered in a cell of
/// exactly `tab_w` points.
///
/// *Why hand-painted instead of [`full_selectable`],* which the stacked-row
/// version used: `full_selectable` is `top_down_justified`, i.e. deliberately
/// full-width — the wrong primitive once three of these have to share one row.
/// Its selection fill is also a plain rounded rect, which in a horizontal strip
/// reads as "one cell is slightly lighter" rather than "this is the open tab".
///
/// Selected state therefore layers two cues, both derived from `accent` (the
/// tab's own color — `ACCENT` for IDE Mode, a scope/neutral color for the
/// other two, see `render_nav_category_box`):
///
/// - **fill/border**: `theme::selection_tint(accent)` — the same ~16%/~42%
///   tint formula `SEL`/`SELBD` are hand-expanded from, applied to whichever
///   color this tab owns — rounded on **all four corners** at the same `8.0`
///   radius every other rounded container in the app uses
///   (`Rounding::same(8.0)`, per `docs/ui/STYLING-GUIDE.md`);
/// - **ink**: glyph in `accent`, label in full-brightness `TEXT`.
///
/// *Why per-tab color only on selected, not unselected too:* unselected tabs
/// stay `FAINT`/`DIM` (identical across all three) so exactly one tab reads as
/// "live" and the resting strip doesn't look like an undecided 3-color legend
/// — the color only shows up once you're actually looking at that tab's
/// content.
///
/// *Why no underline anymore:* an earlier version rounded only the top two
/// corners and sat the cell on a separate 2px `ACCENT` underline across its
/// bottom edge, so the selected tab read as "sitting on a strip." In practice
/// that read as a flat outline bar rather than a selected tab. The fill/border
/// plus the tinted icon + bright label already carry "this one is selected" on
/// their own — the underline was a second, redundant cue that also fought the
/// uniform rounding every other control in the app uses.
///
/// Unselected is transparent (a `HOV` wash on hover only), glyph in `FAINT`,
/// label in `DIM` — legible but clearly receded, so exactly one tab reads as
/// live. *Why color and not a bold weight:* the bold family
/// (`FontFamily::Name("bold")`) is reserved for the logo wordmark, and a weight
/// switch would reflow the label inside a fixed-width cell.
///
/// Icons go through a plain [`LayoutJob`], **not** [`IconCenterCache`]: that
/// cache corrects glyph *ink* for a square `icon_button`, whereas here the glyph
/// is just the first run of a text line, identical in construction to [`leaf`]'s.
fn nav_category_tab(
    ui: &mut egui::Ui,
    tab_w: f32,
    selected: bool,
    glyph: &str,
    label: &str,
    mono: bool,
    accent: Color32,
) -> egui::Response {
    // Same font-independent icon→text gap idiom the tree leaves use, scaled to
    // the smaller tab text.
    let gap = if mono { 7.0 } else { 6.0 };
    let (icon_col, text_col) = if selected {
        (accent, theme::TEXT)
    } else {
        (theme::FAINT, theme::DIM)
    };
    let font_id = egui::FontId::new(TAB_TEXT_SIZE, egui::FontFamily::Proportional);
    let mut job = LayoutJob::default();
    job.append(glyph, 0.0, TextFormat { font_id: font_id.clone(), color: icon_col, ..Default::default() });
    job.append(label, gap, TextFormat { font_id, color: text_col, ..Default::default() });
    let galley = ui.fonts(|f| f.layout_job(job));

    // Cell height: the standard control height. No underline band anymore, so
    // there's no extra space to reserve beyond the normal control height.
    let h = ui.spacing().interact_size.y;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(tab_w, h), egui::Sense::click());

    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        // All four corners, matching the app-wide 8px rounding convention (see
        // `docs/ui/STYLING-GUIDE.md`, "Corner rounding is Rounding::same(8.0)
        // everywhere").
        let rounding = egui::Rounding::same(8.0);
        if selected {
            let (fill, border) = theme::selection_tint(accent);
            painter.rect(rect, rounding, fill, egui::Stroke::new(1.0, border));
        } else if resp.hovered() {
            painter.rect_filled(rect, rounding, theme::HOV);
        }
        // Centered on the whole cell now that there's no underline strip to
        // subtract.
        let pos = rect.center() - galley.size() * 0.5;
        ui.painter().galley(pos, galley, theme::TEXT);
    }
    resp
}

/// A single selectable extension leaf.
/// Colored icons prefix the name: ICON_INHERIT for inherited layers, ICON_EDIT for
/// the current editing scope. The name itself stays the default UI color.
fn leaf(ui: &mut egui::Ui, cache: &mut IconCenterCache, i: usize, specs: &[Extension], icon_lists: &[Vec<(&'static str, Color32)>], selected: &mut usize, mono: bool) {
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
        // Centered on its ink, not its layout box: `Align2::RIGHT_CENTER` put the
        // cog wherever the font's line box happened to fall, which read a couple
        // of pixels high next to the row text. `centered_pos` returns a corrected
        // top-left, so this MUST anchor LEFT_TOP or it double-corrects.
        let font = egui::TextStyle::Body.resolve(ui.style());
        let r = resp.rect;
        let center = egui::pos2(r.right() - 11.0 - ICON_CELL / 2.0, r.center().y);
        let pos = cache.centered_pos(ui, "\u{f013}", &font, center);
        ui.painter()
            .text(pos, egui::Align2::LEFT_TOP, "\u{f013}", font, theme::FAINT);
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
    // Subtract item_spacing.x: egui consumes it when the track widget is placed,
    // but available_width() does not subtract it in advance.
    let track_w = (ui.available_width() - ui.spacing().item_spacing.x).max(40.0);
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
        const RADIUS: f32 = 7.5;
        // Clamp so the handle circle never spills outside the track rect (e.g. when
        // value == min and fill_x == rect.left()).
        let handle_x = fill_x.clamp(rect.left() + RADIUS, rect.right() - RADIUS);
        painter.rect_filled(track, egui::Rounding::same(3.0), theme::BORDER);
        painter.rect_filled(
            egui::Rect::from_min_max(track.min, egui::pos2(fill_x, track.max.y)),
            egui::Rounding::same(3.0),
            scope,
        );
        painter.circle_filled(egui::pos2(handle_x, cy), RADIUS, scope);
        painter.circle_stroke(egui::pos2(handle_x, cy), RADIUS, egui::Stroke::new(2.5, theme::PANEL));
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

/// Lay out and sense a single clickable `[box] label` row: the whole rect (box +
/// gap + label) is one click target with a subtle hover background. `paint_box`
/// draws the 18×18 box. Returns the row's response.
/// `interactive: false` allocates the row with `Sense::hover()` instead of
/// `Sense::click()` and skips the hover highlight, so the row paints identically
/// but can never report a click. Used by the read-only (preview) render path,
/// where a returned `Some(Tri)` would write config — see [`scope_checkbox`].
fn checkbox_row(
    ui: &mut egui::Ui,
    label: &str,
    badge: Option<(&str, Color32)>,
    interactive: bool,
    paint_box: impl FnOnce(&egui::Painter, egui::Rect),
) -> egui::Response {
    const BOX: f32 = 18.0;
    const GAP: f32 = 7.0;
    let font = egui::TextStyle::Body.resolve(ui.style());
    let badge_galley = badge.map(|(text, col)| ui.painter().layout_no_wrap(text.to_owned(), font.clone(), col));
    let badge_w = badge_galley.as_ref().map(|g| g.size().x + GAP).unwrap_or(0.0);
    let galley = ui.painter().layout_no_wrap(label.to_owned(), font, theme::TEXT);
    let size = egui::vec2(BOX + GAP + badge_w + galley.size().x, BOX.max(galley.size().y));
    let sense = if interactive { egui::Sense::click() } else { egui::Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(size, sense);
    if ui.is_rect_visible(rect) {
        let painter = ui.painter();
        if interactive && resp.hovered() {
            painter.rect_filled(rect.expand2(egui::vec2(4.0, 2.0)), egui::Rounding::same(6.0), theme::HOV);
        }
        let box_rect = egui::Rect::from_min_size(
            egui::pos2(rect.left(), rect.center().y - BOX / 2.0),
            egui::Vec2::splat(BOX),
        );
        paint_box(painter, box_rect);
        let mut tx = box_rect.right() + GAP;
        if let (Some(bg), Some((_, col))) = (badge_galley, badge) {
            painter.galley(egui::pos2(tx, rect.center().y - bg.size().y / 2.0), bg, col);
            tx += badge_w;
        }
        painter.galley(egui::pos2(tx, rect.center().y - galley.size().y / 2.0), galley, theme::TEXT);
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
            checkbox_row(ui, label, None, true, |p, r| paint_scope_box(p, r, on, false, theme::COL_DEFAULT));
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
    let resp = checkbox_row(ui, label, None, true, |p, r| paint_scope_box(p, r, on, false, COL_PROFILE));
    if resp.clicked() {
        *checked = !*checked;
    }
    resp
}

/// The tri-state a [`scope_checkbox`] renders, together with the effective
/// (inherited) on/off it falls back to when the state is `Unset`. The two are
/// only ever meaningful together, so they travel as one argument — which also
/// keeps `scope_checkbox` at the arity limit now that it takes `read_only`.
struct TriValue {
    state: Tri,
    inherited_on: bool,
}

/// A mockup-style checkbox over the tri-state model, with the label as part of
/// the click target:
/// - **left-click** toggles Enabled ⇄ Disabled (flipping from the current
///   effective on/off, so the first click on an inherited value sets the opposite)
/// - **right-click** resets to Unset (inherit from a lower scope)
///
/// `read_only` makes the row inert: it paints exactly the same box, badge and
/// label but is allocated non-interactively and always returns `None`, so the
/// caller can never reach `apply_tri` (which writes config *and* seeds
/// `text_buffers`). The gate is this parameter, not `ui.is_enabled()` — a
/// disabled parent `Ui` would grey the row out but is not what stops the write.
fn scope_checkbox(
    ui: &mut egui::Ui,
    label: &str,
    hover: &str,
    value: TriValue,
    scope: Color32,
    badge: Option<(&str, Color32)>,
    read_only: bool,
) -> Option<Tri> {
    let TriValue { state, inherited_on } = value;
    let on = match state {
        Tri::Enabled => true,
        Tri::Disabled => false,
        Tri::Unset => inherited_on,
    };
    let faded = state == Tri::Unset;
    let mut resp = checkbox_row(ui, label, badge, !read_only, |p, r| {
        paint_scope_box(p, r, on, faded, scope)
    });
    if !hover.is_empty() {
        resp = resp.on_hover_text(hover);
    }

    if read_only {
        None
    } else if resp.secondary_clicked() {
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
    fn editor_control_width_fills_remainder_and_floors() {
        // Whatever is left of the row after the label column, minus a right-pinned
        // reservation — this is what makes every editor textbox share one right
        // edge instead of each ending where its own content ran out.
        assert_eq!(editor_control_width(400.0, 0.0), 400.0);
        assert_eq!(editor_control_width(400.0, ACTION_COL_W), 400.0 - ACTION_COL_W);
        // Never collapses, however narrow the window (or deep the nesting) gets.
        assert_eq!(editor_control_width(90.0, ACTION_COL_W), MIN_CONTROL_W);
        assert_eq!(editor_control_width(-50.0, 0.0), MIN_CONTROL_W);
    }

    /// Every label the editor puts in front of a control, in both UI fonts.
    ///
    /// Kept as a flat list rather than derived from the call sites (there is no
    /// way to reflect over string literals): if you add an editor row, add its
    /// label here too.
    const EDITOR_ROW_LABELS: &[&str] = &[
        "Author", "Name", "Var Name", "Variable", "Type", "Description",
        "Requires", "Option", "Range", "Default", "Operation", "Value",
        "Separator", "Command", "Priority",
    ];

    /// No editor row label may outgrow the fixed [`EDITOR_LABEL_W`] column.
    ///
    /// *Why this is a test and not a comment:* `editor_row_label` pads the cursor
    /// out to a constant x so every row's control shares one left edge. A label
    /// wider than the reserve doesn't clip — `(EDITOR_LABEL_W - used).max(0.0)`
    /// just stops padding — it silently pushes *that one row's* control right,
    /// breaking the alignment the whole column exists to provide. That is a
    /// pixel-level regression nobody notices in review, and it is exactly the
    /// risk taken on by renaming "Op" → "Operation" and "Name" → "Var Name"
    /// (2026-07-19, issue #25). Measuring beats assuming: the widest label must
    /// leave the reserve intact in *both* the mono and proportional UI fonts,
    /// since `mono_ui` is a user setting and mono is the wider of the two.
    #[test]
    fn every_editor_row_label_fits_the_label_column() {
        for mono in [true, false] {
            let ctx = egui::Context::default();
            crate::fonts::install(&ctx, mono);
            crate::theme::apply(&ctx);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1200.0, 900.0),
                )),
                ..Default::default()
            };
            let mut widths: Vec<(&str, f32)> = Vec::new();
            let _ = ctx.run(input, |ctx| {
                egui::CentralPanel::default().show(ctx, |ui| {
                    for label in EDITOR_ROW_LABELS {
                        // Measured the way `editor_row_label` measures it: the
                        // cursor advance the label costs, which includes the
                        // `item_spacing.x` gap that follows it.
                        ui.horizontal(|ui| {
                            let left = ui.cursor().min.x;
                            field_label(ui, label);
                            widths.push((label, ui.cursor().min.x - left));
                        });
                    }
                });
            });
            let over: Vec<_> = widths.iter().filter(|(_, w)| *w > EDITOR_LABEL_W).collect();
            assert!(
                over.is_empty(),
                "mono={mono}: label(s) exceed the {EDITOR_LABEL_W}px reserve, \
                 which would push their row's control out of alignment: {over:?}\n\
                 all: {widths:?}",
            );
        }
    }

    /// An [`EditorHeaderInfo`] with every optional status line switched on:
    /// dirty (state line) + colliding section names + an unparseable `Requires`
    /// + a blocked pending rename.
    ///
    /// `validate_err` is `Some` as well, to prove the
    /// `sections_unique`/`validate_err` pair really is one line and not two — if
    /// that `else if` ever became a second `if`, the stack would grow a line and
    /// the exhaustive walk below would see it.
    fn worst_case_header_info() -> EditorHeaderInfo {
        EditorHeaderInfo {
            name: "Gamescope".to_string(),
            version: "1.2.3".to_string(),
            author: "Ritze".to_string(),
            description: Some("Compositing micro-compositor for games.".to_string()),
            editable: true,
            dirty: true,
            unsaved: true,
            save_on: false,
            has_identity: true,
            identity_err: Some("a module with that name already exists".to_string()),
            sections_unique: false,
            validate_err: Some("some other schema problem".to_string()),
            req_ok: false,
        }
    }

    /// Every draft state produces **exactly one** [`DiagSeverity::Info`] status
    /// line, and it is always the first one.
    ///
    /// *What this used to assert, and why it changed* (2026-07-19, issue #26):
    /// this was `status_line_count_never_exceeds_the_reserved_slot`, and it
    /// bounded the stack at four — the number of lines the IDE header band
    /// reserved space for, where a fifth would have painted over the editor and
    /// preview columns (the black-bar failure mode of `1142dd8`, in reverse).
    /// That bound is **gone, deliberately**: the messages now render in the
    /// diagnostics band's scroll area, which absorbs any number of them at no
    /// height cost, so there is no longer a layout budget for a message count to
    /// overrun. Asserting a ceiling nothing enforces would be theatre.
    ///
    /// The exhaustive combination walk is kept, because two *new* invariants
    /// took the old one's place and both are exactly as easy to break by adding
    /// a branch to [`editor_status_lines`]:
    ///
    /// 1. **At least one line, always** — the state line is unconditional. The
    ///    diagnostics band pins index 0 to the top of its list; if the ladder
    ///    could produce an empty list, that pinning would silently promote a
    ///    warning into the info slot and paint it accent blue.
    /// 2. **Exactly one `Info`, and it is index 0** — the band's tally counts
    ///    problems by skipping info entries, so a second `Info` would be a
    ///    message that renders as informational and is counted as nothing. It
    ///    would simply never show up in the heading row.
    ///
    /// The old "the reservation is tight" check is kept in spirit as the
    /// `MAX_LINES` assertion below: it no longer guards a layout budget, but it
    /// still catches the ladder growing a branch nobody described.
    ///
    /// *Why `unsaved` joined the walk* (2026-07-19, issue #34): the state line's
    /// first branch split into three arms (dirty / rename-only / clean), and the
    /// rename-only arm is the whole point of that issue. It is walked
    /// **independently** of `dirty` and `has_identity` rather than derived from
    /// them: production upholds `unsaved == dirty || has_identity`, but this
    /// test's job is to prove the ladder stays well-formed for *any* flag
    /// combination, including ones a future refactor could make reachable.
    /// Doubles the walk to 256 cases and still runs instantly.
    #[test]
    fn status_lines_have_exactly_one_pinned_info_line() {
        /// The four branches [`editor_status_lines`] enumerates: state line,
        /// section-collision *or* validate error, `Requires` parse, pending
        /// identity. Not a layout budget any more — a description of the ladder,
        /// asserted tight so it cannot silently stop describing it.
        ///
        /// Unchanged by issue #34: the state line grew a third *arm*, not a second
        /// line — the arms are mutually exclusive.
        const MAX_LINES: usize = 4;
        let mut worst = 0usize;
        for editable in [true, false] {
            // `dirty` and `unsaved` walked as a pair rather than as a nested loop:
            // all four combinations, no extra indentation level on a stack that is
            // already seven deep.
            for (dirty, unsaved) in
                [(true, true), (true, false), (false, true), (false, false)]
            {
                for sections_unique in [true, false] {
                    for validate_err in [true, false] {
                        for req_ok in [true, false] {
                            for has_identity in [true, false] {
                                for identity_err in [true, false] {
                                    let info = EditorHeaderInfo {
                                        editable,
                                        dirty,
                                        unsaved,
                                        sections_unique,
                                        validate_err: validate_err
                                            .then(|| "boom".to_string()),
                                        req_ok,
                                        has_identity,
                                        identity_err: identity_err
                                            .then(|| "nope".to_string()),
                                        ..worst_case_header_info()
                                    };
                                    let lines = editor_status_lines(&info);
                                    let n = lines.len();
                                    // Spelled out rather than `{info:?}`:
                                    // `EditorHeaderInfo` is production state and
                                    // does not derive `Debug` just to serve a
                                    // test's failure message.
                                    let case = format!(
                                        "editable={editable} dirty={dirty} \
                                         unsaved={unsaved} \
                                         sections_unique={sections_unique} \
                                         validate_err={validate_err} req_ok={req_ok} \
                                         has_identity={has_identity} \
                                         identity_err={identity_err}"
                                    );
                                    // The state line is unconditional.
                                    assert!(n >= 1, "no state line for {case}");
                                    assert_eq!(
                                        lines[0].severity,
                                        DiagSeverity::Info,
                                        "the first status line is {:?}, not Info, for \
                                         {case} \u{2014} the diagnostics band pins index \
                                         0 to the top of its list in info blue",
                                        lines[0].severity,
                                    );
                                    let infos = lines
                                        .iter()
                                        .filter(|l| l.severity == DiagSeverity::Info)
                                        .count();
                                    assert_eq!(
                                        infos, 1,
                                        "{infos} Info status lines for {case} \u{2014} the \
                                         band's tally skips Info, so a second one would \
                                         render as a message nothing counts",
                                    );
                                    assert!(
                                        n <= MAX_LINES,
                                        "{n} status lines for {case}, but the ladder is \
                                         documented as having {MAX_LINES} branches",
                                    );
                                    worst = worst.max(n);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Tight, not just "fits": if nothing can reach MAX_LINES any more, the
        // enumeration above has stopped describing the ladder and should be
        // corrected rather than left as a comfortable over-estimate.
        assert_eq!(
            worst, MAX_LINES,
            "the ladder is documented as {MAX_LINES} branches but nothing can \
             produce more than {worst}",
        );
    }

    /// The diagnostics band's list: the draft's info line pinned first, its
    /// problems next, the env-overwrite warnings last — and the tally counting
    /// the problems only.
    ///
    /// *Why this replaces `ide_header_status_slot_is_one_small_row_per_line`*
    /// (2026-07-19, issue #26): that test pinned the 14pt galley pitch the
    /// header band's fixed status slot was sized from. The slot is gone, so the
    /// pitch is not load-bearing any more — the band lays these out as ordinary
    /// `ui.label`s inside a scroll area. What *is* load-bearing now is the
    /// assembly: which entry comes first, what rank each source gets, and what
    /// the tally counts. That is what this asserts instead.
    #[test]
    fn diagnostic_entries_pin_the_info_line_and_rank_env_warnings() {
        let info = worst_case_header_info();
        let env = [EnvOverwrite {
            var: "DXVK_HUD".to_string(),
            victims: vec!["MangoHud".to_string()],
            culprit: "Gamescope".to_string(),
            by_unset: false,
        }];
        let entries = ide_diagnostic_entries(Some(&info), &env);

        // Pinned info line, first, whatever else is in the list.
        assert_eq!(entries[0].severity, DiagSeverity::Info);
        assert_eq!(entries[0].text, editor_status_lines(&info)[0].text);
        // Env warnings land after the draft's own messages, and are warnings.
        let last = entries.last().expect("at least the env warning");
        assert_eq!(last.severity, DiagSeverity::Warning);
        assert!(last.text.contains("DXVK_HUD"), "{}", last.text);

        // The tally the heading row draws: problems only, info excluded.
        let count = |s: DiagSeverity| entries.iter().filter(|e| e.severity == s).count();
        assert_eq!(count(DiagSeverity::Info), 1, "exactly one pinned info line");
        // worst_case_header_info: section collision (validate_err is the same
        // `else if` branch, so it does NOT add a line), unparseable Requires,
        // blocked rename — three errors. Plus the one env warning.
        assert_eq!(count(DiagSeverity::Error), 3);
        assert_eq!(count(DiagSeverity::Warning), 1);

        // No draft resolved yet: env warnings only, and nothing to pin.
        let no_draft = ide_diagnostic_entries(None, &env);
        assert_eq!(no_draft.len(), 1);
        assert_eq!(no_draft[0].severity, DiagSeverity::Warning);

        // Clean draft, no env warnings: one info entry and zero problems, which
        // is what makes the band draw "No issues." under the pinned line rather
        // than leaving an empty box.
        let clean = EditorHeaderInfo {
            dirty: false,
            sections_unique: true,
            validate_err: None,
            req_ok: true,
            has_identity: false,
            ..worst_case_header_info()
        };
        let clean_entries = ide_diagnostic_entries(Some(&clean), &[]);
        assert_eq!(clean_entries.len(), 1);
        assert_eq!(clean_entries[0].severity, DiagSeverity::Info);
    }

    /// The IDE header band's content lays out to **exactly** [`IDE_HEADER_H`].
    ///
    /// *Why this test exists* (2026-07-19, issue #26): `IDE_HEADER_H` is a
    /// hand-summed derivation feeding an `exact_height` panel, and the two
    /// failure modes are silent and opposite. Under-count and egui clips the
    /// frame *fill* to `exact_height` while advancing the cursor from the
    /// frame's true, larger rect — nothing paints the difference and the
    /// framebuffer clear colour shows through as a black bar (the `1142dd8`
    /// bug, caused by exactly this sum being wrong by 4pt). Over-count and the
    /// band carries dead space nobody notices. Neither shows up in `cargo
    /// check`, and both are one careless margin edit away.
    ///
    /// So this reproduces the real panel's frame — same margins, same **two**
    /// rows, real bundled font, real `theme::apply` — and asserts the laid-out
    /// height equals the constant on the nose. `min_rect` (not the cursor) is
    /// the measurement, because the cursor sits one `item_spacing.y` *past* the
    /// last widget and would overcount by 7.
    ///
    /// *Why two rows and not three* (2026-07-19, issue #26, revised same day):
    /// the third row was the fixed status slot, which has moved to the
    /// diagnostics band — so the measured content is the heading row and the
    /// description, and the expected sum dropped from `134.0` back to `70.0`.
    /// The test is otherwise untouched: it is the only thing standing between a
    /// margin edit and the black bar, and it still measures the real widgets
    /// rather than a re-declared copy of their numbers.
    ///
    /// The status stack in `worst_case_header_info` no longer affects this
    /// measurement at all — kept as the fixture because the *header row* still
    /// reads `dirty`/`save_on` from it, and a fixture whose flags are all set is
    /// the one most likely to surface a row that sizes itself differently when
    /// something is wrong.
    #[test]
    fn ide_header_content_is_exactly_ide_header_h() {
        for mono in [true, false] {
            let ctx = egui::Context::default();
            crate::fonts::install(&ctx, mono);
            crate::theme::apply(&ctx);
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::pos2(0.0, 0.0),
                    egui::vec2(1200.0, 900.0),
                )),
                ..Default::default()
            };
            let info = worst_case_header_info();
            let mut cache = IconCenterCache::new();
            let mut measured = f32::NAN;
            let _ = ctx.run(input, |ctx| {
                // Literally the same frame margin the `ide_module_header`
                // panel uses — [`IDE_HEADER_MARGIN`], the constant
                // `IDE_HEADER_H` itself is derived from. Re-declaring the four
                // numbers here instead would make this test measure a *copy* of
                // the band: an edit to the panel's margin alone would then leave
                // this green while shipping the black bar (tried it — it does).
                let frame = egui::Frame::none().inner_margin(IDE_HEADER_MARGIN);
                egui::CentralPanel::default().frame(frame).show(ctx, |ui| {
                    // Wrapped in a `scope` because the measurement is the
                    // *content* bounding box: a `CentralPanel`'s own `Ui`
                    // expands to fill the viewport, so its `min_rect` reports
                    // the window height (900) and says nothing about the rows.
                    // A scope's response rect is its child `Ui`'s `min_rect`,
                    // which is exactly the two rows and the gap between them
                    // — and unlike a cursor delta it does not include the
                    // trailing `item_spacing.y` that follows the last widget.
                    let content = ui
                        .scope(|ui| {
                            // `ide_mode: true` — the band's own call. These two
                            // calls must stay in step with the panel's closure:
                            // a row rendered there and not here measures short.
                            let _ = render_editor_header_row(ui, &mut cache, &info, true);
                            render_editor_header_description(ui, info.description.as_deref());
                        })
                        .response
                        .rect;
                    // Content bounding box + the frame's own vertical margins is
                    // what the panel has to be tall enough to hold.
                    measured = content.height()
                        + IDE_HEADER_MARGIN.top
                        + IDE_HEADER_MARGIN.bottom;
                });
            });
            assert_eq!(
                measured, IDE_HEADER_H,
                "mono={mono}: the header band lays out {measured}pt of content but \
                 IDE_HEADER_H is {IDE_HEADER_H}pt \u{2014} under-counting paints a \
                 black bar under the separator, over-counting wastes band height. \
                 Re-derive the sum in IDE_HEADER_H's doc comment.",
            );
        }
    }

    #[test]
    fn selection_tint_reproduces_sel_selbd_for_accent() {
        // theme::SEL/SELBD are ACCENT hand-run through the same formula
        // selection_tint now exposes generically (see the doc comment on
        // both) — this pins that equivalence so a future edit to either the
        // consts or the helper can't silently drift the two apart, which
        // would make the IDE tab (still painted via ACCENT) look different
        // from before this change even though nothing about it was supposed
        // to move.
        assert_eq!(theme::selection_tint(theme::ACCENT), (theme::SEL, theme::SELBD));
    }

    #[test]
    fn entry_title_falls_back_to_numbered_placeholder() {
        assert_eq!(entry_title("PROTON_LOG", "Variable", 0), "PROTON_LOG");
        // Blank / whitespace-only entries still get a header (a card's title row
        // anchors its action cluster, so it must never be empty).
        assert_eq!(entry_title("", "Variable", 0), "Variable 1");
        assert_eq!(entry_title("   ", "Section", 2), "Section 3");
        // Surrounding whitespace is trimmed off a real title.
        assert_eq!(entry_title("  Display  ", "Section", 0), "Display");
    }

    #[test]
    fn parse_env_row_splits_on_first_equals() {
        assert_eq!(parse_env_row("FOO=a=b"), ("FOO".to_string(), "a=b".to_string()));
        assert_eq!(parse_env_row("FOO=bar"), ("FOO".to_string(), "bar".to_string()));
        // No `=` at all: whole string is the name, value is empty.
        assert_eq!(parse_env_row("FOO"), ("FOO".to_string(), String::new()));
    }

    #[test]
    fn is_valid_env_name_enforces_posix_charset() {
        assert!(is_valid_env_name("FOO"));
        assert!(is_valid_env_name("_FOO_bar9"));
        // Empty name, leading digit, and non-alnum chars are all invalid.
        assert!(!is_valid_env_name(""));
        assert!(!is_valid_env_name("1BAD"));
        assert!(!is_valid_env_name("FOO-BAR"));
        assert!(!is_valid_env_name("FOO BAR"));
    }

    #[test]
    fn env_row_name_validity_gates_persistence() {
        // Mirrors the filter `render_env_pair_field` applies before persisting:
        // an empty or invalid name means the row is dropped.
        let rows = ["=x", "1BAD=x", "", "GOOD=x"];
        let kept: Vec<&str> = rows
            .iter()
            .filter(|r| {
                let (n, _) = parse_env_row(r);
                let n = n.trim();
                !n.is_empty() && is_valid_env_name(n)
            })
            .copied()
            .collect();
        assert_eq!(kept, vec!["GOOD=x"]);
    }

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

    /// Build a draft from an on-disk manifest JSON, mirroring `ensure_draft`.
    fn draft_from(manifest: Value, editable: bool) -> ModuleDraft {
        let spec: Extension = serde_json::from_value(manifest).unwrap();
        let baseline = serde_json::to_value(&spec).unwrap();
        // Ordered walk then set, exactly as `ensure_draft` does (issue #31) — the
        // helper is only useful while it mirrors production.
        let baseline_var_order: Vec<String> =
            spec.ui.values().flatten().map(|f| f.variable.clone()).collect();
        let baseline_vars: std::collections::HashSet<String> =
            baseline_var_order.iter().cloned().collect();
        let sections = spec.ui.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let mut ext = spec;
        ext.ui.clear();
        let identity = PendingIdentity {
            author: ext.meta.author.clone(),
            name: ext.meta.name.clone(),
            var_edits: baseline_var_order
                .iter()
                .map(|v| (v.clone(), v.clone()))
                .collect(),
        };
        ModuleDraft {
            id: ext.id(),
            manifest: PathBuf::from("/nonexistent/mod.json"),
            editable,
            ext,
            sections,
            baseline,
            baseline_vars,
            name_error: None,
            identity,
            identity_error: None,
        }
    }

    fn sample_manifest() -> Value {
        json!({
            "Extension": {"Name": "Sample", "Author": "Ritze", "Version": "1.0"},
            "UI": {"Main": [{"Type": "toggle", "Variable": "enabled"}]}
        })
    }

    #[test]
    fn save_gate_predicate_requires_all_conditions() {
        // All four must hold for Save to enable.
        assert!(save_gate(true, true, true, false));
        assert!(!save_gate(false, true, true, false)); // not dirty
        assert!(!save_gate(true, false, true, false)); // invalid
        assert!(!save_gate(true, true, false, false)); // a Requires won't parse
        assert!(!save_gate(true, true, true, true)); // name collision
    }

    #[test]
    fn clean_draft_is_not_saveable_and_is_not_dirty() {
        let draft = draft_from(sample_manifest(), true);
        // A freshly-loaded draft equals disk: not dirty → Save disabled. This used
        // to also assert `!config_autosave_held(draft.dirty())` — that predicate
        // and the interlock it fed were deleted in S4b (`persist()` is now a
        // no-op under `Mode::Ide` instead), but "a clean draft reports clean" is
        // still worth asserting on its own.
        assert!(!draft.dirty());
        assert!(!draft.save_enabled());
    }

    #[test]
    fn valid_dirty_editable_draft_is_saveable_and_reverts_to_clean() {
        let mut draft = draft_from(sample_manifest(), true);
        draft.ext.meta.description = Some("now edited".to_string());
        assert!(draft.dirty());
        assert!(draft.save_enabled());
        // …and reports clean again once the edit is reverted to match disk. This
        // used to also assert the (now-deleted) `config_autosave_held` predicate
        // engaging/releasing across the same transition — see the note on
        // `clean_draft_is_not_saveable_and_is_not_dirty`.
        draft.ext.meta.description = None;
        assert!(!draft.dirty());
    }

    #[test]
    fn bundled_module_is_never_saveable() {
        let mut draft = draft_from(sample_manifest(), false); // not editable
        draft.ext.meta.description = Some("edited".to_string());
        assert!(draft.dirty());
        assert!(!draft.save_enabled(), "bundled modules must not save (Fork to edit)");
    }

    #[test]
    fn invalid_draft_write_is_refused() {
        // Add a field with an empty Variable → `validate` fails → Save refused.
        let mut draft = draft_from(sample_manifest(), true);
        draft.sections[0].1.push(new_field(String::new()));
        assert!(draft.dirty());
        assert!(!core_extension::validate(&draft.snapshot()).is_ok());
        assert!(!draft.save_enabled(), "invalid manifest must block Save");
    }

    #[test]
    fn unparseable_requires_blocks_save() {
        let mut draft = draft_from(sample_manifest(), true);
        // A trailing operator never parses.
        draft.sections[0].1[0].requires = Some("enabled AND".to_string());
        assert!(!all_requires_parse(&draft.snapshot()));
        assert!(!draft.save_enabled(), "a bad Requires must block Save");
    }

    #[test]
    fn newly_added_field_variable_is_editable_existing_is_locked() {
        let draft = draft_from(sample_manifest(), true);
        // The on-disk field is locked (its variable is in the baseline set)…
        assert!(draft.baseline_vars.contains("enabled"));
        // …while a brand-new variable is not, so its Variable stays editable.
        assert!(!draft.baseline_vars.contains("new_var"));
    }

    fn renames_of(pairs: &[(&str, &str)]) -> IndexMap<String, String> {
        pairs.iter().map(|(o, n)| (o.to_string(), n.to_string())).collect()
    }

    #[test]
    fn chained_rename_is_rejected_but_free_rename_is_accepted() {
        let current = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        // Free rename to a fresh name → accepted.
        assert!(validate_var_renames(&current, &renames_of(&[("a", "z")])).is_ok());
        // Chained rename: a → b while b is still a live variable → rejected.
        assert!(validate_var_renames(&current, &renames_of(&[("a", "b")])).is_err());
        // Swap (a→b, b→a) is a chain in both directions → rejected.
        assert!(validate_var_renames(&current, &renames_of(&[("a", "b"), ("b", "a")])).is_err());
        // Two vars renamed to the same new name → rejected.
        assert!(validate_var_renames(&current, &renames_of(&[("a", "x"), ("b", "x")])).is_err());
        // Rename to empty → rejected.
        assert!(validate_var_renames(&current, &renames_of(&[("a", "")])).is_err());
        // No renames → trivially ok.
        assert!(validate_var_renames(&current, &IndexMap::new()).is_ok());
    }

    /// Regression (2026-07-19): surrounding whitespace in the staged identity is
    /// **not** a rename. Pre-fix `has_pending_identity` compared the raw buffers,
    /// so typing one trailing space made the module report a pending rename and
    /// unsaved work — invisible on screen, and it lit the Rename button, the
    /// `(rename pending)` note, the diagnostics warning and the discard prompt.
    ///
    /// Verified to bite: reverting `has_pending_identity` to the raw
    /// `self.identity.author != self.ext.meta.author` comparison fails this test.
    #[test]
    fn whitespace_only_identity_edit_is_not_a_pending_rename() {
        let mut draft = draft_from(sample_manifest(), true);
        assert!(!draft.has_pending_identity());

        // Pad every part of the identity with whitespace the user cannot see.
        draft.identity.author = "  Ritze ".to_string();
        draft.identity.name = "\tSample  ".to_string();
        *draft.identity.var_edits.get_mut("enabled").unwrap() = " enabled ".to_string();

        assert!(!draft.has_pending_identity(), "whitespace is not a rename");
        assert!(draft.changed_var_renames().is_empty());
        assert!(!draft.has_unsaved_work(), "and so is not unsaved work");
        assert!(draft.compute_identity_error(false).is_none());

        // A real edit still registers — the trim must not swallow genuine renames,
        // and interior spaces are part of the name, not padding.
        draft.identity.name = " Sample Two ".to_string();
        assert!(draft.has_pending_identity());
        assert_eq!(draft.identity.staged_name(), "Sample Two");
    }

    /// Regression (2026-07-19): a manifest that already carries untrimmed
    /// identity — hand-edited, or written by a build before the trimming
    /// (de)serializer — must load **clean**, not as a permanently pending rename.
    ///
    /// Verified to bite: dropping `deserialize_with = "de_trimmed"` from
    /// `ExtensionMeta`/`UiField` fails this test.
    #[test]
    fn untrimmed_manifest_loads_trimmed_and_presents_as_clean() {
        let draft = draft_from(
            json!({
                "Extension": {"Name": " Sample ", "Author": "Ritze\t", "Version": "1.0"},
                "UI": {"Main": [{"Type": "toggle", "Variable": " enabled "}]}
            }),
            true,
        );
        // Load boundary: the padding never reached memory.
        assert_eq!(draft.ext.meta.name, "Sample");
        assert_eq!(draft.ext.meta.author, "Ritze");
        assert_eq!(draft.sections[0].1[0].variable, "enabled");
        // …so the draft matches itself and nothing is pending.
        assert!(!draft.dirty());
        assert!(!draft.has_pending_identity());
        assert!(!draft.has_unsaved_work());
    }

    /// Regression (2026-07-19): the **save** boundary. Even if an untrimmed value
    /// is planted directly into the in-memory struct (bypassing the load-side
    /// trim), serializing the snapshot must write it trimmed — so a save can
    /// never re-introduce the invisible character that broke identity matching.
    ///
    /// Verified to bite: dropping `serialize_with = "ser_trimmed"` fails this test.
    #[test]
    fn saving_writes_trimmed_identity_even_if_memory_is_untrimmed() {
        let mut draft = draft_from(sample_manifest(), true);
        draft.ext.meta.author = " Ritze ".to_string();
        draft.ext.meta.name = "Sample\n".to_string();
        draft.sections[0].1[0].variable = "  enabled  ".to_string();
        draft.sections[0].0 = "  Main  ".to_string(); // section name too

        let written = serde_json::to_value(draft.snapshot()).unwrap();
        assert_eq!(written["Extension"]["Author"], json!("Ritze"));
        assert_eq!(written["Extension"]["Name"], json!("Sample"));
        assert_eq!(written["UI"]["Main"][0]["Variable"], json!("enabled"));
        // Section name folded trimmed: the key is "Main", not "  Main  ".
        assert!(written["UI"].get("Main").is_some());
    }

    #[test]
    fn staged_identity_edits_do_not_touch_the_save_path() {
        let mut draft = draft_from(sample_manifest(), true);
        // No pending change on a fresh draft.
        assert!(!draft.has_pending_identity());
        // Staging an Author/Name/Variable change must NOT make the draft dirty or
        // saveable — it travels the Rename path only, never Save/autosave.
        draft.identity.author = "Someone".to_string();
        draft.identity.name = "Renamed".to_string();
        *draft.identity.var_edits.get_mut("enabled").unwrap() = "on".to_string();
        assert!(draft.has_pending_identity());
        assert!(!draft.dirty(), "identity edits must not dirty the snapshot");
        assert!(!draft.save_enabled(), "identity edits must not enable Save");
        // The staged variable rename surfaces as an old→new entry…
        assert_eq!(
            draft.changed_var_renames(),
            renames_of(&[("enabled", "on")])
        );
        // …and a non-colliding identity change is committable.
        assert!(draft.compute_identity_error(false).is_none());
        // A colliding (Author, Name) blocks it.
        assert!(draft.compute_identity_error(true).is_some());
    }

    fn spec_named(author: &str, name: &str, version: &str) -> Extension {
        serde_json::from_value(json!({
            "Extension": {"Name": name, "Author": author, "Version": version},
            "UI": {"Main": [{"Type": "toggle", "Variable": "enabled"}]}
        }))
        .unwrap()
    }

    #[test]
    fn name_collision_is_version_blind_and_excludes_self() {
        // Two loaded modules; the second shares (Author,Name) with a candidate but
        // differs in Version — still a collision (config keys on Author+Name).
        let specs = vec![
            spec_named("Ritze", "Alpha", "1.0"),
            spec_named("Ritze", "Beta", "2.0"),
        ];
        let manifests = vec![PathBuf::from("/a/alpha.json"), PathBuf::from("/a/beta.json")];

        // Same Author+Name, different Version → collides.
        assert!(name_collides(&specs, &manifests, "Ritze", "Beta", None));
        // Distinct Name → free.
        assert!(!name_collides(&specs, &manifests, "Ritze", "Gamma", None));
        // Excluding the very module by manifest path clears the self-collision.
        assert!(!name_collides(
            &specs,
            &manifests,
            "Ritze",
            "Beta",
            Some(Path::new("/a/beta.json")),
        ));
        // Excluding a *different* path still reports the collision.
        assert!(name_collides(
            &specs,
            &manifests,
            "Ritze",
            "Beta",
            Some(Path::new("/a/alpha.json")),
        ));
    }

    #[test]
    fn slug_uniquify_suffixes_on_clash() {
        // The base is free → used as-is.
        let taken: std::collections::HashSet<String> = ["ritze__mod.json".to_string()]
            .into_iter()
            .collect();
        assert_eq!(uniquify_slug("ritze__other", |c| taken.contains(c)), "ritze__other.json");
        // The base is taken → first free suffix is `-2`.
        assert_eq!(uniquify_slug("ritze__mod", |c| taken.contains(c)), "ritze__mod-2.json");
        // `-2` also taken → `-3`.
        let taken2: std::collections::HashSet<String> =
            ["ritze__mod.json".to_string(), "ritze__mod-2.json".to_string()]
                .into_iter()
                .collect();
        assert_eq!(uniquify_slug("ritze__mod", |c| taken2.contains(c)), "ritze__mod-3.json");
    }

    #[test]
    fn sanitize_slug_maps_non_alnum_and_guards_empty() {
        assert_eq!(sanitize_slug("My Module!"), "my_module");
        assert_eq!(sanitize_slug("LSFG-VK"), "lsfg_vk");
        assert_eq!(sanitize_slug("  "), "module");
    }

    #[test]
    fn deferred_reorder_and_add_apply_without_mid_iteration_mutation() {
        let mut draft = draft_from(sample_manifest(), true);
        apply_deferred(&mut draft, Deferred::SectionAdd);
        assert_eq!(draft.sections.len(), 2);
        apply_deferred(&mut draft, Deferred::Section(0, RowAction::Down));
        assert_eq!(draft.sections[1].0, "Main");
        apply_deferred(&mut draft, Deferred::FieldAdd(1));
        assert_eq!(draft.sections[1].1.len(), 2);
    }

    /// A minimal but fully real `GuiApp` — every field is an inert placeholder,
    /// built by hand (not through `GuiApp::new`) because that constructor needs a
    /// live `AppContext`: extensions actually present on disk, a real `Paths`
    /// root it reads `general.json`/`global.json` through, a game list from a
    /// directory scan. None of that exists in a unit test, and `Paths` itself
    /// needs nothing but a `PathBuf` to construct (`pub base: PathBuf`, no I/O in
    /// the constructor), so a direct struct literal is the smallest honest way to
    /// get a `GuiApp` whose `set_scoped` / `unset_scoped` are the *real* methods
    /// under test, not a reimplementation of their routing.
    ///
    /// *Why this is the seam, not a bare pure-function extraction:* the write
    /// routing this fix touches (`with_preview_writes`, `set_scoped`,
    /// `unset_scoped`) already lives on `&mut GuiApp` and reads/writes several of
    /// its fields (`write_target`, `nav_sel`, `game_config`, `preview_config`,
    /// `editing_preset_buf`, `global_config`) — extracting a "pure" stand-in
    /// function would just be a second implementation of the routing that could
    /// drift from the real one and pass even if the real one regressed. Building
    /// the real struct, tedious as the literal is, is what makes the test
    /// actually exercise the code path the fix changed.
    fn test_app() -> GuiApp {
        GuiApp {
            paths: Paths { base: PathBuf::from("/nonexistent/ritz-gui-test") },
            default_preset: None,
            all_specs: Vec::new(),
            all_dirs: Vec::new(),
            all_manifests: Vec::new(),
            all_is_folder_ext: Vec::new(),
            extension_errors: Vec::new(),
            cur_specs: Vec::new(),
            cur_dirs: Vec::new(),
            cur_is_folder_ext: Vec::new(),
            cur_specs_filter: None,
            group_by_author: true,
            show_inheritance: true,
            games: Vec::new(),
            appid: "42".to_string(),
            game_command: Vec::new(),
            selected_ext: 0,
            game_config: GameConfig::new("42", "Test Game"),
            preset: None,
            text_buffers: HashMap::new(),
            multi_edit: HashMap::new(),
            launch_mode: false,
            outcome: Arc::new(Mutex::new(EditOutcome::Continue)),
            detect: None,
            nav_sel: NavSel::Game("42".to_string()),
            mode: Mode::Config,
            focused_module: None,
            write_target: WriteTarget::Scope,
            preview_config: GameConfig::new("__preview__", "None"),
            preview_preset: None,
            preview_preset_chain: Vec::new(),
            preview_game: None,
            ide_selected: 0,
            all_presets: Vec::new(),
            preset_pins: HashMap::new(),
            general_config: GeneralConfig::default(),
            global_config: Preset::default(),
            editing_preset_buf: None,
            nav_name_buf: String::new(),
            nav_appid_buf: String::new(),
            creating_profile: false,
            duplicating_preset: None,
            creating_game: false,
            focus_nav_name: false,
            confirm: None,
            logo: None,
            drafts: IndexMap::new(),
            unsaved_drafts: std::collections::HashSet::new(),
            pending_close: false,
            module_dialog: None,
            delete_module_purge: false,
            carryover_report: None,
            icon_cache: IconCenterCache::new(),
        }
    }

    /// The regression `set_scoped`/`unset_scoped` need most: with
    /// `write_target == Preview`, a write must land in `preview_config` and
    /// leave `game_config` byte-for-byte alone — the property guard #3's
    /// `debug_assert_eq!` at the preview render site checks by snapshot-diffing
    /// around a whole render call. This test checks the same property directly
    /// against the two routing functions, so it fails in `--release` builds too
    /// (where `debug_assert_eq!` compiles out) and fails immediately at the
    /// write, not only when a future refactor happens to touch the render site.
    ///
    /// Does NOT cover: `with_preview_writes` restoring `write_target` (that's
    /// `Fix 1`, exercised implicitly by every real call but not asserted here in
    /// isolation), `poll_detect`'s save/restore dance, or `buffer_scope_tag`'s
    /// namespacing of `text_buffers`/`multi_edit` keys. Those are separate call
    /// paths from the ones this test drives directly.
    #[test]
    fn preview_write_target_routes_to_preview_config_not_game_config() {
        let mut app = test_app();
        assert_eq!(app.game_config.get_value("Ritze", "Core", "kbd_layout"), None);
        assert_eq!(app.preview_config.get_value("Ritze", "Core", "kbd_layout"), None);

        app.write_target = WriteTarget::Preview;
        app.set_scoped("Ritze", "Core", "kbd_layout", json!("de"));

        // Landed in the scratch layer...
        assert_eq!(
            app.preview_config.get_value("Ritze", "Core", "kbd_layout"),
            Some(&json!("de"))
        );
        // ...and the real game config — the one `persist()` would eventually
        // write to `games/42.json` — is untouched.
        assert_eq!(app.game_config.get_value("Ritze", "Core", "kbd_layout"), None);

        // unset_scoped is the same story in reverse.
        app.unset_scoped("Ritze", "Core", "kbd_layout");
        assert_eq!(app.preview_config.get_value("Ritze", "Core", "kbd_layout"), None);
        assert_eq!(app.game_config.get_value("Ritze", "Core", "kbd_layout"), None);
    }

    /// The converse of the test above: with `write_target == Scope` (the default,
    /// and what every Config-mode edit still uses), a write must reach
    /// `game_config` exactly as it did before the preview feature existed, and
    /// must never leak into `preview_config`. Pinning this alongside the Preview
    /// case is what makes the pair a genuine regression test for the *routing
    /// decision*, not just for one branch of it — a change that made both
    /// branches write the same place would fail one of these two tests.
    #[test]
    fn scope_write_target_routes_to_game_config_as_before() {
        let mut app = test_app();
        assert_eq!(app.write_target, WriteTarget::Scope);
        assert_eq!(app.nav_sel, NavSel::Game("42".to_string()));

        app.set_scoped("Ritze", "Core", "kbd_layout", json!("de"));
        assert_eq!(
            app.game_config.get_value("Ritze", "Core", "kbd_layout"),
            Some(&json!("de"))
        );
        assert_eq!(app.preview_config.get_value("Ritze", "Core", "kbd_layout"), None);

        app.unset_scoped("Ritze", "Core", "kbd_layout");
        assert_eq!(app.game_config.get_value("Ritze", "Core", "kbd_layout"), None);
    }

    /// **Guard: [`GuiApp::buffer_scope_tag`]'s namespacing.** A preview text edit
    /// and a Config-mode edit of the *same* module variable must not share a
    /// `text_buffers` / `multi_edit` key.
    ///
    /// *Why the collision matters and why it is not hypothetical* (issue #16):
    /// before S5a the keys were `"{spec.id()}::{var}"` with no scope tag at all,
    /// and both `multi_edit` tag sites mapped the old `NavSel::ModuleEditor(_)`
    /// onto `"game:{appid}"` — so the IDE preview and Config mode's Game view
    /// computed byte-identical keys for one module/variable while resolving
    /// against two different bases. That was inert only while the preview was
    /// read-only; making its fields editable arms it. The same string also mints
    /// an **absolute** `egui::Id` in the `String` field arm
    /// (`egui::Id::new(("text_edit", &key))`), which `push_id` does not
    /// namespace — so a collision is not merely a shared buffer, it is a shared
    /// widget identity.
    ///
    /// Asserting the tags differ is the weak half; the test also composes the
    /// real key shape (the one `poll_detect` and `render_value_editor` build) and
    /// asserts *those* differ, because the key is what actually indexes the map.
    #[test]
    fn buffer_scope_tag_namespaces_preview_away_from_every_real_scope() {
        let mut app = test_app();

        // The same module and variable, addressed from the preview and from each
        // real scope in turn. `nav_sel` is held constant across the first pair on
        // purpose: `write_target` alone has to be enough to separate them, since
        // the preview renders under whatever `nav_sel` the user is on.
        app.write_target = WriteTarget::Preview;
        let preview_tag = app.buffer_scope_tag();
        assert_eq!(preview_tag, "preview");

        app.write_target = WriteTarget::Scope;
        assert_eq!(app.nav_sel, NavSel::Game("42".to_string()));
        let game_tag = app.buffer_scope_tag();
        assert_eq!(game_tag, "game:42");
        assert_ne!(
            preview_tag, game_tag,
            "the preview and Config's Game view share a buffer namespace",
        );

        // The composed key is what indexes `text_buffers` / `multi_edit` and what
        // seeds the absolute `egui::Id` — the tags differing is only useful if it
        // survives into the key.
        let key = |tag: &str| format!("{tag}::Ritze::Core::1.0::kbd_layout");
        assert_ne!(key(&preview_tag), key(&game_tag));

        // Every other real scope is distinct from the preview too, and from each
        // other — a tag that collapsed two real scopes would be the same class of
        // bug one layer down.
        let mut tags = vec![preview_tag.clone()];
        for nav in [
            NavSel::GlobalSettings,
            NavSel::Profile("Handheld".to_string()),
            NavSel::Game("42".to_string()),
            NavSel::GeneralSettings,
        ] {
            app.nav_sel = nav;
            let tag = app.buffer_scope_tag();
            assert_ne!(tag, preview_tag, "a real scope collided with the preview");
            tags.push(tag);
        }
        let unique: std::collections::HashSet<_> = tags.iter().collect();
        assert_eq!(unique.len(), tags.len(), "two scopes share a tag: {tags:?}");

        // And the preview tag does not depend on `nav_sel` — the early return has
        // to win over every arm of the match below it, or the preview's namespace
        // would drift with whatever screen the user happened to be on.
        for nav in [
            NavSel::GlobalSettings,
            NavSel::Profile("Handheld".to_string()),
            NavSel::Game("999".to_string()),
            NavSel::GeneralSettings,
        ] {
            app.nav_sel = nav;
            app.write_target = WriteTarget::Preview;
            assert_eq!(app.buffer_scope_tag(), "preview");
        }
    }

    /// **Guard: [`GuiApp::with_preview_writes`]' restore.** `write_target` returns
    /// to the value it had on entry — including when the closure returns early,
    /// and including when that value was already `Preview`.
    ///
    /// *Why the early-return case is the point* (issue #16): the restore lives
    /// after `f(self)` *inside the wrapper*, so a `return` in the caller's
    /// closure body returns into this function and cannot skip it. That is the
    /// entire reason this is a closure wrapper rather than a bare
    /// `self.write_target = …` around the call site —
    /// `render_module_settings_body` and the `push_id` block that wraps it both
    /// contain early returns, and a trailing assignment would be bypassed by
    /// adding one more. A stuck `Preview` is the worst available failure: the
    /// *next* Config-mode edit would silently land in the scratch layer and be
    /// thrown away.
    ///
    /// *Why the already-`Preview` case is tested* even though no caller nests
    /// today: it is what distinguishes save-and-restore from a hardcoded reset to
    /// `Scope`, which the doc comment flags as a landmine for a future nested
    /// call. The two are indistinguishable on the single call site that exists,
    /// so only this test can hold the line.
    #[test]
    fn with_preview_writes_restores_the_previous_target_even_on_early_return() {
        let mut app = test_app();
        assert_eq!(app.write_target, WriteTarget::Scope);

        // 1. Straight-line closure, entered from `Scope`.
        let seen = app.with_preview_writes(|app| app.write_target);
        assert_eq!(seen, WriteTarget::Preview, "the closure did not see Preview");
        assert_eq!(app.write_target, WriteTarget::Scope, "target left flipped");

        // 2. Closure that returns early. The condition is read out of app state
        // rather than written `if true`, so the compiler cannot fold the branch
        // away and turn this into case 1 with extra steps.
        let bail = app.appid == "42";
        let out = app.with_preview_writes(|app| {
            app.set_scoped("Ritze", "Core", "kbd_layout", json!("de"));
            if bail {
                return "early";
            }
            app.set_scoped("Ritze", "Core", "other", json!("x"));
            "late"
        });
        assert_eq!(out, "early", "the early return did not happen");
        assert_eq!(
            app.write_target,
            WriteTarget::Scope,
            "an early return skipped the restore \u{2014} the next Config-mode edit \
             would silently land in the scratch layer",
        );
        // The write inside the closure went where the flag said, not where the
        // restore later put it back to.
        assert_eq!(
            app.preview_config.get_value("Ritze", "Core", "kbd_layout"),
            Some(&json!("de"))
        );
        assert_eq!(app.game_config.get_value("Ritze", "Core", "kbd_layout"), None);

        // 3. Entered from `Preview` (the nested case). A hardcoded `= Scope`
        // restore passes cases 1 and 2 and fails only here — which is exactly the
        // silent regression this test exists to catch.
        app.write_target = WriteTarget::Preview;
        app.with_preview_writes(|app| {
            assert_eq!(app.write_target, WriteTarget::Preview);
        });
        assert_eq!(
            app.write_target,
            WriteTarget::Preview,
            "a nested call disarmed the enclosing one's Preview target \u{2014} the \
             rest of the outer closure's writes would reach the real config",
        );

        // 4. Nested for real, two deep, from `Scope`: unwinding must walk back
        // Preview → Preview → Scope, not collapse to Scope at the first return.
        app.write_target = WriteTarget::Scope;
        app.with_preview_writes(|app| {
            app.with_preview_writes(|app| {
                assert_eq!(app.write_target, WriteTarget::Preview);
            });
            assert_eq!(app.write_target, WriteTarget::Preview, "inner return widened");
        });
        assert_eq!(app.write_target, WriteTarget::Scope);
    }

    /// **Guard: [`GuiApp::poll_detect`]'s save/restore.** A detection started in
    /// the IDE preview must land in the scratch layer under the `"preview"`
    /// buffer tag, restore `write_target` afterwards, and report `changed =
    /// false` so no `persist()` fires for it.
    ///
    /// *Why this is the guard with the widest blast radius* (issue #16):
    /// `poll_detect` runs at the **tail** of the frame, long after
    /// `with_preview_writes` has put `write_target` back to `Scope`. So it is the
    /// one write path that happens *outside* the render call the other guards
    /// wrap — and making the preview's fields editable also makes its "Detect
    /// window class" button live. Without the captured `Detect::target` being
    /// re-established around the write, a preview detection would go straight to
    /// the real `games/<appid>.json`, bypassing every other guard in the set.
    ///
    /// Three properties, all checked in both directions (`Preview` and `Scope`):
    /// where the value lands, which buffer key it lands under, and whether
    /// `changed` is reported. The `changed` half matters on its own: it feeds
    /// `ui()`'s `changed`, which calls `persist()`. `persist()` has its own
    /// `Mode::Ide` no-op, but relying on that would make this function's
    /// correctness depend on a guard in another function — and `Mode::Config`
    /// with a `Preview` target is reachable state.
    #[test]
    fn poll_detect_restores_the_captured_target_and_reports_changed_only_for_scope() {
        // A finished detection for `var`, started under `target`.
        fn finished_detect(target: WriteTarget, nav: NavSel, mode: Mode, class: &str) -> Detect {
            Detect {
                result: Arc::new(Mutex::new(DetectStatus::Done(Some(class.to_string())))),
                start: Instant::now(),
                ext_id: "Ritze::Core::1.0".to_string(),
                author: "Ritze".to_string(),
                name: "Core".to_string(),
                var: "wm_class".to_string(),
                nav,
                target,
                mode,
            }
        }
        // `poll_detect` only ever calls `request_repaint` / `request_repaint_after`
        // on this, so a bare context is enough — no window, no painter.
        let ctx = egui::Context::default();

        // ── Preview: scratch layer, "preview" key, no persist ────────────────
        let mut app = test_app();
        let nav = app.nav_sel.clone();
        app.detect = Some(finished_detect(WriteTarget::Preview, nav.clone(), app.mode, "gamescope"));
        let changed = app.poll_detect(&ctx);

        assert!(
            !changed,
            "a preview detection reported changed \u{2014} that feeds persist(), which \
             would save a scratch value to disk",
        );
        assert_eq!(
            app.preview_config.get_value("Ritze", "Core", "wm_class"),
            Some(&json!("gamescope")),
        );
        assert_eq!(
            app.game_config.get_value("Ritze", "Core", "wm_class"),
            None,
            "a preview detection reached the real game config",
        );
        // The text buffer is keyed with the preview tag, not the ambient game's —
        // written under the wrong tag it would be a value nothing ever reads back.
        assert_eq!(
            app.text_buffers.get("preview::Ritze::Core::1.0::wm_class"),
            Some(&"gamescope".to_string()),
        );
        assert!(!app.text_buffers.contains_key("game:42::Ritze::Core::1.0::wm_class"));
        // Restored, so nothing downstream in the same frame sees a flipped flag.
        assert_eq!(
            app.write_target,
            WriteTarget::Scope,
            "poll_detect left write_target on Preview",
        );
        assert!(app.detect.is_none(), "the finished detection was not cleared");

        // ── Scope: real config, scoped key, persist ──────────────────────────
        let mut app = test_app();
        let nav = app.nav_sel.clone();
        app.detect = Some(finished_detect(WriteTarget::Scope, nav, app.mode, "steam_app_42"));
        let changed = app.poll_detect(&ctx);

        assert!(changed, "a real detection did not report changed, so it never persists");
        assert_eq!(
            app.game_config.get_value("Ritze", "Core", "wm_class"),
            Some(&json!("steam_app_42")),
        );
        assert_eq!(app.preview_config.get_value("Ritze", "Core", "wm_class"), None);
        assert_eq!(
            app.text_buffers.get("game:42::Ritze::Core::1.0::wm_class"),
            Some(&"steam_app_42".to_string()),
        );
        assert_eq!(app.write_target, WriteTarget::Scope);
    }

    /// A detection in flight for `var` on the module `ext_id`, started under
    /// `target`/`mode`. `result` is supplied so a test can hand in a poisoned one.
    fn pending_detect(
        result: Arc<Mutex<DetectStatus>>,
        ext_id: &str,
        var: &str,
        nav: NavSel,
        target: WriteTarget,
        mode: Mode,
    ) -> Detect {
        Detect {
            result,
            start: Instant::now(),
            ext_id: ext_id.to_string(),
            author: "Ritze".to_string(),
            name: "HyprMonctl".to_string(),
            var: var.to_string(),
            nav,
            target,
            mode,
        }
    }

    /// **Issue #2.** The "Detecting… Ns" countdown belongs to the module+field the
    /// detection was actually started on — not to every field that happens to be
    /// named `window_class`.
    ///
    /// *Why this is worth a test given only one shipped module declares
    /// `window_class`:* the second declarer does not have to ship with ritz.
    /// Users drop their own modules into `~/.config/ritz/extensions/`, and the old
    /// gate ("some detection is waiting" AND "this variable is called
    /// window_class") would then draw hypr-monctl's countdown on the user's field
    /// while `poll_detect` wrote the detected value to hypr-monctl — the user
    /// watches one module and a different one changes.
    #[test]
    fn detect_countdown_is_scoped_to_the_module_and_field_that_started_it() {
        let mut app = test_app();
        let nav = app.nav_sel.clone();
        app.detect = Some(pending_detect(
            Arc::new(Mutex::new(DetectStatus::Waiting)),
            "Ritze::HyprMonctl::1.0",
            "window_class",
            nav,
            WriteTarget::Scope,
            Mode::Config,
        ));

        assert!(
            app.detect_countdown_secs("Ritze::HyprMonctl::1.0", "window_class").is_some(),
            "the field that started the detection lost its own countdown",
        );
        assert_eq!(
            app.detect_countdown_secs("Someone::MyMonitors::1.0", "window_class"),
            None,
            "a second module declaring window_class drew another module's countdown",
        );
        assert_eq!(
            app.detect_countdown_secs("Ritze::HyprMonctl::1.0", "some_other_var"),
            None,
            "the countdown leaked onto a different field of the same module",
        );

        // The same module+field drawn by the *other* surface. `write_target` and
        // `mode` are what tell Config mode's copy of a field from the IDE
        // preview's — `nav_sel` cannot, see `cancel_stale_detect`.
        app.write_target = WriteTarget::Preview;
        assert_eq!(
            app.detect_countdown_secs("Ritze::HyprMonctl::1.0", "window_class"),
            None,
            "a Config-mode detection drew its countdown in the IDE preview column",
        );
        app.write_target = WriteTarget::Scope;
        app.mode = Mode::Ide;
        assert_eq!(
            app.detect_countdown_secs("Ritze::HyprMonctl::1.0", "window_class"),
            None,
            "a detection started in Config mode drew its countdown under Mode::Ide",
        );
    }

    /// **Issue #3.** A panic inside the detector thread must not be able to take
    /// the settings window with it.
    ///
    /// `start_detect` no longer holds the guard across
    /// `detect_active_window_class`, so nothing *should* poison this mutex any
    /// more — but the GUI thread reads it every frame from two places, and
    /// `.unwrap()` on either would turn a failed `hyprctl` into a lost window and
    /// every unsaved draft in it. This pins the tolerance, not the structural fix:
    /// both readers must survive a poisoned lock and resolve it to "detection
    /// finished, found nothing".
    ///
    /// Expect one "thread panicked" line on stderr from the poisoning helper —
    /// that panic is the fixture, not a failure.
    #[test]
    fn a_poisoned_detect_mutex_does_not_take_down_the_gui_thread() {
        let result = Arc::new(Mutex::new(DetectStatus::Waiting));
        let handle = result.clone();
        let _ = std::thread::spawn(move || {
            let _guard = handle.lock().unwrap();
            panic!("hyprctl blew up while the guard was held");
        })
        .join();
        assert!(result.is_poisoned(), "the fixture did not actually poison the mutex");

        let ctx = egui::Context::default();
        let mut app = test_app();
        let nav = app.nav_sel.clone();
        app.detect = Some(pending_detect(
            result,
            "Ritze::HyprMonctl::1.0",
            "window_class",
            nav,
            WriteTarget::Scope,
            Mode::Config,
        ));

        // Render-side read: back to the plain Detect button, not a countdown stuck
        // at 0 with no way out (the poisoned slot still literally holds `Waiting`).
        assert_eq!(
            app.detect_countdown_secs("Ritze::HyprMonctl::1.0", "window_class"),
            None,
        );

        // Frame-tail read: completes as "found nothing", writes nothing, clears.
        let changed = app.poll_detect(&ctx);
        assert!(!changed, "a poisoned detection reported a config change");
        assert!(app.detect.is_none(), "the poisoned detection was never cleared");
        assert_eq!(
            app.game_config.get_value("Ritze", "HyprMonctl", "window_class"),
            None,
            "a poisoned detection wrote a value",
        );
    }

    /// **Issue #38.** Ctrl+R must not reload out from under an open confirmation,
    /// for the same reason Ctrl+S must not save under one: the modal's backdrop
    /// eats pointer input only.
    ///
    /// The asserted damage is the real one — `reload_configs` → `switch_game`
    /// clears `text_buffers` and `multi_edit`, and rows that have not yet reached
    /// `cleaned` (a blank multi_string row, an env pair with an empty NAME) live
    /// *only* in `multi_edit`.
    #[test]
    fn ctrl_r_reload_is_refused_while_a_confirmation_is_open() {
        let mut app = test_app();
        app.text_buffers.insert("game:42::T::M::1::s".to_string(), "half-typed".to_string());
        app.multi_edit
            .insert("game:42::T::M::1::env".to_string(), vec!["".to_string()]);

        app.confirm = Some(ConfirmAction::SaveWithPendingRename);
        app.request_reload();
        assert!(
            app.text_buffers.contains_key("game:42::T::M::1::s"),
            "Ctrl+R reloaded under an open dialog and dropped an in-progress text buffer",
        );
        assert!(
            app.multi_edit.contains_key("game:42::T::M::1::env"),
            "Ctrl+R reloaded under an open dialog and dropped an uncommitted list row",
        );
        assert!(app.confirm.is_some(), "the reload attempt dismissed the dialog");

        // …and the guard is a guard, not a disablement: with no dialog up, the
        // same call does reload (and clears, which is correct then).
        app.confirm = None;
        app.request_reload();
        assert!(
            app.multi_edit.is_empty() && app.text_buffers.is_empty(),
            "with no dialog open, Ctrl+R did not reload at all",
        );
    }

    /// **Issue #8.** The IDE preview shades inheritance depth against its **own**
    /// preset chain.
    ///
    /// Three properties in one, because the bug and its original fix are two sides
    /// of the same branch:
    /// 1. a preview pointed at a game whose profile has a parent gets a real
    ///    chain, deepest-first — this is what was inert;
    /// 2. a preview pointed at nothing gets an empty chain, which is *correct* and
    ///    must stay that way (there is no chain to shade against);
    /// 3. the preview's chain is its own, not the one belonging to whatever
    ///    `nav_sel` still holds behind IDE mode — the property the `Preview` arm
    ///    was originally added for, and the one a naive "just delete the arm" fix
    ///    would regress.
    // ── Deleting a profile sweeps every reference to it (issue #36) ─────────
    //
    // `delete_profile` used to fix only `self.game_config` (the ambient game).
    // Every other reference — other games' `Preset`, other profiles' `Parent`,
    // the general-config `DefaultPreset` — was left dangling. Dangling degrades
    // gracefully (`load_preset` returns `Ok(None)`) right up until someone
    // creates a new profile with the same name, at which point the stale
    // reference silently re-attaches to a profile nobody assigned.

    /// Scratch config dir for the profile-reference tests, plus an app pointed at
    /// it whose ambient game is 42 (`test_app`'s default) — deliberately *not*
    /// the game that references the profile, which is the whole point of #36.
    fn preset_ref_app(tag: &str) -> (GuiApp, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "ritz-gui-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let mut app = test_app();
        app.paths = Paths { base: base.clone() };
        (app, base)
    }

    /// **Issue #36, the reported repro.** Assign profile `P` to game X, navigate
    /// away to game Y, delete `P` — `games/X.json` must not keep `"Preset": "P"`.
    ///
    /// Verified to fail before the fix: `delete_profile` only touched
    /// `self.game_config`, so this asserted `Some("P")` and failed on the first
    /// `assert_eq!`.
    #[test]
    fn deleting_a_profile_clears_it_from_every_game_not_just_the_ambient_one() {
        let (mut app, base) = preset_ref_app("del-profile-games");
        app.paths.save_preset(&Preset { name: "P".to_string(), ..Default::default() }).unwrap();
        let mut x = GameConfig::new("100", "Game X");
        x.config.modules.preset = Some("P".to_string());
        app.paths.save_game(&x).unwrap();
        // The ambient game is 42 — we are "standing on" a different game, exactly
        // as the issue's step 2 describes.
        assert_ne!(app.appid, "100");

        app.delete_profile("P");

        let x = app.paths.load_game("100").unwrap().unwrap();
        assert_eq!(
            x.config.modules.preset, None,
            "a game the user was not viewing must still lose the deleted profile"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// **The bite.** The dangling reference is only dormant: re-creating a
    /// profile with the same name must not silently re-attach the game that used
    /// to reference the deleted one.
    ///
    /// Verified to fail before the fix: `games/100.json` still held `"Preset":
    /// "P"`, so the freshly created, unrelated `P` resolved as game 100's
    /// profile and this asserted `Some("P")`.
    #[test]
    fn a_recreated_profile_name_does_not_reattach_to_the_game_that_used_it() {
        let (mut app, base) = preset_ref_app("del-profile-resurrect");
        app.paths.save_preset(&Preset { name: "P".to_string(), ..Default::default() }).unwrap();
        let mut x = GameConfig::new("100", "Game X");
        x.config.modules.preset = Some("P".to_string());
        app.paths.save_game(&x).unwrap();

        app.delete_profile("P");

        // A new, unrelated profile that merely happens to share the name.
        let mut reborn = Preset { name: "P".to_string(), ..Default::default() };
        reborn.set_value("Ritze", "Sample", "Enabled", json!(true));
        app.paths.save_preset(&reborn).unwrap();

        let x = app.paths.load_game("100").unwrap().unwrap();
        assert_eq!(
            x.config.modules.preset, None,
            "the game must stay unassigned — the name is a coincidence, not an assignment"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `Parent` is the second kind of reference, and it resurrects the same way —
    /// worse, in fact, since a stale parent injects a whole inherited layer under
    /// an existing profile. Both delete (clears it) and rename (re-points it) must
    /// sweep it.
    ///
    /// Verified to fail before the fix on **both** halves: `delete_profile` never
    /// looked at profiles at all, and `rename_preset`'s inline sweep covered only
    /// games and `DefaultPreset`, so the child kept `"Parent": "base"` in both.
    #[test]
    fn profile_parent_references_are_swept_on_delete_and_on_rename() {
        let (mut app, base) = preset_ref_app("preset-parent-sweep");
        app.paths.save_preset(&Preset { name: "base".to_string(), ..Default::default() }).unwrap();
        app.paths
            .save_preset(&Preset {
                name: "child".to_string(),
                parent: Some("base".to_string()),
                ..Default::default()
            })
            .unwrap();

        // Rename: the child must follow its parent to the new name.
        app.rename_preset("base", "renamed");
        let child = app.paths.load_preset("child").unwrap().unwrap();
        assert_eq!(
            child.parent.as_deref(),
            Some("renamed"),
            "a renamed parent must be re-pointed, not left dangling"
        );

        // Delete: the child must lose the parent entirely.
        app.delete_profile("renamed");
        let child = app.paths.load_preset("child").unwrap().unwrap();
        assert_eq!(
            child.parent, None,
            "a deleted parent must be cleared, or re-creating the name re-attaches it"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// The general-config `DefaultPreset` is the third reference. Rename already
    /// handled it; delete did not, so a deleted default came back the moment a
    /// profile of that name existed again.
    ///
    /// Verified to fail before the fix: `default_preset` stayed `Some("P")` both
    /// on disk and in `app.default_preset`.
    #[test]
    fn deleting_the_default_profile_clears_the_general_config_default() {
        let (mut app, base) = preset_ref_app("del-profile-default");
        app.paths.save_preset(&Preset { name: "P".to_string(), ..Default::default() }).unwrap();
        app.general_config.default_preset = Some("P".to_string());
        app.default_preset = Some("P".to_string());
        app.paths.save_general(&app.general_config).unwrap();

        app.delete_profile("P");

        assert_eq!(app.general_config.default_preset, None, "in memory");
        assert_eq!(app.default_preset, None, "and its mirror");
        assert_eq!(
            app.paths.load_general().unwrap().default_preset,
            None,
            "and on disk, or the next startup resurrects it"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Deleting the profile currently *open in the editor* must not have its file
    /// written straight back by the autosave that follows.
    ///
    /// Verified to fail before the fix: `delete_profile` called `persist()`, which
    /// under `NavSel::Profile(name)` runs `save_preset(editing_preset_buf)` — and
    /// `profiles/P.json` reappeared on disk immediately after being removed.
    #[test]
    fn deleting_the_profile_being_edited_does_not_resurrect_its_file() {
        let (mut app, base) = preset_ref_app("del-profile-open");
        let p = Preset { name: "P".to_string(), ..Default::default() };
        app.paths.save_preset(&p).unwrap();
        app.nav_sel = NavSel::Profile("P".to_string());
        app.editing_preset_buf = Some(p);
        // The ambient game references it, which is what used to trigger `persist()`.
        app.game_config.config.modules.preset = Some("P".to_string());

        app.delete_profile("P");
        app.persist();

        assert!(
            app.paths.load_preset("P").unwrap().is_none(),
            "the deleted profile's file must stay deleted"
        );
        assert_eq!(app.game_config.config.modules.preset, None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn ide_preview_shades_against_its_own_preset_chain_not_nav_sels() {
        let base = std::env::temp_dir().join(format!(
            "ritz-gui-test-preview-chain-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let paths = Paths { base: base.clone() };

        let parent = Preset { name: "parent".to_string(), parent: None, ..Default::default() };
        let child = Preset {
            name: "child".to_string(),
            parent: Some("parent".to_string()),
            ..Default::default()
        };
        // A third, unrelated profile, standing in for "whatever Config mode was
        // last looking at" — property 3's control.
        let unrelated = Preset { name: "unrelated".to_string(), parent: None, ..Default::default() };
        paths.save_preset(&parent).unwrap();
        paths.save_preset(&child).unwrap();
        paths.save_preset(&unrelated).unwrap();

        let mut game = GameConfig::new("77", "Preview Target");
        game.config.modules.preset = Some("child".to_string());
        paths.save_game(&game).unwrap();

        let mut app = test_app();
        app.paths = paths;
        // Config mode's selection is left pointing at an unrelated profile, as it
        // is for real: entering IDE mode does not touch `nav_sel` (S4b).
        app.nav_sel = NavSel::Profile("unrelated".to_string());
        app.editing_preset_buf = Some(unrelated);
        app.mode = Mode::Ide;
        app.set_preview_game(Some("77"));

        // (1) Inside the preview render, the chain is the preview's.
        app.write_target = WriteTarget::Preview;
        let chain = app.field_chain();
        let names: Vec<&str> = chain.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["child", "parent"],
            "the IDE preview computed no usable chain, so depth shading can never render",
        );

        // (3) …and it is not `nav_sel`'s, which is what the Preview arm exists for.
        assert!(
            !names.contains(&"unrelated"),
            "the preview picked up the chain of the scope nav_sel still holds",
        );

        // (2) No preview game selected → no chain, and that is the right answer.
        app.set_preview_game(None);
        assert!(
            app.field_chain().is_empty(),
            "a preview resolving against nothing invented a chain to shade against",
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Build a one-var module with the given ops, in order.
    /// `("Append", "x")` → one `Builder` step appending `x`.
    fn env_module(name: &str, var: &str, ops: &[(&str, &str)]) -> Extension {
        let builder: Vec<Value> = ops
            .iter()
            .map(|(op, val)| json!({ "Type": op, "Value": val }))
            .collect();
        serde_json::from_value(json!({
            "Extension": { "Author": "T", "Name": name, "Version": "1" },
            "ENV_VARS": [ { "Name": var, "Builder": builder } ]
        }))
        .unwrap()
    }

    /// The regression this replaced `set_set_collisions` for: two modules both
    /// **appending** to one var lose nothing, and must not warn.
    ///
    /// This is the case the user hit on `RADV_PERFTEST`. It fails against the old
    /// implementation by construction: that one re-assembled each module alone and
    /// called anything that came back as `EnvAction::Set(_)` a setter — and a lone
    /// `Append` assembles to exactly that, because the fold has no `Append` variant
    /// to report. Verified by reverting the detection and watching this assert fire.
    #[test]
    fn append_append_is_not_a_loss() {
        let specs = [
            env_module("A", "RADV_PERFTEST", &[("append", "gpl")]),
            env_module("B", "RADV_PERFTEST", &[("append", "nggc")]),
        ];
        assert_eq!(lossy_env_overwrites(&specs, &Resolution::default()), Vec::new());
    }

    /// `Set` then `Append` also concatenates — the append lands on top of the set,
    /// nothing is discarded. The old lint counted both modules as setters here too.
    #[test]
    fn set_then_append_is_not_a_loss() {
        let specs = [
            env_module("A", "V", &[("set", "base")]),
            env_module("B", "V", &[("append", "extra")]),
        ];
        assert_eq!(lossy_env_overwrites(&specs, &Resolution::default()), Vec::new());
    }

    /// The genuine losses, and who gets named for each.
    #[test]
    fn set_and_unset_over_another_module_are_losses() {
        let res = Resolution::default();

        // Set over Set — the original, correct case.
        let ss = lossy_env_overwrites(
            &[env_module("A", "V", &[("set", "a")]), env_module("B", "V", &[("set", "b")])],
            &res,
        );
        assert_eq!(ss.len(), 1);
        assert_eq!(ss[0].var, "V");
        assert_eq!(ss[0].culprit, "B");
        assert_eq!(ss[0].victims, vec!["A".to_string()]);
        assert!(!ss[0].by_unset);

        // Append THEN Set: order-dependent, and lossy in this order — B's `Set`
        // discards the text A appended. The reverse order is `set_then_append_is_
        // not_a_loss` above, and stays silent. Losing an append silently is worse
        // than the false positive we removed, so this one warns.
        let as_ = lossy_env_overwrites(
            &[env_module("A", "V", &[("append", "a")]), env_module("B", "V", &[("set", "b")])],
            &res,
        );
        assert_eq!(as_.len(), 1);
        assert_eq!(as_[0].culprit, "B");
        assert!(!as_[0].by_unset);

        // Unset over a value is a loss, and reads as its own kind.
        let su = lossy_env_overwrites(
            &[env_module("A", "V", &[("set", "a")]), env_module("B", "V", &[("unset", "")])],
            &res,
        );
        assert_eq!(su.len(), 1);
        assert!(su[0].by_unset);
        assert!(su[0].message().contains("unsets V"));

        // Unset THEN Set: the unset carried no value, so the later write destroys
        // nothing. Intent is overridden; data is not lost. Deliberately silent.
        let us = lossy_env_overwrites(
            &[env_module("A", "V", &[("unset", "")]), env_module("B", "V", &[("set", "b")])],
            &res,
        );
        assert_eq!(us, Vec::new());
    }

    /// A single module's own `Set` after its own `Append` is an authored sequence,
    /// not a collision — the lint is strictly cross-module.
    #[test]
    fn one_modules_own_steps_never_collide_with_themselves() {
        let specs = [env_module("A", "V", &[("append", "x"), ("set", "y"), ("unset", "")])];
        assert_eq!(lossy_env_overwrites(&specs, &Resolution::default()), Vec::new());
    }

    /// ENV_VARS and GAME_ENV_VARS get separate accumulators in `build_env_block`,
    /// so the same name in the two blocks is two variables and never collides.
    #[test]
    fn env_and_game_env_blocks_do_not_cross_report() {
        let a = env_module("A", "V", &[("set", "a")]);
        let b: Extension = serde_json::from_value(json!({
            "Extension": { "Author": "T", "Name": "B", "Version": "1" },
            "GAME_ENV_VARS": [ { "Name": "V", "Builder": [ { "Type": "set", "Value": "b" } ] } ]
        }))
        .unwrap();
        assert_eq!(lossy_env_overwrites(&[a, b], &Resolution::default()), Vec::new());
    }

    // ── Leaving the module editor with unsaved edits (2026-07-19) ───────────
    //
    // The bug these cover: clicking a category tab from an open editor switched
    // instantly, and the frame-start draft-drop guard in `ui()` then destroyed the
    // draft without a word. Every route out is now either locked (the Config-mode
    // nav tree and MODULES tree, while a draft is open) or prompts.

    /// The gate that decides whether a tab click needs a prompt at all — one arm
    /// per destination, each matching what that destination actually drops
    /// (issue #35).
    ///
    /// Rewritten from the pre-#35 version, which asserted that **both** config
    /// destinations prompt on `any_unsaved`. That was the bug: `GeneralSettings`
    /// drops nothing, so its prompt existed only to offer a "yes" that
    /// `drafts.clear()` then made expensive.
    #[test]
    fn nav_category_gate_prompts_only_when_the_draft_would_be_dropped() {
        // Profiles drops the focused draft — and only asks about that one.
        assert!(nav_category_drops_draft(NavCategory::GamesProfiles, true));
        assert!(!nav_category_drops_draft(NavCategory::GamesProfiles, false));
        // General Settings keeps every draft (`select_nav_category` only clears
        // `focused_module`), so there is nothing to warn about, ever.
        assert!(!nav_category_drops_draft(NavCategory::GeneralSettings, true));
        assert!(!nav_category_drops_draft(NavCategory::GeneralSettings, false));
        // Staying in IDE mode keeps every draft, dirty or not.
        assert!(!nav_category_drops_draft(NavCategory::Ide, true));
        assert!(!nav_category_drops_draft(NavCategory::Ide, false));
    }

    /// **Issue #35, the amplification.** Switching to General Settings with
    /// unsaved drafts open must cost nothing: the destination keeps every draft
    /// by design, so no prompt may be raised — and if one somehow is, confirming
    /// it must still not destroy anything.
    ///
    /// Verified to fail before the fix: with `GeneralSettings => any_unsaved` and
    /// the `drafts.clear()` in `apply_discard_edits`'s `Nav` arm, the gate
    /// asserted `true` and the draft count went to 0.
    #[test]
    fn switching_to_general_settings_never_costs_a_draft() {
        let mut app = test_app();
        app.mode = Mode::Ide;
        let alpha = insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        insert_draft(&mut app, "Ritze", "Beta", "/x/beta.json");
        for p in ["/x/alpha.json", "/x/beta.json"] {
            app.drafts.get_mut(&PathBuf::from(p)).unwrap().ext.meta.version =
                "9.9".to_string();
        }
        app.focused_module = Some(alpha);
        app.refresh_unsaved_drafts();
        assert_eq!(app.unsaved_module_names().len(), 2, "premise: both are unsaved");

        // No prompt: the destination requires no loss, so the question is not
        // asked at all.
        assert!(!nav_category_drops_draft(
            NavCategory::GeneralSettings,
            app.focused_draft_unsaved()
        ));
        // And the click, applied through the confirmed path, keeps both drafts.
        app.apply_discard_edits(DiscardThen::Nav(NavCategory::GeneralSettings));
        assert_eq!(app.drafts.len(), 2, "General Settings keeps every draft");
        assert_eq!(app.mode, Mode::Config, "the click is still applied");
        assert_eq!(app.nav_sel, NavSel::GeneralSettings);
        assert_eq!(app.focused_module, None, "only focus is cleared");
    }

    /// **Issue #35, the milder `GamesProfiles` version.** Its destination drops
    /// exactly one draft (`close_focused_draft`), so confirming must drop exactly
    /// one — not all N — and the dialog must name exactly that one.
    ///
    /// Verified to fail before the fix: `apply_discard_edits` ran `drafts.clear()`
    /// (leaving 0, not 1) and the dialog listed both modules via
    /// `unsaved_module_names`.
    #[test]
    fn switching_to_profiles_drops_only_the_focused_draft_and_names_only_it() {
        let mut app = test_app();
        app.mode = Mode::Ide;
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        let beta = insert_draft(&mut app, "Ritze", "Beta", "/x/beta.json");
        for p in ["/x/alpha.json", "/x/beta.json"] {
            app.drafts.get_mut(&PathBuf::from(p)).unwrap().ext.meta.version =
                "9.9".to_string();
        }
        app.focused_module = Some(beta);
        app.refresh_unsaved_drafts();

        // The prompt fires (the focused draft really is at risk) and names one.
        assert!(nav_category_drops_draft(
            NavCategory::GamesProfiles,
            app.focused_draft_unsaved()
        ));
        assert_eq!(
            app.discard_names(DiscardThen::Nav(NavCategory::GamesProfiles)),
            vec!["Ritze::Beta".to_string()],
            "the prompt must name exactly what it destroys"
        );

        app.apply_discard_edits(DiscardThen::Nav(NavCategory::GamesProfiles));

        assert_eq!(app.drafts.len(), 1, "only the focused draft may go");
        assert!(app.drafts.contains_key(&PathBuf::from("/x/alpha.json")));
        assert_eq!(app.mode, Mode::Config, "the click is still applied");
    }

    /// A background draft the click would never touch must not raise a prompt.
    /// Pre-fix the gate read `!unsaved_drafts.is_empty()`, so an unsaved draft
    /// sitting behind a *clean* focused one made every tab click ask — and then
    /// destroy it on Confirm.
    ///
    /// *Not independently pre-fix-failing, and deliberately so:* the predicate it
    /// pins (`focused_draft_unsaved`) did not exist before the fix, and the real
    /// call-site expression only runs inside a live egui frame. It is a
    /// characterisation test for the new gate's argument. The *behavioural* delta
    /// for this state is covered by
    /// `switching_to_profiles_drops_only_the_focused_draft_and_names_only_it`,
    /// which does fail pre-fix. The assertion below spells out the old expression
    /// beside the new one so the difference is visible in the test itself.
    #[test]
    fn an_unfocused_unsaved_draft_does_not_raise_a_prompt() {
        let mut app = test_app();
        app.mode = Mode::Ide;
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        let beta = insert_draft(&mut app, "Ritze", "Beta", "/x/beta.json");
        // Alpha (unfocused) is unsaved; Beta (focused) is clean.
        app.drafts.get_mut(&PathBuf::from("/x/alpha.json")).unwrap().ext.meta.version =
            "9.9".to_string();
        app.focused_module = Some(beta);
        app.refresh_unsaved_drafts();

        assert!(!app.unsaved_drafts.is_empty(), "premise: something is unsaved");
        assert!(!app.focused_draft_unsaved(), "premise: but not the focused one");
        // The old argument (whole-map) vs. the new one (focused draft), side by
        // side on the same state: the old one asked, the new one does not.
        assert!(nav_category_drops_draft(
            NavCategory::GamesProfiles,
            !app.unsaved_drafts.is_empty()
        ));
        assert!(!nav_category_drops_draft(
            NavCategory::GamesProfiles,
            app.focused_draft_unsaved()
        ));
    }

    /// The dialog's list and the destruction are one answer, per destination.
    /// `ExitApp` is the only one that names every unsaved draft, because it is the
    /// only one that destroys them all.
    #[test]
    fn discard_names_match_the_destination() {
        let mut app = test_app();
        app.mode = Mode::Ide;
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        let beta = insert_draft(&mut app, "Ritze", "Beta", "/x/beta.json");
        for p in ["/x/alpha.json", "/x/beta.json"] {
            app.drafts.get_mut(&PathBuf::from(p)).unwrap().ext.meta.version =
                "9.9".to_string();
        }
        app.focused_module = Some(beta);
        app.refresh_unsaved_drafts();

        assert_eq!(
            app.discard_names(DiscardThen::CloseEditor),
            vec!["Ritze::Beta".to_string()]
        );
        assert_eq!(
            app.discard_names(DiscardThen::Nav(NavCategory::GeneralSettings)),
            Vec::<String>::new(),
            "nothing is destroyed, so nothing is named"
        );
        assert_eq!(
            app.discard_names(DiscardThen::ExitApp(EditOutcome::Cancel)),
            vec!["Ritze::Alpha".to_string(), "Ritze::Beta".to_string()],
            "the process ends, so every unsaved draft is at risk"
        );
    }

    /// The prompt names the modules at risk, in the plural form S4a grew into
    /// without rewording the dialog.
    #[test]
    fn discard_prompt_names_only_dirty_modules() {
        let mut app = test_app();
        // A clean draft is byte-identical to disk: nothing at risk, nothing named.
        let clean = draft_from(sample_manifest(), true);
        app.drafts.insert(clean.manifest.clone(), clean);
        app.refresh_unsaved_drafts();
        assert_eq!(app.unsaved_module_names(), Vec::<String>::new());
        // Any edit makes it dirty and puts it in the prompt, by on-disk identity.
        let mut draft = draft_from(sample_manifest(), true);
        draft.ext.meta.version = "9.9".to_string();
        assert!(draft.dirty());
        app.drafts.insert(draft.manifest.clone(), draft);
        app.refresh_unsaved_drafts();
        assert_eq!(app.unsaved_module_names(), vec!["Ritze::Sample".to_string()]);
    }

    // ── S4a: several drafts at once (2026-07-19) ────────────────────────────

    /// A manifest for `author::name`, so a test can build two distinct modules.
    fn named_manifest(author: &str, name: &str) -> Value {
        json!({
            "Extension": {"Name": name, "Author": author, "Version": "1.0"},
            "UI": {"Main": [{"Type": "toggle", "Variable": "enabled"}]}
        })
    }

    /// Build a draft for `author::name` keyed at `path`, and register it.
    fn insert_draft(app: &mut GuiApp, author: &str, name: &str, path: &str) -> String {
        let mut d = draft_from(named_manifest(author, name), true);
        d.manifest = PathBuf::from(path);
        let id = d.id.clone();
        app.drafts.insert(d.manifest.clone(), d);
        id
    }

    /// **The silent one.** `name_collides` compares against *disk* only, so two
    /// dirty drafts can both stage `(Ritze, Alpha)` and neither sees the other —
    /// both Save gates open and the second Save clobbers the first module's
    /// config namespace, with no symptom until a module goes missing.
    ///
    /// Verified to bite: making `draft_identity_collides` return `false`
    /// unconditionally makes this assert fire.
    #[test]
    fn a_second_draft_staging_another_drafts_identity_collides() {
        let mut app = test_app();
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        let beta_id = insert_draft(&mut app, "Ritze", "Beta", "/x/beta.json");

        // Beta stages a rename onto Alpha's identity. Nothing on disk changed, so
        // `name_collides` alone cannot see this.
        app.drafts
            .get_mut(&PathBuf::from("/x/beta.json"))
            .unwrap()
            .identity
            .name = "Alpha".to_string();

        app.focused_module = Some(beta_id);
        app.refresh_identity_state();

        assert_eq!(
            app.draft().unwrap().identity_error.as_deref(),
            Some("Author + Name already in use"),
            "a pending identity that duplicates another open draft's must be rejected"
        );
    }

    /// The converse: a pending identity nobody else is staging is fine, and a
    /// draft must never collide with *itself* (it is excluded by manifest path —
    /// the key point of path-keying).
    #[test]
    fn a_unique_pending_identity_and_a_drafts_own_identity_do_not_collide() {
        let mut app = test_app();
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        let beta_id = insert_draft(&mut app, "Ritze", "Beta", "/x/beta.json");
        app.focused_module = Some(beta_id);

        // No pending change at all: the draft's own identity must not flag.
        app.refresh_identity_state();
        assert_eq!(app.draft().unwrap().identity_error, None);
        app.refresh_draft_name_error();
        assert_eq!(app.draft().unwrap().name_error, None);

        // A genuinely free name is accepted.
        app.drafts
            .get_mut(&PathBuf::from("/x/beta.json"))
            .unwrap()
            .identity
            .name = "Gamma".to_string();
        app.refresh_identity_state();
        assert_eq!(app.draft().unwrap().identity_error, None);
    }

    /// The requirement S4a exists for: edits to one module survive switching
    /// focus to another and back. Pre-S4a `ensure_draft` replaced the single slot
    /// on every switch, so this lost the edit outright.
    #[test]
    fn a_dirty_draft_survives_switching_to_another_module() {
        let mut app = test_app();
        let alpha_id = insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        let beta_id = insert_draft(&mut app, "Ritze", "Beta", "/x/beta.json");
        app.drafts
            .get_mut(&PathBuf::from("/x/alpha.json"))
            .unwrap()
            .ext
            .meta
            .description = Some("unsaved work".to_string());

        // Focus Beta. `ensure_draft` for Beta must not disturb Alpha's entry.
        app.focused_module = Some(beta_id);
        let focused = app.focused_module().unwrap().to_owned();
        app.ensure_draft(&focused);
        assert_eq!(app.drafts.len(), 2, "switching focus must not evict a draft");

        // Focus back: the edit is still there.
        app.focused_module = Some(alpha_id);
        assert_eq!(
            app.draft().unwrap().ext.meta.description.as_deref(),
            Some("unsaved work")
        );
        app.refresh_unsaved_drafts();
        assert_eq!(app.unsaved_module_names(), vec!["Ritze::Alpha".to_string()]);
    }

    /// A draft whose module has left `all_specs` (external delete, or a manifest
    /// that stopped validating across a Ctrl+R) is **retained**, stays reachable
    /// through the focused-draft accessors, and is surfaced by
    /// `orphan_draft_names` — the tree renders `all_specs` and can no longer show
    /// it a row. Pre-S4a `ensure_draft` deleted it on sight.
    #[test]
    fn an_orphaned_draft_is_retained_and_surfaced() {
        let mut app = test_app();
        let alpha_id = insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        app.drafts
            .get_mut(&PathBuf::from("/x/alpha.json"))
            .unwrap()
            .ext
            .meta
            .description = Some("unsaved work".to_string());
        // `all_specs` is empty in `test_app`, so Alpha is an orphan by construction.
        app.focused_module = Some(alpha_id.clone());

        app.ensure_draft(&alpha_id);
        assert_eq!(app.drafts.len(), 1, "an orphaned draft must never be dropped");
        assert_eq!(
            app.draft().unwrap().ext.meta.description.as_deref(),
            Some("unsaved work"),
            "the orphan must stay reachable through the focused-draft accessor"
        );
        assert_eq!(app.orphan_draft_names(), vec!["Ritze::Alpha".to_string()]);

        // A module that IS on disk is not an orphan.
        app.all_specs = vec![serde_json::from_value(named_manifest("Ritze", "Alpha")).unwrap()];
        assert_eq!(app.orphan_draft_names(), Vec::<String>::new());
    }

    /// Forking from a dirty draft carries the edits into the fork **and** leaves
    /// the original draft's edits in place. Pre-S4a `perform_fork` sourced
    /// `all_specs` (i.e. disk), silently forking the *saved* version.
    ///
    /// Exercises the sourcing decision directly rather than driving `perform_fork`
    /// — that method writes files and reloads extensions, neither of which exists
    /// under a `/nonexistent` `Paths`.
    #[test]
    fn a_fork_sources_the_open_draft_not_disk() {
        let mut app = test_app();
        let alpha_id = insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        // Disk still has the un-edited module…
        app.all_specs = vec![serde_json::from_value(named_manifest("Ritze", "Alpha")).unwrap()];
        // …while the draft carries an unsaved edit.
        app.drafts
            .get_mut(&PathBuf::from("/x/alpha.json"))
            .unwrap()
            .ext
            .meta
            .description = Some("unsaved work".to_string());

        // The exact expression `perform_fork` uses to pick its source.
        let sourced = app
            .drafts
            .values()
            .find(|d| d.id == alpha_id)
            .map(|d| d.snapshot())
            .or_else(|| app.all_specs.iter().find(|s| s.id() == alpha_id).cloned())
            .unwrap();
        assert_eq!(
            sourced.meta.description.as_deref(),
            Some("unsaved work"),
            "the fork must carry the draft's unsaved edits, not the on-disk version"
        );
        // …and the original draft still has them.
        assert_eq!(app.drafts.len(), 1);
        app.refresh_unsaved_drafts();
        assert_eq!(app.unsaved_module_names(), vec!["Ritze::Alpha".to_string()]);
    }

    /// Discard is per-module (S4a decision): it drops the focused draft and
    /// leaves every other one alone. The whole-map clear belongs to the confirmed
    /// tab-switch path only.
    #[test]
    fn discard_drops_only_the_focused_draft() {
        let mut app = test_app();
        app.mode = Mode::Ide;
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        let beta_id = insert_draft(&mut app, "Ritze", "Beta", "/x/beta.json");
        for p in ["/x/alpha.json", "/x/beta.json"] {
            app.drafts.get_mut(&PathBuf::from(p)).unwrap().ext.meta.version =
                "9.9".to_string();
        }
        app.focused_module = Some(beta_id);

        app.apply_discard_edits(DiscardThen::CloseEditor);

        assert_eq!(app.drafts.len(), 1, "only the focused draft may be discarded");
        assert!(app.drafts.contains_key(&PathBuf::from("/x/alpha.json")));
        // Beta is not re-seeded here: `all_specs` is empty, so there is nothing on
        // disk to re-seed FROM — the point is that Alpha survived.
        assert_eq!(app.unsaved_module_names(), vec!["Ritze::Alpha".to_string()]);
    }

    /// Confirm on a tab-click prompt must do BOTH halves: complete the navigation
    /// the user clicked *and* drop what that destination drops. Dropping without
    /// navigating would lose the edits and go nowhere — the worst of both.
    ///
    /// Rewritten for S4b: `editor_return` is gone, and `nav_sel` is no longer
    /// what carries "which module is focused" (that's `focused_module` now), so
    /// there is nothing to "restore" — the destination comes entirely from
    /// `select_nav_category(cat)`, exercised here exactly as the real click path
    /// exercises it.
    ///
    /// Rewritten again for issue #35: the destination is now `GamesProfiles`
    /// (the only tab that drops anything), and the assertion is that the *one*
    /// focused draft went — not that the map was emptied.
    #[test]
    fn confirming_a_tab_click_discards_and_completes_the_switch() {
        let mut app = test_app();
        app.mode = Mode::Ide;
        let mut draft = draft_from(sample_manifest(), true);
        draft.ext.meta.version = "9.9".to_string();
        app.drafts.insert(draft.manifest.clone(), draft);
        app.focused_module = Some("Ritze::Sample::1.0".to_string());

        app.apply_discard_edits(DiscardThen::Nav(NavCategory::GamesProfiles));

        assert!(app.drafts.is_empty(), "the focused draft — the only one — must go");
        assert_eq!(app.mode, Mode::Config, "the click must still be applied");
        assert_eq!(app.focused_module, None, "focus must be cleared along with the mode switch");
    }

    /// Confirm on the *Close* prompt ([`DiscardThen::CloseEditor`]) discards the focused
    /// draft and clears focus.
    ///
    /// Rewritten for S4b: the pre-S4b version of this test asserted `nav_sel`
    /// was restored to a "remembered view" via `editor_return`. That mechanism
    /// is gone — focusing a module never moves `nav_sel` in the first place (see
    /// `Mode`'s doc comment), so there is nothing to restore. What actually
    /// matters now, and is still true, is that `nav_sel` is left **exactly** as
    /// it was — this asserts that directly instead of asserting a restore that
    /// no longer happens.
    #[test]
    fn confirming_close_discards_and_clears_focus_leaving_nav_sel_untouched() {
        let mut app = test_app();
        let mut draft = draft_from(sample_manifest(), true);
        draft.ext.meta.version = "9.9".to_string();
        app.drafts.insert(draft.manifest.clone(), draft);
        app.focused_module = Some("Ritze::Sample::1.0".to_string());
        app.nav_sel = NavSel::Profile("Handhelds".to_string());

        app.apply_discard_edits(DiscardThen::CloseEditor);

        assert!(app.drafts.is_empty());
        assert_eq!(app.focused_module, None, "focus must be cleared");
        assert_eq!(
            app.nav_sel,
            NavSel::Profile("Handhelds".to_string()),
            "nav_sel must be left exactly as it was — nothing to restore it to any more"
        );
    }

    // ── Which modules a screen lists (2026-07-19) ───────────────────────────

    /// `cur_specs` used to be filtered by the last-selected game on every screen,
    /// so the Global Profile and Profile screens — which apply across games — hid
    /// modules gated to some other game's `AppIds`. They now list everything; the
    /// Game screens still filter.
    #[test]
    fn only_the_game_screens_filter_the_module_list() {
        let mut app = test_app();
        let universal: Extension = serde_json::from_value(json!({
            "Extension": { "Author": "T", "Name": "Universal", "Version": "1" }
        }))
        .unwrap();
        let other_game: Extension = serde_json::from_value(json!({
            "Extension": { "Author": "T", "Name": "OtherGame", "Version": "1" },
            "AppIds": ["999"]
        }))
        .unwrap();
        app.all_specs = vec![universal, other_game];
        app.all_dirs = vec![PathBuf::from("a"), PathBuf::from("b")];
        app.all_is_folder_ext = vec![false, false];

        // Game screen (`appid == "42"`): the module gated to appid 999 is not
        // applicable here, so it stays hidden.
        app.nav_sel = NavSel::Game("42".to_string());
        app.rebuild_cur_specs();
        assert_eq!(names_of(&app.cur_specs), vec!["Universal"]);
        assert_eq!(app.cur_dirs.len(), 1, "the aligned side lists must track the filter");
        assert_eq!(app.cur_is_folder_ext.len(), 1);

        // Global Profile and Profile apply across games: everything is listed.
        for scope in [NavSel::GlobalSettings, NavSel::Profile("Handhelds".to_string())] {
            app.nav_sel = scope;
            app.rebuild_cur_specs();
            assert_eq!(names_of(&app.cur_specs), vec!["Universal", "OtherGame"]);
            assert_eq!(app.cur_dirs.len(), 2);
        }
    }

    /// `selected_ext` is a raw index into `cur_specs`, so a list that changes
    /// length must not silently re-point it at a different module.
    #[test]
    fn selection_follows_the_module_across_a_screen_change() {
        let mut app = test_app();
        let other_game: Extension = serde_json::from_value(json!({
            "Extension": { "Author": "T", "Name": "OtherGame", "Version": "1" },
            "AppIds": ["999"]
        }))
        .unwrap();
        let universal: Extension = serde_json::from_value(json!({
            "Extension": { "Author": "T", "Name": "Universal", "Version": "1" }
        }))
        .unwrap();
        // Order matters: on the unfiltered screen "Universal" is at index 1, on the
        // filtered one it is index 0. A naive index carry-over would move.
        app.all_specs = vec![other_game, universal];
        app.all_dirs = vec![PathBuf::from("a"), PathBuf::from("b")];
        app.all_is_folder_ext = vec![false, false];

        app.nav_sel = NavSel::GlobalSettings;
        app.rebuild_cur_specs();
        app.selected_ext = 1; // "Universal"
        app.nav_sel = NavSel::Game("42".to_string());
        app.rebuild_cur_specs();
        assert_eq!(app.cur_specs[app.selected_ext].meta.name, "Universal");

        // The selected module vanishing from the new list falls back to the first
        // entry rather than leaving a dangling index.
        app.nav_sel = NavSel::GlobalSettings;
        app.rebuild_cur_specs();
        app.selected_ext = names_of(&app.cur_specs)
            .iter()
            .position(|n| n == "OtherGame")
            .unwrap();
        app.nav_sel = NavSel::Game("42".to_string());
        app.rebuild_cur_specs();
        assert_eq!(app.selected_ext, 0);
        assert_eq!(app.cur_specs[0].meta.name, "Universal");
    }

    /// The launch-command preview keeps the game filter on every screen: a launch
    /// command is for one concrete game, so it must never assemble modules that
    /// cannot apply to it — even while the Profile screen beside it lists them.
    #[test]
    fn the_launch_preview_stays_game_filtered_on_profile_screens() {
        let mut app = test_app();
        let universal: Extension = serde_json::from_value(json!({
            "Extension": { "Author": "T", "Name": "Universal", "Version": "1" }
        }))
        .unwrap();
        let other_game: Extension = serde_json::from_value(json!({
            "Extension": { "Author": "T", "Name": "OtherGame", "Version": "1" },
            "AppIds": ["999"]
        }))
        .unwrap();
        app.all_specs = vec![universal, other_game];
        app.all_dirs = vec![PathBuf::from("a"), PathBuf::from("b")];
        app.all_is_folder_ext = vec![false, false];
        app.nav_sel = NavSel::Profile("Handhelds".to_string());
        app.rebuild_cur_specs();

        assert_eq!(names_of(&app.cur_specs), vec!["Universal", "OtherGame"]);
        assert_eq!(names_of(&app.game_specs()), vec!["Universal"]);
    }

    fn names_of(specs: &[Extension]) -> Vec<String> {
        specs.iter().map(|s| s.meta.name.clone()).collect()
    }

    // ── Merely rendering a draft must not dirty it (2026-07-19) ─────────────

    /// Render `render_editor_body` once, headless, with no input events at all.
    ///
    /// A throwaway `egui::Context` runs one frame entirely offscreen — no window,
    /// no winit, no event loop, and a `RawInput` carrying no events. That is the
    /// whole point: the bug under test only manifests inside the *render* pass
    /// (the widget write-backs), so a pure-function test could not see it, and
    /// driving a real window is neither available in CI nor acceptable on a dev
    /// box.
    fn render_editor_body_once(draft: &mut ModuleDraft) {
        let mut cache = IconCenterCache::new();
        let mut deferred: Vec<Deferred> = Vec::new();
        let ctx = egui::Context::default();
        // An explicit screen rect: the default `RawInput` leaves it `None`, and a
        // zero-sized viewport can short-circuit layout before the write-backs run.
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::pos2(0.0, 0.0),
                egui::vec2(1200.0, 900.0),
            )),
            ..Default::default()
        };
        // The `FullOutput` (textures, shapes, platform commands) is for a painter
        // to consume; there is no painter here, and the effect under test is the
        // mutation `render_editor_body` makes to `draft`.
        let _ = ctx.run(input, |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                render_editor_body(ui, &mut cache, draft, &mut deferred);
            });
        });
        // Structural edits only ever come from a click; with no input there must
        // be none. Asserted rather than applied, so a future renderer that queues
        // a `Deferred` unprompted trips here instead of silently mutating.
        assert!(deferred.is_empty(), "an untouched render queued a structural edit");
    }

    /// A real bundled manifest, read from `resources/extensions/` at test time.
    ///
    /// *Why the real file and not an inline `json!`:* the whole bug was that
    /// three shipped manifests contain a JSON shape the editor round-trips
    /// lossily. An inline fixture would only prove the fix against a shape
    /// *we* chose to write down; reading what actually ships proves the shipped
    /// modules are clean, and keeps proving it if someone edits them.
    fn bundled_manifest(rel: &str) -> Value {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../resources/extensions")
            .join(rel);
        let text = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("reading {}: {e}", path.display()));
        serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("parsing {}: {e}", path.display()))
    }

    /// Opening a module in the IDE and touching **nothing** must leave its draft
    /// clean — no dirty marker in the tree, no "unsaved changes" prompt on the way
    /// out. Reported 2026-07-19 against exactly `amd`, `dxvk` and `misc`.
    ///
    /// *Why those three and not the other five bundled modules:* they are the only
    /// shipped manifests carrying an **empty-string** `Value` / `Requires`
    /// (`"Value": ""` in amd + misc, `"Requires": ""` in dxvk). The editor's text
    /// write-backs collapsed `Some("")` to `None` on every frame — an unprompted
    /// edit, so the draft was born dirty the instant it first rendered. The
    /// correlation the report drew with "modules that have stored config for this
    /// game" was coincidence: gamescope has stored config and was never marked,
    /// dxvk had no stored config and was.
    ///
    /// Every bundled module is checked, not just the three, so the property is
    /// "rendering never dirties a draft" rather than "these three specific files
    /// happen to be fine".
    #[test]
    fn rendering_a_draft_without_touching_it_leaves_it_clean() {
        for rel in [
            "default/amd.json",
            "default/dxvk.json",
            "default/misc.json",
            "default/gamescope.json",
            "default/proton.json",
            "default/pulse.json",
            "default/vkd3d.json",
            "default/scripts/scripts.json",
            "built-in/lsfg-vk/lsfg-vk.json",
            "built-in/hypr-monctl/hypr-monctl.json",
            "built-in/custom-env/custom-env.json",
            "built-in/custom-args/custom-args.json",
            "built-in/custom-game-env/custom-game-env.json",
        ] {
            // `editable: false` — these are the bundled, read-only set, which is
            // precisely the case the report hit: `add_enabled_ui(false, ..)` greys
            // the widgets out but the write-backs after them still run.
            let mut draft = draft_from(bundled_manifest(rel), false);
            render_editor_body_once(&mut draft);
            assert!(
                !draft.dirty(),
                "{rel}: rendering the editor with no input marked the draft dirty\n\
                 baseline: {}\n\
                 snapshot: {}",
                serde_json::to_string(&draft.baseline).unwrap(),
                serde_json::to_string(&draft.snapshot()).unwrap(),
            );
        }
    }

    /// The same "rendering is not editing" rule, for the one **structural**
    /// write-back that survived the 2026-07-19 sweep: `ensure_list` (issue #27).
    ///
    /// The Selection arm of the field editor used to call `ensure_list` to get a
    /// `&mut Vec<String>` to draw option rows from, which *installs*
    /// `Some(Options: [])` whenever the field's `Options` is not already a list.
    /// Unlike the text write-backs there was nothing to gate on — the mutation
    /// happens before the widget, not after — so the fix was to change the shape:
    /// `selection_options` returns an empty slice instead of materialising one.
    ///
    /// Two reachable trigger states, both covered here:
    ///
    /// - **absent** `Options` — a Selection driven by `DisplayOptions` alone, and
    ///   also what `new_field` produces the moment its `Type` combo is switched
    ///   to Selection.
    /// - **`Range`** `Options` — left behind by switching an Integer/Float field
    ///   over to Selection (and expressible directly in a hand-written manifest,
    ///   since `OptionsSpec` is `#[serde(untagged)]`).
    ///
    /// *Why not covered by the bundled-manifest test above:* no shipped manifest
    /// reaches either state — every bundled Selection field has an array
    /// `Options` — so the bug was latent and only a user-authored module could
    /// hit it. That is exactly why it needs a fixture rather than a shipped file.
    ///
    /// Verified to bite: restoring `ensure_list(&mut field.options)` at the
    /// render site fails both the `dirty()` assert and the shape asserts.
    #[test]
    fn rendering_a_selection_field_never_materialises_its_options() {
        let mut draft = draft_from(
            json!({
                "Extension": { "Name": "Sample", "Author": "Ritze", "Version": "1.0" },
                "UI": {"Main": [
                    // No `Options` at all — the display labels are the whole story.
                    {
                        "Type": "selection", "Variable": "mode", "Name": "Mode",
                        "DisplayOptions": ["Alpha", "Beta"]
                    },
                    // A `Range` a `Type` switch could have left behind.
                    {
                        "Type": "selection", "Variable": "level", "Name": "Level",
                        "Options": {"min": 0.0, "max": 10.0, "step": 1.0}
                    }
                ]}
            }),
            // Read-only, like the reported case: greying the widgets out never
            // stopped a plain assignment from running.
            false,
        );
        render_editor_body_once(&mut draft);

        assert!(
            !draft.dirty(),
            "rendering Selection fields with no list marked the draft dirty\n\
             baseline: {}\n\
             snapshot: {}",
            serde_json::to_string(&draft.baseline).unwrap(),
            serde_json::to_string(&draft.snapshot()).unwrap(),
        );
        // The shape-level statement, so a regression is diagnosable without
        // diffing two JSON blobs: absent stays absent, a range stays a range.
        let fields = &draft.sections[0].1;
        assert!(fields[0].options.is_none(), "absent Options must stay absent");
        assert!(
            matches!(fields[1].options, Some(OptionsSpec::Range { .. })),
            "a Range must not be overwritten with an empty list",
        );
    }

    /// The narrow, shape-level statement of the same bug, so a regression is
    /// diagnosable without diffing two whole manifests: an empty string in an
    /// optional slot must survive a render as an empty string, not become `null`.
    #[test]
    fn an_empty_string_in_an_optional_slot_survives_a_render() {
        let mut draft = draft_from(
            json!({
                "Extension": {
                    "Name": "Sample", "Author": "Ritze", "Version": "1.0",
                    "Description": ""
                },
                "UI": {"Main": [
                    {"Type": "toggle", "Variable": "clear", "Name": "", "Requires": ""}
                ]},
                "ENV_VARS": [{
                    "Name": "LD_PRELOAD",
                    "Requires": "",
                    // `set` to the empty string is a real operation — it clears the
                    // variable — and `Separator: ""` is NOT the same as absent
                    // (`builder.rs` defaults an absent separator to ","), so
                    // neither may be normalised away behind the user's back.
                    "Builder": [
                        {"Requires": "clear", "Type": "set", "Value": ""},
                        {"Requires": "clear", "Type": "append", "Value": "x", "Separator": ""}
                    ]
                }]
            }),
            false,
        );
        render_editor_body_once(&mut draft);

        assert!(!draft.dirty(), "an untouched render must not dirty the draft");
        let e = &draft.ext;
        assert_eq!(e.meta.description.as_deref(), Some(""), "meta Description");
        assert_eq!(draft.sections[0].1[0].name.as_deref(), Some(""), "field Name");
        assert_eq!(draft.sections[0].1[0].requires.as_deref(), Some(""), "field Requires");
        assert_eq!(e.env_vars[0].requires.as_deref(), Some(""), "env Requires");
        assert_eq!(e.env_vars[0].builder[0].value.as_deref(), Some(""), "step Value");
        assert_eq!(e.env_vars[0].builder[1].separator.as_deref(), Some(""), "step Separator");
    }

    // ── Save and Rename are independent operations (2026-07-19) ─────────────
    //
    // The user's call, verbatim: "rename should actually only do the renaming,
    // and saving should actually only do the saving." Save writes the body and
    // leaves a staged identity staged; Rename writes the identity and leaves body
    // edits pending. Both pending → two clicks, either order, no prompt.
    //
    // Independent, but not silent: pressing Save with an identity staged raises
    // `ConfirmAction::SaveWithPendingRename` first, at the user's request — "it
    // should notify me, you got to first save your rename and then you can save
    // your changes". Its affirmative applies the rename and stops; the body is
    // still the user's own second press. Advisory, not protective.
    //
    // The data-loss path that is genuinely gone: `PendingIdentity` sits outside
    // `snapshot()`, so a staged Author/Name edit neither dirties the draft nor
    // opens the Save gate. `save_module` wrote the body under the old identity and
    // then `drafts.shift_remove(&manifest)` deleted the draft, taking the staged
    // rename with it, with no message. `save_module` now keeps the draft, so the
    // prompt no longer guards anything — it only orients.

    /// Scratch-dir variant of the module-editor fixture: real files, a real
    /// draft, one body edit and one staged rename. Returns `(app, base, manifest)`;
    /// the caller removes `base`.
    fn app_with_pending_body_and_rename(tag: &str, new_name: &str) -> (GuiApp, PathBuf, PathBuf) {
        let (base, manifest) = scratch_base(tag);
        let mut app = test_app();
        app.paths = Paths { base: base.clone() };
        app.reload_extensions();

        let id = app.all_specs.iter().find(|s| s.meta.name == "Old").unwrap().id();
        app.focused_module = Some(id.clone());
        app.ensure_draft(&id);
        {
            let d = app.drafts.get_mut(&manifest).expect("draft keyed by manifest path");
            d.ext.meta.description = Some("body edit".to_string());
            d.identity.name = new_name.to_string();
        }
        app.refresh_identity_state();
        app.refresh_unsaved_drafts();
        (app, base, manifest)
    }

    fn manifest_json(manifest: &PathBuf) -> Value {
        serde_json::from_str(&std::fs::read_to_string(manifest).unwrap()).unwrap()
    }

    /// A dirty, saveable draft with a *valid* staged rename must raise the
    /// prompt instead of writing, and must leave both pending edits intact so
    /// Cancel loses nothing.
    ///
    /// Verified to bite: routing `TopAction::Save`/Ctrl+S back to `save_module`
    /// leaves `app.confirm` at `None` and this fails on the first assert.
    #[test]
    fn save_with_a_valid_staged_rename_prompts_instead_of_writing() {
        let (mut app, base, manifest) = app_with_pending_body_and_rename("save-prompt", "New");
        assert!(app.draft().unwrap().save_enabled(), "the body edit must open the Save gate");
        assert_eq!(app.draft().unwrap().identity_error, None, "the staged rename is valid");

        app.request_save_module();

        assert!(
            matches!(app.confirm, Some(ConfirmAction::SaveWithPendingRename)),
            "Save with a staged rename must notify, not write; got {:?}",
            app.confirm
        );
        // Nothing written, nothing dropped: the prompt is a question, not an act.
        let after = manifest_json(&manifest);
        assert_eq!(after["Extension"]["Name"], "Old");
        assert!(after["Extension"].get("Description").is_none(), "the prompt must not write");
        assert_eq!(app.draft().unwrap().identity.staged_name(), "New");
        assert_eq!(app.draft().unwrap().ext.meta.description.as_deref(), Some("body edit"));

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The affirmative applies the **rename only**. This is the user's acceptance
    /// criterion: "you got to first save your rename and then you can save your
    /// changes" — control comes back to them with the body still pending, and
    /// they press Save themselves.
    ///
    /// Verified to bite: making the arm run a rename-then-save combo (the deleted
    /// `rename_and_save`, the coupling the user rejected) writes
    /// `"Description": "body edit"` and fails the absence assert.
    #[test]
    fn confirming_the_rename_prompt_applies_the_rename_and_not_the_body() {
        let (mut app, base, manifest) = app_with_pending_body_and_rename("prompt-confirm", "New");
        app.request_save_module();
        assert!(matches!(app.confirm, Some(ConfirmAction::SaveWithPendingRename)));

        // Exactly what the dialog's Confirm arm runs.
        app.confirm = None;
        app.apply_save_with_pending_rename();

        let after = manifest_json(&manifest);
        assert_eq!(after["Extension"]["Name"], "New", "the rename must land");
        assert!(
            after["Extension"].get("Description").is_none(),
            "and the body must NOT ride along with it: {after}"
        );
        let d = app.draft().expect("the editor stays open on the renamed module");
        assert!(!d.has_pending_identity(), "the identity landed");
        assert!(d.dirty(), "the body did not, so it is still pending");
        assert!(d.save_enabled(), "and Save — the user's own second press — is lit");

        // That second press: no rename staged any more, so it writes directly.
        app.request_save_module();
        assert!(app.confirm.is_none(), "nothing left to notify about");
        assert_eq!(manifest_json(&manifest)["Extension"]["Description"], "body edit");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The primitive **underneath** the prompt: `save_module` writes the body
    /// only and leaves a staged identity staged — not committed, not cleared, not
    /// discarded — in an editor that stayed open, because the draft *is* where the
    /// staged rename lives.
    ///
    /// Tested one layer down on purpose: through the UI a *valid* staged rename
    /// now routes to the prompt, whose affirmative renames instead. This pins the
    /// non-destructiveness the prompt's advisory-not-protective framing rests on.
    ///
    /// Verified to bite: restoring `save_module`'s unconditional
    /// `drafts.shift_remove` + `close_focused_draft` fails the `draft must
    /// survive` expect — the original silent loss.
    #[test]
    fn save_with_a_staged_rename_writes_the_body_and_leaves_the_identity_staged() {
        let (mut app, base, manifest) = app_with_pending_body_and_rename("save-pending", "New");
        assert_eq!(app.draft().unwrap().identity_error, None, "the staged rename is valid");

        app.save_module();

        let after = manifest_json(&manifest);
        assert_eq!(after["Extension"]["Description"], "body edit", "the body must land");
        assert_eq!(
            after["Extension"]["Name"], "Old",
            "Save must NOT commit the staged identity — that is Rename's job"
        );

        let d = app.draft().expect("Save must keep the draft while a rename is pending");
        assert_eq!(d.identity.staged_name(), "New", "the staged rename survives Save");
        assert!(d.has_pending_identity());
        assert!(!d.dirty(), "the body just landed, so it is clean against the new baseline");
        assert!(d.has_unsaved_work(), "but the staged rename is still unsaved work");
        assert!(app.focused_module.is_some(), "and the editor stays open on it");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Cancel is a genuine no-op: no write, no draft dropped, both the staged
    /// rename and the body edits exactly where the user left them.
    #[test]
    fn cancelling_the_rename_prompt_leaves_both_edits_pending() {
        let (mut app, base, manifest) = app_with_pending_body_and_rename("prompt-cancel", "New");
        app.request_save_module();
        assert!(matches!(app.confirm, Some(ConfirmAction::SaveWithPendingRename)));

        app.confirm = None; // what the dialog's Cancel button does, in full

        let after = manifest_json(&manifest);
        assert_eq!(after["Extension"]["Name"], "Old");
        assert!(after["Extension"].get("Description").is_none());
        let d = app.draft().expect("Cancel must not drop the draft");
        assert_eq!(d.identity.staged_name(), "New");
        assert!(d.has_pending_identity());
        assert_eq!(d.ext.meta.description.as_deref(), Some("body edit"));
        assert!(d.dirty());

        let _ = std::fs::remove_dir_all(&base);
    }

    /// An *invalid* staged rename still prompts — `perform_rename` would filter
    /// itself out and do nothing, so the affirmative offers the only action
    /// available: the **body-only save**. That escape is what keeps a user with an
    /// unresolvable rename staged (a name collision, say) from being locked out of
    /// saving legitimate body work.
    ///
    /// The reason is read live off `identity_error` at render time rather than
    /// captured into the action, so this pins that it is *readable* there.
    #[test]
    fn save_with_an_invalid_staged_rename_still_writes_only_the_body() {
        // Staging an empty Name is a pending-but-uncommittable identity.
        let (mut app, base, manifest) = app_with_pending_body_and_rename("save-invalid", "   ");
        assert!(app.draft().unwrap().identity_error.is_some(), "premise: the rename is invalid");

        app.request_save_module();
        assert!(
            matches!(app.confirm, Some(ConfirmAction::SaveWithPendingRename)),
            "a blocked rename is still worth notifying about"
        );
        assert!(
            app.draft().unwrap().identity_error.is_some(),
            "and its reason must still be live-readable when the dialog renders"
        );

        app.confirm = None;
        app.apply_save_with_pending_rename();

        let after = manifest_json(&manifest);
        assert_eq!(after["Extension"]["Description"], "body edit", "the body must land");
        assert_eq!(after["Extension"]["Name"], "Old", "under the unchanged identity");
        let d = app.draft().expect("the draft must survive");
        assert_eq!(d.identity.name, "   ", "the invalid buffer is left untouched, to be fixed");
        assert!(d.has_pending_identity(), "still staged — the save cost it nothing");
        assert!(!d.dirty(), "the body landed, so it is clean against the new baseline");
        assert!(d.has_unsaved_work(), "but the staged rename is still unsaved work");
        assert!(app.focused_module.is_some(), "and the editor stays open on it");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// **Issue #34, the regression that motivated `has_unsaved_work`.** A draft
    /// whose *only* change is a staged rename — clean body, `dirty() == false` —
    /// must report unsaved work at **every** gate that asks "will the user lose
    /// something?". Before the fix all four asked bare `dirty()`, so a rename-only
    /// draft was invisible to all of them at once: the state line said "All
    /// changes saved", Close discarded without a prompt (and Save was greyed, so
    /// Close was the only lit button), clicking a category tab `clear()`ed the
    /// draft silently, and the confirm dialog could never name the module.
    ///
    /// Asserted together in one test rather than split into four, because the bug
    /// *was* that four sites had four separate answers to one question. If a
    /// future change routes any of them back through `dirty()`, this fails.
    #[test]
    fn a_rename_only_draft_reports_unsaved_work_at_every_gate() {
        let mut app = test_app();
        let id = insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        app.focused_module = Some(id.clone());
        // Stage a rename and nothing else. The body is untouched.
        app.drafts
            .get_mut(&PathBuf::from("/x/alpha.json"))
            .unwrap()
            .identity
            .name = "Renamed".to_string();
        app.refresh_identity_state();
        app.refresh_unsaved_drafts();

        // Premise: the body really is clean, so this is the exact state that used
        // to slip through. If this fails, the test has stopped testing the bug.
        let d = app.draft().unwrap();
        assert!(!d.dirty(), "the body must be byte-identical to disk");
        assert!(d.has_pending_identity());
        assert!(d.has_unsaved_work(), "the unifying predicate itself");
        assert!(
            !d.save_enabled(),
            "Save stays gated on the body \u{2014} Rename is what commits this"
        );

        // Gate 4 — the state line (what the user complained about). It reports
        // unsaved work, AND the separate warning explaining what is incomplete
        // survives beside it. Two jobs, two entries, neither dropped.
        let info = app.editor_header_info(&id).unwrap();
        assert!(info.unsaved, "the header snapshot must carry unsaved work");
        let lines = editor_status_lines(&info);
        assert_eq!(lines[0].severity, DiagSeverity::Info);
        assert!(
            lines[0].text.starts_with("Unsaved rename"),
            "state line still claims saved: {:?}",
            lines[0].text
        );
        assert!(
            lines.iter().any(|l| l.severity == DiagSeverity::Warning
                && l.text.starts_with("Pending identity change")),
            "the pending-identity warning must NOT be merged away by the fix"
        );
        // Exactly one Info: the new state line is an arm, not a second entry, so
        // the band's tally still counts problems only.
        assert_eq!(
            lines.iter().filter(|l| l.severity == DiagSeverity::Info).count(),
            1
        );

        // Gate 2 — the category-tab guard sees the draft. (Since issue #35 the
        // guard asks about the *focused* draft, which is this one.)
        assert!(!app.unsaved_drafts.is_empty());
        assert!(app.focused_draft_unsaved());
        assert!(nav_category_drops_draft(
            NavCategory::GamesProfiles,
            app.focused_draft_unsaved()
        ));

        // Gate 3 — and the confirm dialog names it, by its on-disk identity (not
        // the half-typed one). This list must match what `apply_discard_edits`
        // destroys, which is the whole draft, staged identity included.
        assert_eq!(
            app.unsaved_module_names(),
            vec!["Ritze::Alpha".to_string()],
            "the prompt must name a module it is about to destroy"
        );
        assert_eq!(
            app.discard_names(DiscardThen::Nav(NavCategory::GamesProfiles)),
            vec!["Ritze::Alpha".to_string()],
            "and the per-destination list agrees"
        );

        // Gate 1 — Close prompts instead of discarding on the spot.
        app.dispatch_top_action(TopAction::Close, &id, info.unsaved, info.editable);
        assert!(
            matches!(
                app.confirm,
                Some(ConfirmAction::DiscardEdits { then: DiscardThen::CloseEditor })
            ),
            "Close must confirm before dropping a staged rename; got {:?}",
            app.confirm
        );
        assert_eq!(app.drafts.len(), 1, "and must not have dropped it yet");
    }

    /// A draft with **no** staged rename must not gain a prompt: the router
    /// notifies about one specific situation, it is not a new confirmation on
    /// every Save.
    #[test]
    fn save_without_a_staged_rename_does_not_prompt() {
        let mut app = test_app();
        let id = insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        app.focused_module = Some(id);
        app.drafts
            .get_mut(&PathBuf::from("/x/alpha.json"))
            .unwrap()
            .ext
            .meta
            .description = Some("body edit".to_string());
        app.refresh_unsaved_drafts();

        app.request_save_module();
        assert!(app.confirm.is_none(), "a plain body save must not prompt");
        // The write itself fails (the manifest path is fictional) — irrelevant
        // here; this test is about the routing decision, which happens first.
    }

    /// Create a scratch config base with one user extension manifest in it, and
    /// return `(base, manifest path)`. Cleaned up by the caller.
    ///
    /// *Why real files:* the whole point of the Save / Rename split is what ends
    /// up **on disk** — `perform_rename` sweeps stored config and then rewrites
    /// the manifest in place. A mocked writer would only re-assert the mock.
    fn scratch_base(tag: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "ritz-gui-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let ext_dir = base.join("extensions");
        std::fs::create_dir_all(&ext_dir).unwrap();
        let manifest = ext_dir.join("Ritze__Old.json");
        std::fs::write(
            &manifest,
            serde_json::to_string_pretty(&named_manifest("Ritze", "Old")).unwrap(),
        )
        .unwrap();
        (base, manifest)
    }

    /// **The acceptance criterion.** This test's ancestor
    /// (`rename_and_save_writes_the_new_identity_and_the_body_edit`) asserted the
    /// opposite — that Rename wrote the body edit in the same write. That premise
    /// is exactly what the user rejected: "pressing the Rename button currently
    /// also saves the whole config, which is wrong". Rename now writes the ON-DISK
    /// body under the new identity, and the body edit stays pending in the draft.
    ///
    /// Also pins the post-rename bookkeeping triple — the subtle half. The draft
    /// must come out clean of pending *identity* and still dirty in the *body*; a
    /// draft reporting "saved" while holding unwritten edits is issue #34 in
    /// reverse.
    ///
    /// Verified to bite: building the manifest from `snapshot()` again (the old
    /// behaviour) writes `"Description": "body edit"` and fails the absence
    /// assert; re-seeding the draft via `ensure_draft` afterwards loses the
    /// pending edit and fails the `still dirty` assert.
    #[test]
    fn rename_writes_the_new_identity_and_leaves_the_body_edit_pending() {
        let (mut app, base, manifest) = app_with_pending_body_and_rename("rename-only", "New");

        app.perform_rename();

        let after = manifest_json(&manifest);
        assert_eq!(after["Extension"]["Name"], "New", "the staged rename must land");
        assert_eq!(after["Extension"]["Author"], "Ritze");
        assert!(
            after["Extension"].get("Description").is_none(),
            "Rename must NOT write the body edit — it writes the on-disk body: {after}"
        );
        // The manifest is rewritten in place, never renamed (id comes from meta).
        assert!(manifest.exists());

        // The editor stays open, on the new id, with the body edit intact.
        assert_eq!(app.focused_module.as_deref(), Some("Ritze::New::1.0"));
        let d = app.drafts.get(&manifest).expect("the draft must survive the rename");
        assert_eq!(
            d.ext.meta.description.as_deref(),
            Some("body edit"),
            "the pending body edit must survive the post-rename reload round-trip"
        );
        assert_eq!(d.id, "Ritze::New::1.0", "and be re-keyed to the new identity");

        // The triple, in every combination that matters.
        assert!(!d.has_pending_identity(), "the identity landed: staged == on-disk");
        assert_eq!(d.identity.staged_name(), "New");
        assert!(d.dirty(), "the body did NOT land, so it is still dirty");
        assert!(d.has_unsaved_work(), "and still reports unsaved work");
        assert!(d.save_enabled(), "Save is the affordance that commits it, and it is lit");
        assert_eq!(app.unsaved_module_names(), vec!["Ritze::New".to_string()]);

        // Second click, the other operation: now the body lands, under the new name.
        app.request_save_module();
        let after = manifest_json(&manifest);
        assert_eq!(after["Extension"]["Name"], "New");
        assert_eq!(after["Extension"]["Description"], "body edit");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// The removed gate. `perform_rename` used to also be gated on
    /// `!dirty() || save_enabled()`, because it wrote the body and so could
    /// commit edits that `extension::validate` would reject. It no longer writes
    /// the body, so a half-finished field (here: one with no `Variable` yet) is
    /// not a reason to refuse the rename.
    ///
    /// Verified to bite: restoring that filter makes the rename a no-op and the
    /// manifest still says `"Old"`.
    #[test]
    fn rename_succeeds_with_an_invalid_body_in_progress() {
        let (mut app, base, manifest) = app_with_pending_body_and_rename("rename-invalid", "New");
        {
            let d = app.drafts.get_mut(&manifest).unwrap();
            // A field the user has added but not named yet: `validate` rejects an
            // empty `Variable`, so the Save gate is shut.
            d.sections[0]
                .1
                .push(serde_json::from_value(json!({"Type": "toggle", "Variable": ""})).unwrap());
        }
        assert!(app.draft().unwrap().dirty());
        assert!(!app.draft().unwrap().save_enabled(), "premise: the body cannot be saved");

        app.perform_rename();

        let after = manifest_json(&manifest);
        assert_eq!(
            after["Extension"]["Name"], "New",
            "a rename must not be blocked by a body that happens to be mid-edit"
        );
        // And the invalid body is still where the user left it, still unsaveable.
        let d = app.drafts.get(&manifest).expect("the draft must survive");
        assert_eq!(d.sections[0].1.last().unwrap().variable, "");
        assert!(!d.save_enabled());
        assert!(!d.has_pending_identity(), "while the identity half did land");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// When the rename cannot run at all, nothing happens: no write, no closed
    /// editor, no dropped draft, staged identity left for the user to fix.
    #[test]
    fn rename_with_an_invalid_identity_does_nothing_and_keeps_the_editor_open() {
        let mut app = test_app();
        let id = insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        app.focused_module = Some(id);
        {
            let d = app.drafts.get_mut(&PathBuf::from("/x/alpha.json")).unwrap();
            d.ext.meta.description = Some("body edit".to_string());
            d.identity.name = "Renamed".to_string();
            // Invalid, so `perform_rename`'s own gate rejects it and it returns
            // having touched nothing.
            d.identity_error = Some("nope".to_string());
        }
        app.refresh_unsaved_drafts();

        app.perform_rename();

        assert!(app.focused_module.is_some(), "an aborted rename must not close the editor");
        assert_eq!(app.drafts.len(), 1, "an aborted rename must not drop the draft");
        assert_eq!(app.draft().unwrap().identity.name, "Renamed");
        assert_eq!(app.draft().unwrap().ext.meta.description.as_deref(), Some("body edit"));
    }

    // ── Closing the window with unsaved drafts (2026-07-19, issue #33) ───────
    //
    // The data-loss path these cover: closing the settings window — the X
    // button, Alt+F4, or either launch-mode title-bar button — dropped every
    // open module draft without a word. The process exits immediately after, so
    // this is the one loss with no way back.
    //
    // What is NOT covered here, honestly: the viewport round-trip itself.
    // `close_requested()` / `CancelClose` / `Close` only exist inside a live
    // eframe event loop, and driving one needs a window server. Everything below
    // tests the *decision* (`close_needs_confirm` and the state it reads and
    // writes); the frame that carries `pending_close` out to the backend is a
    // single `ctx.send_viewport_cmd` at the tail of `ui()`.

    #[test]
    fn close_gate_predicate_asks_only_when_there_is_unsaved_work_to_lose() {
        assert!(close_needs_confirm(true, false), "unsaved work must stop the close");
        assert!(!close_needs_confirm(false, false), "a clean app must close on one click");
        // Once approved, the close we re-issue must sail through the same guard —
        // otherwise it cancels itself forever and the window can never shut.
        assert!(!close_needs_confirm(true, true));
        assert!(!close_needs_confirm(false, true));
    }

    /// The common case must not cost a click: no drafts → the close is approved
    /// on the spot and the outcome is applied.
    #[test]
    fn closing_with_no_unsaved_work_closes_immediately() {
        let mut app = test_app();
        app.refresh_unsaved_drafts();

        app.request_app_close(EditOutcome::Cancel);

        assert!(app.confirm.is_none(), "a clean close must not raise a dialog");
        assert!(app.pending_close, "and must actually ask the window to close");
        assert_eq!(*app.outcome.lock().unwrap(), EditOutcome::Cancel);
    }

    /// The acceptance criterion: closing with unsaved work raises **the same**
    /// `DiscardEdits` prompt the Profiles-tab switch raises, naming the same
    /// modules, and closes nothing until it is answered.
    #[test]
    fn closing_with_unsaved_work_raises_the_discard_prompt_instead() {
        let mut app = test_app();
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        app.drafts.get_mut(&PathBuf::from("/x/alpha.json")).unwrap().ext.meta.version =
            "9.9".to_string();
        app.refresh_unsaved_drafts();

        app.request_app_close(EditOutcome::Continue);

        assert!(
            matches!(
                app.confirm,
                Some(ConfirmAction::DiscardEdits { then: DiscardThen::ExitApp(_) })
            ),
            "the close must reuse the DiscardEdits prompt; got {:?}",
            app.confirm
        );
        assert!(!app.pending_close, "nothing may close before the prompt is answered");
        assert_eq!(app.drafts.len(), 1, "and the draft must still be there");
        // Same message the Profiles-tab switch produces — same list, same source.
        assert_eq!(app.unsaved_module_names(), vec!["Ritze::Alpha".to_string()]);
        // The outcome must NOT have moved yet: Cancel has to leave the
        // "Editor closing action" default exactly as it was.
        assert_eq!(*app.outcome.lock().unwrap(), EditOutcome::Continue);
    }

    /// **Issue #34's blind spot, on the path where it is unrecoverable.** A draft
    /// whose *only* change is a staged rename is not `dirty()`, but it is unsaved
    /// work — and closing the window destroys it. The guard must ask.
    ///
    /// Verified to bite: swapping `has_unsaved_work()` for `dirty()` in
    /// `refresh_unsaved_drafts` leaves `confirm` at `None` and this fails.
    #[test]
    fn closing_prompts_for_a_rename_only_draft_too() {
        let mut app = test_app();
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        {
            let d = app.drafts.get_mut(&PathBuf::from("/x/alpha.json")).unwrap();
            // Staged identity only — the body is byte-identical to disk.
            d.identity.name = "Renamed".to_string();
            assert!(!d.dirty(), "precondition: a rename-only draft is not dirty");
            assert!(d.has_unsaved_work(), "but it is unsaved work");
        }
        app.refresh_unsaved_drafts();

        app.request_app_close(EditOutcome::Continue);

        assert!(
            matches!(
                app.confirm,
                Some(ConfirmAction::DiscardEdits { then: DiscardThen::ExitApp(_) })
            ),
            "a staged rename must stop the close; got {:?}",
            app.confirm
        );
    }

    /// Confirm on the close prompt drops every draft, applies the carried
    /// outcome, and marks the close approved so the tail of `ui()` re-issues it.
    #[test]
    fn confirming_the_close_prompt_discards_everything_and_approves_the_close() {
        let mut app = test_app();
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        insert_draft(&mut app, "Ritze", "Beta", "/x/beta.json");
        app.refresh_unsaved_drafts();

        app.apply_discard_edits(DiscardThen::ExitApp(EditOutcome::Cancel));

        assert!(app.drafts.is_empty(), "the prompt named them all, so they all go");
        assert!(app.unsaved_drafts.is_empty(), "and the frame's set must follow");
        assert!(app.pending_close, "the close must actually be re-issued");
        assert_eq!(*app.outcome.lock().unwrap(), EditOutcome::Cancel);
        // The re-issued close must now pass the guard rather than re-prompt.
        assert!(!close_needs_confirm(!app.unsaved_drafts.is_empty(), app.pending_close));
    }

    /// Launch mode: "Launch Game" still launches after the user approves the
    /// discard. The guard adds a question, it does not change the answer.
    #[test]
    fn confirming_the_close_prompt_from_launch_game_still_launches() {
        let mut app = test_app();
        app.launch_mode = true;
        *app.outcome.lock().unwrap() = EditOutcome::Cancel; // the configured default
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        app.drafts.get_mut(&PathBuf::from("/x/alpha.json")).unwrap().ext.meta.version =
            "9.9".to_string();
        app.refresh_unsaved_drafts();

        app.request_app_close(EditOutcome::Continue);
        let Some(ConfirmAction::DiscardEdits { then }) = app.confirm.clone() else {
            panic!("expected the discard prompt, got {:?}", app.confirm);
        };
        app.apply_discard_edits(then);

        assert!(app.pending_close);
        assert_eq!(
            *app.outcome.lock().unwrap(),
            EditOutcome::Continue,
            "the button's outcome must survive the detour through the prompt"
        );
    }

    /// Cancel on the close prompt leaves the app fully usable: drafts intact, no
    /// half-closed state, and closing again re-raises the prompt.
    #[test]
    fn cancelling_the_close_prompt_leaves_the_app_open_with_its_drafts() {
        let mut app = test_app();
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        app.drafts.get_mut(&PathBuf::from("/x/alpha.json")).unwrap().ext.meta.version =
            "9.9".to_string();
        app.refresh_unsaved_drafts();
        app.request_app_close(EditOutcome::Continue);

        // What `render_confirm_dialog`'s Cancel branch does, and all it does.
        app.confirm = None;

        assert_eq!(app.drafts.len(), 1, "Cancel must not cost the user anything");
        assert!(!app.pending_close, "and must not leave the window half-closed");
        app.refresh_unsaved_drafts();
        app.request_app_close(EditOutcome::Continue);
        assert!(app.confirm.is_some(), "asking again must work");
    }

    /// Re-entrancy: a close requested while *another* confirmation is already on
    /// screen must not clobber it. The decision the user is part-way through
    /// making wins; the close is dropped and can simply be asked again.
    #[test]
    fn a_close_request_does_not_clobber_a_dialog_already_on_screen() {
        let mut app = test_app();
        insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        app.drafts.get_mut(&PathBuf::from("/x/alpha.json")).unwrap().ext.meta.version =
            "9.9".to_string();
        app.refresh_unsaved_drafts();
        app.confirm = Some(ConfirmAction::DeleteGame("42".to_string()));

        app.request_app_close(EditOutcome::Continue);

        assert!(
            matches!(app.confirm, Some(ConfirmAction::DeleteGame(_))),
            "the pending decision must survive; got {:?}",
            app.confirm
        );
        assert!(!app.pending_close);
    }

    // ---------------------------------------------------------------- issue #29

    /// A `Requires` differing from disk **only** by surrounding whitespace must
    /// not read as dirty, and must not travel to the manifest with its padding.
    ///
    /// `requires_edit` decides `None`-vs-`Some` on `s.trim()` but stores the raw
    /// `s`, so `" enabled "` was kept verbatim; `dirty()` is a serialized-value
    /// comparison, so the draft went permanently "unsaved" with nothing visible
    /// to change back. Evaluation was never affected (`condition::tokenize` skips
    /// whitespace) — this is data hygiene.
    ///
    /// Verified to fail before the fix: without the `normalize_requires` call in
    /// `snapshot()` the first assert fires ("differs only by padding").
    #[test]
    fn requires_padding_is_canonicalised_at_the_snapshot_boundary() {
        let mut d = draft_from(
            json!({
                "Extension": {"Name": "Sample", "Author": "Ritze", "Version": "1.0"},
                "UI": {"Main": [
                    {"Type": "toggle", "Variable": "enabled"},
                    {"Type": "toggle", "Variable": "clear", "Requires": "enabled"}
                ]}
            }),
            true,
        );
        assert!(!d.dirty(), "premise: a freshly seeded draft is clean");

        // What typing a leading/trailing space into the Requires box leaves behind.
        d.sections[0].1[1].requires = Some("  enabled  ".to_string());
        assert!(!d.dirty(), "a Requires differing only by padding is not an edit");
        assert_eq!(
            d.snapshot().ui["Main"][1].requires.as_deref(),
            Some("enabled"),
            "the padding must not reach the written manifest"
        );

        // Whitespace-only trims to the empty string and is NOT collapsed to None:
        // `Some("")` is a legitimate on-disk state (dxvk.json ships one) and
        // nulling it would spuriously dirty every module that has one. See
        // `normalize_requires`.
        d.sections[0].1[1].requires = Some("   ".to_string());
        assert_eq!(d.snapshot().ui["Main"][1].requires.as_deref(), Some(""));

        // A real edit still registers.
        d.sections[0].1[1].requires = Some("enabled AND !other".to_string());
        assert!(d.dirty(), "a genuine Requires change is still an edit");
    }

    // ---------------------------------------------------------------- issue #31

    /// `identity.var_edits` must be seeded in the extension's UI **declaration
    /// order** — the order the user sees the fields in — not in `HashSet` order.
    ///
    /// Verified to fail before the fix: seeding from the `HashSet<String>` of
    /// baseline vars, this asserts unequal on essentially every run (10 keys,
    /// so accidentally landing in declaration order is ~1/10!).
    #[test]
    fn ensure_draft_seeds_var_edits_in_declaration_order() {
        let vars = [
            "zulu", "alpha", "mike", "bravo", "yankee", "charlie", "xray", "delta",
            "whiskey", "echo",
        ];
        let manifest = json!({
            "Extension": {"Name": "Ordered", "Author": "Ritze", "Version": "1.0"},
            "UI": {
                "First": vars[..5].iter().map(|v| json!({"Type": "toggle", "Variable": v}))
                    .collect::<Vec<_>>(),
                "Second": vars[5..].iter().map(|v| json!({"Type": "toggle", "Variable": v}))
                    .collect::<Vec<_>>(),
            }
        });
        let spec: Extension = serde_json::from_value(manifest).unwrap();
        let id = spec.id();

        let mut app = test_app();
        app.all_specs = vec![spec];
        app.all_dirs = vec![PathBuf::from("user")]; // non-bundled → editable
        app.all_manifests = vec![PathBuf::from("/x/ordered.json")];

        app.ensure_draft(&id);

        let d = &app.drafts[&PathBuf::from("/x/ordered.json")];
        assert_eq!(
            d.identity.var_edits.keys().collect::<Vec<_>>(),
            vars.iter().collect::<Vec<_>>(),
            "var_edits must follow UI declaration order across sections"
        );
        // The set the `contains` consumers use is unchanged by the reordering.
        assert_eq!(d.baseline_vars.len(), vars.len());
        assert!(vars.iter().all(|v| d.baseline_vars.contains(*v)));
    }

    // ---------------------------------------------------------------- issue #32

    /// Pins the invariant that makes the focused-only `identity_error` refresh
    /// sound: a background draft's marker may go stale, but refocusing it
    /// recomputes the value before anything reads it, so a stale marker can never
    /// gate a rename. See `ModuleDraft::identity_error`.
    ///
    /// **Not a regression test** — it passes before and after, because #32 turned
    /// out not to be a reachable bug. It exists so that a future change which
    /// starts reading `identity_error` off an unfocused draft, or which drops the
    /// per-frame refresh, fails here instead of shipping.
    #[test]
    fn identity_error_is_refreshed_on_refocus() {
        let mut app = test_app();
        let alpha_id = insert_draft(&mut app, "Ritze", "Alpha", "/x/alpha.json");
        let beta_id = insert_draft(&mut app, "Ritze", "Beta", "/x/beta.json");

        // Alpha, while focused, stages a free name: no error.
        app.focused_module = Some(alpha_id.clone());
        app.drafts.get_mut(&PathBuf::from("/x/alpha.json")).unwrap().identity.name =
            "Gamma".to_string();
        app.refresh_identity_state();
        assert_eq!(app.draft().unwrap().identity_error, None);

        // Focus moves to Beta, which stages the very name Alpha holds. Alpha is no
        // longer refreshed, so its marker is now stale (still None) even though the
        // pair is contested — this is exactly the staleness issue #32 describes.
        app.focused_module = Some(beta_id);
        app.drafts.get_mut(&PathBuf::from("/x/beta.json")).unwrap().identity.name =
            "Gamma".to_string();
        app.refresh_identity_state();
        assert_eq!(
            app.draft().unwrap().identity_error.as_deref(),
            Some("Author + Name already in use"),
            "the focused draft sees the other draft's staged identity"
        );
        assert_eq!(
            app.drafts[&PathBuf::from("/x/alpha.json")].identity_error, None,
            "premise: the unfocused draft's marker is stale"
        );

        // Refocusing Alpha recomputes it BEFORE any read — which is what `ui()`
        // does at the top of every frame. The stale value never reaches a reader,
        // so `perform_rename`'s `identity_error.is_none()` gate cannot be fooled.
        app.focused_module = Some(alpha_id);
        app.refresh_identity_state();
        assert_eq!(
            app.draft().unwrap().identity_error.as_deref(),
            Some("Author + Name already in use"),
            "refocus must correct the stale marker before it can gate a rename"
        );
    }

    // ══ IDE-mode focus / selection regressions (issues #4 #5 #6 #37) ═════════

    /// A scratch config dir holding one manifest per `(author, name)` pair.
    /// Returns the base; the caller removes it.
    ///
    /// *Why real files rather than assigning `all_specs` by hand:* every bug in
    /// this block is about what `reload_extensions` does — its alphabetical
    /// re-sort, and the index/id bookkeeping around it. A hand-built `all_specs`
    /// would skip the exact code under test.
    fn ide_scratch(tag: &str, mods: &[(&str, &str)]) -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "ritz-ide-test-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        let ext_dir = base.join("extensions");
        std::fs::create_dir_all(&ext_dir).unwrap();
        for (author, name) in mods {
            std::fs::write(
                ext_dir.join(format!("{author}__{name}.json")),
                serde_json::to_string_pretty(&named_manifest(author, name)).unwrap(),
            )
            .unwrap();
        }
        base
    }

    fn ide_app(tag: &str, mods: &[(&str, &str)]) -> (GuiApp, PathBuf) {
        let base = ide_scratch(tag, mods);
        let mut app = test_app();
        app.paths = Paths { base: base.clone() };
        app.mode = Mode::Ide;
        app.reload_extensions();
        (app, base)
    }

    /// The row `ide_selected` currently points at, by module name.
    fn selected_name(app: &GuiApp) -> String {
        app.all_specs[app.ide_selected].meta.name.clone()
    }

    /// Issue #5 — the one-frame fallback flash.
    ///
    /// `GuiApp::ui` declares the nav panel *after* its top-of-frame
    /// `sync_focused_draft` but *before* the header band and both columns, and the
    /// nav is where interactive focus changes happen (a tree row click, or the IDE
    /// tab via `select_nav_category`). So focus moving without a draft behind it is
    /// visible for a frame. `focus_module` now builds it on the spot.
    ///
    /// Asserted through `editor_header_info`, because that is the exact predicate
    /// the three affected renderers branch on: `None` is what makes the editor
    /// column print "Loading…" and the header band disappear.
    ///
    /// Verified to bite: deleting the `sync_focused_draft()` call from
    /// `focus_module` makes both asserts fail (`None` / "no draft was built").
    #[test]
    fn focusing_a_module_builds_its_draft_before_any_column_can_render() {
        let (mut app, base) = ide_app("focus-flash", &[("Ritze", "Alpha"), ("Ritze", "Beta")]);
        assert!(app.drafts.is_empty(), "premise: nothing is open yet");

        // Exactly what `render_nav_panel` does with the id an IDE tree click
        // returns, and what `select_nav_category(Ide)` does on mode entry.
        let beta = app.all_specs.iter().find(|s| s.meta.name == "Beta").unwrap().id();
        app.focus_module(beta.clone());

        assert!(!app.drafts.is_empty(), "no draft was built at the focus change");
        assert!(
            app.editor_header_info(&beta).is_some(),
            "the columns render right after the nav panel and must not see None"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #6 — a Ctrl+R hot reload that removes the open module.
    ///
    /// **Already fixed by the multi-draft rework (S4a), and deliberately so.** The
    /// issue was filed against the single-slot `module_draft`, where `ensure_draft`
    /// dropped the draft outright once the spec vanished, leaving both columns on
    /// "module not found". With `drafts` keyed by manifest path, `draft_key` falls
    /// back to scanning the map by id, so the draft survives as an **orphan**, the
    /// editor keeps rendering it, and `orphan_draft_names` surfaces it in the
    /// banner above the tree — which is what makes Save (recreating the manifest)
    /// the recovery path rather than a dead end.
    ///
    /// Kept as a regression guard: this behaviour is easy to "tidy away" by making
    /// `ensure_draft` or `reload_extensions` prune drafts with no spec, which would
    /// silently destroy unsaved work.
    #[test]
    fn a_reload_that_deletes_the_open_module_keeps_its_draft_reachable() {
        let (mut app, base) = ide_app("reload-orphan", &[("Ritze", "Alpha"), ("Ritze", "Beta")]);
        let beta = app.all_specs.iter().find(|s| s.meta.name == "Beta").unwrap().id();
        app.focus_module(beta.clone());
        app.drafts
            .values_mut()
            .next()
            .unwrap()
            .ext
            .meta
            .description = Some("unsaved work".to_string());

        // Someone deletes the manifest behind the app's back; the user hits Ctrl+R.
        std::fs::remove_file(base.join("extensions/Ritze__Beta.json")).unwrap();
        app.reload_extensions();

        assert!(
            !app.all_specs.iter().any(|s| s.id() == beta),
            "premise: the reload really did drop the module"
        );
        assert_eq!(
            app.editor_header_info(&beta).map(|i| i.name),
            Some("Beta".to_string()),
            "the editor must keep rendering the orphaned draft, not 'no longer available'"
        );
        assert_eq!(
            app.orphan_draft_names(),
            vec!["Ritze::Beta".to_string()],
            "and the banner must offer the user a route back to the work"
        );
        assert_eq!(
            app.draft().unwrap().ext.meta.description.as_deref(),
            Some("unsaved work"),
            "the unsaved edit itself must survive the reload"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #37, drift source 1 of 4 — **Fork**.
    ///
    /// The fork is a new alphabetical row, so every index at or after it shifts.
    /// Pre-fix `perform_fork` set `focused_module` and left `ide_selected` alone:
    /// the tree highlighted a different module than the editor was editing.
    ///
    /// Verified to bite: removing `anchor_ide_selection` from `perform_fork` makes
    /// the selection land on "Beta" (the shifted-into row) instead of "Aaa".
    #[test]
    fn forking_moves_the_tree_selection_onto_the_fork() {
        let (mut app, base) = ide_app("ide37-fork", &[("Ritze", "Beta"), ("Ritze", "Zeta")]);
        let beta = app.all_specs.iter().find(|s| s.meta.name == "Beta").unwrap().id();
        app.ide_selected = app.all_specs.iter().position(|s| s.id() == beta).unwrap();
        app.focus_module(beta.clone());

        // "Aaa" sorts ahead of both, so it lands at index 0 and pushes everything
        // down — the renumbering the raw index could not survive.
        app.perform_fork(&beta, "Ritze".to_string(), "Aaa".to_string(), false);

        assert_eq!(app.focused_module.as_deref(), Some("Ritze::Aaa::1.0"), "premise");
        assert_eq!(
            selected_name(&app),
            "Aaa",
            "the tree must highlight the module the editor is editing"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #37, drift source 2 of 4 — **Create**.
    ///
    /// Verified to bite: removing `anchor_ide_selection` from `perform_create`
    /// leaves the selection on "Beta".
    #[test]
    fn creating_a_module_moves_the_tree_selection_onto_it() {
        let (mut app, base) = ide_app("ide37-create", &[("Ritze", "Beta"), ("Ritze", "Zeta")]);
        let beta = app.all_specs.iter().find(|s| s.meta.name == "Beta").unwrap().id();
        app.ide_selected = app.all_specs.iter().position(|s| s.id() == beta).unwrap();
        app.focus_module(beta);

        app.perform_create("Ritze".to_string(), "Aaa".to_string());

        assert_eq!(app.focused_module.as_deref(), Some("Ritze::Aaa::1.0"), "premise");
        assert_eq!(selected_name(&app), "Aaa", "the new module must be the selected row");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #37, drift source 3 of 4 — **Delete**.
    ///
    /// Delete clears focus and lets the `Mode::Ide` reopen invariant re-focus
    /// `all_specs[ide_selected]` next frame, so the index has to be **in range**
    /// and on a sensible neighbour. Deleting the last row is the case that
    /// previously left it dangling one past the end, where the invariant found
    /// `None` and simply never reopened anything.
    ///
    /// Verified to bite: dropping the clamp fallback from `anchor_ide_selection`
    /// (returning early instead) leaves `ide_selected == 2` against a 2-element
    /// list and the reopen assert fails.
    #[test]
    fn deleting_the_last_module_leaves_a_selectable_neighbour() {
        let (mut app, base) =
            ide_app("ide37-delete", &[("Ritze", "Alpha"), ("Ritze", "Beta"), ("Ritze", "Zeta")]);
        let zeta_idx = app.all_specs.iter().position(|s| s.meta.name == "Zeta").unwrap();
        let zeta = app.all_specs[zeta_idx].id();
        app.ide_selected = zeta_idx;
        app.focus_module(zeta);
        let manifest = app.all_manifests[zeta_idx].clone();

        app.delete_module(&manifest, false);

        assert_eq!(app.all_specs.len(), 2, "premise: the module is gone");
        assert!(app.focused_module.is_none(), "premise: delete clears focus");
        assert!(
            app.ide_selected < app.all_specs.len(),
            "ide_selected must stay in range or the reopen invariant finds nothing"
        );
        // Reproduce the `Mode::Ide` invariant at the top of `ui()`.
        let reopen = app.all_specs[app.ide_selected].id();
        app.focus_module(reopen);
        assert_eq!(selected_name(&app), "Beta", "delete lands on the surviving neighbour");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #37, drift source 4 of 4 — a plain **reload that re-sorts**.
    ///
    /// `load_extensions` sorts by `meta.name` on every call, so a module appearing
    /// on disk (an external edit, a Ctrl+R after copying a file in) renumbers every
    /// row after it. This is also the Save path: `save_module` reloads and then
    /// clears focus, and the reopen invariant re-focuses `all_specs[ide_selected]`
    /// — which pre-fix could be an arbitrary neighbour of the module just saved.
    ///
    /// Verified to bite: removing the `prev_ide_id` re-anchor from
    /// `reload_extensions` selects "Aaa" (the row that shifted into index 0).
    #[test]
    fn a_reload_that_resorts_keeps_the_selection_on_the_same_module() {
        let (mut app, base) = ide_app("ide37-resort", &[("Ritze", "Beta"), ("Ritze", "Zeta")]);
        app.ide_selected = app.all_specs.iter().position(|s| s.meta.name == "Beta").unwrap();
        assert_eq!(app.ide_selected, 0, "premise: Beta sorts first of the two");

        std::fs::write(
            base.join("extensions/Ritze__Aaa.json"),
            serde_json::to_string_pretty(&named_manifest("Ritze", "Aaa")).unwrap(),
        )
        .unwrap();
        app.reload_extensions();

        assert_eq!(app.all_specs.len(), 3, "premise: the new module was picked up");
        assert_eq!(
            selected_name(&app),
            "Beta",
            "a re-sort must not slide the selection onto a different module"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #37, the Rename twin of the re-sort case: renaming changes the very
    /// key the sort runs on, so the module moves rows while its file does not — and
    /// its id changes, so `reload_extensions`' own re-anchor (which remembers the
    /// *old* id) cannot find it. `perform_rename` anchors on the new id explicitly.
    ///
    /// Verified to bite: removing `anchor_ide_selection` from `perform_rename`
    /// leaves the selection on "Aaa".
    #[test]
    fn renaming_keeps_the_tree_selection_on_the_renamed_module() {
        let (mut app, base) = ide_app("ide37-rename", &[("Ritze", "Beta"), ("Ritze", "Zeta")]);
        let beta_idx = app.all_specs.iter().position(|s| s.meta.name == "Beta").unwrap();
        let beta = app.all_specs[beta_idx].id();
        assert_eq!(beta_idx, 0, "premise: Beta sorts first");
        app.ide_selected = beta_idx;
        app.focus_module(beta);
        // "Zzz" sorts *after* Zeta, so the renamed module has to move from row 0 to
        // row 1 — the case a stale index cannot survive.
        app.draft_mut().unwrap().identity.name = "Zzz".to_string();
        app.refresh_identity_state();

        app.perform_rename();

        assert_eq!(app.focused_module.as_deref(), Some("Ritze::Zzz::1.0"), "premise");
        assert_eq!(selected_name(&app), "Zzz", "the selection follows the rename");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Issue #4 — IDE mode entered with an empty `all_specs`.
    ///
    /// The report's premise held (the window came up module-less), but its
    /// suggested fix — refusing to switch mode — would have been a regression:
    /// "+ New Module" lives in the IDE nav footer and renders regardless of
    /// focus, so IDE mode is the only way to author a first module into an empty
    /// extensions dir. What was actually wrong was the copy: the panel fell
    /// through to Config mode's game-filter wording, which names a game IDE mode
    /// does not have and a filter that is not why the list is empty.
    ///
    /// Verified to bite: returning the Config string unconditionally from
    /// `empty_central_message` fails the first assert.
    #[test]
    fn the_empty_central_panel_explains_itself_per_mode() {
        assert_eq!(
            empty_central_message(true),
            "No modules loaded. Use \u{201c}+ New Module\u{201d} below the tree to create one."
        );
        assert_eq!(empty_central_message(false), "No extensions apply to this game.");
        assert_ne!(
            empty_central_message(true),
            empty_central_message(false),
            "the two empty states have different causes and must not share one string"
        );
    }
}
