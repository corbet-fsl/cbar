// cbar launcher — a launcher whose layout is a MATRIX: machines across, folders down, appsets within.
//
// The shape is the point. Every other launcher on Wayland is a search box over ONE list, so the
// only way it can express "which machine" or "which kind of thing" is by making you narrow a
// single flat set. A screen is two-dimensional; a matrix uses both axes, so "the editors on
// that workstation" is a POSITION you move to rather than a query you compose.
//
// THREE LEVELS, TWO OF THEM SIMULTANEOUS ON SCREEN:
//
//   columns = machines            (outside: left/right)
//   rows    = folders             (outside: up/down)
//   a cell  = one machine's apps in one folder, as LINES
//   a line  = an APPSET           (inside: up/down picks the line)
//   an item = one app on that line (inside: left/right walks it)
//
// Tab is the only mode key. That single split is what lets the same four arrow keys mean
// something different at each level without a modifier soup, and it is what makes a LINE a
// first-class object: a line is a set you can start in one keystroke, which is a thing neither
// rofi nor fuzzel can express at all.
//
// ── THIS FILE IS THE GTK SHELL ──────────────────────────────────────────────────────────────
// It runs the inventory providers declared in config and draws the core model. Missing config is
// an honest empty launcher; rich fixture data exists only in core tests.
use gtk4 as gtk;

use cbar_launch_service::{submit_detached_batch, warm_launch_service};
use gtk::gdk::{Key, ModifierType};
use gtk::prelude::*;
use gtk::{
    Align, Application, ApplicationWindow, Box as GBox, CssProvider, EventControllerKey, Image,
    Label, Orientation,
};
use gtk4_layer_shell::{KeyboardMode, Layer, LayerShell};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;

mod desktop;
mod icons;
mod provider;
mod stream_model;
// The launcher itself, from the crate that has no toolkit in it. `model::*` is glob-imported
// because this file speaks in its vocabulary throughout -- App, Line, Machine, Focus, State.
use cbar_launcher_core::{config, keymap, model, usage};
use model::*;

type Callback = Rc<dyn Fn()>;
type CallbackSlot = Rc<RefCell<Option<Callback>>>;
type WeakCallbackSlot = std::rc::Weak<RefCell<Option<Callback>>>;
type OutputPreference = Rc<dyn Fn(&[String])>;

fn invalidate_render(holder: &WeakCallbackSlot) {
    let Some(holder) = holder.upgrade() else {
        return;
    };
    let callback = holder.borrow().as_ref().cloned();
    if let Some(callback) = callback {
        callback();
    }
}

/// Keep the GTK object tree bounded without deleting any provider/model data. Ordinary Golden
/// Master inventories fit on one page and are byte-for-byte unchanged; exceptionally large
/// inventories are projected into deterministic pages around the model cursor. Arrow navigation
/// still walks the complete model and materializes the next page as the cursor crosses a boundary.
const MAX_MATERIALIZED_APP_WIDGETS: usize = 512;
const MAX_MATERIALIZED_ROWS: usize = 64;
const MAX_MATERIALIZED_COLUMNS: usize = 8;
const MAX_MATERIALIZED_ICON_BYTES: usize = 128 * 1024 * 1024;
const MAX_APPSET_LAUNCH: usize = 64;

/// Ironbar loads its user stylesheet display-wide at GTK's USER priority. The embedded launcher
/// must keep the standalone Golden Master appearance even when that stylesheet contains ordinary
/// global selectors such as `window`, `label`, or `button`. Its own selectors are all rooted at
/// `.cbar-launcher`, so one step above USER isolates only this window and cannot restyle a bar.
const LAUNCHER_STYLE_PRIORITY: u32 = gtk::STYLE_PROVIDER_PRIORITY_USER + 1;

struct World {
    folders: Vec<String>,
    machines: Vec<Machine>,
    theme: config::Theme,
    layout: config::Layout,
    terminal: Vec<String>,
    surface: String,
    keyboard: String,
    exit_on_focus_loss: bool,
    config: Option<config::Config>,
    error: Option<String>,
}

/// never has to land in the thin space between two widgets to mean something.
fn insert_index_at(container: &GBox, x: f64) -> usize {
    // `compute_bounds` rather than `allocation()`, which GTK deprecated in 4.12. It answers in the
    // coordinate space of the widget you pass, which is exactly the space the drop's own `x` is
    // already in -- so the two are directly comparable with no offset arithmetic to get wrong.
    let mut idx = 0usize;
    let mut child = container.first_child();
    while let Some(w) = child {
        let Some(b) = w.compute_bounds(container) else {
            break;
        };
        let mid = (b.x() + b.width() / 2.0) as f64;
        if x <= mid {
            break;
        }
        idx += 1;
        child = w.next_sibling();
    }
    idx
}

// ── styling ─────────────────────────────────────────────────────────────────────────────────
// Near-black ground, warm off-white ink -- the palette the rest of this desktop already uses.
// The per-machine accent on the column head is the same identity colour the window frames and
// forwarded-window badges use, so a column is recognisable before you read its label.
/// The stylesheet, generated from config values rather than written as a constant. See
/// `config::Theme` -- a colour nobody can reach is this repo carrying one setup's taste.
fn css(t: &config::Theme) -> String {
    format!("{EMBEDDED_STYLE_RESET}\n{}", scoped_golden_css(t))
}

fn scoped_golden_css(t: &config::Theme) -> String {
    golden_css(t)
        .replace("window {", "window.cbar-launcher {")
        .replace("\n.", "\n.cbar-launcher .")
        .replace(", .", ", .cbar-launcher .")
}

/// Neutralize the generic selectors shipped by Ironbar's own example stylesheets before applying
/// the scoped Golden Master sheet. This is intentionally limited to properties those global rules
/// set on the widget kinds used by the launcher: GTK's own theme remains responsible for native
/// controls such as scrollbars.
const EMBEDDED_STYLE_RESET: &str = r#"
window.cbar-launcher {
    font-family: initial;
    font-size: initial;
    background-image: initial;
    border: initial;
    border-radius: initial;
    box-shadow: initial;
    padding: 0;
    margin: 0;
}
.cbar-launcher .root,
.cbar-launcher .search,
.cbar-launcher .colhead,
.cbar-launcher .rowhead,
.cbar-launcher .subrow,
.cbar-launcher .cell,
.cbar-launcher .line,
.cbar-launcher .app,
.cbar-launcher .appname,
.cbar-launcher .hide-action,
.cbar-launcher .dim,
.cbar-launcher .hint,
.cbar-launcher .vector-rail {
    font-family: inherit;
    font-size: inherit;
    background-image: initial;
    border: initial;
    border-radius: initial;
    box-shadow: initial;
    padding: 0;
    margin: 0;
}
.cbar-launcher box,
.cbar-launcher label,
.cbar-launcher image,
.cbar-launcher grid,
.cbar-launcher overlay {
    color: inherit;
    background-color: transparent;
}
"#;

/// The standalone shell's exact stylesheet. `css` only scopes these selectors to the launcher
/// window so embedding it cannot style cbar's bar windows.
fn golden_css(t: &config::Theme) -> String {
    format!(
        "
window {{ background-color: {ground}; color: {fg}; }}
.root {{ padding: 18px; }}
.search {{ font-size: 15px; padding: 8px 12px; margin-bottom: 14px;
          border: 1px solid #262626; border-radius: 6px; background-color: {surface}; }}
.search.empty {{ color: {dim}; }}
.colhead {{ font-weight: bold; font-size: 13px; padding: 4px 8px; margin-bottom: 6px;
           border-bottom: 2px solid #262626; }}
.rowhead {{ font-size: 13px; color: {muted}; padding-right: 12px; }}
.rowhead.active {{ color: {fg}; font-weight: bold; }}
.cell {{ border: 1px solid {border}; border-radius: 6px; padding: 5px; margin: 3px;
        background-color: {surface}; }}
.cell.cursor {{ border-color: {accent}; }}
.cell.inside {{ border-width: 2px; padding: 4px; }}
.cell.empty {{ border-style: dashed; }}
.line {{ border-radius: 4px; padding: 2px; }}
.line.sel {{ background-color: alpha({accent}, 0.10); }}
.app {{ padding: 3px 6px; border-radius: 4px; }}
.app.sel {{ background-color: alpha({accent}, 0.20); }}
.app.arming {{ background-color: alpha({error}, 0.22); box-shadow: inset 0 0 0 1px {error}; }}
/* HOVER IS NOT SELECTION. Weaker than .sel and a different weight, so the thing the keyboard is
   on and the thing the pointer is over can never be mistaken for each other. */
.app:hover {{ background-color: alpha({fg}, 0.10); }}
.line:hover {{ background-color: alpha({fg}, 0.04); }}
.cell:hover {{ border-color: alpha({accent}, 0.45); }}
.colhead:hover, .rowhead:hover, .subrow:hover {{ color: {fg}; }}
.hide-action {{ padding: 0; margin: 0; background-color: alpha({ground}, 0.90); }}
.appname {{ font-size: 12px; }}
.subrow {{ font-size: 10px; color: {muted}; padding-right: 6px; }}
.dim {{ color: {dim}; font-size: 12px; font-style: italic; }}
.hint {{ color: #666666; font-size: 11px; margin-top: 12px; }}
.hint b {{ color: {accent}; }}
",
        ground = t.ground,
        surface = t.surface,
        fg = t.fg,
        muted = t.muted,
        dim = t.dim,
        accent = t.accent,
        border = t.border,
        error = t.error,
    )
}

/// Where the highlight is. Everything a repaint needs to know, and nothing it does not.
#[derive(Clone, Copy, PartialEq, Eq)]
struct Cursor {
    col: usize,
    row: usize,
    line: usize,
    item: usize,
    inside: bool,
}

struct LineW {
    bx: GBox,
    model_line: usize,
    apps: Vec<(usize, GBox)>,
}

struct CellW {
    bx: GBox,
    lines: Vec<LineW>,
    /// A long vector scrolls inside its own cell. Without this boundary the vector's
    /// natural width becomes the width of the entire machine column, leaving the same blank
    /// expanse in every ordinary row above it.
    rail: Option<gtk::ScrolledWindow>,
}

/// Handles for the widgets that carry selection state, so moving the cursor does not have to
/// rebuild the grid to find them again.
///
/// Deliberately NOT a mirror of the whole tree: nothing here is the icons, the labels or the drop
/// targets, because a repaint never touches those. It holds exactly the widgets whose CSS classes
/// change when the cursor moves, which is what keeps a repaint O(1) in the size of the grid.
#[derive(Default)]
struct Painted {
    rowheads: Vec<Label>,
    /// Original model row indices for the bounded page currently materialized as GTK.
    rows: Vec<usize>,
    /// Original model indices for the bounded contiguous page currently materialized as GTK.
    columns: Vec<usize>,
    /// `[row][materialized col]`; `columns` maps a model Cursor into this bounded widget page.
    cells: Vec<Vec<CellW>>,
    page_hint: Option<String>,
    /// What was highlighted at the last paint. The un-highlight step reads this rather than
    /// searching, which is the whole reason a repaint costs the same on a full grid as an empty one.
    last: Option<Cursor>,
}

impl Painted {
    fn reset(&mut self) {
        self.rowheads.clear();
        self.rows.clear();
        self.columns.clear();
        self.cells.clear();
        self.page_hint = None;
        self.last = None;
    }

    /// Add (`on`) or remove (`!on`) the selection classes for one cursor position.
    ///
    /// Every lookup is fallible and simply does nothing when it misses. A cursor can legitimately
    /// point past the end -- an app filtered away by a query, a line emptied by a drag -- and the
    /// clamp that fixes it runs against the model, not against the widgets. Painting must not be
    /// the thing that panics on a state the model considers ordinary.
    fn mark(&self, c: Cursor, on: bool) {
        let Some(row) = self.rows.iter().position(|row| *row == c.row) else {
            return;
        };
        if let Some(rh) = self.rowheads.get(row) {
            class(rh, "active", on);
        }
        let Some(column) = self.columns.iter().position(|column| *column == c.col) else {
            return;
        };
        let Some(cell) = self.cells.get(row).and_then(|r| r.get(column)) else {
            return;
        };
        class(&cell.bx, "cursor", on);
        if !c.inside {
            return;
        }
        class(&cell.bx, "inside", on);
        if let Some(line) = cell.lines.iter().find(|line| line.model_line == c.line) {
            class(&line.bx, "sel", on);
            if let Some((_, app)) = line.apps.iter().find(|(item, _)| *item == c.item) {
                class(app, "sel", on);
            }
        }
    }

    fn contains(&self, c: Cursor) -> bool {
        let Some(row) = self.rows.iter().position(|row| *row == c.row) else {
            return false;
        };
        let Some(column) = self.columns.iter().position(|column| *column == c.col) else {
            return false;
        };
        if !c.inside {
            return self
                .cells
                .get(row)
                .and_then(|cells| cells.get(column))
                .is_some();
        }
        self.cells
            .get(row)
            .and_then(|cells| cells.get(column))
            .and_then(|cell| cell.lines.iter().find(|line| line.model_line == c.line))
            .is_some_and(|line| line.apps.iter().any(|(item, _)| *item == c.item))
    }
}

fn class<W: IsA<gtk::Widget>>(w: &W, name: &str, on: bool) {
    if on {
        w.add_css_class(name);
    } else {
        w.remove_css_class(name);
    }
}

fn machine_app_count(machine: &Machine) -> usize {
    machine
        .cells
        .iter()
        .flatten()
        .map(|line| line.apps.len())
        .fold(0usize, usize::saturating_add)
}

fn materialized_columns(machines: &[Machine], selected: usize, app_budget: usize) -> Vec<usize> {
    if machines.is_empty() {
        return Vec::new();
    }
    let selected = selected.min(machines.len() - 1);
    let mut pages = Vec::new();
    let mut page = Vec::new();
    let mut cost = 0usize;
    for (index, machine) in machines.iter().enumerate() {
        // Empty/offline machines still cost a heading and a cell per visible row, so they cannot
        // bypass the column bound merely because they have no applications.
        let machine_cost = machine_app_count(machine).max(1).min(app_budget.max(1));
        if !page.is_empty()
            && (page.len() >= MAX_MATERIALIZED_COLUMNS
                || cost.saturating_add(machine_cost) > app_budget.max(1))
        {
            pages.push(std::mem::take(&mut page));
            cost = 0;
        }
        page.push(index);
        cost = cost.saturating_add(machine_cost);
    }
    if !page.is_empty() {
        pages.push(page);
    }
    pages
        .into_iter()
        .find(|page| page.contains(&selected))
        .unwrap_or_else(|| vec![selected])
}

fn max_materialized_icons(icon_size: i32) -> usize {
    usize::try_from(icon_size)
        .ok()
        .and_then(|size| size.checked_mul(size))
        .and_then(|pixels| pixels.checked_mul(4))
        // One raw RGBA snapshot plus the GTK texture which presents it.
        .and_then(|bytes| bytes.checked_mul(2))
        .map_or(0, |bytes| MAX_MATERIALIZED_ICON_BYTES / bytes.max(1))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct AppPosition {
    col: usize,
    row: usize,
    line: usize,
    item: usize,
}

struct Materialization {
    columns: Vec<usize>,
    rows: Vec<usize>,
    apps: HashSet<AppPosition>,
    lines: HashSet<(usize, usize, usize)>,
    page: usize,
    pages: usize,
}

impl Materialization {
    fn app_visible(&self, col: usize, row: usize, line: usize, item: usize) -> bool {
        self.apps.contains(&AppPosition {
            col,
            row,
            line,
            item,
        })
    }

    fn line_visible(&self, col: usize, row: usize, line: usize) -> bool {
        self.lines.contains(&(col, row, line))
    }

    fn is_paged(&self) -> bool {
        self.pages > 1
    }
}

/// Build a bounded GTK projection around `cursor` while retaining the complete `Machine` model.
/// Pages are deterministic in the launcher's visual order (row, machine, line, item), so provider
/// arrival order cannot move an existing cursor within a machine and a cursor always names the
/// same model item whether its neighbouring widgets are materialized or not.
fn materialization(machines: &[Machine], cursor: Cursor, icon_size: i32) -> Materialization {
    let app_budget = MAX_MATERIALIZED_APP_WIDGETS
        .min(max_materialized_icons(icon_size))
        .max(1);
    let columns = materialized_columns(machines, cursor.col, app_budget);
    let mut positions = Vec::new();
    for row in 0..machines
        .iter()
        .map(|machine| machine.cells.len())
        .max()
        .unwrap_or(0)
    {
        for &col in &columns {
            let Some(lines) = machines.get(col).and_then(|machine| machine.cells.get(row)) else {
                continue;
            };
            for (line, model_line) in lines.iter().enumerate() {
                for item in 0..model_line.apps.len() {
                    positions.push(AppPosition {
                        col,
                        row,
                        line,
                        item,
                    });
                }
            }
        }
    }

    // Partition by both application widgets and distinct GTK grid rows. This bounds app chips,
    // line boxes, cells and their controllers; a provider with one app in each of 4096 rows is no
    // more expensive than one provider with 4096 apps in a single rail.
    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut rows = HashSet::new();
    for (index, position) in positions.iter().enumerate() {
        let new_row = !rows.contains(&position.row);
        if index > start
            && (index - start >= app_budget || (new_row && rows.len() >= MAX_MATERIALIZED_ROWS))
        {
            ranges.push(start..index);
            start = index;
            rows.clear();
        }
        rows.insert(position.row);
    }
    if start < positions.len() {
        ranges.push(start..positions.len());
    }
    if ranges.is_empty() {
        ranges.push(0..0);
    }

    let anchor = positions
        .iter()
        .position(|position| {
            position.col == cursor.col
                && position.row == cursor.row
                && (!cursor.inside
                    || (position.line == cursor.line && position.item == cursor.item))
        })
        .or_else(|| {
            positions
                .iter()
                .position(|position| position.col == cursor.col && position.row == cursor.row)
        })
        .or_else(|| {
            positions
                .iter()
                .position(|position| position.col == cursor.col)
        })
        .unwrap_or(0);
    let mut page = ranges
        .iter()
        .position(|range| range.contains(&anchor))
        .unwrap_or(0);
    let visible = &positions[ranges[page].clone()];
    let mut apps = visible.iter().copied().collect::<HashSet<_>>();
    let mut lines = visible
        .iter()
        .map(|position| (position.col, position.row, position.line))
        .collect::<HashSet<_>>();
    // A named-but-empty selected line is a real drag target. Keep it when it is the cursor's cell,
    // without materializing the thousands of unrelated empty lines a malicious config could add.
    if columns.contains(&cursor.col)
        && machines
            .get(cursor.col)
            .and_then(|machine| machine.cells.get(cursor.row))
            .and_then(|cell| cell.get(cursor.line))
            .is_some()
    {
        lines.insert((cursor.col, cursor.row, cursor.line));
    }
    let mut visible_rows = visible
        .iter()
        .map(|position| position.row)
        .collect::<HashSet<_>>();
    if columns.contains(&cursor.col)
        && machines
            .get(cursor.col)
            .and_then(|machine| machine.cells.get(cursor.row))
            .is_some()
    {
        visible_rows.insert(cursor.row);
    }
    let mut rows = visible_rows.into_iter().collect::<Vec<_>>();
    rows.sort_unstable();
    if rows.len() > MAX_MATERIALIZED_ROWS {
        let anchor = rows.iter().position(|row| *row == cursor.row).unwrap_or(0);
        let start = (anchor / MAX_MATERIALIZED_ROWS) * MAX_MATERIALIZED_ROWS;
        rows = rows[start..(start + MAX_MATERIALIZED_ROWS).min(rows.len())].to_vec();
        apps.retain(|position| rows.binary_search(&position.row).is_ok());
        lines.retain(|(_, row, _)| rows.binary_search(row).is_ok());
    }

    let all_content_rows = (0..machines
        .iter()
        .map(|machine| machine.cells.len())
        .max()
        .unwrap_or(0))
        .filter(|row| {
            machines
                .iter()
                .any(|machine| machine.cells.get(*row).is_some_and(|cell| !cell.is_empty()))
        })
        .collect::<Vec<_>>();
    if apps.is_empty()
        && let Some(row) = all_content_rows.iter().position(|row| *row == cursor.row)
    {
        page = row / MAX_MATERIALIZED_ROWS;
    }

    // When there is only one bounded page, preserve the Golden Master's empty named rows and every
    // ordinary cell exactly. Paging is therefore invisible for the existing 191-app reference UI.
    if ranges.len() == 1
        && columns.len() == machines.len()
        && all_content_rows.len() <= MAX_MATERIALIZED_ROWS
    {
        rows = all_content_rows.clone();
        for &col in &columns {
            if let Some(machine) = machines.get(col) {
                for &row in &rows {
                    if let Some(cell) = machine.cells.get(row) {
                        lines.extend((0..cell.len()).map(|line| (col, row, line)));
                    }
                }
            }
        }
    }

    Materialization {
        columns,
        rows,
        apps,
        lines,
        page,
        pages: ranges
            .len()
            .max(all_content_rows.len().div_ceil(MAX_MATERIALIZED_ROWS)),
    }
}

/// How far the search bar may be squeezed to let a narrow output honour its width cap.
///
/// A floor rather than nothing at all: the point of giving width back is to keep the far machine
/// column on screen, and a search bar squeezed to a few characters would trade one unusable thing
/// for another. Below this the screen is simply too narrow for the configured grid, which is a
/// scrollbar's problem and not a sizing one.
const MIN_SEARCH_WIDTH: i32 = 200;

/// Hand the idle heap back to the kernel, which on this class of machine means handing it to a
/// compressor.
///
/// A resident launcher spends almost all of its life hidden. What it holds then is ~7MB of clean
/// file-backed pages -- GTK's own code, which the kernel can simply drop and re-read, so it costs
/// nothing to leave alone -- and ~15MB of dirty anonymous pages: the widget tree, the textures,
/// the model. Anonymous pages have no file behind them, so the only way to reclaim them is swap,
/// and they are therefore the entire cost of staying resident.
///
/// MADV_PAGEOUT offers them up at the moment the window hides, rather than waiting for the machine
/// to come under pressure and find them. Where swap is fronted by zswap or backed by zram -- zstd
/// here -- they land compressed in RAM.
///
/// MEASURED, because the estimate was wrong: 56 regions advised, resident 84.6MB -> 83.0MB. About
/// 1.6MB, not the ten-plus this was projected to save. `malloc_trim` above had already returned the
/// bulk of it (52MB of dirty down to 35MB when it was introduced), and what remains is genuinely
/// live -- the widget tree GTK is still holding, which is the whole point of staying resident.
/// Kept because it costs nothing and a hidden launcher should not sit on what it is not using, but
/// the case for residency rests on the 22MB it occupies, not on this.
///
/// Anonymous and writable only, and never the stack: this is about data the program is finished
/// with for now, not about pages it is standing on. A failure anywhere is ignored -- the kernel
/// declining to reclaim is not a reason for a launcher to misbehave.
/// Whether to print the machine-readable trace, decided once from `CBAR_LAUNCHER_TRACE`.
fn tracing_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CBAR_LAUNCHER_TRACE").is_some())
}

/// One line of machine-readable progress, for the headless session check.
///
/// A LAYER SURFACE IS INVISIBLE TO THE COMPOSITOR'S IPC -- it is not a window and never appears in
/// `get_tree` -- so a test has no way to ask where this one mapped, how large it is, or how often
/// it resized. Without something like this the only observable is pixels, and asserting on
/// screenshots means a test that fails whenever a font or a colour changes.
///
/// Off unless asked for, printed to stderr rather than stdout, and deliberately dull: `key=value`
/// pairs a shell can grep, never a format anything is expected to parse cleverly.
fn trace(fields: std::fmt::Arguments<'_>) {
    if tracing_on() {
        eprintln!("cbar-launcher-trace {fields}");
    }
}

fn advance_generation(generation: &Cell<u64>) -> u64 {
    let next = generation.get().wrapping_add(1);
    generation.set(next);
    next
}

fn generation_is_current(generation: &Cell<u64>, expected: u64) -> bool {
    generation.get() == expected
}

/// The launcher surface embedded in cbar's existing `gtk::Application`.
///
/// Cloning this clones GTK/Rc handles only. There is one window and one provider task per
/// configured machine, all owned by the cbar process.
#[derive(Clone)]
pub struct LauncherUi {
    window: ApplicationWindow,
    display: gtk::gdk::Display,
    reveal: Rc<dyn Fn()>,
    providers: Rc<RefCell<Option<Rc<provider::ProviderManager>>>>,
    interaction_generation: Rc<Cell<u64>>,
    /// Own the render callback slot for exactly as long as this UI is live. Widget controllers
    /// intentionally retain only weak references to the slot, so omitting this owner makes every
    /// post-build drag, hide and middle-click invalidation silently stop rendering.
    render_holder: CallbackSlot,
    /// A complete UI replacement is delayed while launch receipts still belong to this state.
    /// Otherwise a late receipt can persist an old whole-usage snapshot over launches already
    /// recorded by the replacement UI.
    inflight_launches: Rc<RefCell<HashSet<(String, String)>>>,
    source_config: Option<config::Config>,
    icon_identity: Option<(i32, u64)>,
    style_provider: CssProvider,
    state: Rc<RefCell<State>>,
    state_baseline: Rc<RefCell<MutableStateSnapshot>>,
}

/// File-backed inputs prepared on cbar's blocking runtime before GTK constructs any launcher
/// widgets. The type is intentionally opaque outside this crate; it carries no GTK object and is
/// safe to discard if an explicit show initializes the launcher before startup warmup completes.
pub struct PreparedLauncher {
    config: Result<Option<config::Config>, String>,
    placement: (Placement, Option<String>),
    visibility: (Visibility, Option<String>),
    usage: (usage::Usage, Option<String>),
    icons: Option<icons::PreparedIcons>,
}

/// GTK-owned theme metadata captured cheaply on the event thread. The filesystem walk and
/// bounded cache read derived from it are deliberately performed later on cbar's blocking pool.
pub struct IconThemeSnapshot(icons::ThemeSnapshot);

#[derive(Clone, PartialEq)]
struct MutableStateSnapshot {
    placement: Placement,
    placement_writable: bool,
    visibility: Visibility,
    visibility_writable: bool,
    usage: usage::Usage,
    usage_writable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ReplacementDecision {
    rebuild: bool,
    transfer_current: bool,
    advance_baseline: bool,
}

fn replacement_decision(
    config_changed: bool,
    launches_inflight: bool,
    baseline: &MutableStateSnapshot,
    current: &MutableStateSnapshot,
    prepared: &MutableStateSnapshot,
) -> ReplacementDecision {
    if launches_inflight {
        return ReplacementDecision {
            rebuild: false,
            transfer_current: false,
            advance_baseline: false,
        };
    }
    let advance_baseline = prepared == current;
    let external_state_changed = current == baseline && prepared != current;
    let transfer_current = config_changed && current != baseline && prepared != current;
    ReplacementDecision {
        rebuild: config_changed || external_state_changed,
        transfer_current,
        advance_baseline,
    }
}

impl PreparedLauncher {
    fn state_snapshot(&self) -> MutableStateSnapshot {
        MutableStateSnapshot {
            placement: self.placement.0.clone(),
            placement_writable: self.placement.1.is_none(),
            visibility: self.visibility.0.clone(),
            visibility_writable: self.visibility.1.is_none(),
            usage: self.usage.0.clone(),
            usage_writable: self.usage.1.is_none(),
        }
    }

    fn replace_state(&mut self, snapshot: MutableStateSnapshot) {
        let retained_error = |writable: bool, kind: &str| {
            (!writable).then(|| format!("retained {kind} state after an earlier read failure"))
        };
        self.placement = (
            snapshot.placement,
            retained_error(snapshot.placement_writable, "placement"),
        );
        self.visibility = (
            snapshot.visibility,
            retained_error(snapshot.visibility_writable, "visibility"),
        );
        self.usage = (
            snapshot.usage,
            retained_error(snapshot.usage_writable, "usage"),
        );
    }

    fn retain_failed_state_from(&mut self, current: &MutableStateSnapshot) {
        // A read/parse error is not evidence that the user's state became empty. Retain the live
        // value while leaving the error intact, which makes the replacement fail closed
        // (`*_writable=false`) and allows a later valid read to recover automatically.
        if self.placement.1.is_some() {
            self.placement.0 = current.placement.clone();
        }
        if self.visibility.1.is_some() {
            self.visibility.0 = current.visibility.clone();
        }
        if self.usage.1.is_some() {
            self.usage.0 = current.usage.clone();
        }
    }
}

/// Load every bounded file-backed launcher input on a blocking worker. GTK-specific color
/// validation intentionally remains on GTK because GDK owns the canonical parser, but neither an
/// IPC reveal nor cbar's shared event thread performs configuration or mutable-state I/O.
pub fn prepare() -> PreparedLauncher {
    let _ = model::wait_for_state_writes(std::time::Duration::from_millis(250));
    PreparedLauncher {
        config: config::load(),
        placement: load_placement(),
        visibility: load_visibility(),
        usage: usage::load(),
        icons: None,
    }
}

pub fn capture_icon_theme(display: &gtk::gdk::Display) -> Option<IconThemeSnapshot> {
    let theme = gtk::IconTheme::for_display(display);
    icons::capture_theme(&theme).map(IconThemeSnapshot)
}

/// Finish icon cache preparation on a blocking worker. No GTK object crosses threads: the
/// snapshot is ordinary strings/paths and the result contains only Arc-backed RGBA bytes.
pub fn prepare_icons(
    mut prepared: PreparedLauncher,
    theme: Option<IconThemeSnapshot>,
) -> PreparedLauncher {
    let px = prepared
        .config
        .as_ref()
        .ok()
        .and_then(Option::as_ref)
        .map(|config| config.theme.icon_size);
    if let (Some(px), Some(theme)) = (px, theme) {
        prepared.icons = Some(icons::PreparedIcons::prepare(px, theme.0));
    }
    prepared
}

/// Load the configured launcher's bounded config and mutable state without touching GTK. Missing
/// or structurally invalid config skips speculative warmup; GTK-specific color validation remains
/// part of attach and produces the same honest error window as an explicit show.
pub fn prepare_if_configured() -> Option<PreparedLauncher> {
    let config = config::load();
    if !matches!(&config, Ok(Some(_))) {
        return None;
    }
    let _ = model::wait_for_state_writes(std::time::Duration::from_millis(250));
    Some(PreparedLauncher {
        config,
        placement: load_placement(),
        visibility: load_visibility(),
        usage: usage::load(),
        icons: None,
    })
}

fn prepared_config_requires_rebuild(
    current: Option<&config::Config>,
    prepared: &PreparedLauncher,
) -> bool {
    match &prepared.config {
        Ok(next) => current != next.as_ref(),
        // A half-written declarative replacement retains the last coherent UI and retries on the
        // next action. Deletion is different: Ok(None) above intentionally replaces it with the
        // honest empty launcher, so removed configuration never persists indefinitely.
        Err(_) => false,
    }
}

impl std::fmt::Debug for LauncherUi {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LauncherUi")
            .field("visible", &self.window.is_visible())
            .finish_non_exhaustive()
    }
}

impl LauncherUi {
    pub fn attach_prepared(
        application: &Application,
        runtime: &tokio::runtime::Handle,
        prepared: PreparedLauncher,
        display: &gtk::gdk::Display,
    ) -> Self {
        Self::attach_prepared_with_dismiss(application, runtime, prepared, display, Rc::new(|| {}))
    }

    /// Attach the embedded launcher and report dismissals initiated inside its own UI. The cbar
    /// owner uses this to invalidate an in-flight preparation and keep its visibility intent in
    /// sync after Escape, focus loss, or a successful launch.
    pub fn attach_prepared_with_dismiss(
        application: &Application,
        runtime: &tokio::runtime::Handle,
        prepared: PreparedLauncher,
        display: &gtk::gdk::Display,
        on_dismiss: Rc<dyn Fn()>,
    ) -> Self {
        build(application, runtime, prepared, display, on_dismiss)
    }

    /// Map the already-built window first; refresh is only a broadcast to independent providers.
    pub fn show(&self) {
        (self.reveal)();
        self.window.present();
    }

    pub fn hide(&self) {
        advance_generation(&self.interaction_generation);
        self.window.set_visible(false);
    }

    pub fn toggle(&self) {
        if self.window.is_visible() {
            self.hide();
        } else {
            self.show();
        }
    }

    pub fn refresh(&self) {
        if let Some(providers) = self.providers.borrow().as_ref() {
            providers.refresh();
        }
    }

    pub fn is_visible(&self) -> bool {
        self.window.is_visible()
    }

    pub fn owns_window(&self, window: &gtk::Window) -> bool {
        window == self.window.upcast_ref::<gtk::Window>()
    }

    /// Whether this prepared snapshot changes any launcher policy, including fields captured by
    /// GTK controllers/window construction such as theme, keymap, terminal or focus semantics.
    /// Such a change is applied by atomically replacing the complete launcher UI, never by storing
    /// a partially-applied config that would remain sticky until cbar restarts.
    pub fn prepare_replacement(&self, prepared: &mut PreparedLauncher) -> bool {
        // Invalid config is a transient read failure: retain the entire last-known-good launcher,
        // including its mutable state, and retry on the next action.
        if prepared.config.is_err() {
            return false;
        }
        if prepared
            .config
            .as_ref()
            .ok()
            .and_then(Option::as_ref)
            .is_some_and(|config| !config_colors_are_valid(config))
        {
            // GTK/GDK owns the accepted CSS color grammar, so this check cannot run in the
            // file-preparation worker. A semantically invalid theme is still a failed reload:
            // retain the coherent resident UI rather than replacing it with the startup error
            // shell and making the last-good configuration unrecoverable until restart.
            return false;
        }
        let config_changed =
            prepared_config_requires_rebuild(self.source_config.as_ref(), prepared);
        let icon_changed = prepared
            .icons
            .as_ref()
            .map(icons::PreparedIcons::identity)
            .is_some_and(|identity| Some(identity) != self.icon_identity);
        let current = {
            let state = self.state.borrow();
            MutableStateSnapshot {
                placement: state.placement.clone(),
                placement_writable: state.placement_writable,
                visibility: state.visibility.clone(),
                visibility_writable: state.visibility_writable,
                usage: state.usage.clone(),
                usage_writable: state.usage_writable,
            }
        };
        prepared.retain_failed_state_from(&current);
        let prepared_state = prepared.state_snapshot();
        let baseline = self.state_baseline.borrow().clone();
        let decision = replacement_decision(
            config_changed || icon_changed,
            !self.inflight_launches.borrow().is_empty(),
            &baseline,
            &current,
            &prepared_state,
        );
        if decision.advance_baseline {
            *self.state_baseline.borrow_mut() = current.clone();
        }
        if decision.transfer_current {
            // A drag/hide/launch save may still be queued while config preparation reads disk.
            // Transfer the live snapshot into the replacement so an atomic config rebuild cannot
            // roll the user's just-made state back to an older file image.
            prepared.replace_state(current);
        }
        decision.rebuild
    }

    /// Retire a replaced launcher and its display-global (but selector-scoped) stylesheet.
    pub fn retire(&self) {
        advance_generation(&self.interaction_generation);
        self.providers.borrow_mut().take();
        self.render_holder.borrow_mut().take();
        self.window.close();
        gtk::style_context_remove_provider_for_display(&self.display, &self.style_provider);
    }
}

/// The first configured output that is actually attached, if any.
///
/// TWO KINDS OF MATCH, because a monitor has two kinds of name and they behave differently.
///
/// The CONNECTOR (`DP-1`, `HDMI-A-1`) is an exact handle and is matched exactly: it is short,
/// there is one per output, and a substring rule on something that short would have `DP-1` claim
/// `DP-11`.
///
/// Everything else is matched as a case-insensitive SUBSTRING of the monitor's descriptive names,
/// and that is not laxity, it is the shape of the data. GDK fills `manufacturer` and `model` only
/// when the backend hands them over separately; on wlroots compositors both come back as the
/// literal string `Unknown` and the entire identity arrives in the DESCRIPTION instead:
///
/// ```text
/// connector "DP-1"   manufacturer "Unknown"   model "Unknown"
/// description "Dell Inc. DELL U4323QE DPMH1P3 (DP-1)"
/// ```
///
/// A configured `DELL U4323QE` has to find that. Exact-matching a field that also carries the
/// vendor, the serial and the connector could only ever fail, and failing here is quiet -- it
/// falls back to the compositor's choice, which looks exactly like the option not being read.
fn preferred_monitor(outputs: &[String], display: &gtk::gdk::Display) -> Option<gtk::gdk::Monitor> {
    if outputs.is_empty() {
        return None;
    }
    let monitors = display.monitors();
    let attached: Vec<gtk::gdk::Monitor> = (0..monitors.n_items())
        .filter_map(|i| monitors.item(i).and_downcast::<gtk::gdk::Monitor>())
        .collect();
    // ORDER COMES FROM THE CONFIG, not from the display list: the outer loop is the preference and
    // the inner one is merely what is plugged in. Iterating the monitors outside would return
    // whichever screen the compositor happens to list first among the matches, which is exactly the
    // arbitrary answer this option exists to replace.
    outputs.iter().find_map(|wanted| {
        let wanted = wanted.trim().to_lowercase();
        if wanted.is_empty() {
            return None;
        }
        attached
            .iter()
            .find(|monitor| monitor_matches(monitor, &wanted))
            .cloned()
    })
}

/// Whether one already-lowercased configured name identifies this monitor.
fn monitor_matches(monitor: &gtk::gdk::Monitor, wanted: &str) -> bool {
    let text = |value: Option<gtk::glib::GString>| {
        value.map(|s| s.trim().to_lowercase()).unwrap_or_default()
    };
    if text(monitor.connector()) == wanted {
        return true;
    }
    // "Unknown" is not a name, it is GDK saying it was told nothing -- and it is the SAME
    // non-answer on every output, so honouring it would make one configured word match whichever
    // screen happened to be enumerated first.
    let known = |value: String| {
        if value.is_empty() || value == "unknown" {
            None
        } else {
            Some(value)
        }
    };
    let manufacturer = known(text(monitor.manufacturer()));
    let model = known(text(monitor.model()));
    let described = known(text(monitor.description()));
    let full = match (&manufacturer, &model) {
        (Some(m), Some(n)) => Some(format!("{m} {n}")),
        _ => None,
    };
    [model, described, full, manufacturer]
        .into_iter()
        .flatten()
        .any(|name| name.contains(wanted))
}

fn build(
    application: &Application,
    runtime: &tokio::runtime::Handle,
    prepared: PreparedLauncher,
    display: &gtk::gdk::Display,
    on_dismiss: Rc<dyn Fn()>,
) -> LauncherUi {
    warm_launch_service();
    let source_config = prepared.config.as_ref().ok().and_then(Clone::clone);
    let state_baseline = prepared.state_snapshot();
    let prepared_icons = prepared.icons;
    let icon_identity = prepared_icons.as_ref().map(icons::PreparedIcons::identity);
    let world = world_from_loaded(prepared.config);
    let placement_loaded = prepared.placement;
    let visibility_loaded = prepared.visibility;
    let usage_loaded = prepared.usage;
    let World {
        folders,
        machines: base,
        theme,
        layout,
        terminal: terminal_cmd_outer,
        surface: surface_mode,
        keyboard: keyboard_mode,
        exit_on_focus_loss,
        config: loaded_config,
        error: config_error,
    } = world;
    // A placement that exists and does not parse is reported, never assumed empty: the next drag
    // rewrites whatever we decide it was, so guessing "nothing" would overwrite a real arrangement.
    let (placement, placement_error) = placement_loaded;
    let (visibility, visibility_error) = visibility_loaded;
    let (loaded_usage, usage_error) = usage_loaded;
    let placement_writable = placement_error.is_none();
    let visibility_writable = visibility_error.is_none();
    let usage_writable = usage_error.is_none();
    let startup_error = config_error
        .clone()
        .or(placement_error)
        .or(visibility_error)
        .or(usage_error);

    // NO default size. A launcher should be exactly as big as what it is showing: a fixed size
    // leaves dead space under a short grid and clips a tall one, and BOTH are wrong for a surface
    // whose whole content is known before it maps. Unanchored, a layer surface takes the natural
    // size GTK computes from the children, so the window hugs the matrix and grows with it -- one
    // more folder row makes it taller, a fourth machine makes it wider, with nothing to re-tune.
    //
    // The search entry carries the only explicit measurement, a minimum width, so an empty or
    // heavily-filtered grid cannot collapse the window to a sliver mid-keystroke.
    let window = ApplicationWindow::builder()
        .application(application)
        .build();
    // The embedded launcher must use cbar's default display because gtk4-layer-shell has one
    // process-global Wayland proxy. The scoped USER+1 provider and explicit reset above isolate the
    // standalone Golden Master styling while keeping every layer surface on that one connection.
    window.set_display(display);
    window.add_css_class("cbar-launcher");

    // Layer shell: an overlay that OWNS the keyboard while open. Exclusive rather than OnDemand
    // because every key here is a navigation key -- a launcher that only half-takes the keyboard
    // sends arrow keys to whatever was focused underneath it.
    //
    // CBAR_LAUNCHER_NO_LAYER=1 drops back to an ordinary toplevel. That is not a fallback for hosts
    // without layer-shell (there are none here); it is for WORKING ON THIS FILE. A layer surface
    // is invisible to the compositor's window tree and grabs the keyboard exclusively, which makes
    // it exactly the wrong thing to iterate a layout inside -- a toplevel tiles, appears in
    // `get_tree`, and can be screenshotted and closed like anything else.
    if surface_mode == "layer" && std::env::var_os("CBAR_LAUNCHER_NO_LAYER").is_none() {
        window.init_layer_shell();
        window.set_layer(Layer::Overlay);
        // EXCLUSIVE by default, and this is a correction: on-demand reads like the polite choice
        // and does not work. On every released sway (1.10-1.12) and its forks, `handle_map` grants
        // a mapping layer surface focus and then the `arrange_layers` call at the end of the SAME
        // handler takes it straight back for anything that is not EXCLUSIVE -- so the launcher
        // maps and never receives a key. Every shipping launcher defaults to exclusive for this
        // reason. Configurable, because the day a compositor fixes it, on-demand is the nicer
        // behaviour and nobody should need a new build to use it.
        //
        // SET BEFORE `present()`, always: sway reads the mode at map time out of the surface's
        // INITIAL commit, and gtk4-layer-shell only puts it there if it was set on the window
        // before the surface was created. A mode applied after presenting is silently ignored.
        window.set_keyboard_mode(match keyboard_mode.as_str() {
            "ondemand" => KeyboardMode::OnDemand,
            "none" => KeyboardMode::None,
            _ => KeyboardMode::Exclusive,
        });
        trace(format_args!("surface=layer-shell keyboard={keyboard_mode}"));
    }

    let provider = CssProvider::new();
    provider.load_from_string(&css(&theme));
    gtk::style_context_add_provider_for_display(display, &provider, LAUNCHER_STYLE_PRIORITY);

    if let Some(e) = &config_error {
        eprintln!("cbar launcher: {e}");
    }

    // One theme handle and one texture cache for the life of the process. The active theme and its
    // search path are part of the cache stamp, so they must exist before the persisted pixels load.
    let icon_theme = gtk::IconTheme::for_display(display);
    let icon_cache = Rc::new(RefCell::new(icons::Icons::from_prepared(
        prepared_icons.unwrap_or_else(|| icons::PreparedIcons::empty(None, theme.icon_size, 0)),
    )));

    let state = Rc::new(RefCell::new(State {
        folders,
        layout: layout.clone(),
        usage: loaded_usage,
        usage_writable,
        // Two standard errors, ~95% confidence. Lower and the grid twitches; higher and a real
        // preference takes weeks to show up.
        z: 2.0,
        half_life_days: usage::HALF_LIFE_DAYS,
        base,
        placement,
        placement_writable,
        visibility,
        visibility_writable,
        machines: Vec::new(),
        view: Vec::new(),
        col: 0,
        row: 0,
        line: 0,
        item: 0,
        item_goal: 0,
        focus: Focus::Outside,
        query: String::new(),
    }));
    let layout = Rc::new(RefCell::new(layout));
    // Carry arrangements written before ids existed. Runs before the first rebuild, because a
    // rebuild against unmigrated state would find nothing and draw the computed grouping -- which
    // looks exactly like "my arrangement is gone".
    state.borrow_mut().migrate_names_to_ids();

    // Applies saved filings, then populates `view` with an empty query, i.e. everything.
    state.borrow_mut().rebuild();

    let root = GBox::new(Orientation::Vertical, 0);
    root.add_css_class("root");

    // The search bar begins where the FIRST MACHINE COLUMN begins, not at the window edge: it
    // searches the machines, not the folder labels, so starting it over the label gutter would
    // line it up with the one thing it has nothing to do with. `spacer` is an empty widget put in
    // the same size group as the folder labels each render, so it tracks that column's real width
    // instead of guessing a margin that goes wrong the moment a folder is renamed.
    let searchrow = GBox::new(Orientation::Horizontal, 0);
    // THE CORNER, and the search box starting where the MACHINES start.
    //
    // The search row reserved the width of one label column, which was right while there was one.
    // With a folder column and a subcategory column beside it the box began under the
    // subcategories instead of under the first machine, so the thing you type into did not line up
    // with the thing it filters.
    //
    // Two spacers, one in each label column's size group, so the corner is exactly as wide as both
    // -- computed rather than guessed, and it stays right when a longer folder name changes the
    // first column's width.
    let spacer = GBox::new(Orientation::Horizontal, 0);
    let spacer_folder = Label::new(None);
    let spacer_sub = Label::new(None);
    spacer.append(&spacer_folder);
    spacer.append(&spacer_sub);

    // And since that corner is now a real space rather than a gap, it can hold something. Empty
    // unless configured: a launcher shipping someone else's mark would be wearing it.
    if !theme.logo.is_empty() {
        let logo = if std::path::Path::new(&theme.logo).is_absolute() {
            Image::from_file(&theme.logo)
        } else {
            Image::from_icon_name(&theme.logo)
        };
        logo.set_pixel_size(theme.logo_size);
        logo.set_halign(Align::Start);
        spacer.append(&logo);
    }
    searchrow.append(&spacer);

    let search = Label::new(None);
    search.set_xalign(0.0);
    search.set_hexpand(true);
    search.add_css_class("search");
    search.set_width_request(theme.width);
    searchrow.append(&search);
    root.append(&searchrow);

    // A real inventory is hundreds of applications, and unconstrained "size to content" turns it
    // into a window taller than the display, clipped at both ends with no
    // way to reach the middle. The grid keeps its natural size until it hits a ceiling and then
    // scrolls, so a handful of machines still gets a window that hugs its content.
    let scroller = gtk::ScrolledWindow::new();
    // BOTH AXES. Horizontal was Never, which is only safe while the content happens to fit: add
    // machines until the grid is wider than the display and the far columns become unreachable by
    // any means at all -- no scrollbar, no keyboard, and a layer surface has no titlebar to drag.
    // The same failure the height cap already existed to prevent, on the axis nobody had hit yet.
    scroller.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Automatic);
    scroller.set_propagate_natural_height(true);
    scroller.set_propagate_natural_width(true);
    // Relative to the DISPLAY, not a magic number. A launcher that is 820px tall is fine on this
    // panel and wrong on the next one, and the failure is not cosmetic -- overrun the screen and
    // the top and bottom are simply unreachable, since a layer surface has no titlebar to drag.
    // Two thirds leaves the session visible behind it, which is most of why this is an overlay
    // rather than a window.
    // Before the surface maps GTK cannot tell us which output the compositor will choose, so the
    // opening cap is the SMALLEST attached one on BOTH axes. Overflowing a panel is the failure
    // with no way out -- a layer surface has no titlebar to drag, so whatever went past the edge
    // is simply gone -- where a window that maps too small corrects itself one signal later, as
    // soon as the compositor names the output it actually put us on.
    let smallest = |pick: fn(&gtk::gdk::Rectangle) -> i32, fallback: i32| {
        let ms = display.monitors();
        (0..ms.n_items())
            .filter_map(|i| ms.item(i).and_downcast::<gtk::gdk::Monitor>())
            .map(|m| pick(&m.geometry()))
            .filter(|v| *v > 0)
            .min()
            .unwrap_or(fallback)
    };
    let screen_h = smallest(|g| g.height(), 1080);
    let screen_w = smallest(|g| g.width(), 1920);
    scroller.set_max_content_height((screen_h as f64 * theme.max_height_fraction) as i32);
    // The width cap is the same rule as the height one and exists for the same reason: content is
    // allowed to decide the window's size right up to the point where it would put part of itself
    // off the screen, and past that it scrolls instead.
    scroller.set_max_content_width((screen_w as f64 * theme.max_width_fraction) as i32);

    // THE OUTPUT WE ARE ACTUALLY ON -- on every map, on both axes, in both directions.
    //
    // A resident process makes this decision repeatedly, and it used to be made once and then
    // remembered. Whichever output the FIRST resolution happened to name sized the window for the
    // rest of the process, which on a desk with screens of different sizes is a launcher that opens
    // with machine columns hanging off the right edge and never recovers:
    //
    //   * The cap was applied with `set_default_width`, and a GTK default size is REMEMBERED. It
    //     survives the hide, and every later size negotiation is computed from it rather than from
    //     what the content measures, so one narrow answer outlives the situation that produced it.
    //   * Raising the ScrolledWindow's maximum again on a wider output did nothing, because that
    //     maximum only bounds a natural width the window had stopped consulting.
    //
    // So the caps are re-derived from the entered output every map, height included -- deriving it
    // from the smallest attached screen was the same mistake one axis over -- and the size is
    // measured and requested outright rather than left to be remembered.
    // WHEN THIS SHOWING BEGAN, so the frame callback below can say how long it took to become
    // pixels. Every other number this program reports is internal work; this is the only one that
    // measures what a person waits for.
    let revealed_at: Rc<std::cell::Cell<Option<std::time::Instant>>> =
        Rc::new(std::cell::Cell::new(None));

    let apply_monitor: Rc<dyn Fn(&gtk::gdk::Monitor)> = Rc::new({
        let scroller = scroller.clone();
        let root = root.clone();
        let search = search.clone();
        let window = window.downgrade();
        let search_width = theme.width;
        let width_fraction = theme.max_width_fraction;
        let height_fraction = theme.max_height_fraction;
        move |monitor: &gtk::gdk::Monitor| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let t_settle = std::time::Instant::now();
            let geometry = monitor.geometry();
            if geometry.width() <= 0 || geometry.height() <= 0 {
                return;
            }
            let width_cap = (geometry.width() as f64 * width_fraction) as i32;
            scroller.set_max_content_width(width_cap);
            scroller.set_max_content_height((geometry.height() as f64 * height_fraction) as i32);

            // THE SEARCH BAR DOES NOT GET TO OUTVOTE THE SCREEN.
            //
            // `theme.width` is a minimum, and a minimum is a floor no cap can reach under: at the
            // default 560, plus the row's padding and the label column beside it, the window cannot
            // measure much under 700px however small the fraction makes the cap. On a narrow panel
            // that quietly turns the cap off -- on precisely the screen it exists to protect, since
            // a wide one was never in danger. Hand the excess back by asking the search bar for
            // less: it is the only widget here whose width is a preference rather than content, and
            // a shorter search bar is a smaller thing to lose than the right-hand machine column.
            //
            // Reset first, so returning to a large screen restores the configured width instead of
            // inheriting whatever the smallest screen visited so far settled on.
            search.set_width_request(search_width);
            let (minimum, _, _, _) = root.measure(gtk::Orientation::Horizontal, -1);
            let overrun = minimum - width_cap;
            if overrun > 0 {
                search.set_width_request((search_width - overrun).max(MIN_SEARCH_WIDTH));
            }

            // MEASURE, THEN ASK FOR EXACTLY THAT -- rather than clearing the size and hoping the
            // window follows its content down.
            //
            // It will not. Dropping the default size lets a toplevel GROW, because GTK has to
            // honour a minimum that just increased, and that half works on its own. Shrinking is
            // not symmetric: a mapped toplevel keeps whatever size it has, and a smaller cap
            // merely leaves the ScrolledWindow with room to spare, so the window sails on at its
            // old width with most of it hanging off the side of a smaller screen. Only an explicit
            // request moves it in both directions, and the size to request is the one the content
            // would have chosen anyway, now that the caps describe the output we are really on.
            let (_, width, _, _) = root.measure(gtk::Orientation::Horizontal, -1);
            let (_, height, _, _) = root.measure(gtk::Orientation::Vertical, width);
            window.set_default_size(width, height);
            trace(format_args!(
                "settle us={} output={} screen={}x{} size={}x{} cap={}x{} min={}",
                t_settle.elapsed().as_micros(),
                monitor.connector().unwrap_or_default(),
                geometry.width(),
                geometry.height(),
                width,
                height,
                width_cap,
                (geometry.height() as f64 * height_fraction) as i32,
                minimum
            ));
        }
    });

    {
        let apply_monitor = apply_monitor.clone();
        let revealed_at = revealed_at.clone();
        window.connect_realize(move |w| {
            let Some(surface) = w.surface() else { return };
            // ONCE PER MAP, and this guard is load-bearing rather than tidiness.
            //
            // A window large enough to overlap two outputs gets `enter-monitor` for both. Sizing
            // for the second changes which outputs it overlaps, which delivers another enter, which
            // resizes it back: measured on a two-output session, one open resized the window five
            // times, alternating between the two caps before settling. Every one of those is a full
            // re-measure and relayout of the whole grid, and it is visible.
            //
            // The surface does not migrate while it is up -- a launcher is opened, used and
            // dismissed -- so the first output named after a map is the answer, and later enters
            // for the same showing have nothing to add. The flag reopens on the next map.
            let settled = Rc::new(std::cell::Cell::new(false));
            surface.connect_enter_monitor({
                let apply_monitor = apply_monitor.clone();
                let settled = settled.clone();
                move |_, monitor| {
                    if settled.replace(true) {
                        return;
                    }
                    apply_monitor(monitor);
                }
            });
            // A BACKSTOP FOR BACKENDS THAT ENTER EARLY. Some deliver the first `enter` while the
            // surface is still being realised -- before the handler above exists -- so that map
            // would keep the pre-map guess with no signal ever arriving to correct it. Probing
            // shortly after each map covers that ordering, and it is guarded on being mapped
            // because an unmapped surface is exactly the question that produced the wrong answer
            // above.
            w.connect_map({
                let apply_monitor = apply_monitor.clone();
                let settled = settled.clone();
                let revealed_at = revealed_at.clone();
                move |w| {
                    // A new showing is a new question: the launcher may well be opened on a
                    // different screen than it was last time, and the guard above must not answer
                    // for a map it never saw.
                    settled.set(false);
                    trace(format_args!("map"));
                    // THE FIRST FRAME AFTER THE MAP. A tick callback runs as the frame clock
                    // paints, so this is the moment the window becomes something on a screen
                    // rather than a size negotiation -- and it removes itself immediately, because
                    // the interesting frame is the first one and a launcher must not hold a repaint
                    // loop open behind it.
                    if tracing_on() {
                        let started = revealed_at.clone();
                        w.add_tick_callback(move |_, _| {
                            if let Some(t) = started.take() {
                                trace(format_args!("presented us={}", t.elapsed().as_micros()));
                            }
                            gtk::glib::ControlFlow::Break
                        });
                    }
                    let window = w.clone();
                    let apply_monitor = apply_monitor.clone();
                    gtk::glib::timeout_add_local_once(
                        std::time::Duration::from_millis(100),
                        move || {
                            if !window.is_mapped() {
                                return;
                            }
                            let Some(surface) = window.surface() else {
                                return;
                            };
                            let display = gtk::prelude::WidgetExt::display(&window);
                            if let Some(monitor) = display.monitor_at_surface(&surface) {
                                apply_monitor(&monitor);
                            }
                        },
                    );
                }
            });
        });
    }

    // WHICH SCREEN IT OPENS ON, when the compositor's answer is not the wanted one.
    //
    // Nothing configured is the default and means the compositor decides, which is right: it knows
    // where you are working and a launcher that overrides that uninvited is worse than one that
    // never tries. `outputs` is for the desk where that answer is reliably wrong -- a big screen
    // beside a small vertical one, where the launcher wants to be on the big one every time
    // regardless of which window happened to have focus.
    //
    // Applied on EVERY reveal rather than once, and before `present`, because both halves are what
    // make it work: a layer surface's output is decided at map time, and the size we want is the
    // one for the screen we are about to appear on -- computing it here means the first frame is
    // already the right size instead of being corrected a frame later, in front of you.
    let prefer_output: OutputPreference = Rc::new({
        let window = window.downgrade();
        let apply_monitor = apply_monitor.clone();
        let display = display.clone();
        let layered =
            surface_mode == "layer" && std::env::var_os("CBAR_LAUNCHER_NO_LAYER").is_none();
        move |outputs: &[String]| {
            let Some(window) = window.upgrade() else {
                return;
            };
            let Some(monitor) = preferred_monitor(outputs, &display) else {
                // Either nothing is configured, or what is configured is not plugged in right now.
                // Hand the decision back rather than pinning the launcher to a screen that is not
                // there -- which is how a docked-desk preference would otherwise open a window
                // nobody can see once the laptop is carried away.
                if layered {
                    window.set_monitor(None);
                }
                return;
            };
            if layered {
                window.set_monitor(Some(&monitor));
            }
            apply_monitor(&monitor);
        }
    });

    let grid = gtk::Grid::new();
    // NOT column-homogeneous. That makes EVERY column the same width including column 0, which
    // holds nothing but the folder labels -- so the label column gets sized like a machine column
    // and leaves a wide empty gutter down the left with the labels shoved into its far edge.
    // Instead the machine columns carry `hexpand` (set per cell below) and share the spare width
    // equally between themselves, while column 0 takes only the width its longest label needs.
    grid.set_column_homogeneous(false);
    scroller.set_child(Some(&grid));
    root.append(&scroller);

    let hint = Label::new(None);
    hint.set_xalign(0.0);
    hint.set_use_markup(true);
    hint.add_css_class("hint");
    root.append(&hint);

    window.set_child(Some(&root));

    // CLOSE WHEN THE KEYBOARD GOES ELSEWHERE. This is the half that makes an exclusive grab
    // acceptable, and leaving it out is what let this thing swallow real typing: a launcher that
    // holds the seat and stays open is a lock screen with an app list on it.
    //
    // Gated on having been active at least once, because a surface is briefly inactive between
    // mapping and being focused -- closing on that first false would make it flash and vanish.
    // DISMISS, not exit. Hiding keeps the window, the widget tree, the inventory and the decoded
    // icons alive, so the next open is a map rather than a start. GtkApplication only exits when
    // the last window is DESTROYED, so a hidden one keeps the process resident with no explicit
    // hold. The launcher stays resident so the next open is a map rather than a start.
    let interaction_generation = Rc::new(Cell::new(0u64));
    let inflight_launches = Rc::new(RefCell::new(HashSet::<(String, String)>::new()));
    let dismiss: Rc<dyn Fn(&ApplicationWindow)> = Rc::new({
        let interaction_generation = interaction_generation.clone();
        let on_dismiss = on_dismiss.clone();
        move |w: &ApplicationWindow| {
            if !w.is_visible() {
                return;
            }
            advance_generation(&interaction_generation);
            w.set_visible(false);
            on_dismiss();
        }
    });

    // A GRACE PERIOD ON EVERY REVEAL, not merely after process startup. A daemon starts hidden and
    // may wait hours for its first map; arming once during build means every real open has already
    // expired the guard and one focus bounce from a bar or dock dismisses it immediately.
    // A deadline rather than a timer. Replacing it on every reveal really restarts the grace;
    // overlapping one-shot timers let an older reveal re-arm dismissal too early.
    let focus_ready_at = Rc::new(std::cell::Cell::new(None::<std::time::Instant>));
    let arm_focus: Rc<dyn Fn()> = Rc::new({
        let focus_ready_at = focus_ready_at.clone();
        move || {
            focus_ready_at
                .set(std::time::Instant::now().checked_add(std::time::Duration::from_millis(400)));
        }
    });
    if exit_on_focus_loss {
        let dismiss_on_blur = dismiss.clone();
        let focus_ready_at = focus_ready_at.clone();
        window.connect_is_active_notify(move |w| {
            let ready = focus_ready_at
                .get()
                .is_some_and(|deadline| std::time::Instant::now() >= deadline);
            if !w.is_active() && ready {
                dismiss_on_blur(w);
            }
        });
    }

    // ── TWO KINDS OF UPDATE, AND WHY THEY ARE NOT THE SAME FUNCTION ──────────────────────────
    //
    // Moving the cursor and changing the query are utterly different amounts of work, and treating
    // them alike is what made this slow. The grid is ~900 widgets on a real three-machine
    // inventory; rebuilding it costs hundreds of milliseconds, and it used to happen on EVERY
    // keypress -- including the arrow keys, whose entire effect is to move one highlight.
    //
    //   `render`  STRUCTURAL. Tears the grid down and builds it again. Needed only when the set of
    //             things on screen changes: a new query filters apps out, a drag re-files one, a
    //             launch reorders by frecency.
    //   `paint`   COSMETIC. Moves the selection classes from where they were to where they now
    //             are, and updates the two text labels. Touches at most eight widgets regardless
    //             of how many are on screen, because it remembers what it highlighted last time.
    //
    // Arrow keys, Tab and Enter-into-a-cell take the second path, which is the one the user is in
    // for most of a session -- this launcher's whole premise is that you navigate a grid rather
    // than type at it.
    // The bindings, resolved once. Defaults unless configuration says otherwise -- see the keymap
    // module on why overriding beats replacing wholesale.
    let keys_map: Rc<keymap::Keymap> = Rc::new(
        loaded_config
            .as_ref()
            .map(|c| keymap::Keymap::from_overrides(&c.keys))
            .unwrap_or_default(),
    );

    let painted: Rc<RefCell<Painted>> = Rc::new(RefCell::new(Painted::default()));

    // `render` must be callable from inside a drop handler that `render` itself installed, so it
    // needs a handle to itself. The holder is that indirection -- filled in immediately after
    // construction, and only ever read while no other borrow of it is live.
    let render_holder: CallbackSlot = Rc::new(RefCell::new(None));

    let paint: Rc<dyn Fn()> = Rc::new({
        let state = state.clone();
        let layout = layout.clone();
        let painted = painted.clone();
        let search = search.clone();
        let hint = hint.clone();
        let theme_error = theme.error.clone();
        let config_err = startup_error.clone();
        let scroller = scroller.clone();
        let grid_for_scroll = grid.clone();
        move || {
            let s = state.borrow();

            // A config that EXISTS but does not parse is shown IN THE WINDOW, not just on stderr.
            // This surface is launched from a compositor keybind, so nothing is watching stderr,
            // and inventing machines while the real ones failed to load would be precisely the
            // outcome config.rs's own comment says must not happen silently.
            if let Some(err) = &config_err {
                search.set_markup(&format!(
                    "<span foreground=\"{}\">startup problem:</span> {}",
                    theme_error,
                    escape(err)
                ));
                search.remove_css_class("empty");
            } else if s.query.is_empty() {
                search.set_text("type to search\u{2026}");
                search.add_css_class("empty");
            } else {
                search.set_text(&s.query);
                search.remove_css_class("empty");
            }

            let rail = s.folders.get(s.row).is_some_and(|row| {
                layout
                    .borrow()
                    .is_rail(row, s.cell().iter().map(|line| line.apps.len()).sum())
            });
            let base_hint = match (s.focus, rail) {
                (Focus::Outside, false) => {
                    "<b>\u{2190}\u{2192}</b> machine   <b>\u{2191}\u{2193}</b> folder   <b>Tab</b>/<b>Enter</b> inside   <b>Shift+Enter</b> launch cell   <b>drag</b> file/reorder   <b>Esc</b> close"
                }
                (Focus::Inside, false) => {
                    "<b>\u{2190}\u{2192}</b> app   <b>\u{2191}\u{2193}</b> line   <b>Enter</b> launch   <b>Shift+Enter</b> launch line   <b>Tab</b>/<b>Esc</b> out"
                }
                (Focus::Outside, true) => {
                    "<b>\u{2190}\u{2192}</b> machine   <b>\u{2191}\u{2193}</b> rail   <b>Tab</b>/<b>Enter</b> browse   <b>drag</b> file/reorder   <b>Esc</b> close"
                }
                (Focus::Inside, true) => {
                    "<b>\u{2190}\u{2192}</b> title   <b>Enter</b> launch   <b>Tab</b>/<b>Esc</b> out"
                }
            };
            let hidden = s.hidden_count();
            let page_hint = painted
                .borrow()
                .page_hint
                .as_deref()
                .map(|page| format!("   <b>{page}</b>"))
                .unwrap_or_default();
            if hidden == 0 {
                hint.set_markup(&format!(
                    "{base_hint}   <b>right-click</b> then click to hide{page_hint}"
                ));
            } else {
                hint.set_markup(&format!(
                    "{base_hint}   <b>right-click</b> then click to hide   <b>Ctrl+Shift+H</b> show all ({hidden}){page_hint}"
                ));
            }

            let now = Cursor {
                col: s.col,
                row: s.row,
                line: s.line,
                item: s.item,
                inside: s.focus == Focus::Inside,
            };
            let mut p = painted.borrow_mut();
            if p.last == Some(now) {
                return;
            }
            // Un-highlight exactly what was highlighted, then highlight the new place. Walking the
            // whole grid to clear stale classes would put the cost back that this exists to remove.
            if let Some(was) = p.last {
                p.mark(was, false);
            }
            p.mark(now, true);
            p.last = Some(now);

            // BRING THE CURSOR INTO VIEW. The grid is capped at a fraction of the display and
            // scrolls past that, but nothing was ever moving the viewport -- so arrowing down into
            // a clipped row moved the selection somewhere the user could not see, and the launcher
            // silently became a thing you navigate blind. A scrollbar reaches those rows; the
            // keyboard could not, which is the wrong way round for a keyboard-driven launcher.
            let painted_row = p.rows.iter().position(|row| *row == now.row);
            let painted_column = p.columns.iter().position(|column| *column == now.col);
            if let Some(cell) = painted_row.and_then(|row| {
                painted_column.and_then(|column| p.cells.get(row).and_then(|row| row.get(column)))
            }) {
                // DEFERRED, because at this point the widget may have no allocation yet: a repaint
                // that follows a rebuild runs before layout, and `compute_bounds` on an
                // unallocated widget answers with nothing (or with zeroes, which would scroll to
                // the top and look like the viewport jumping on its own).
                // The OUTER scroller follows the whole rail cell; the title itself lives in
                // that cell's private horizontal viewport and is revealed separately below.
                // Letting its coordinates drive the outer scroller would pan the entire matrix
                // to follow content which no longer belongs to the matrix's width negotiation.
                let bx = if now.inside && cell.rail.is_none() {
                    cell.lines
                        .iter()
                        .find(|line| line.model_line == now.line)
                        .and_then(|line| {
                            line.apps
                                .iter()
                                .find(|(item, _)| *item == now.item)
                                .map(|(_, app)| app)
                        })
                        .cloned()
                        .unwrap_or_else(|| cell.bx.clone())
                } else {
                    cell.bx.clone()
                };
                let rail_target = cell.rail.as_ref().and_then(|rail| {
                    cell.lines
                        .iter()
                        .find(|line| line.model_line == now.line)
                        .and_then(|line| {
                            line.apps
                                .iter()
                                .find(|(item, _)| *item == now.item)
                                .map(|(_, app)| (rail.clone(), line.bx.clone(), app.clone()))
                        })
                });
                let scroller = scroller.clone();
                let grid = grid_for_scroll.clone();
                gtk::glib::idle_add_local_once(move || {
                    let Some(b) = bx.compute_bounds(&grid) else {
                        return;
                    };
                    // ONE RULE, BOTH AXES. Moving left and right across machines runs off the
                    // edge exactly as moving down through folders runs off the bottom, and a fix
                    // that only followed the cursor vertically would be the same bug left half
                    // done. Nearest edge only: centring the selection would move the whole grid on
                    // every keypress, and a spatial launcher is fast precisely because things stay
                    // where they were learned -- scrolling only when the target is genuinely
                    // off-screen keeps everything else still.
                    let reveal = |adj: &gtk::Adjustment, near: f64, far: f64| {
                        let (view, page) = (adj.value(), adj.page_size());
                        if near < view {
                            adj.set_value(near);
                        } else if far > view + page {
                            adj.set_value(far - page);
                        }
                    };
                    reveal(
                        &scroller.vadjustment(),
                        b.y() as f64,
                        (b.y() + b.height()) as f64,
                    );
                    reveal(
                        &scroller.hadjustment(),
                        b.x() as f64,
                        (b.x() + b.width()) as f64,
                    );
                    if let Some((rail, line, app)) = rail_target
                        && let Some(b) = app.compute_bounds(&line)
                    {
                        reveal(
                            &rail.hadjustment(),
                            b.x() as f64,
                            (b.x() + b.width()) as f64,
                        );
                    }
                });
            }
        }
    });

    // The currently revealed inline action, if any. Only one is shown at a time so right-clicking
    // another application moves the affordance instead of leaving a trail of Hide markers.
    // Inline is important: a GtkPopover creates a second Wayland surface, which makes the layer
    // window report focus loss and correctly triggers this launcher's dismiss-on-blur policy.
    //
    // A MARKER, NOT A BUTTON, and that is the fix to a bug rather than a matter of taste. It was a
    // 20px GtkButton overlaid on the icon, inside an application box that already carries a drag
    // source and a click gesture of its own -- three handlers contending for one press on one small
    // square. The press was reliably eaten by one of the other two: the eye appeared, clicking it
    // did nothing at all, and nothing was launched either, so there was not even a wrong outcome to
    // notice. Making it inert and letting the ARMED application's own click gesture perform the
    // hide removes the contention instead of trying to win it -- the same gesture that has always
    // launched reliably, so the path is known good.
    // THE MARKER AND THE CHIP IT BELONGS TO. The chip is what gets clicked, so the chip is what has
    // to look armed: an eye over a 20px icon says something is possible, not that this whole
    // application is now the button. Both are held so arming the next one un-arms this one
    // completely, rather than leaving a tinted chip with no marker on it.
    let visible_hide_action = Rc::new(RefCell::new(None::<(Image, GBox)>));

    // WHICH application is armed, by identity rather than by widget: the grid is rebuilt whenever
    // the query changes or a drag lands, so a widget handle would outlive the thing it stood for.
    let armed_hide = Rc::new(RefCell::new(None::<(String, String)>));

    let render: Rc<dyn Fn()> = Rc::new({
        let state = state.clone();
        let layout = layout.clone();
        let grid = grid.clone();
        let spacer_folder = spacer_folder.clone();
        let spacer_sub = spacer_sub.clone();
        // Child controllers need to request a structural rebuild, but must not keep the render
        // closure (and therefore the complete embedded launcher) alive after its owner is gone.
        let holder = Rc::downgrade(&render_holder);
        let theme_error = theme.error.clone();
        let painted = painted.clone();
        let paint = paint.clone();
        let icon_px = theme.icon_size;
        let icon_theme = icon_theme.clone();
        let icon_cache = icon_cache.clone();
        let visible_hide_action = visible_hide_action.clone();
        let armed_hide = armed_hide.clone();
        // For the click path: launching needs the window to dismiss, the terminal wrapper for
        // programs that draw none, and the state to record the launch against.
        let window = window.downgrade();
        let terminal_cmd = terminal_cmd_outer.clone();
        let runtime = runtime.clone();
        let dismiss = dismiss.clone();
        let interaction_generation = interaction_generation.clone();
        let inflight_launches = inflight_launches.clone();
        move || {
            let t_render = std::time::Instant::now();
            let s = state.borrow();
            let cursor = Cursor {
                col: s.col,
                row: s.row,
                line: s.line,
                item: s.item,
                inside: s.focus == Focus::Inside,
            };
            let materialization = materialization(&s.view, cursor, icon_px);

            // Every handle recorded below belongs to a widget that is about to be destroyed, so the
            // record is cleared FIRST. Leaving the old ones in place would have the next repaint
            // remove a class from a widget that is no longer in the tree -- harmless, and a silent
            // way for the real selection to keep a highlight it should have lost.
            painted.borrow_mut().reset();
            *visible_hide_action.borrow_mut() = None;
            // Disarmed with it. A rebuilt grid is a new set of widgets, and an application left
            // armed across one would hide on a click the user meant as a launch.
            *armed_hide.borrow_mut() = None;

            while let Some(c) = grid.first_child() {
                grid.remove(&c);
            }

            // Rebuilt every render rather than kept: the row-head widgets are recreated each time,
            // and a long-lived group would accumulate memberships for widgets that no longer exist.
            let labelcol = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
            labelcol.add_widget(&spacer_folder);
            // The subcategory column gets its own group, so the corner can be the sum of the two
            // rather than an approximation of either.
            let subcol = gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal);
            subcol.add_widget(&spacer_sub);
            let machine_columns = layout
                .borrow()
                .equal_columns
                .then(|| gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal));
            {
                let mut painted = painted.borrow_mut();
                painted.columns.clone_from(&materialization.columns);
                painted.rows.clone_from(&materialization.rows);
                painted.page_hint = materialization.is_paged().then(|| {
                    format!(
                        "page {}/{}",
                        materialization.page + 1,
                        materialization.pages
                    )
                });
            }

            for (grid_column, &c) in materialization.columns.iter().enumerate() {
                let m = &s.view[c];
                let head = Label::new(None);
                head.set_xalign(0.0);
                head.add_css_class("colhead");
                // A machine that could not be asked says so IN ITS OWN HEADING, next to its name.
                // The alternative -- an empty column -- reads identically to a machine that simply
                // has nothing installed, which is the one thing it must not be confused with.
                head.set_markup(&match &m.error {
                    None => format!("<span foreground=\"{}\">{}</span>", m.accent, escape(&m.name)),
                    Some(e) => format!(
                        "<span foreground=\"{}\">{}</span>  <span foreground=\"{}\" size=\"small\">{}</span>",
                        m.accent,
                        escape(&m.name),
                        theme_error,
                        escape(e.lines().next().unwrap_or("unreachable"))
                    ),
                });
                if let Some(e) = &m.error {
                    head.set_tooltip_text(Some(e));
                }
                if let Some(group) = &machine_columns {
                    group.add_widget(&head);
                }
                grid.attach(&head, grid_column as i32 + 2, 0, 1, 1);
            }

            let mut last_rendered_group: Option<String> = None;
            for (grid_row, &r) in materialization.rows.iter().enumerate() {
                let Some(folder) = s.folders.get(r) else {
                    continue;
                };
                // `folder/sub` splits across two label columns, and the folder name is drawn
                // only on the FIRST row that carries it -- repeating "Chat" down three rows would
                // be noise, and its absence is what makes the group read as a group.
                let (fname, sub) = match folder.split_once('/') {
                    Some((f, s)) => (f, Some(s)),
                    None => (folder.as_str(), None),
                };
                // Compare with the last row actually DRAWN. The preceding canonical row may have
                // been skipped as globally empty; comparing with it suppresses the only visible
                // folder label and leaves a block of subrows with no heading.
                let first_of_group = last_rendered_group.as_deref() != Some(fname);
                last_rendered_group = Some(fname.to_string());
                let rh = Label::new(if first_of_group { Some(fname) } else { None });
                rh.set_xalign(1.0);
                rh.set_valign(Align::Center);
                rh.add_css_class("rowhead");
                labelcol.add_widget(&rh);
                grid.attach(&rh, 0, grid_row as i32 + 1, 1, 1);

                // The subcategory, in its own column OUTSIDE the machines, so it lines up across
                // every one of them instead of only with itself.
                let sublabel = Label::new(sub);
                sublabel.set_xalign(1.0);
                sublabel.set_valign(Align::Center);
                sublabel.add_css_class("subrow");
                subcol.add_widget(&sublabel);
                grid.attach(&sublabel, 1, grid_row as i32 + 1, 1, 1);
                painted.borrow_mut().rowheads.push(rh.clone());
                let mut row_cells: Vec<CellW> = Vec::with_capacity(materialization.columns.len());
                for (grid_column, &c) in materialization.columns.iter().enumerate() {
                    let m = &s.view[c];
                    let lines = &m.cells[r];
                    let items = lines.iter().map(|line| line.apps.len()).sum();
                    let rail = layout.borrow().is_rail(folder, items);
                    let cell = GBox::new(Orientation::Vertical, 2);
                    cell.add_css_class("cell");
                    if let Some(group) = &machine_columns {
                        group.add_widget(&cell);
                    }
                    let mut rail_viewport = None;
                    // Dropping onto a cell files the app into that folder, for that machine,
                    // permanently. Same column only -- see the drag payload's own note.
                    {
                        let tgt = gtk::DropTarget::new(
                            gtk::glib::Type::STRING,
                            gtk::gdk::DragAction::MOVE,
                        );
                        let st = state.clone();
                        let holder2 = holder.clone();
                        tgt.connect_drop(move |_, value, _, _| {
                            let Ok(p) = value.get::<String>() else {
                                return false;
                            };
                            let Some((from_col, name)) = p.split_once('\u{1}') else {
                                return false;
                            };
                            if from_col.parse::<usize>().ok() != Some(c) {
                                return false;
                            }
                            // Dropped on the cell's own background, not on a line: give it a
                            // line to itself. Joining an appset is what dropping ON a line means.
                            st.borrow_mut().place_app(c, name, r, None, None);
                            invalidate_render(&holder2);
                            true
                        });
                        cell.add_controller(tgt);
                    }
                    // CLICKING A BOX MOVES THE KEYBOARD CURSOR INTO IT. Without this the mouse
                    // could launch and rearrange but never change where the keyboard was, so the
                    // two halves of the interface disagreed about where you are -- click a box,
                    // press an arrow, and the selection jumped back somewhere else entirely.
                    {
                        let st = state.clone();
                        let paint2 = paint.clone();
                        let pick = gtk::GestureClick::new();
                        pick.connect_released(move |_, _, _, _| {
                            {
                                let mut s = st.borrow_mut();
                                s.col = c;
                                s.row = r;
                                s.focus = if s.cell().is_empty() {
                                    Focus::Outside
                                } else {
                                    Focus::Inside
                                };
                                s.line = 0;
                                s.item = 0;
                                s.item_goal = 0;
                                s.clamp();
                            }
                            paint2();
                        });
                        cell.add_controller(pick);
                    }

                    let mut cell_lines: Vec<LineW> = Vec::new();
                    if lines.is_empty() {
                        cell.add_css_class("empty");
                        let dash = Label::new(Some("\u{2014}"));
                        dash.set_xalign(0.0);
                        dash.add_css_class("dim");
                        cell.append(&dash);
                    }
                    let visible_lines = lines
                        .iter()
                        .enumerate()
                        .filter(|(line, _)| materialization.line_visible(c, r, *line))
                        .collect::<Vec<_>>();
                    if !lines.is_empty() && visible_lines.is_empty() {
                        let more = Label::new(Some("\u{2026}"));
                        more.set_xalign(0.0);
                        more.add_css_class("dim");
                        more.set_tooltip_text(Some("More applications on another launcher page"));
                        cell.append(&more);
                    }
                    for (model_line, ln) in visible_lines {
                        let lb = GBox::new(Orientation::Horizontal, 2);
                        lb.add_css_class("line");
                        // Dropping ON a line inserts INTO it, at the gap nearest the pointer --
                        // which is simultaneously "join this appset", "put it third rather than
                        // first", and (when the app is already on this line) "reorder it".
                        {
                            let tgt = gtk::DropTarget::new(
                                gtk::glib::Type::STRING,
                                gtk::gdk::DragAction::MOVE,
                            );
                            let st = state.clone();
                            let holder2 = holder.clone();
                            let lb2 = lb.clone();
                            // WHAT is on this line, not WHICH line it is. A rendered line index is
                            // an index into a grid that is filtered and frecency-ordered -- two
                            // transformations away from the placement it would be written to. The
                            // names survive both; see `place_app`'s own account.
                            let visible_items = ln
                                .apps
                                .iter()
                                .enumerate()
                                .filter(|(item, _)| {
                                    materialization.app_visible(c, r, model_line, *item)
                                })
                                .map(|(item, _)| item)
                                .collect::<Vec<_>>();
                            let names: Vec<String> = ln.apps.iter().map(|a| a.id.clone()).collect();
                            tgt.connect_drop(move |_, value, x, _| {
                                let Ok(payload) = value.get::<String>() else {
                                    return false;
                                };
                                let Some((from_col, name)) = payload.split_once('\u{1}') else {
                                    return false;
                                };
                                if from_col.parse::<usize>().ok() != Some(c) {
                                    return false;
                                }
                                // The gap the pointer is nearest, expressed as "goes before this
                                // app". Past the last gap there is no such app, and None is
                                // exactly right: it means the end of the line.
                                let at = insert_index_at(&lb2, x);
                                let before = visible_items
                                    .get(at)
                                    .and_then(|item| names.get(*item))
                                    .or_else(|| {
                                        visible_items.last().and_then(|item| names.get(item + 1))
                                    })
                                    .cloned();
                                st.borrow_mut().place_app(
                                    c,
                                    name,
                                    r,
                                    Some(&names),
                                    before.as_deref(),
                                );
                                invalidate_render(&holder2);
                                true
                            });
                            lb.add_controller(tgt);
                        }
                        // The row's name, when it has one, at its head. Small and muted: it is a
                        // label for what follows, not an entry you can act on, and drawing it like
                        // the applications would invite clicking something that does nothing.
                        let mut line_apps: Vec<(usize, GBox)> = Vec::new();
                        for (model_item, app) in ln.apps.iter().enumerate().filter(|(item, _)| {
                            materialization.app_visible(c, r, model_line, *item)
                        }) {
                            let b = GBox::new(Orientation::Horizontal, 4);
                            b.add_css_class("app");

                            let img = icons::Icons::image(&icon_cache, &app.icon, &icon_theme);
                            img.set_pixel_size(icon_px);

                            // Overlay only the existing icon, never the whole application. The app
                            // box remains the exact same direct child of the line that it was before
                            // hiding existed, preserving its expand and natural-width negotiation.
                            // The action is explicitly excluded from measurement as a second guard:
                            // revealing it must consume exactly zero new layout space.
                            let icon_overlay = gtk::Overlay::new();
                            icon_overlay.set_child(Some(&img));
                            icon_overlay.set_hexpand(false);

                            // INERT. It states that this application is armed and takes no input of
                            // its own: `can_target(false)` puts it out of the picking pass
                            // entirely, so a press on the icon reaches the box's own gesture the
                            // way a press anywhere else on the application already does.
                            let hide_action = Image::from_icon_name("view-conceal-symbolic");
                            hide_action.add_css_class("hide-action");
                            hide_action.set_tooltip_text(Some(&format!("Hide {}", app.name)));
                            hide_action.set_pixel_size(16);
                            hide_action.set_can_target(false);
                            hide_action.set_size_request(icon_px, icon_px);
                            hide_action.set_halign(gtk::Align::Center);
                            hide_action.set_valign(gtk::Align::Center);
                            hide_action.set_visible(false);
                            icon_overlay.add_overlay(&hide_action);
                            icon_overlay.set_measure_overlay(&hide_action, false);
                            icon_overlay.set_clip_overlay(&hide_action, true);
                            // The payload carries the COLUMN it came from as well as the name, so
                            // the drop side can refuse a cross-machine drag without having to ask
                            // anyone: filing is per machine, and "Firefox on one machine" is not the
                            // same object as "Firefox on another".
                            {
                                let src = gtk::DragSource::new();
                                src.set_actions(gtk::gdk::DragAction::MOVE);
                                let payload = format!("{}\u{1}{}", c, app.id);
                                src.connect_prepare(move |_, _, _| {
                                    Some(gtk::gdk::ContentProvider::for_value(&payload.to_value()))
                                });
                                b.add_controller(src);
                            }
                            // PRIMARY launches. Middle launches and leaves the launcher open.
                            // Secondary reveals a small inline action: hiding must be a visible,
                            // deliberate command rather than an irreversible-looking surprise.
                            {
                                let st = state.clone();
                                let win = window.clone();
                                let term = terminal_cmd.clone();
                                let runtime = runtime.clone();
                                let dismiss = dismiss.clone();
                                let interaction_generation = interaction_generation.clone();
                                let inflight_launches = inflight_launches.clone();
                                let holder2 = holder.clone();
                                // Captured by IDENTITY, never by index: the grid may be rebuilt
                                // between this being wired and the click arriving -- a query
                                // filters, a drag reorders, frecency moves a line -- and an index
                                // would by then name a different application.
                                let id = app.id.clone();

                                let hide_action_for_click = hide_action.clone();
                                // WEAK, because this controller is attached to the very widget it
                                // refers to. A strong handle would be a reference cycle, and the
                                // grid is torn down and rebuilt on every render -- so it would leak
                                // an application box per chip per render, not just once.
                                let chip = b.downgrade();
                                let visible = visible_hide_action.clone();
                                let armed = armed_hide.clone();
                                let machine_name = m.name.clone();
                                let click = gtk::GestureClick::new();
                                // Every button, so middle and right arrive here too rather than
                                // only the primary one.
                                click.set_button(0);
                                click.connect_released(move |g, _, _, _| {
                                    let button = g.current_button();
                                    // Claimed, so the drag source on this same widget does not
                                    // also read the press as the beginning of a drag.
                                    g.set_state(gtk::EventSequenceState::Claimed);

                                    if button == 3 {
                                        let Some(chip) = chip.upgrade() else { return };
                                        if let Some((marker, previous)) = visible
                                            .borrow_mut()
                                            .replace((hide_action_for_click.clone(), chip.clone()))
                                        {
                                            marker.set_visible(false);
                                            previous.remove_css_class("arming");
                                        }
                                        hide_action_for_click.set_visible(true);
                                        chip.add_css_class("arming");
                                        *armed.borrow_mut() =
                                            Some((machine_name.clone(), id.clone()));
                                        return;
                                    }
                                    if button != 1 && button != 2 {
                                        return;
                                    }

                                    // ARMED MEANS HIDE, and only for the application that was
                                    // armed: a primary click anywhere else disarms and does what it
                                    // always did, so changing your mind costs one ordinary click
                                    // rather than a gesture nobody would guess.
                                    let armed_here = armed
                                        .borrow()
                                        .as_ref()
                                        .is_some_and(|(am, ai)| *am == machine_name && *ai == id);
                                    if armed_here && button == 1 {
                                        *armed.borrow_mut() = None;
                                        if let Some((marker, chip)) = visible.borrow_mut().take() {
                                            marker.set_visible(false);
                                            chip.remove_css_class("arming");
                                        }
                                        let mut state = st.borrow_mut();
                                        let changed = state.hide_app(&machine_name, &id);
                                        state.clamp();
                                        drop(state);
                                        if changed {
                                            invalidate_render(&holder2);
                                        }
                                        return;
                                    }
                                    // Clicking a DIFFERENT application disarms the one that was
                                    // armed, so the marker is taken from whichever widget is
                                    // showing it rather than from this one.
                                    if armed.borrow_mut().take().is_some()
                                        && let Some((marker, chip)) = visible.borrow_mut().take()
                                    {
                                        marker.set_visible(false);
                                        chip.remove_css_class("arming");
                                    }

                                    let Some(machine) = st.borrow().view.get(c).cloned() else {
                                        return;
                                    };
                                    let found = machine
                                        .cells
                                        .iter()
                                        .flatten()
                                        .flat_map(|l| l.apps.iter())
                                        .find(|a| a.id == id)
                                        .cloned();
                                    let Some(app) = found else { return };
                                    let Some(win) = win.upgrade() else {
                                        return;
                                    };

                                    // The keyboard owns appset launching through Shift+Enter;
                                    // pointer buttons act on exactly the item under the pointer.
                                    let launched_machine = machine.name.clone();
                                    let st = st.clone();
                                    let holder = holder2.clone();
                                    let dismiss = dismiss.clone();
                                    let launch_generation = interaction_generation.get();
                                    let completion_generation = interaction_generation.clone();
                                    queue_launch(
                                        &runtime,
                                        machine,
                                        vec![app],
                                        term.clone(),
                                        inflight_launches.clone(),
                                        move |launched| {
                                            if launched.is_empty() {
                                                return;
                                            }
                                            let mut state = st.borrow_mut();
                                            for app_id in launched {
                                                state.record_launch(&launched_machine, &app_id);
                                            }
                                            state.save_usage();
                                            let current = generation_is_current(
                                                &completion_generation,
                                                launch_generation,
                                            ) && win.is_visible();
                                            if button == 2 {
                                                // Stay open and show any frecency reorder earned
                                                // only after the worker confirms the handoff.
                                                if !current {
                                                    return;
                                                }
                                                state.rebuild();
                                                drop(state);
                                                invalidate_render(&holder);
                                            } else {
                                                drop(state);
                                                if current {
                                                    dismiss(&win);
                                                }
                                            }
                                        },
                                    );
                                });
                                b.add_controller(click);
                            }
                            b.append(&icon_overlay);
                            let l = Label::new(Some(&app.name));
                            l.add_css_class("appname");
                            l.set_ellipsize(gtk::pango::EllipsizeMode::End);
                            l.set_max_width_chars(layout.borrow().max_label_chars);
                            l.set_tooltip_text(Some(&app.name));
                            b.append(&l);
                            lb.append(&b);
                            line_apps.push((model_item, b));
                        }
                        if rail {
                            // A rail is deliberately one long vector, but that does not make
                            // it the width authority for every row in this machine column. Give
                            // the vector a local viewport whose natural width does not propagate;
                            // ordinary appset rows now decide the compact column width, and only
                            // the rail pans when a title lies beyond it.
                            let viewport = gtk::ScrolledWindow::new();
                            viewport.add_css_class("vector-rail");
                            viewport.set_policy(gtk::PolicyType::Automatic, gtk::PolicyType::Never);
                            viewport.set_overlay_scrolling(true);
                            viewport.set_propagate_natural_width(false);
                            viewport.set_propagate_natural_height(true);
                            viewport.set_hexpand(true);
                            viewport.set_child(Some(&lb));
                            cell.append(&viewport);
                            rail_viewport = Some(viewport);
                        } else {
                            cell.append(&lb);
                        }
                        cell_lines.push(LineW {
                            bx: lb.clone(),
                            model_line,
                            apps: line_apps,
                        });
                    }
                    grid.attach(&cell, grid_column as i32 + 2, grid_row as i32 + 1, 1, 1);
                    row_cells.push(CellW {
                        bx: cell.clone(),
                        lines: cell_lines,
                        rail: rail_viewport,
                    });
                }
                painted.borrow_mut().cells.push(row_cells);
            }

            // The selection classes are NOT set above. Painting them is the other function's job,
            // and doing it here as well would be a second implementation of the same rule, free to
            // disagree with the first the moment either changes.
            let drawn = materialization.apps.len();
            drop(s);
            paint();
            trace(format_args!(
                "render us={} apps={drawn}",
                t_render.elapsed().as_micros()
            ));
            // Provider columns arrive opportunistically after the initial frame. Schedule a
            // cheap dirty check after every render so icons decoded by those later columns are
            // persisted as well; `save` clears the dirty bit before moving serialization and I/O
            // to its worker, so repeated query renders do not duplicate the write.
            let icon_cache = icon_cache.clone();
            gtk::glib::idle_add_local_once(move || {
                icon_cache.borrow_mut().save();
            });
        }
    });
    *render_holder.borrow_mut() = Some(render.clone());

    // WHAT REOPENING MEANS FOR A RESIDENT PROCESS.
    //
    // A process that never exits still re-asks every independent provider on reveal. The cbar owner
    // prepares launcher config/state off GTK at the same time and atomically replaces this whole UI
    // when any config field changed. Keeping config reload at that ownership boundary avoids the
    // old partial-update bug where folders changed but theme/keymap/terminal/focus policy silently
    // remained stale until cbar restarted.
    //
    // The query is cleared too. A search left over from last time is not a state anyone expects to
    // reopen into, and it would hide most of the grid while looking like an empty launcher.
    let providers = Rc::new(RefCell::new(None::<Rc<provider::ProviderManager>>));
    let provider_generation = Rc::new(Cell::new(0u64));
    let output_preferences = loaded_config
        .as_ref()
        .map(|config| config.outputs.clone())
        .unwrap_or_default();
    if let Some(config) = loaded_config.clone() {
        *providers.borrow_mut() = Some(start_providers(
            runtime,
            config,
            state.clone(),
            layout.clone(),
            render.clone(),
            scroller.clone(),
            provider_generation.clone(),
        ));
    }
    let reveal: Rc<dyn Fn()> = {
        let state = state.clone();
        let render = render.clone();
        let arm_focus = arm_focus.clone();
        let prefer_output = prefer_output.clone();
        let revealed_at = revealed_at.clone();
        let providers = providers.clone();
        let output_preferences = output_preferences.clone();
        let interaction_generation = interaction_generation.clone();
        Rc::new(move || {
            advance_generation(&interaction_generation);
            let t_reveal = std::time::Instant::now();
            revealed_at.set(Some(t_reveal));
            arm_focus();
            // Reveal the coherent cached grid immediately. Inventory refresh is external I/O and
            // belongs off the GTK thread; one wedged peer must not stop the existing window from
            // mapping or make focus-loss handling unresponsive.
            {
                let mut s = state.borrow_mut();
                s.query.clear();
                s.line = 0;
                s.item = 0;
                s.item_goal = 0;
                s.rebuild();
                s.clamp();
            }
            render();

            // The common reveal is a constant-time broadcast to the already-running independent
            // providers. No file or process operation precedes the `present()` following this
            // closure. The last valid output preference is likewise immediately available.
            prefer_output(&output_preferences);
            if let Some(manager) = providers.borrow().as_ref() {
                manager.refresh();
            }
            trace(format_args!("reveal us={}", t_reveal.elapsed().as_micros()));
        })
    };

    render();

    let keys = EventControllerKey::new();
    {
        let state = state.clone();
        let window = window.downgrade();
        let render = render.clone();
        let paint = paint.clone();
        let terminal_cmd = terminal_cmd_outer.clone();
        let runtime = runtime.clone();
        let keys_map = keys_map.clone();
        let layout = layout.clone();
        let armed_hide = armed_hide.clone();
        let visible_hide_action = visible_hide_action.clone();
        let interaction_generation = interaction_generation.clone();
        let inflight_launches = inflight_launches.clone();
        let painted = painted.clone();
        keys.connect_key_pressed(move |_, key, _, mods| {
            let Some(window) = window.upgrade() else {
                return gtk::glib::Propagation::Stop;
            };
            let shift = mods.contains(ModifierType::SHIFT_MASK);
            // WHAT the key means comes from the keymap, not from this match. GTK reports the
            // physical key; `keymap::Action` says what the user asked for, and anything unbound
            // falls through to the text arm -- which is why typing a name never needs a binding.
            //
            // Shift+Tab is asked for as `shift+tab` even though X11 hands it over as a different
            // keysym entirely (ISO_Left_Tab). Normalising here means a configuration file can say
            // the obvious thing.
            let name = match key {
                Key::ISO_Left_Tab => "tab".to_string(),
                k => k.name().map(|n| n.to_string()).unwrap_or_default(),
            };
            let chord = keymap::Keymap::chord(
                &name,
                mods.contains(ModifierType::CONTROL_MASK),
                mods.contains(ModifierType::ALT_MASK),
                shift,
                mods.contains(ModifierType::SUPER_MASK),
            );
            let act = keys_map.action(&chord);
            if matches!(act, Some(keymap::Action::Cancel)) {
                // Do not trace arbitrary key names or text. The headless lifecycle check only
                // needs to prove that its synthetic dismissal reached GTK's key controller.
                trace(format_args!("cancel-key"));
            }
            let structural;
            {
                let mut s = state.borrow_mut();
                // THE test for "did the grid's contents change", and it is exact rather than a
                // guess: every arm that calls `refilter` does so because the query moved, and no
                // other arm touches it. Comparing the query afterwards therefore identifies the
                // structural keys without each arm having to remember to declare itself -- a flag
                // set by hand would be one `s.query.push` away from being wrong, and the symptom
                // would be a grid that silently stops matching what was typed.
                let before = s.query.clone();
                let mut state_rebuilt = false;
                let rail = s.folders.get(s.row).is_some_and(|row| {
                    layout
                        .borrow()
                        .is_rail(row, s.cell().iter().map(|line| line.apps.len()).sum())
                });
                match (s.focus, act) {
                    // Esc unwinds one layer at a time rather than always closing: a typed query is
                    // state the user can lose accidentally, so it gets its own step.
                    (_, Some(keymap::Action::Cancel)) => {
                        // ARMED FIRST, and before the query, because it is the most recent thing the
                        // user did and the only one that is about to remove something. Escape has
                        // always meant "back out of the last step"; without this rung the way out of
                        // arming was to click some other application, which is a strange thing to
                        // have to know.
                        if armed_hide.borrow_mut().take().is_some() {
                            if let Some((marker, chip)) = visible_hide_action.borrow_mut().take() {
                                marker.set_visible(false);
                                chip.remove_css_class("arming");
                            }
                            return gtk::glib::Propagation::Stop;
                        }
                        if !s.query.is_empty() {
                            s.set_query(String::new());
                        } else if s.focus == Focus::Inside {
                            s.focus = Focus::Outside;
                        } else {
                            dismiss(&window);
                            return gtk::glib::Propagation::Stop;
                        }
                    }
                    (_, Some(keymap::Action::GoOutside)) => {
                        // Shift+Tab arrives as a DIFFERENT keysym, so the plain Tab arm never saw
                        // it and the binding was simply dead.
                        s.focus = Focus::Outside;
                    }
                    (_, Some(keymap::Action::ToggleInside)) => {
                        s.focus = if s.focus == Focus::Outside && !s.cell().is_empty() {
                            Focus::Inside
                        } else {
                            Focus::Outside
                        };
                        s.line = 0;
                        s.item = 0;
                        s.item_goal = 0;
                    }

                    (Focus::Outside, Some(keymap::Action::MoveLeft)) => {
                        s.col = s.col.saturating_sub(1)
                    }
                    (Focus::Outside, Some(keymap::Action::MoveRight)) => {
                        s.col = (s.col + 1).min(s.view.len().saturating_sub(1))
                    }
                    (Focus::Outside, Some(keymap::Action::MoveUp)) => s.row = s.next_row(s.row, -1),
                    (Focus::Outside, Some(keymap::Action::MoveDown)) => {
                        s.row = s.next_row(s.row, 1)
                    }
                    (Focus::Outside, Some(keymap::Action::Enter)) => {
                        if !s.cell().is_empty() {
                            s.focus = Focus::Inside;
                            s.line = 0;
                            s.item = 0;
                            s.item_goal = 0;
                        }
                    }
                    (
                        Focus::Outside,
                        Some(
                            action @ (keymap::Action::LaunchLine
                            | keymap::Action::LaunchCell
                            | keymap::Action::LaunchSelection),
                        ),
                    ) => {
                        // A long rail has no safe whole-cell action. The same chord that launches
                        // an inline vector enters the rail, where every launch action
                        // is deliberately reduced to the selected title.
                        if rail {
                            if !s.cell().is_empty() {
                                s.focus = Focus::Inside;
                                s.line = 0;
                                s.item = 0;
                                s.item_goal = 0;
                            }
                            s.clamp();
                            drop(s);
                            paint();
                            return gtk::glib::Propagation::Stop;
                        }
                        let Some(machine) = s.view.get(s.col).cloned() else {
                            return gtk::glib::Propagation::Stop;
                        };
                        let apps: Vec<Arc<App>> = match action {
                            keymap::Action::LaunchLine => {
                                s.current_line().map(|l| l.apps.clone()).unwrap_or_default()
                            }
                            _ => s
                                .cell()
                                .iter()
                                .flat_map(|l| l.apps.iter().cloned())
                                .collect(),
                        };
                        let machine_name = machine.name.clone();
                        let launch_state = state.clone();
                        let launch_window = window.clone();
                        let launch_dismiss = dismiss.clone();
                        let launch_generation = interaction_generation.get();
                        let completion_generation = interaction_generation.clone();
                        drop(s);
                        queue_launch(
                            &runtime,
                            machine,
                            apps,
                            terminal_cmd.clone(),
                            inflight_launches.clone(),
                            move |launched| {
                                if launched.is_empty() {
                                    return;
                                }
                                let mut state = launch_state.borrow_mut();
                                for app_id in launched {
                                    state.record_launch(&machine_name, &app_id);
                                }
                                state.save_usage();
                                drop(state);
                                if generation_is_current(&completion_generation, launch_generation)
                                    && launch_window.is_visible()
                                {
                                    launch_dismiss(&launch_window);
                                }
                            },
                        );
                        return gtk::glib::Propagation::Stop;
                    }

                    // Left/right are the only keys that CHOOSE a column, so they are the only ones
                    // that move the goal. Up/down just change line and let `clamp` re-aim.
                    (Focus::Inside, Some(keymap::Action::MoveLeft)) => {
                        s.item = s.item.saturating_sub(1);
                        s.item_goal = s.item;
                    }
                    (Focus::Inside, Some(keymap::Action::MoveRight)) => {
                        let n = s.current_line().map(|l| l.apps.len()).unwrap_or(0);
                        s.item = (s.item + 1).min(n.saturating_sub(1));
                        s.item_goal = s.item;
                    }
                    (Focus::Inside, Some(keymap::Action::MoveUp)) => {
                        s.line = s.line.saturating_sub(1)
                    }
                    (Focus::Inside, Some(keymap::Action::MoveDown)) => {
                        s.line = (s.line + 1).min(s.cell().len().saturating_sub(1))
                    }
                    (
                        Focus::Inside,
                        Some(
                            action @ (keymap::Action::Enter
                            | keymap::Action::LaunchLine
                            | keymap::Action::LaunchCell
                            | keymap::Action::LaunchSelection),
                        ),
                    ) => {
                        let Some(machine) = s.view.get(s.col).cloned() else {
                            return gtk::glib::Propagation::Stop;
                        };
                        let apps: Vec<Arc<App>> = if rail {
                            s.current_line()
                                .and_then(|l| l.apps.get(s.item))
                                .cloned()
                                .into_iter()
                                .collect()
                        } else {
                            match action {
                                keymap::Action::Enter => s
                                    .current_line()
                                    .and_then(|l| l.apps.get(s.item))
                                    .cloned()
                                    .into_iter()
                                    .collect(),
                                keymap::Action::LaunchCell => s
                                    .cell()
                                    .iter()
                                    .flat_map(|l| l.apps.iter().cloned())
                                    .collect(),
                                _ => s.current_line().map(|l| l.apps.clone()).unwrap_or_default(),
                            }
                        };
                        let machine_name = machine.name.clone();
                        let launch_state = state.clone();
                        let launch_window = window.clone();
                        let launch_dismiss = dismiss.clone();
                        let launch_generation = interaction_generation.get();
                        let completion_generation = interaction_generation.clone();
                        drop(s);
                        queue_launch(
                            &runtime,
                            machine,
                            apps,
                            terminal_cmd.clone(),
                            inflight_launches.clone(),
                            move |launched| {
                                if launched.is_empty() {
                                    return;
                                }
                                let mut state = launch_state.borrow_mut();
                                for app_id in launched {
                                    state.record_launch(&machine_name, &app_id);
                                }
                                state.save_usage();
                                drop(state);
                                // A launcher that stays up after launching is a window you then
                                // have to dismiss. Closing is the confirmed handoff.
                                if generation_is_current(&completion_generation, launch_generation)
                                    && launch_window.is_visible()
                                {
                                    launch_dismiss(&launch_window);
                                }
                            },
                        );
                        return gtk::glib::Propagation::Stop;
                    }

                    (_, Some(keymap::Action::Backspace)) => {
                        let mut q = s.query.clone();
                        q.pop();
                        s.set_query(q);
                    }
                    (_, Some(keymap::Action::ResetVisibility)) => {
                        state_rebuilt = s.reset_visibility();
                    }
                    _ => {
                        // A chord is a command, not text. Without this, Ctrl-W and Alt-F typed a
                        // literal "w" and "f" into the search box.
                        let chord = mods.contains(ModifierType::CONTROL_MASK)
                            || mods.contains(ModifierType::ALT_MASK)
                            || mods.contains(ModifierType::SUPER_MASK);
                        if !chord
                            && let Some(ch) = key.to_unicode()
                            && !ch.is_control()
                        {
                            let mut q = s.query.clone();
                            q.push(ch);
                            s.set_query(q);
                        }
                    }
                }
                s.clamp();
                let cursor = Cursor {
                    col: s.col,
                    row: s.row,
                    line: s.line,
                    item: s.item,
                    inside: s.focus == Focus::Inside,
                };
                structural =
                    state_rebuilt || s.query != before || !painted.borrow().contains(cursor);
            }
            if structural {
                render();
            } else {
                paint();
            }
            gtk::glib::Propagation::Stop
        });
    }
    window.add_controller(keys);

    // cbar itself is the resident process. The launcher is built once and intentionally remains
    // hidden until its IPC action maps this exact window.
    LauncherUi {
        window,
        display: display.clone(),
        reveal,
        providers,
        interaction_generation,
        render_holder,
        inflight_launches,
        source_config,
        icon_identity,
        style_provider: provider,
        state,
        state_baseline: Rc::new(RefCell::new(state_baseline)),
    }
}

fn start_providers(
    runtime: &tokio::runtime::Handle,
    config: config::Config,
    state: Rc<RefCell<State>>,
    layout: Rc<RefCell<config::Layout>>,
    render: Rc<dyn Fn()>,
    scroller: gtk::ScrolledWindow,
    latest_generation: Rc<Cell<u64>>,
) -> Rc<provider::ProviderManager> {
    let generation = latest_generation.get().wrapping_add(1);
    latest_generation.set(generation);
    let (manager, mut updates) = provider::ProviderManager::start(&config, runtime);
    gtk::glib::spawn_future_local(async move {
        while let Some(update) = updates.recv().await {
            if latest_generation.get() != generation {
                break;
            }
            // Provider answers that become ready in the same main-loop turn share one deep model
            // rebuild and one GTK render. The 1ms coalescing window is far below a frame and keeps
            // a fleet coming online together from rebuilding the complete matrix N times.
            gtk::glib::timeout_future(std::time::Duration::from_millis(1)).await;
            let mut batch = vec![update];
            while let Ok(update) = updates.try_recv() {
                batch.push(update);
            }
            if latest_generation.get() != generation {
                break;
            }
            let changed = stream_model::apply_updates_to_state(
                &config,
                &mut state.borrow_mut(),
                batch.iter().map(AsRef::as_ref),
            );
            if changed {
                *layout.borrow_mut() = config.layout.clone();
                let horizontal = scroller.hadjustment().value();
                let vertical = scroller.vadjustment().value();
                render();
                let scroller = scroller.clone();
                gtk::glib::idle_add_local_once(move || {
                    let restore = |adjustment: gtk::Adjustment, value: f64| {
                        let maximum =
                            (adjustment.upper() - adjustment.page_size()).max(adjustment.lower());
                        adjustment.set_value(value.clamp(adjustment.lower(), maximum));
                    };
                    restore(scroller.hadjustment(), horizontal);
                    restore(scroller.vadjustment(), vertical);
                });
            }
        }
    });
    Rc::new(manager)
}

fn world_from_loaded(loaded: Result<Option<config::Config>, String>) -> World {
    match loaded {
        Err(e) => World {
            folders: vec!["Other".to_string()],
            machines: Vec::new(),
            theme: config::Theme::default(),
            layout: config::Layout::default(),
            terminal: vec![],
            surface: "layer".into(),
            keyboard: "exclusive".into(),
            exit_on_focus_loss: true,
            config: None,
            error: Some(e),
        },
        Ok(None) => World {
            folders: vec!["Other".to_string()],
            machines: Vec::new(),
            theme: config::Theme::default(),
            layout: config::Layout::default(),
            terminal: vec![],
            surface: "layer".into(),
            keyboard: "exclusive".into(),
            exit_on_focus_loss: true,
            config: None,
            error: None,
        },
        Ok(Some(mut cfg)) => {
            if let Err(error) = canonicalize_config_colors(&mut cfg) {
                return World {
                    folders: vec!["Other".to_string()],
                    machines: Vec::new(),
                    theme: config::Theme::default(),
                    layout: config::Layout::default(),
                    terminal: vec![],
                    surface: "layer".into(),
                    keyboard: "exclusive".into(),
                    exit_on_focus_loss: true,
                    config: None,
                    error: Some(error),
                };
            }
            let rows = cfg.folder_rows();
            // Cold construction is local-only. Cached/fresh columns stream in independently once
            // the GTK tree exists, so no provider command can delay cbar activation or reveal.
            let machines = cfg
                .machines
                .iter()
                .map(|machine| Machine {
                    name: machine.name.clone(),
                    aliases: machine.aliases.clone(),
                    accent: machine.accent.clone(),
                    launch: machine.launch.clone(),
                    error: None,
                    cells: vec![Vec::new(); rows.len()],
                })
                .collect();
            World {
                folders: rows,
                machines,
                theme: cfg.theme.clone(),
                layout: cfg.layout.clone(),
                terminal: cfg.terminal.clone(),
                surface: cfg.surface.clone(),
                keyboard: cfg.keyboard.clone(),
                exit_on_focus_loss: cfg.exit_on_focus_loss,
                config: Some(cfg),
                error: None,
            }
        }
    }
}

fn canonicalize_config_colors(config: &mut config::Config) -> Result<(), String> {
    fn canonical(name: &str, value: &mut String) -> Result<(), String> {
        let color = gtk::gdk::RGBA::parse(value.as_str())
            .map_err(|_| format!("{name} is not a valid GTK color"))?;
        let channel = |component: f32| (component.clamp(0.0, 1.0) * 255.0).round() as u8;
        let red = channel(color.red());
        let green = channel(color.green());
        let blue = channel(color.blue());
        let alpha = channel(color.alpha());
        *value = if alpha == u8::MAX {
            format!("#{red:02x}{green:02x}{blue:02x}")
        } else {
            format!("#{red:02x}{green:02x}{blue:02x}{alpha:02x}")
        };
        Ok(())
    }

    canonical("theme.ground", &mut config.theme.ground)?;
    canonical("theme.surface", &mut config.theme.surface)?;
    canonical("theme.fg", &mut config.theme.fg)?;
    canonical("theme.muted", &mut config.theme.muted)?;
    canonical("theme.dim", &mut config.theme.dim)?;
    canonical("theme.accent", &mut config.theme.accent)?;
    canonical("theme.error", &mut config.theme.error)?;
    canonical("theme.border", &mut config.theme.border)?;
    for machine in &mut config.machines {
        canonical("machine accent", &mut machine.accent)?;
    }
    Ok(())
}

fn config_colors_are_valid(config: &config::Config) -> bool {
    let mut checked = config.clone();
    canonicalize_config_colors(&mut checked).is_ok()
}

/// Golden-Master test helper which asks every fixture machine concurrently and returns results in
/// configured order. Production discovery uses `ProviderManager`: independently recovering
/// per-machine state machines admitted through a hardware-aware bounded command lane. Keeping this
/// scoped-thread version test-only preserves the old parity fixtures without making its unbounded
/// one-thread-per-fixture policy a runtime claim.
#[cfg(test)]
#[allow(dead_code)]
fn inventory_bytes_all(machines: &[config::MachineConfig]) -> Vec<Result<Vec<u8>, String>> {
    std::thread::scope(|scope| {
        let handles: Vec<_> = machines
            .iter()
            .map(|mc| scope.spawn(move || inventory_bytes(mc)))
            .collect();
        handles
            .into_iter()
            .map(|h| {
                h.join()
                    .unwrap_or_else(|_| Err("inventory panicked".into()))
            })
            .collect()
    })
}

/// The grid those answers describe. Pure, so it can be skipped when the answers have not moved.
#[cfg(test)]
#[allow(dead_code)]
fn machines_from(
    machines: &[config::MachineConfig],
    printed: &[Result<Vec<u8>, String>],
    rows: &[String],
    subrows: &std::collections::HashMap<String, Vec<config::SubRow>>,
) -> Vec<Machine> {
    machines
        .iter()
        .zip(printed)
        .map(|(mc, raw)| machine_from(mc, raw, rows, subrows))
        .collect()
}

#[cfg(test)]
#[allow(dead_code)]
fn inventory_all(
    machines: &[config::MachineConfig],
    rows: &[String],
    subrows: &std::collections::HashMap<String, Vec<config::SubRow>>,
) -> Vec<Machine> {
    machines_from(machines, &inventory_bytes_all(machines), rows, subrows)
}

/// Ask ONE machine what it has, by running the command config named for it.
///
/// Everything this program knows about detection is in these few lines: run argv, read JSON. No
/// SSH, no .desktop parsing, no package managers -- see config.rs on why that boundary is the
/// point rather than a simplification.
/// WHAT THE MACHINE PRINTED, unparsed -- or why it could not be asked.
///
/// This direct command helper is retained only for Golden-Master tests. Production uses the
/// independently recovering bounded provider manager, which hashes an answer before normalization
/// and suppresses byte-identical refreshes without rebuilding GTK state.
#[cfg(test)]
#[allow(dead_code)]
fn inventory_bytes(mc: &config::MachineConfig) -> Result<Vec<u8>, String> {
    mc.inventory
        .split_first()
        .ok_or_else(|| "no inventory command configured".to_string())
        .and_then(|(bin, args)| {
            command_output(
                bin,
                args,
                std::time::Duration::from_millis(mc.inventory_timeout_ms),
            )
        })
        .and_then(|out| {
            if out.status.success() {
                Ok(out.stdout)
            } else {
                let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
                Err(if stderr.is_empty() {
                    format!("inventory exited with {}", out.status)
                } else {
                    stderr
                })
            }
        })
}

/// Turn one machine's printed answer into its column. Pure: no processes, no clock, no I/O.
#[cfg(test)]
#[allow(dead_code)]
fn machine_from(
    mc: &config::MachineConfig,
    printed: &Result<Vec<u8>, String>,
    rows: &[String],
    subrows: &std::collections::HashMap<String, Vec<config::SubRow>>,
) -> Machine {
    let mut cells: Vec<Vec<Line>> = vec![Vec::new(); rows.len()];
    // Declared without a value: both arms of the match below assign it, so an initial `None`
    // would be a value nothing ever reads -- which is exactly what the compiler was saying.
    let error;

    let parsed = match printed {
        Ok(bytes) => config::parse_inventory(bytes),
        Err(e) => Err(e.clone()),
    };

    match parsed {
        Ok(inv) => {
            // The machine reporting its own failure outranks anything inferred here.
            error = inv.error;
            for folder in inv.folders {
                // A label the config does not list falls into the inbox rather than being
                // dropped: an app nobody categorised must still be reachable.
                // WHICH ROW, and a declared subcategory beats the catch-all.
                //
                // The category table said which BOX this belongs in; the subcategory says which
                // rung inside it. Matching happens here, once, against the operator's own list,
                // rather than each application having to be dragged into place -- two hundred of
                // them is not a drag-and-drop job.
                let declared = subrows.get(&folder.label);
                let row_label = |a: &config::InventoryApp| -> String {
                    let id = a.id.clone().unwrap_or_default().to_lowercase();
                    let name = a.name.to_lowercase();
                    declared
                        .into_iter()
                        .flatten()
                        .find(|sr| {
                            sr.apps.iter().any(|want| {
                                let w = want.to_lowercase();
                                !w.is_empty() && (id.contains(&w) || name.contains(&w))
                            })
                        })
                        .map(|sr| format!("{}/{}", folder.label, sr.name))
                        .unwrap_or_else(|| folder.label.clone())
                };
                // Grouped by the row each app lands in, so a subcategory's members end up on its
                // row together rather than scattered by the order the inventory happened to list
                // them in.
                let mut by_row: Vec<Vec<&config::InventoryApp>> = vec![Vec::new(); rows.len()];
                for a in &folder.apps {
                    let label = row_label(a);
                    let r = rows
                        .iter()
                        .position(|x| *x == label)
                        .unwrap_or(rows.len().saturating_sub(1));
                    if let Some(bucket) = by_row.get_mut(r) {
                        bucket.push(a);
                    }
                }
                for (r, apps) in by_row
                    .into_iter()
                    .enumerate()
                    .filter(|(_, apps)| !apps.is_empty())
                {
                    cells[r].push(Line {
                        name: None,
                        apps: apps
                            .iter()
                            .map(|a| {
                                Arc::new(App {
                                    id: a.id.clone().unwrap_or_else(|| a.name.clone()),
                                    name: a.name.clone(),
                                    icon: a.icon.clone(),
                                    exec: a.exec.clone(),
                                    terminal: a.terminal,
                                    desktop_file: a.desktop_file.clone(),
                                })
                            })
                            .collect(),
                    });
                }
            }
        }
        Err(e) => error = Some(e),
    }

    // The declared sub-rows, appended empty. They are drawn so they can be dragged into; an app
    // that has actually been filed into one arrives through the placement instead, which runs
    // after this and matches them by name.
    Machine {
        name: mc.name.clone(),
        aliases: mc.aliases.clone(),
        accent: mc.accent.clone(),
        launch: mc.launch.clone(),
        error,
        cells,
    }
}

/// Run one inventory command with a wall-clock bound. A process is put in its own process group so
/// a timeout can terminate the command and the helpers it started rather than leaving an SSH child
/// behind to keep the captured stdout/stderr pipes open forever.
#[cfg(test)]
fn command_output(
    bin: &str,
    args: &[String],
    timeout: std::time::Duration,
) -> Result<std::process::Output, String> {
    use std::os::fd::AsRawFd;
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new(bin)
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|e| format!("{bin}: {e}"))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{bin}: stdout pipe was not created"))?;
    let mut stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{bin}: stderr pipe was not created"))?;
    for fd in [stdout.as_raw_fd(), stderr.as_raw_fd()] {
        // Nonblocking reads let this thread drain BOTH pipes while it also watches the deadline.
        // A reader thread per pipe looks simpler, but cannot be stopped if a grandchild inherits a
        // write end and keeps it open; one reveal would then leak two threads indefinitely.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            stop_child_group(&mut child);
            return Err(format!(
                "{bin}: could not make inventory pipes nonblocking: {}",
                std::io::Error::last_os_error()
            ));
        }
    }

    let started = std::time::Instant::now();
    let mut status = None;
    let mut stdout_bytes = Vec::new();
    let mut stderr_bytes = Vec::new();
    let mut stdout_done = false;
    let mut stderr_done = false;
    let mut collect_until = None;
    loop {
        if status.is_none() {
            match child.try_wait() {
                Ok(Some(exit)) => {
                    status = Some(exit);
                    // Usually EOF is already visible. Give readers at least a short grace when the
                    // process exits on its deadline, while never waiting beyond the configured
                    // bound merely because a descendant inherited a pipe.
                    let now = std::time::Instant::now();
                    let grace = now
                        .checked_add(std::time::Duration::from_millis(100))
                        .unwrap_or(now);
                    collect_until = Some(
                        started
                            .checked_add(timeout)
                            .map(|deadline| deadline.max(grace))
                            .unwrap_or(grace),
                    );
                }
                Ok(None) => {}
                Err(e) => {
                    stop_child_group(&mut child);
                    return Err(format!("{bin}: {e}"));
                }
            }
        }

        let drained = (|| -> Result<(), String> {
            if !stdout_done {
                stdout_done = drain_capture(&mut stdout, &mut stdout_bytes, "stdout")?;
            }
            if !stderr_done {
                stderr_done = drain_capture(&mut stderr, &mut stderr_bytes, "stderr")?;
            }
            Ok(())
        })();
        if let Err(e) = drained {
            if status.is_none() {
                stop_child_group(&mut child);
            }
            return Err(format!("{bin}: {e}"));
        }

        if let Some(exit) = status {
            if stdout_done && stderr_done {
                return Ok(std::process::Output {
                    status: exit,
                    stdout: stdout_bytes,
                    stderr: stderr_bytes,
                });
            }
            if collect_until.is_some_and(|deadline| std::time::Instant::now() >= deadline) {
                return Err(format!(
                    "{bin}: inventory output remained open after the command exited"
                ));
            }
        } else if started.elapsed() >= timeout {
            // Negative pid means the process group created above. The leader has NOT been reaped
            // in this branch, so its pgid cannot have been recycled under us. Once try_wait returns
            // a status we deliberately never send to that number again.
            stop_child_group(&mut child);
            return Err(format!(
                "inventory timed out after {} ms",
                timeout.as_millis()
            ));
        }

        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// Drain one nonblocking pipe without giving an arbitrary inventory command unbounded memory.
/// Sixteen MiB is orders of magnitude above a normal inventory, while still making a runaway
/// producer a column error rather than a launcher-wide OOM.
#[cfg(test)]
fn drain_capture(
    pipe: &mut impl std::io::Read,
    bytes: &mut Vec<u8>,
    label: &str,
) -> Result<bool, String> {
    const MAX_BYTES: usize = 16 * 1024 * 1024;
    let mut buf = [0u8; 16 * 1024];
    loop {
        let remaining = (MAX_BYTES + 1).saturating_sub(bytes.len());
        if remaining == 0 {
            return Err(format!("inventory {label} exceeded {MAX_BYTES} bytes"));
        }
        let take = remaining.min(buf.len());
        match pipe.read(&mut buf[..take]) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                bytes.extend_from_slice(&buf[..n]);
                if bytes.len() > MAX_BYTES {
                    return Err(format!("inventory {label} exceeded {MAX_BYTES} bytes"));
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("could not read inventory {label}: {e}")),
        }
    }
}

/// Kill and reap a child which is still known to lead the process group created for it.
#[cfg(test)]
fn stop_child_group(child: &mut std::process::Child) {
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.wait();
}

/// Atomically queue one appset through the process-wide detached-launch service. Submission is a
/// single all-or-none reservation: queue pressure can reject the set, but can never start only its
/// first few applications. Tickets stay in input order, so each successful handoff maps back to
/// the exact stable app id that earns usage. All process creation, manager probing and reaping live
/// in the shared service; GTK only prepares owned argv and applies the completed identities.
fn queue_launch(
    runtime: &tokio::runtime::Handle,
    machine: Machine,
    apps: Vec<Arc<App>>,
    terminal: Vec<String>,
    inflight: Rc<RefCell<HashSet<(String, String)>>>,
    completed: impl FnOnce(Vec<String>) + 'static,
) {
    if let Err(error) = validate_launch_batch_len(apps.len()) {
        eprintln!("cbar launcher: {error}");
        completed(Vec::new());
        return;
    }
    let mut requests = Vec::with_capacity(apps.len());
    for app in apps {
        let key = (machine.name.clone(), app.id.clone());
        if !inflight.borrow_mut().insert(key.clone()) {
            continue;
        }
        match LaunchRequest::new(&machine, app, &terminal) {
            Ok(request) => requests.push((key, request)),
            Err(error) => {
                inflight.borrow_mut().remove(&key);
                eprintln!("cbar launcher: {error}");
            }
        }
    }
    if requests.is_empty() {
        completed(Vec::new());
        return;
    }
    let queued_keys = requests
        .iter()
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    let batch = requests
        .iter()
        .map(|(_, request)| request.argv.clone())
        .collect::<Vec<_>>();
    let tickets = match submit_detached_batch(batch) {
        Ok(tickets) => tickets,
        Err(error) => {
            for key in queued_keys {
                inflight.borrow_mut().remove(&key);
            }
            eprintln!("cbar launcher: launch batch was not queued: {error}");
            completed(Vec::new());
            return;
        }
    };
    let worker = runtime.spawn(async move {
        let mut outcomes = Vec::with_capacity(requests.len());
        for ((key, request), ticket) in requests.into_iter().zip(tickets) {
            outcomes.push((key, request, ticket.await));
        }
        outcomes
    });
    gtk::glib::spawn_future_local(async move {
        let outcomes = match worker.await {
            Ok(outcomes) => outcomes,
            Err(error) => {
                for key in queued_keys {
                    inflight.borrow_mut().remove(&key);
                }
                eprintln!("cbar launcher: launch worker failed: {error}");
                completed(Vec::new());
                return;
            }
        };
        let mut launched = Vec::new();
        for (key, request, outcome) in outcomes {
            inflight.borrow_mut().remove(&key);
            match outcome {
                Ok(_receipt) => {
                    eprintln!(
                        "cbar launcher: started {} on {}",
                        request.app_name, request.machine_name
                    );
                    launched.push(request.app_id);
                }
                Err(error) => eprintln!(
                    "cbar launcher: {} on {}: {error}",
                    request.app_name, request.machine_name
                ),
            }
        }
        completed(launched);
    });
}

fn validate_launch_batch_len(apps: usize) -> Result<(), String> {
    if apps > MAX_APPSET_LAUNCH {
        Err(format!(
            "refusing to launch {apps} applications as one appset (maximum {MAX_APPSET_LAUNCH})"
        ))
    } else {
        Ok(())
    }
}

struct LaunchRequest {
    app_id: String,
    app_name: String,
    machine_name: String,
    argv: Vec<String>,
}

impl LaunchRequest {
    fn new(machine: &Machine, app: Arc<App>, terminal: &[String]) -> Result<Self, String> {
        if app.terminal && terminal.is_empty() {
            return Err(format!(
                "{} needs a terminal and none is configured (set `terminal` in config)",
                app.name
            ));
        }
        if app.exec.trim().is_empty() {
            return Err(format!("{} has an empty exec line", app.name));
        }
        let argv = launch_argv(machine, &app, terminal)
            .ok_or_else(|| format!("{} is a read-only column", machine.name))?;
        if argv.is_empty() {
            return Err(format!("{} has no exec line", app.name));
        }
        Ok(Self {
            app_id: app.id.clone(),
            app_name: app.name.clone(),
            machine_name: machine.name.clone(),
            argv,
        })
    }
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn machine_with_apps(name: &str, count: usize) -> Machine {
        let apps = (0..count)
            .map(|index| {
                Arc::new(App {
                    id: format!("{name}-{index}"),
                    name: format!("{name} {index}"),
                    icon: String::new(),
                    exec: "true".into(),
                    terminal: false,
                    desktop_file: String::new(),
                })
            })
            .collect();
        Machine {
            name: name.into(),
            aliases: Vec::new(),
            accent: "#fff".into(),
            launch: vec!["{}".into()],
            error: None,
            cells: vec![vec![Line { name: None, apps }]],
        }
    }

    #[test]
    fn large_machine_columns_page_without_deleting_model_data() {
        let machines = vec![
            machine_with_apps("first", 3_000),
            machine_with_apps("second", 3_000),
        ];
        assert_eq!(materialized_columns(&machines, 0, 512), [0]);
        assert_eq!(materialized_columns(&machines, 1, 512), [1]);
        assert_eq!(machines[0].cells[0][0].apps.len(), 3_000);
        assert_eq!(machines[1].cells[0][0].apps.len(), 3_000);

        let ordinary = vec![
            machine_with_apps("first", 191),
            machine_with_apps("second", 191),
        ];
        assert_eq!(materialized_columns(&ordinary, 0, 512), [0, 1]);
        assert_eq!(materialized_columns(&ordinary, 1, 512), [0, 1]);
        assert!(max_materialized_icons(20) > 4_096);
        assert_eq!(max_materialized_icons(256), 256);
    }

    #[test]
    fn one_maximum_inventory_materializes_a_bounded_cursor_page() {
        let machines = vec![machine_with_apps(
            "maximum",
            cbar_launcher_core::config::MAX_INVENTORY_APPS,
        )];
        let cursor = Cursor {
            col: 0,
            row: 0,
            line: 0,
            item: cbar_launcher_core::config::MAX_INVENTORY_APPS - 1,
            inside: true,
        };
        let page = materialization(&machines, cursor, 256);
        assert!(page.is_paged());
        assert_eq!(page.apps.len(), max_materialized_icons(256));
        assert!(page.apps.contains(&AppPosition {
            col: cursor.col,
            row: cursor.row,
            line: cursor.line,
            item: cursor.item,
        }));
        assert!(page.rows.len() <= MAX_MATERIALIZED_ROWS);
        assert!(page.columns.len() <= MAX_MATERIALIZED_COLUMNS);
        assert_eq!(
            machines[0].cells[0][0].apps.len(),
            cbar_launcher_core::config::MAX_INVENTORY_APPS,
            "the complete model is retained"
        );
    }

    #[test]
    fn sparse_rows_and_many_machines_are_bounded_independently() {
        let mut sparse = machine_with_apps("sparse", 0);
        sparse.cells = (0..256)
            .map(|row| {
                vec![Line {
                    name: None,
                    apps: vec![Arc::new(App {
                        id: format!("sparse-{row}"),
                        name: format!("Sparse {row}"),
                        icon: String::new(),
                        exec: "true".into(),
                        terminal: false,
                        desktop_file: String::new(),
                    })],
                }]
            })
            .collect();
        let page = materialization(
            &[sparse],
            Cursor {
                col: 0,
                row: 255,
                line: 0,
                item: 0,
                inside: true,
            },
            32,
        );
        assert!(page.rows.len() <= MAX_MATERIALIZED_ROWS);
        assert!(page.apps.len() <= MAX_MATERIALIZED_APP_WIDGETS);
        assert!(page.apps.contains(&AppPosition {
            col: 0,
            row: 255,
            line: 0,
            item: 0,
        }));

        let machines = (0..32)
            .map(|index| machine_with_apps(&format!("machine-{index}"), 0))
            .collect::<Vec<_>>();
        let columns = materialized_columns(&machines, 31, 512);
        assert!(columns.len() <= MAX_MATERIALIZED_COLUMNS);
        assert!(columns.contains(&31));
    }

    #[test]
    fn thousands_of_empty_named_rows_materialize_only_one_bounded_page() {
        let mut machine = machine_with_apps("empty-rows", 0);
        machine.cells = (0..1_024)
            .map(|row| {
                vec![Line {
                    name: Some(format!("named-{row}")),
                    apps: Vec::new(),
                }]
            })
            .collect();
        let cursor = Cursor {
            col: 0,
            row: 1_023,
            line: 0,
            item: 0,
            inside: false,
        };
        let page = materialization(&[machine], cursor, 20);
        assert!(page.is_paged());
        assert!(page.rows.len() <= MAX_MATERIALIZED_ROWS);
        assert!(page.rows.contains(&cursor.row));
        assert!(page.lines.len() <= MAX_MATERIALIZED_ROWS);
    }

    #[test]
    fn stale_launch_completion_is_not_current_after_reveal_or_hide() {
        let generation = Cell::new(0);
        let launch = generation.get();
        assert!(generation_is_current(&generation, launch));
        advance_generation(&generation);
        assert!(!generation_is_current(&generation, launch));
    }

    #[test]
    fn oversized_appset_is_rejected_before_any_partial_launch() {
        assert!(validate_launch_batch_len(MAX_APPSET_LAUNCH).is_ok());
        let error = validate_launch_batch_len(MAX_APPSET_LAUNCH + 1).unwrap_err();
        assert!(error.contains("refusing to launch"));
    }

    fn prepared_with_config(config: config::Config) -> PreparedLauncher {
        PreparedLauncher {
            config: Ok(Some(config)),
            placement: (Placement::default(), None),
            visibility: (Visibility::default(), None),
            usage: (usage::Usage::default(), None),
            icons: None,
        }
    }

    #[test]
    fn every_whole_ui_config_policy_change_requires_reconstruction() {
        let current: config::Config = serde_json::from_str(r#"{"machines":[]}"#).unwrap();
        let variants: Vec<config::Config> = [
            r#"{"machines":[],"keys":{"ctrl+k":"cancel"}}"#,
            r##"{"machines":[],"theme":{"ground":"#101010","icon_size":48}}"##,
            r#"{"machines":[],"terminal":["foot","-e"]}"#,
            r#"{"machines":[],"exit_on_focus_loss":false,"keyboard":"none"}"#,
        ]
        .into_iter()
        .map(|json| serde_json::from_str(json).unwrap())
        .collect();
        for changed in variants {
            assert!(prepared_config_requires_rebuild(
                Some(&current),
                &prepared_with_config(changed)
            ));
        }
        assert!(!prepared_config_requires_rebuild(
            Some(&current),
            &prepared_with_config(current.clone())
        ));
        let invalid = PreparedLauncher {
            config: Err("half-written declarative config".into()),
            placement: (Placement::default(), None),
            visibility: (Visibility::default(), None),
            usage: (usage::Usage::default(), None),
            icons: None,
        };
        assert!(
            !prepared_config_requires_rebuild(Some(&current), &invalid),
            "a transient invalid reload retains last-known-good UI"
        );
        let deleted = PreparedLauncher {
            config: Ok(None),
            placement: (Placement::default(), None),
            visibility: (Visibility::default(), None),
            usage: (usage::Usage::default(), None),
            icons: None,
        };
        assert!(
            prepared_config_requires_rebuild(Some(&current), &deleted),
            "deleting config replaces the resident launcher with an honest empty state"
        );
    }

    fn mutable_snapshot(writable: bool) -> MutableStateSnapshot {
        MutableStateSnapshot {
            placement: Placement::default(),
            placement_writable: writable,
            visibility: Visibility::default(),
            visibility_writable: writable,
            usage: usage::Usage::default(),
            usage_writable: writable,
        }
    }

    #[test]
    fn replacement_recovers_repaired_or_external_state_without_rolling_back_live_edits() {
        let unreadable = mutable_snapshot(false);
        let repaired = mutable_snapshot(true);
        assert_eq!(
            replacement_decision(false, false, &unreadable, &unreadable, &repaired),
            ReplacementDecision {
                rebuild: true,
                transfer_current: false,
                advance_baseline: false,
            },
            "repairing a state file must recover writability without restarting cbar"
        );

        let baseline = mutable_snapshot(true);
        let mut external = baseline.clone();
        external
            .visibility
            .entry("arbitrary".into())
            .or_default()
            .insert("external-app".into());
        assert!(replacement_decision(false, false, &baseline, &baseline, &external).rebuild);

        let mut live = baseline.clone();
        live.visibility
            .entry("arbitrary".into())
            .or_default()
            .insert("just-hidden".into());
        let pending = replacement_decision(true, false, &baseline, &live, &baseline);
        assert!(pending.rebuild);
        assert!(
            pending.transfer_current,
            "a pending async save must be transferred into a config rebuild"
        );
        assert!(
            !replacement_decision(false, false, &baseline, &live, &baseline).rebuild,
            "an unchanged config must retain unsaved live state rather than reload stale disk"
        );

        let caught_up = replacement_decision(false, false, &baseline, &live, &live);
        assert!(!caught_up.rebuild);
        assert!(caught_up.advance_baseline);
    }

    #[test]
    fn inflight_launch_defers_replacement_then_clean_refresh_adopts_saved_usage() {
        let baseline = mutable_snapshot(true);
        let prepared_before_receipt = baseline.clone();
        let deferred =
            replacement_decision(true, true, &baseline, &baseline, &prepared_before_receipt);
        assert_eq!(
            deferred,
            ReplacementDecision {
                rebuild: false,
                transfer_current: false,
                advance_baseline: false,
            },
            "a replacement must not split ownership of a pending launch receipt"
        );

        let mut completed = baseline.clone();
        completed.usage.insert(
            usage::key("machine", "completed-app"),
            usage::Entry {
                score: 1.0,
                last: 1,
            },
        );
        let refreshed = replacement_decision(true, false, &baseline, &completed, &completed);
        assert!(refreshed.rebuild);
        assert!(!refreshed.transfer_current);
        assert!(refreshed.advance_baseline);
        assert_eq!(
            completed
                .usage
                .get(&usage::key("machine", "completed-app"))
                .map(|entry| entry.score),
            Some(1.0),
            "the next clean refresh carries the persisted launch into the replacement"
        );
    }

    #[test]
    fn retained_render_holder_services_post_build_invalidations_until_retire() {
        let renders = Rc::new(Cell::new(0usize));
        let holder: CallbackSlot = Rc::new(RefCell::new(None));
        let weak = Rc::downgrade(&holder);
        *holder.borrow_mut() = Some(Rc::new({
            let renders = renders.clone();
            move || renders.set(renders.get() + 1)
        }));

        // This is the ownership shape returned by `build`: controllers retain `weak`, while
        // LauncherUi retains `holder` after the constructor's local variables have gone away.
        invalidate_render(&weak);
        assert_eq!(renders.get(), 1);

        holder.borrow_mut().take();
        invalidate_render(&weak);
        assert_eq!(
            renders.get(),
            1,
            "retirement clears the callback before stale controllers can invalidate it"
        );
    }

    #[test]
    fn failed_state_reload_retains_live_values_and_becomes_read_only() {
        let mut current = mutable_snapshot(true);
        current
            .placement
            .entry("machine".into())
            .or_default()
            .insert("Other".into(), vec![StoredLine::Bare(vec!["app".into()])]);
        current
            .visibility
            .entry("machine".into())
            .or_default()
            .insert("hidden".into());
        current.usage.insert(
            usage::key("machine", "app"),
            usage::Entry {
                score: 2.0,
                last: 1,
            },
        );
        let mut prepared = PreparedLauncher {
            config: Ok(Some(serde_json::from_str(r#"{"machines":[]}"#).unwrap())),
            placement: (
                Placement::default(),
                Some("temporary placement error".into()),
            ),
            visibility: (
                Visibility::default(),
                Some("temporary visibility error".into()),
            ),
            usage: (
                usage::Usage::default(),
                Some("temporary usage error".into()),
            ),
            icons: None,
        };
        prepared.retain_failed_state_from(&current);
        let retained = prepared.state_snapshot();
        assert_eq!(retained.placement, current.placement);
        assert_eq!(retained.visibility, current.visibility);
        assert_eq!(retained.usage, current.usage);
        assert!(!retained.placement_writable);
        assert!(!retained.visibility_writable);
        assert!(!retained.usage_writable);
    }

    #[test]
    fn gtk_invalid_color_reload_is_not_a_coherent_replacement() {
        let mut config: config::Config = serde_json::from_str(r#"{"machines":[]}"#).unwrap();
        config.theme.accent = "#fff; } window { color: red".into();
        assert!(!config_colors_are_valid(&config));
    }

    #[test]
    fn cbar_scoping_preserves_the_exact_golden_stylesheet() {
        let theme = config::Theme::default();
        let unscoped = scoped_golden_css(&theme)
            .replace("window.cbar-launcher", "window")
            .replace(".cbar-launcher ", "");
        assert_eq!(unscoped, golden_css(&theme));
    }

    #[test]
    fn scoped_launcher_sheet_wins_adversarial_display_global_bar_css_only_in_launcher() {
        // Representative valid upstream/user bar CSS. These selectors match launcher widgets too
        // when both windows share one GTK display, which is why APPLICATION priority is not enough.
        let adversarial_bar_css = "* { color: red; padding: 20px; margin: 10px; } window { background: magenta; } label, button { padding: 99px; }";
        assert!(adversarial_bar_css.contains("window"));
        assert!(adversarial_bar_css.contains("label"));
        const { assert!(LAUNCHER_STYLE_PRIORITY > gtk::STYLE_PROVIDER_PRIORITY_USER) };

        let embedded = css(&config::Theme::default());
        assert!(!embedded.contains(".cbar-launcher *"));
        assert!(embedded.contains("font-family: inherit"));
        assert!(embedded.contains("padding: 0"));
        assert!(embedded.contains("margin: 0"));
        assert!(embedded.contains("window.cbar-launcher"));
        assert!(embedded.contains(".cbar-launcher .app"));
        assert!(!embedded.contains("\nwindow {"));
        assert!(!embedded.contains("\n.app {"));
    }

    #[test]
    fn missing_or_invalid_config_never_invents_runtime_data() {
        let missing = world_from_loaded(Ok(None));
        assert!(missing.machines.is_empty());
        assert_eq!(missing.folders, ["Other"]);
        assert!(missing.error.is_none());

        let invalid = world_from_loaded(Err("invalid config".to_string()));
        assert!(invalid.machines.is_empty());
        assert_eq!(invalid.folders, ["Other"]);
        assert_eq!(invalid.error.as_deref(), Some("invalid config"));
    }

    #[test]
    fn colors_use_gdks_parser_and_are_canonical_before_css_or_markup() {
        let mut config: config::Config = serde_json::from_str(
            r#"{"machines":[{"name":"host","inventory":["true"],"accent":"rgba(1, 2, 3, 0.5)"}],"theme":{"accent":"rgb(4, 5, 6)"}}"#,
        )
        .unwrap();
        canonicalize_config_colors(&mut config).expect("valid GTK CSS colors");
        assert!(config.theme.accent.starts_with('#'));
        assert!(config.machines[0].accent.starts_with('#'));

        config.theme.accent = "#fff; } window { color: red".into();
        assert!(canonicalize_config_colors(&mut config).is_err());
    }

    /// The old wait-then-read implementation blocked as soon as either pipe reached 64 KiB. Real
    /// inventories can cross that threshold simply through long absolute Exec paths.
    #[test]
    fn inventory_output_larger_than_a_pipe_is_drained_while_running() {
        let script = concat!(
            "dd if=/dev/zero bs=131072 count=1 2>/dev/null; ",
            "dd if=/dev/zero bs=131072 count=1 1>&2 2>/dev/null"
        );
        let output = command_output(
            "sh",
            &["-c".to_string(), script.to_string()],
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 131_072);
        assert_eq!(output.stderr.len(), 131_072);
    }

    #[test]
    fn inventory_timeout_is_a_wall_clock_bound() {
        let started = std::time::Instant::now();
        let error = command_output(
            "sh",
            &["-c".to_string(), "sleep 10".to_string()],
            std::time::Duration::from_millis(50),
        )
        .unwrap_err();
        assert!(error.contains("timed out after 50 ms"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
