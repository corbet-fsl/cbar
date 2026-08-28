// config.rs — what cbar's launcher is told, as opposed to what it discovers or rearranges.
//
// FOUR KINDS OF DATA, AND KEEPING THEM APART IS THE WHOLE DESIGN:
//
//   config     (this file)  declared by the user, read-only here. Which machines
//                           exist, what they are called, what colour they are, and HOW TO ASK each
//                           one what it has. Rebuilt by a deploy; never written by this program.
//   inventory  (model.rs)   what a machine actually has right now. Discovered, cached, disposable
//                           -- a re-inventory may replace it wholesale at any time.
//   placement  (model.rs)   what the user rearranged. Written by this program, and must survive
//                           both of the above being replaced.
//   visibility (model.rs)   what the user chose not to see. Also user state, but separate from
//                           placement so showing an app again restores its exact old position.
//
// Conflating any two of them is how a launcher ends up either forgetting your arrangement on every
// rebuild, or freezing an app list that has since changed.
//
// ── WHY THIS SCHEMA IS NOT NEW ───────────────────────────────────────────────────────────────
//
// The schema keeps ordering and identity explicit:
//
//   * `machines` is a LIST, not a map. The order is the tab order and it is meaningful; a map
//     would silently alphabetise it.
//   * `folders` is a LIST in PRIORITY ORDER, for the same reason and a sharper one: grouping is
//     first-match-wins, so "TerminalEmulator" must be able to precede "System" or every terminal
//     lands in System. Alphabetising that changes which group an app falls into.
//   * `hide` matches a `.desktop` FILENAME, never a display name. The filename is the stable
//     identifier; `Name=` is localised and changes with a package update.
//
// ── HOW PROGRAMS ARE DETECTED: THEY ARE NOT, HERE ────────────────────────────────────────────
//
// `inventory` is argv plus a JSON contract, and that is the entire detection story from this
// toolkit-free core's point of view. The GTK shell may satisfy that contract with its optional
// generic freedesktop provider or with any configured executable; this crate knows neither the
// transport nor the discovery mechanism.
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;

const MAX_CONFIG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_ICON_SIZE: i32 = 256;
const MAX_LOGO_SIZE: i32 = 1024;
const MAX_WINDOW_WIDTH: i32 = 16_384;
const MAX_MACHINES: usize = 256;
const MAX_ROWS: usize = 1024;
const MAX_INVENTORY_FOLDERS: usize = 1024;
pub const MAX_INVENTORY_APPS: usize = 4_096;
pub const MAX_INVENTORY_HOST_BYTES: usize = 1_024;
pub const MAX_INVENTORY_ERROR_BYTES: usize = 64 * 1_024;
pub const MAX_INVENTORY_FOLDER_BYTES: usize = 1_024;
pub const MAX_INVENTORY_APP_TEXT_BYTES: usize = 4 * 1_024;
pub const MAX_INVENTORY_EXEC_BYTES: usize = 64 * 1_024;
//
// That keeps this repo buildable and testable by anyone with no remote machines -- point `inventory`
// at a script that echoes fixed JSON and the launcher works completely.
use serde::de::{self, IgnoredAny, SeqAccess, Visitor};
use serde::{Deserialize, Deserializer};
use std::fmt;
use std::path::PathBuf;

/// Read what an inventory command printed.
///
/// HERE RATHER THAN IN A SHELL, because the shape of this JSON is the contract and the contract is
/// the core's. A shell's job is to run the command and hand over the bytes; if each one parsed for
/// itself, each would own a copy of the schema and they would drift the first time a field was
/// added. It also means a shell needs no JSON library of its own -- the GTK one carried serde_json
/// solely for this call.
pub fn parse_inventory(bytes: &[u8]) -> Result<Inventory, String> {
    parse_inventory_bounded(bytes, MAX_INVENTORY_APPS)
}

/// Parse one inventory with a caller-provided fair-share application quota. The structural pass
/// rejects count/field abuse before the complete typed tree is built, which is important when many
/// independent providers share one resident desktop process.
pub fn parse_inventory_bounded(bytes: &[u8], max_apps: usize) -> Result<Inventory, String> {
    let shape = serde_json::from_slice::<InventoryShape>(bytes)
        .map_err(|error| format!("unreadable inventory: {error}"))?;
    if shape.host.len() > MAX_INVENTORY_HOST_BYTES {
        return Err(format!(
            "inventory host exceeds {MAX_INVENTORY_HOST_BYTES} bytes"
        ));
    }
    if shape
        .error
        .as_ref()
        .is_some_and(|error| error.len() > MAX_INVENTORY_ERROR_BYTES)
    {
        return Err(format!(
            "inventory error exceeds {MAX_INVENTORY_ERROR_BYTES} bytes"
        ));
    }
    let max_apps = max_apps.min(MAX_INVENTORY_APPS);
    if shape.folders.apps > max_apps {
        return Err(format!(
            "inventory has more than its fair-share limit of {max_apps} applications"
        ));
    }
    let inventory: Inventory =
        serde_json::from_slice(bytes).map_err(|e| format!("unreadable inventory: {e}"))?;
    if inventory.folders.len() > MAX_INVENTORY_FOLDERS {
        return Err(format!(
            "inventory has more than {MAX_INVENTORY_FOLDERS} folders"
        ));
    }
    let apps = inventory
        .folders
        .iter()
        .try_fold(0usize, |total, folder| total.checked_add(folder.apps.len()))
        .ok_or_else(|| "inventory application count overflowed".to_string())?;
    if apps > max_apps {
        return Err(format!(
            "inventory has more than its fair-share limit of {max_apps} applications"
        ));
    }
    let mut ids = std::collections::HashSet::with_capacity(apps);
    for app in inventory.folders.iter().flat_map(|folder| &folder.apps) {
        let id = app.id.as_deref().unwrap_or(&app.name);
        if !ids.insert(id) {
            return Err(format!(
                "inventory contains duplicate application id {id:?}"
            ));
        }
    }
    Ok(inventory)
}

#[derive(Deserialize)]
struct InventoryShape {
    #[serde(default)]
    host: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    folders: FolderCounter,
}

#[derive(Default)]
struct FolderCounter {
    apps: usize,
}

impl<'de> Deserialize<'de> for FolderCounter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CounterVisitor;
        impl<'de> Visitor<'de> for CounterVisitor {
            type Value = FolderCounter;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded launcher folder array")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut folders = 0usize;
                let mut apps = 0usize;
                while let Some(folder) = sequence.next_element::<FolderShape>()? {
                    folders += 1;
                    apps = apps.checked_add(folder.apps.0).ok_or_else(|| {
                        de::Error::custom("inventory application count overflowed")
                    })?;
                    bounded_field::<A::Error>(
                        "folder label",
                        &folder.label,
                        MAX_INVENTORY_FOLDER_BYTES,
                    )?;
                    if folders > MAX_INVENTORY_FOLDERS {
                        return Err(de::Error::custom(format_args!(
                            "inventory has more than {MAX_INVENTORY_FOLDERS} folders"
                        )));
                    }
                    if apps > MAX_INVENTORY_APPS {
                        return Err(de::Error::custom(format_args!(
                            "inventory has more than {MAX_INVENTORY_APPS} applications"
                        )));
                    }
                }
                Ok(FolderCounter { apps })
            }
        }
        deserializer.deserialize_seq(CounterVisitor)
    }
}

#[derive(Deserialize)]
struct FolderShape {
    #[serde(default)]
    label: String,
    #[serde(default)]
    apps: AppCounter,
}

#[derive(Default)]
struct AppCounter(usize);

impl<'de> Deserialize<'de> for AppCounter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CounterVisitor;
        impl<'de> Visitor<'de> for CounterVisitor {
            type Value = AppCounter;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded launcher application array")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut apps = 0usize;
                while let Some(app) = sequence.next_element::<InventoryAppShape>()? {
                    apps += 1;
                    app.validate::<A::Error>()?;
                    if apps > MAX_INVENTORY_APPS {
                        return Err(de::Error::custom(format_args!(
                            "inventory has more than {MAX_INVENTORY_APPS} applications"
                        )));
                    }
                }
                Ok(AppCounter(apps))
            }
        }
        deserializer.deserialize_seq(CounterVisitor)
    }
}

#[derive(Deserialize, Default)]
struct InventoryAppShape {
    #[serde(default)]
    name: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    icon: String,
    #[serde(default)]
    exec: String,
    #[serde(default)]
    desktop_file: String,
}

impl InventoryAppShape {
    fn validate<E: de::Error>(&self) -> Result<(), E> {
        for (name, value) in [
            ("application name", self.name.as_str()),
            ("application icon", self.icon.as_str()),
            ("desktop file", self.desktop_file.as_str()),
        ] {
            bounded_field::<E>(name, value, MAX_INVENTORY_APP_TEXT_BYTES)?;
        }
        if let Some(id) = &self.id {
            bounded_field::<E>("application id", id, MAX_INVENTORY_APP_TEXT_BYTES)?;
        }
        bounded_field::<E>("application exec", &self.exec, MAX_INVENTORY_EXEC_BYTES)
    }
}

fn bounded_field<E: de::Error>(name: &str, value: &str, limit: usize) -> Result<(), E> {
    if value.len() > limit {
        Err(E::custom(format_args!(
            "inventory {name} exceeds {limit} bytes"
        )))
    } else {
        Ok(())
    }
}

#[derive(Deserialize)]
struct ConfigShape {
    #[serde(rename = "machines", default)]
    _machines: MachineCounter,
}

#[derive(Default)]
struct MachineCounter;

impl<'de> Deserialize<'de> for MachineCounter {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct CounterVisitor;
        impl<'de> Visitor<'de> for CounterVisitor {
            type Value = MachineCounter;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a bounded launcher machine array")
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut machines = 0usize;
                while sequence.next_element::<IgnoredAny>()?.is_some() {
                    machines += 1;
                    if machines > MAX_MACHINES {
                        return Err(de::Error::custom(format_args!(
                            "at most {MAX_MACHINES} machines may be configured"
                        )));
                    }
                }
                Ok(MachineCounter)
            }
        }
        deserializer.deserialize_seq(CounterVisitor)
    }
}

/// One named row inside a box, and which applications belong in it.
///
/// The names alone were half a feature: declaring "biz", "leis" and "priv" builds three empty
/// shelves and leaves every application in the catch-all, which is more rows to look past rather
/// than fewer. What makes a subcategory worth having is that things are IN it.
///
/// Membership is declared beside the name rather than dragged in one application at a time,
/// because two hundred applications is not a drag-and-drop job -- and because an arrangement that
/// exists only in a state file cannot be reviewed, copied to another machine, or explained.
/// Dragging still works and still wins: it writes to placement, which is applied after this.
#[derive(Deserialize, Debug, Clone, Default, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct SubRow {
    pub name: String,
    /// Matched case-insensitively as a SUBSTRING of the application's id or its display name, so
    /// `signal` catches `signal-desktop.desktop` without anyone needing to know which spelling a
    /// package happens to use. Precision is available by writing more of the name.
    #[serde(default)]
    pub apps: Vec<String>,
}

/// The JSON one inventory provider must produce.
#[derive(Deserialize, Debug, Clone)]
pub struct Inventory {
    #[serde(default)]
    pub host: String,
    /// Carried, not raised. An unreachable machine is a normal state on a roaming laptop, and the
    /// UI wants to draw that column greyed out with a reason on it rather than be handed an empty
    /// list it cannot tell apart from "this machine genuinely has nothing".
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub folders: Vec<InventoryFolder>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct InventoryFolder {
    pub label: String,
    #[serde(default)]
    pub apps: Vec<InventoryApp>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct InventoryApp {
    pub name: String,
    /// The provider's own stable identity for this application. OPTIONAL, defaulting to the name,
    /// so a provider that predates this
    /// field still parses: such an inventory behaves exactly as the program did before ids
    /// existed, which is correct until two of its apps share a display name.
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub exec: String,
    #[serde(default)]
    pub terminal: bool,
    /// Absolute path of the source desktop entry, when the provider has one. This is required for
    /// the freedesktop `%k` Exec field code and remains empty for non-desktop providers.
    #[serde(default)]
    pub desktop_file: String,
}

#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    pub name: String,
    /// Shorter things this machine answers to when typed in the search box.
    ///
    /// A local convention, which is why it is configuration and not a guess: what a setup
    /// shortens its hostnames to is not derivable from the hostnames. Declared aliases beat prefix
    /// matching, so a shortcut somebody relies on cannot be broken later by adding a machine whose
    /// name happens to begin with the same letters.
    #[serde(default)]
    pub aliases: Vec<String>,
    /// The identity colour for this machine's column. Same value the window frames and
    /// forwarded-window badges use, so a column is recognisable before its label is read.
    #[serde(default = "default_accent")]
    pub accent: String,
    /// argv that prints an `Inventory` as JSON on stdout. NOT a shell string: a list cannot be
    /// re-split on spaces by accident, and a machine name with a space in it stays one argument.
    pub inventory: Vec<String>,
    /// Maximum time one inventory command may run. Inventory is arbitrary external code and an
    /// unreachable machine is normal; without a bound, one wedged command can keep a cold start
    /// waiting forever and can leak one refresh worker on every reopen.
    #[serde(default = "default_inventory_timeout_ms")]
    pub inventory_timeout_ms: u64,
    /// argv template for launching one app. `{}` is replaced by the app's own Exec. Absent means
    /// this machine cannot launch anything, which is a legitimate read-only column.
    #[serde(default)]
    pub launch: Vec<String>,
}

fn default_true() -> bool {
    true
}

fn default_surface() -> String {
    "layer".to_string()
}

fn default_keyboard() -> String {
    "exclusive".to_string()
}

fn default_accent() -> String {
    "#22C55E".to_string()
}

fn default_inventory_timeout_ms() -> u64 {
    5_000
}

/// How an ordered row vector is presented inside one machine column.
///
/// This is deliberately count-based. Measuring every label before choosing a shape creates a
/// second layout engine beside GTK and makes the result depend on when fonts finish loading. A
/// bounded label plus an item count gives the stable answer the eye actually needs: short vectors
/// remain compact, medium ones balance over at most three lines, and long ones pan locally.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Layout {
    pub equal_columns: bool,
    pub max_items_per_line: usize,
    pub max_inline_items: usize,
    pub max_label_chars: i32,
    /// Per-row terse overrides: `1x6`, `2x5`, `3x4`, or `rail`.
    pub rows: std::collections::HashMap<String, String>,
}

impl Default for Layout {
    fn default() -> Self {
        Self {
            equal_columns: true,
            max_items_per_line: 5,
            max_inline_items: 12,
            max_label_chars: 16,
            rows: std::collections::HashMap::new(),
        }
    }
}

impl Layout {
    fn override_shape(&self, row: &str) -> Result<Option<(usize, usize)>, String> {
        let Some(value) = self.rows.get(row) else {
            return Ok(None);
        };
        if value == "rail" {
            return Ok(Some((0, 0)));
        }
        let Some((lines, per_line)) = value.split_once('x') else {
            return Err(format!(
                "layout.rows.{row} must be `rail` or a shape such as `1x6`, got {value:?}"
            ));
        };
        let lines = lines.parse::<usize>().ok();
        let per_line = per_line.parse::<usize>().ok();
        match (lines, per_line) {
            (Some(lines @ 1..=3), Some(per_line @ 1..)) => Ok(Some((lines, per_line))),
            _ => Err(format!(
                "layout.rows.{row} must use one to three positive-width lines, got {value:?}"
            )),
        }
    }

    /// Balanced line lengths, or `None` when this vector belongs in its local rail.
    pub fn line_lengths(&self, row: &str, items: usize) -> Option<Vec<usize>> {
        if items == 0 {
            return Some(Vec::new());
        }
        let (max_lines, per_line, capacity) = match self.override_shape(row).ok().flatten() {
            Some((0, 0)) => return None,
            Some((lines, per_line)) => (lines, per_line, lines.saturating_mul(per_line)),
            None => (
                self.max_inline_items.div_ceil(self.max_items_per_line),
                self.max_items_per_line,
                self.max_inline_items,
            ),
        };
        if items > capacity {
            return None;
        }
        let lines = items.div_ceil(per_line).min(max_lines).max(1);
        let short = items / lines;
        let longer = items % lines;
        Some(
            (0..lines)
                .map(|line| short + usize::from(line < longer))
                .collect(),
        )
    }

    pub fn is_rail(&self, row: &str, items: usize) -> bool {
        self.line_lengths(row, items).is_none()
    }
}

/// The palette, as VALUES rather than as code.
///
/// The defaults below are a working dark set so the launcher is usable the moment it is installed
/// -- they are NOT a house palette, and nothing here should be read as one. Override them with
/// whatever the rest of your desktop already uses, so this looks like part of the same product
/// rather than a second one that happens to be running.
///
/// Welding these into the stylesheet was the original mistake: a colour no consumer can reach is
/// this repo carrying one setup's taste as though it were a property of launchers.
#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Theme {
    pub ground: String,
    pub surface: String,
    pub fg: String,
    pub muted: String,
    pub dim: String,
    pub accent: String,
    pub error: String,
    pub border: String,
    pub icon_size: i32,

    /// An image for the corner the label columns leave empty: an absolute path, or an icon name

    /// resolved against the theme. Empty means nothing is drawn -- a launcher that shipped

    /// somebody's mark by default would be wearing it.

    #[serde(default)]
    pub logo: String,

    /// How large that image is drawn. Separate from `icon_size` because the corner has a whole

    /// header row to fill while an application icon has to sit inside a line of text.

    #[serde(default = "default_logo_size")]
    pub logo_size: i32,
    /// How much of the display the grid may occupy before it scrolls. Display-RELATIVE is
    /// mechanism and stays; the fraction itself is a preference about how much of the session
    /// stays visible behind the launcher.
    pub max_height_fraction: f64,
    /// How much of the display's WIDTH the grid may take before it scrolls sideways.
    ///
    /// The companion to `max_height_fraction`, and it exists for the same reason rather than for
    /// symmetry: more machine columns than fit meant a window wider than the screen, with the far
    /// columns unreachable by scrollbar, keyboard or drag. Higher than the height fraction because
    /// a launcher spanning most of the width still leaves the session legible behind it, where one
    /// spanning most of the height does not.
    pub max_width_fraction: f64,
    /// Minimum width of the search bar, and so effectively of the window.
    pub width: i32,
}

fn default_logo_size() -> i32 {
    28
}

impl Default for Theme {
    fn default() -> Self {
        Theme {
            ground: "#0A0A0A".into(),
            surface: "#0E0E0E".into(),
            fg: "#F0F0F0".into(),
            muted: "#999999".into(),
            dim: "#444444".into(),
            accent: "#22C55E".into(),
            error: "#B91322".into(),
            border: "#1C1C1C".into(),
            icon_size: 20,
            logo: String::new(),
            logo_size: default_logo_size(),
            max_height_fraction: 0.9,
            max_width_fraction: 0.9,
            width: 560,
        }
    }
}

#[derive(Deserialize, Debug, Clone, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub machines: Vec<MachineConfig>,
    /// "layer" (default) or "window".
    ///
    /// A layer surface, like every other Wayland launcher. `window` exists for debugging.
    ///
    /// The keyboard question is settled by `exit_on_focus_loss`, not by the surface type. See it.
    #[serde(default = "default_surface")]
    pub surface: String,

    /// Close the moment the keyboard focus goes elsewhere. THIS is what makes an exclusive grab
    /// harmless, and it is exactly what fuzzel does by default.
    ///
    /// Exclusive focus alone is a lock screen: it holds the seat for as long as the surface
    /// exists, so anything you type into another window is swallowed. Exclusive focus PLUS this is
    /// a launcher: it has the keyboard the instant it opens so you can type immediately, and the
    /// moment you click elsewhere it is gone and your keystrokes go where you are looking. The
    /// grab cannot outlive your attention, which is the only property that made it a problem.
    #[serde(default = "default_true")]
    pub exit_on_focus_loss: bool,

    /// Layer-surface keyboard mode, used only when `surface = "layer"`.
    ///
    /// EXCLUSIVE IS THE DEFAULT BECAUSE ON-DEMAND DOES NOT WORK, and that is a compositor bug
    /// rather than a preference. On every released sway (1.10-1.12) and its forks, a mapping layer
    /// surface is granted focus in `handle_map`, and then the `arrange_layers` call at the end of
    /// that same handler revokes it again for anything whose keyboard_interactive is not
    /// EXCLUSIVE. The surface maps and simply never receives a key. Every shipping launcher
    /// defaults to exclusive for this reason -- fuzzel additionally hard-falls-back to it when the
    /// protocol is too old to offer on-demand at all.
    ///
    /// The mode must also be set BEFORE the window is presented: sway reads it at map time from
    /// the surface's initial commit, and a mode applied after that is silently ignored.
    #[serde(default = "default_keyboard")]
    pub keyboard: String,

    /// Which output to open on, in order of preference. EMPTY BY DEFAULT, and that default is the
    /// honest one: outputs are equal, the compositor already knows which screen you are working
    /// on, and a launcher that overrides that without being asked is worse than one that does not
    /// try. This exists for the setup where the compositor's answer is not the wanted one.
    ///
    /// Each entry is matched, case-insensitively, against a monitor's CONNECTOR (`DP-1`,
    /// `HDMI-A-1`), its MODEL (`DELL U4323QE`), and `manufacturer model` joined. The first entry
    /// that matches a connected monitor wins; entries naming nothing currently attached are
    /// skipped, and a list that matches nothing falls back to the compositor's choice. So a laptop
    /// that is sometimes docked names the dock's screen first and needs no second configuration
    /// for the times it is carried away.
    ///
    /// MATCHING THE MODEL IS WHY THIS IS NOT JUST A CONNECTOR LIST. One physical screen plugged
    /// into two machines is `DP-1` on one and `HDMI-A-1` on the other, and the connector also moves
    /// when a cable does -- so a connector list has to be rewritten per machine and re-checked
    /// after every replug, while the model is the same string everywhere and keeps meaning the
    /// same screen.
    #[serde(default)]
    pub outputs: Vec<String>,

    /// argv that wraps a program declaring `Terminal=true` -- e.g. `["foot", "-e"]`.
    ///
    /// Not guessed, and not defaulted to some popular emulator: the right answer is whatever
    /// terminal this desktop already uses, and a launcher that opened a DIFFERENT one than every
    /// other part of the session would be wrong in a way nobody would think to look for. Empty
    /// means such programs are refused with an error rather than launched invisibly and lost.
    #[serde(default)]
    pub terminal: Vec<String>,
    #[serde(default)]
    pub theme: Theme,
    #[serde(default)]
    pub layout: Layout,
    /// Row order. "Other" is appended automatically if absent and forced last if present -- it is
    /// the inbox, not a category, and a config that could bury it in the middle would defeat it.
    #[serde(default)]
    pub folders: Vec<String>,

    /// Named rows INSIDE a box, per folder: `{"Chat": ["business", "leisure", "private"]}`.

    ///

    /// A box with twenty-four things in it is a list wearing a grid's clothes -- the layout stops

    /// paying for itself once a cell is taller than a glance. Sub-rows give the second axis back

    /// inside the cell, and they are declared rather than inferred because a taxonomy is a

    /// judgement: no rule derives "business" from a set of chat clients.

    ///

    /// Declared rows are drawn even when empty, which is what makes them usable -- an invisible

    /// row is one you cannot drag anything into. Apps that were never filed keep appearing in

    /// unnamed lines above them, so declaring a sub-row never hides anything.

    #[serde(default)]
    pub subrows: std::collections::HashMap<String, Vec<SubRow>>,

    /// Key bindings, as chord -> action. Overrides the defaults rather than replacing them, and a

    /// null action unbinds -- see `keymap` for why both matter.

    #[serde(default)]
    pub keys: std::collections::HashMap<String, Option<crate::keymap::Action>>,
}

impl Config {
    /// Refuse values that would otherwise fail later as a blank surface, an index panic, or an
    /// ambiguous machine qualifier. Declarative integrations may catch the same shapes earlier;
    /// the binary must still protect every direct JSON consumer.
    pub fn validate(self) -> Result<Self, String> {
        if self.machines.len() > MAX_MACHINES {
            return Err(format!("at most {MAX_MACHINES} machines may be configured"));
        }
        if self.folder_rows().len() > MAX_ROWS {
            return Err(format!(
                "at most {MAX_ROWS} launcher rows may be configured"
            ));
        }
        if !matches!(self.surface.as_str(), "layer" | "window") {
            return Err(format!(
                "surface must be `layer` or `window`, got {:?}",
                self.surface
            ));
        }
        if !matches!(self.keyboard.as_str(), "exclusive" | "ondemand" | "none") {
            return Err(format!(
                "keyboard must be `exclusive`, `ondemand`, or `none`, got {:?}",
                self.keyboard
            ));
        }
        if self.theme.icon_size <= 0 || self.theme.logo_size <= 0 || self.theme.width <= 0 {
            return Err("theme icon_size, logo_size, and width must be positive".to_string());
        }
        if self.theme.icon_size > MAX_ICON_SIZE
            || self.theme.logo_size > MAX_LOGO_SIZE
            || self.theme.width > MAX_WINDOW_WIDTH
        {
            return Err(format!(
                "theme icon_size, logo_size, and width must be at most {MAX_ICON_SIZE}, {MAX_LOGO_SIZE}, and {MAX_WINDOW_WIDTH}"
            ));
        }
        if self.layout.max_items_per_line == 0
            || self.layout.max_inline_items == 0
            || self.layout.max_label_chars <= 0
        {
            return Err(
                "layout max_items_per_line, max_inline_items, and max_label_chars must be positive"
                    .to_string(),
            );
        }
        if self.layout.max_inline_items > self.layout.max_items_per_line.saturating_mul(3) {
            return Err(
                "layout.max_inline_items must fit within three lines at max_items_per_line"
                    .to_string(),
            );
        }
        for (name, value) in [
            ("max_height_fraction", self.theme.max_height_fraction),
            ("max_width_fraction", self.theme.max_width_fraction),
        ] {
            if !(0.0 < value && value <= 1.0) {
                return Err(format!("theme.{name} must be greater than 0 and at most 1"));
            }
        }
        if self.outputs.iter().any(|o| o.trim().is_empty()) {
            return Err("outputs must not contain empty names".to_string());
        }
        if self.machines.iter().any(|m| m.name.trim().is_empty()) {
            return Err("machine names must not be empty".to_string());
        }
        if self.machines.iter().any(|m| m.inventory.is_empty()) {
            return Err("every machine needs an inventory command".to_string());
        }
        if self.machines.iter().any(|m| m.inventory_timeout_ms == 0) {
            return Err("machine inventory_timeout_ms must be positive".to_string());
        }

        let mut tokens = std::collections::HashMap::<String, String>::new();
        for machine in &self.machines {
            for token in std::iter::once(&machine.name).chain(machine.aliases.iter()) {
                let normal = token.to_lowercase();
                if let Some(first) = tokens.insert(normal, machine.name.clone()) {
                    return Err(format!(
                        "machine names and aliases must be unique case-insensitively; {:?} collides between {first:?} and {:?}",
                        token, machine.name
                    ));
                }
            }
        }

        for folder in self.subrows.keys() {
            if folder == "Other" || !self.folders.iter().any(|f| f == folder) {
                return Err(format!(
                    "subrows.{folder} has no declared folder; subrows must belong to a configured folder other than Other"
                ));
            }
        }
        let rows = self.folder_rows();
        for row in self.layout.rows.keys() {
            if !rows.iter().any(|known| known == row) {
                return Err(format!(
                    "layout.rows.{row} has no configured row; use Folder/subrow for named rungs"
                ));
            }
            self.layout.override_shape(row)?;
        }
        Ok(self)
    }

    /// Row labels, with the inbox guaranteed present and last.
    pub fn folder_rows(&self) -> Vec<String> {
        // DEDUPED, first occurrence wins. A repeated label is not merely untidy: rows are matched
        // by label, so a duplicate makes every app in that folder appear in two rows at once, and
        // a drag onto either writes to whichever the code happens to reach first.
        // A SUBCATEGORY IS A ROW, spelled `folder/sub`.
        //
        // Making it part of the row label rather than a structure inside the cell is what lets the
        // label sit OUTSIDE the machine columns: the grid already draws one label per row, so a
        // subcategory drawn as a row gets that for free and lines up across every machine. Nested
        // inside a cell it could only ever line up with itself.
        //
        // The bare folder name is emitted LAST, after its subcategories, and is the catch-all --
        // which is also what makes this readable by an arrangement written before subcategories
        // existed: those entries are keyed on the bare folder name and still land in it.
        let mut seen = std::collections::HashSet::new();
        let mut rows: Vec<String> = Vec::new();
        let push =
            |rows: &mut Vec<String>, r: String, seen: &mut std::collections::HashSet<String>| {
                if seen.insert(r.clone()) {
                    rows.push(r);
                }
            };
        for f in self.folders.iter().filter(|f| f.as_str() != "Other") {
            for sub in self.subrows.get(f).into_iter().flatten() {
                push(&mut rows, format!("{f}/{}", sub.name), &mut seen);
            }
            push(&mut rows, f.clone(), &mut seen);
        }
        rows.push("Other".to_string());
        rows
    }
}

/// `$CBAR_LAUNCHER_CONFIG` first so a test or a bisect can point at one explicitly without
/// touching the user's real setup, then the cbar-owned XDG location.
pub fn config_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("CBAR_LAUNCHER_CONFIG").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(p));
    }
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".config"))
        })?;
    Some(base.join("cbar").join("launcher.json"))
}

/// None when there is no config at all. A config that EXISTS but does not parse is a different
/// situation entirely and must not be silently ignored: returning the error lets the caller say so
/// rather than pretending the machine list is empty.
pub fn load() -> Result<Option<Config>, String> {
    let Some(path) = config_path() else {
        return Ok(None);
    };
    load_path(&path)
}

fn load_path(path: &std::path::Path) -> Result<Option<Config>, String> {
    // Config symlinks are intentional (Home Manager/dotfile managers), but a path replaced by a
    // FIFO/device must never block the shared cbar process before metadata can reject it.
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.is_file() || metadata.len() > MAX_CONFIG_BYTES {
        return Err(format!(
            "{} is not a regular launcher config at most {MAX_CONFIG_BYTES} bytes",
            path.display()
        ));
    }
    let mut text = String::with_capacity(metadata.len() as usize);
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if text.len() as u64 > MAX_CONFIG_BYTES {
        return Err(format!(
            "{} exceeds the {MAX_CONFIG_BYTES}-byte launcher config limit",
            path.display()
        ));
    }
    serde_json::from_str::<ConfigShape>(&text)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    serde_json::from_str::<Config>(&text)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .validate()
        .map(Some)
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn other_is_appended_when_absent() {
        let c: Config =
            serde_json::from_str(r#"{"machines":[],"folders":["Terminals","Editors"]}"#).unwrap();
        assert_eq!(c.folder_rows(), vec!["Terminals", "Editors", "Other"]);
    }

    /// A repeated label would make every app in that folder appear in two rows at once.
    #[test]
    fn duplicate_folders_are_deduped() {
        let c: Config =
            serde_json::from_str(r#"{"machines":[],"folders":["Chat","Editors","Chat"]}"#).unwrap();
        assert_eq!(c.folder_rows(), vec!["Chat", "Editors", "Other"]);
    }

    /// Every category expands once, in place. Its subrows and catch-all are one contiguous block;
    /// later categories can never be inserted between them.
    #[test]
    fn each_category_expands_to_one_contiguous_block() {
        let c: Config = serde_json::from_str(
            r#"{
                "machines": [],
                "folders": ["AI", "Code"],
                "subrows": {
                    "AI": [{"name":"us"},{"name":"alt"}],
                    "Code": [{"name":"term"},{"name":"graph"},{"name":"build"},{"name":"insp"}]
                }
            }"#,
        )
        .unwrap();
        assert_eq!(
            c.folder_rows(),
            [
                "AI/us",
                "AI/alt",
                "AI",
                "Code/term",
                "Code/graph",
                "Code/build",
                "Code/insp",
                "Code",
                "Other",
            ]
        );
    }

    /// A config that names "Other" in the middle must not be able to bury the inbox.
    #[test]
    fn other_is_forced_last_when_present() {
        let c: Config =
            serde_json::from_str(r#"{"machines":[],"folders":["Other","Terminals"]}"#).unwrap();
        assert_eq!(c.folder_rows(), vec!["Terminals", "Other"]);
    }

    #[test]
    fn a_machine_needs_only_a_name_and_an_inventory_command() {
        let c: Config =
            serde_json::from_str(r#"{"machines":[{"name":"box","inventory":["echo","{}"]}]}"#)
                .unwrap();
        assert_eq!(c.machines[0].name, "box");
        assert_eq!(c.machines[0].accent, "#22C55E", "accent defaulted");
        assert_eq!(
            c.machines[0].inventory_timeout_ms, 5_000,
            "timeout defaulted"
        );
        assert!(
            c.machines[0].launch.is_empty(),
            "a read-only column is legitimate"
        );
    }

    /// The inventory contract accepts a generic provider's output.
    #[test]
    fn inventory_parses_the_provider_shape() {
        let inv: Inventory = serde_json::from_str(
            r#"{"host":"console","error":null,"folders":[
                 {"label":"Terminals","apps":[
                   {"name":"Foot","icon":"foot","exec":"foot","terminal":false}]}]}"#,
        )
        .unwrap();
        assert_eq!(inv.host, "console");
        assert!(inv.error.is_none());
        assert_eq!(inv.folders[0].apps[0].name, "Foot");
    }

    /// An unreachable machine reports a reason and no folders, and that must parse cleanly --
    /// it is the state the UI most needs to draw differently.
    #[test]
    fn an_unreachable_machine_carries_its_reason() {
        let inv: Inventory =
            serde_json::from_str(r#"{"host":"faraway","error":"ssh: timed out","folders":[]}"#)
                .unwrap();
        assert_eq!(inv.error.as_deref(), Some("ssh: timed out"));
        assert!(inv.folders.is_empty());
    }

    #[test]
    fn unknown_config_fields_are_errors_not_silent_typos() {
        let error = serde_json::from_str::<Config>(r#"{"machines":[],"widht":500}"#).unwrap_err();
        assert!(error.to_string().contains("unknown field `widht`"));
    }

    #[test]
    fn read_only_declarative_config_symlinks_are_supported() {
        use std::os::unix::fs::symlink;

        let root =
            std::env::temp_dir().join(format!("cbar-launcher-config-link-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        let target = root.join("declarative.json");
        let link = root.join("launcher.json");
        std::fs::write(&target, br#"{"machines":[]}"#).unwrap();
        symlink(&target, &link).unwrap();

        assert!(load_path(&link).unwrap().is_some());

        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn config_fifo_is_rejected_without_blocking() {
        use std::os::unix::ffi::OsStrExt;

        let path =
            std::env::temp_dir().join(format!("cbar-launcher-config-fifo-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let path_bytes = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(path_bytes.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        let error = load_path(&path).unwrap_err();
        assert!(error.contains("not a regular launcher config"), "{error}");
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn machine_names_and_aliases_cannot_collide_by_case() {
        let config: Config = serde_json::from_str(
            r#"{"machines":[
                {"name":"Server","inventory":["true"]},
                {"name":"laptop","aliases":["server"],"inventory":["true"]}
            ]}"#,
        )
        .unwrap();
        let error = config.validate().unwrap_err();
        assert!(error.contains("unique case-insensitively"), "{error}");
    }

    #[test]
    fn inventory_structure_is_bounded_before_it_reaches_gtk() {
        let apps = (0..=MAX_INVENTORY_APPS)
            .map(|index| format!(r#"{{"name":"{index}"}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let bytes =
            format!(r#"{{"host":"large","folders":[{{"label":"Other","apps":[{apps}]}}]}}"#);
        assert!(
            parse_inventory(bytes.as_bytes())
                .unwrap_err()
                .contains("applications")
        );
    }

    #[test]
    fn inventory_fields_and_stable_ids_are_bounded_before_gtk() {
        let huge_name = "x".repeat(MAX_INVENTORY_APP_TEXT_BYTES + 1);
        let bytes = format!(
            r#"{{"host":"large","folders":[{{"label":"Other","apps":[{{"id":"large","name":"{huge_name}"}}]}}]}}"#
        );
        let error = parse_inventory(bytes.as_bytes()).unwrap_err();
        assert!(error.contains("application name"), "{error}");

        let duplicate = br#"{"host":"duplicate","folders":[{"label":"Other","apps":[{"id":"same","name":"First"},{"id":"same","name":"Second"}]}]}"#;
        let error = parse_inventory(duplicate).unwrap_err();
        assert!(error.contains("duplicate application id"), "{error}");
    }

    #[test]
    fn caller_fair_share_is_checked_before_typed_inventory_build() {
        let bytes = br#"{"host":"fair","folders":[{"label":"Other","apps":[{"name":"one"},{"name":"two"}]}]}"#;
        let error = parse_inventory_bounded(bytes, 1).unwrap_err();
        assert!(error.contains("fair-share limit of 1"), "{error}");
    }

    #[test]
    fn no_configured_outputs_means_the_compositor_decides() {
        let config: Config = serde_json::from_str(r#"{"machines":[]}"#).unwrap();
        assert!(config.outputs.is_empty());
        assert!(config.validate().is_ok());
    }

    #[test]
    fn an_output_preference_keeps_the_order_it_was_written_in() {
        let config: Config =
            serde_json::from_str(r#"{"machines":[],"outputs":["DELL U4323QE","eDP-1"]}"#).unwrap();
        // The order IS the preference, so it must survive parsing exactly -- a set or a map here
        // would silently make the second screen as good as the first.
        assert_eq!(config.outputs, vec!["DELL U4323QE", "eDP-1"]);
    }

    #[test]
    fn a_blank_output_name_is_rejected_rather_than_matching_everything() {
        let config: Config =
            serde_json::from_str(r#"{"machines":[],"outputs":["DP-1","  "]}"#).unwrap();
        let error = config.validate().unwrap_err();
        assert!(error.contains("outputs"), "{error}");
    }

    #[test]
    fn invalid_dimensions_are_rejected_before_the_ui_uses_them() {
        let config: Config = serde_json::from_str(
            r#"{"machines":[],"layout":{"max_items_per_line":0},"theme":{"max_height_fraction":1.5}}"#,
        )
        .unwrap();
        let error = config.validate().unwrap_err();
        assert!(error.contains("max_items_per_line"), "{error}");
    }

    #[test]
    fn adaptive_vectors_balance_then_become_a_rail() {
        let layout = Layout::default();
        assert_eq!(layout.line_lengths("Games/rpg", 5), Some(vec![5]));
        assert_eq!(layout.line_lengths("Games/rpg", 6), Some(vec![3, 3]));
        assert_eq!(layout.line_lengths("Games/rpg", 9), Some(vec![5, 4]));
        assert_eq!(layout.line_lengths("Games/rpg", 11), Some(vec![4, 4, 3]));
        assert_eq!(layout.line_lengths("Games/rpg", 12), Some(vec![4, 4, 4]));
        assert_eq!(layout.line_lengths("Games/rpg", 13), None);
    }

    #[test]
    fn a_row_shape_can_keep_six_short_items_on_one_line() {
        let mut layout = Layout::default();
        layout.rows.insert("Code/term".into(), "1x6".into());
        assert_eq!(layout.line_lengths("Code/term", 6), Some(vec![6]));
        assert_eq!(layout.line_lengths("Code/term", 7), None);
        layout.rows.insert("Games/rpg".into(), "rail".into());
        assert_eq!(layout.line_lengths("Games/rpg", 1), None);
    }

    #[test]
    fn layout_overrides_name_real_rows_and_valid_shapes() {
        let valid: Config = serde_json::from_str(
            r#"{"machines":[],"folders":["Code"],"subrows":{"Code":[{"name":"term"}]},"layout":{"rows":{"Code/term":"1x6"}}}"#,
        )
        .unwrap();
        assert!(valid.validate().is_ok());

        let invalid: Config = serde_json::from_str(
            r#"{"machines":[],"folders":["Code"],"layout":{"rows":{"Code/term":"4x0"}}}"#,
        )
        .unwrap();
        assert!(invalid.validate().is_err());
    }
}
