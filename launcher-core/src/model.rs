// model.rs — everything cbar's launcher knows, with no pixels in it.
//
// SPLIT ON PURPOSE. Every bug found while building this was in here and none of them were in the
// drawing: the box that was not really 2D, the goal column that drifted left, the search that
// filtered nothing, the cursor stranded on an emptied cell. All of it is ordinary data movement,
// and none of it needs a display to exercise -- so it lives apart from GTK and is covered by real
// tests at the bottom of this file rather than by opening the launcher and squinting.
//
// The rule that keeps it that way: nothing in this file may import gtk.
use crate::config::Layout;
use crate::usage::{self, Usage};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use serde::{Deserialize, Serialize};
use serde_json;
use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

// ── the model ───────────────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct App {
    /// WHAT THIS APP IS, as opposed to what it calls itself. Placement and usage are both keyed on
    /// this and never on `name`, because a display name is not an identity: it is translated, it is
    /// upstream's choice rather than the packager's, and two entries sharing one is ordinary.
    /// Keyed on the name, two such apps collapsed into a single row and whichever lost became
    /// unlaunchable -- and the arrangement of one silently drove the other.
    pub id: String,
    pub name: String,
    pub icon: String,
    /// The `Exec` line as the inventory reported it, field codes and all. Kept verbatim rather
    /// than pre-split: splitting is the launcher's job and the raw string is what a future
    /// provider will keep giving us.
    pub exec: String,
    /// `Terminal=true` in the desktop entry: this program draws no window of its own and MUST be
    /// given one. Ignoring it is not a degraded launch, it is no launch at all -- the process
    /// starts with no controlling terminal and dies immediately, silently.
    pub terminal: bool,
    /// Source desktop-entry path for `%k`, empty for inventories without that concept.
    pub desktop_file: String,
}

/// A line IS an appset: the apps on it are meant to be started together, and the fact that they
/// sit on one line is the whole declaration. No separate "group" concept, no naming ceremony.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    /// What this row of the box is FOR, when it is for something in particular.
    ///
    /// A line was always an appset -- a group you start in one keystroke -- but an anonymous one,
    /// so the only thing it could say was "these belong together" and never why. Naming it turns a
    /// box into a small table: Chat becomes business, leisure, private, and a box that had grown
    /// to twenty-four entries becomes four rows you can read.
    ///
    /// `None` is not a lesser case. Most lines are groups nobody needed to explain, and forcing a
    /// name on them would be ceremony -- an unnamed line renders exactly as it always did.
    pub name: Option<String>,
    pub apps: Vec<Arc<App>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Machine {
    pub name: String,
    /// Other things this machine may be called when typed at. A machine's NAME is what it is; an
    /// alias is what a person in a hurry types, and the two are different needs -- the column
    /// heading wants to be unambiguous, the search box wants to be short.
    ///
    /// Empty by default and supplied by configuration, because a nickname is a local convention:
    /// what one setup shortens a hostname to is not a fact this program could ever guess.
    pub aliases: Vec<String>,
    pub accent: String,
    /// argv prefix that turns "run this" into "run this THERE". Empty is a read-only column.
    pub launch: Vec<String>,
    /// Why this machine could not be asked, if it could not. Carried rather than raised: an
    /// unreachable peer is a normal state on a roaming laptop, and a column drawn with a reason on
    /// it is honest, where an empty column is indistinguishable from a machine that has nothing.
    pub error: Option<String>,
    /// Parallel to the row set: cells[r] is this machine's lines for folder r. An empty cell is a
    /// real and common state worth DRAWING rather than collapsing -- it tells you "that machine has no
    /// chat client" at a glance, which a list-based launcher can only tell you by absence.
    pub cells: Vec<Vec<Line>>,
}

/// Rich test-only row set. Production rows come exclusively from configuration.
#[cfg(test)]
pub const DEFAULT_FOLDERS: &[&str] = &[
    "Terminals",
    "Editors",
    "Browsers",
    "Chat",
    "Files",
    "Media",
    "Other",
];

/// The user's own ARRANGEMENT of a machine's apps: machine -> folder label -> lines of app names.
///
/// A folder-per-app map (what this was first) can say "Bottles belongs in Files" and nothing more.
/// It cannot express WHERE on a line, or which line, or the order within one -- and those are
/// exactly the decisions dragging is for. Recording the arrangement itself makes all of them one
/// mechanism: a drop rewrites a position in this structure, and the grid is re-derived from it.
///
/// Apps NOT named here keep whatever folder the category table computed and appear as their own
/// line, so a newly-installed app still lands in "Other" without any entry existing for it. An app
/// named here that no longer exists is skipped silently -- uninstalling something must not corrupt
/// the arrangement of everything around it.
pub type Placement = HashMap<String, HashMap<String, Vec<StoredLine>>>;

/// Applications the user chose not to see: machine -> stable application ids.
///
/// Separate from placement because the decisions are independent. Hiding an application must not
/// forget which shelf or position it had; resetting visibility should reveal the exact arrangement
/// that was there before, not ask the inventory grouping to place it again.
pub type Visibility = HashMap<String, HashSet<String>>;

/// One arranged line as it is written down: the apps on it, and its name if it has one.
///
/// ── READS THE OLD SHAPE TOO, AND THAT IS THE WHOLE REASON FOR THE UNTAGGED ENUM ─────────────
///
/// This used to be a bare `Vec<String>`. Every arrangement anyone has made is stored in that
/// shape, and a schema change that cannot read what is already on disk is a schema change that
/// deletes the user's work -- the file stops parsing, the grid falls back to the computed
/// grouping, and there is no error because from the program's point of view nothing went wrong.
///
/// `#[serde(untagged)]` makes the old form a valid value of the new type, so an existing file
/// loads as unnamed lines and is rewritten in the new shape the next time anything is dragged.
/// The migration is the ordinary code path rather than a separate step that could be skipped.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(untagged)]
pub enum StoredLine {
    /// What every placement file written before named rows existed looks like.
    Bare(Vec<String>),
    Named {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        name: Option<String>,
        apps: Vec<String>,
    },
}

impl StoredLine {
    pub fn apps(&self) -> &Vec<String> {
        match self {
            StoredLine::Bare(a) => a,
            StoredLine::Named { apps, .. } => apps,
        }
    }
    pub fn apps_mut(&mut self) -> &mut Vec<String> {
        match self {
            StoredLine::Bare(a) => a,
            StoredLine::Named { apps, .. } => apps,
        }
    }
    pub fn name(&self) -> Option<&str> {
        match self {
            StoredLine::Bare(_) => None,
            StoredLine::Named { name, .. } => name.as_deref(),
        }
    }
    pub fn new(name: Option<String>, apps: Vec<String>) -> Self {
        match name {
            None => StoredLine::Bare(apps),
            some => StoredLine::Named { name: some, apps },
        }
    }
}

/// STATE, not config: a record of what the user did, written by the program itself, so it belongs
/// under XDG_STATE_HOME and emphatically not in a managed config tree a rebuild would
/// overwrite. "This should be permanent" is the whole requirement.
const MAX_STATE_BYTES: u64 = 16 * 1024 * 1024;

fn nonempty_path(value: std::ffi::OsString) -> Option<PathBuf> {
    (!value.is_empty()).then(|| PathBuf::from(value))
}

fn state_base() -> Option<PathBuf> {
    std::env::var_os("XDG_STATE_HOME")
        .and_then(nonempty_path)
        .or_else(|| {
            std::env::var_os("HOME")
                .and_then(nonempty_path)
                .map(|home| home.join(".local/state"))
        })
}

pub(crate) fn state_path(name: &str) -> Option<PathBuf> {
    state_base().map(|base| base.join("cbar").join("launcher").join(name))
}

pub fn placement_path() -> Option<PathBuf> {
    state_path("placement.json")
}

pub fn visibility_path() -> Option<PathBuf> {
    state_path("visibility.json")
}

pub(crate) fn read_state(path: &Path) -> Result<Option<String>, String> {
    if let Some(error) = pending_write_error(path) {
        return Err(error);
    }
    let root = path
        .parent()
        .ok_or_else(|| format!("launcher state path {} has no parent", path.display()))?;
    let root_metadata = match std::fs::symlink_metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", root.display())),
    };
    if root_metadata.file_type().is_symlink()
        || !root_metadata.is_dir()
        || root_metadata.uid() != unsafe { libc::geteuid() }
        || root_metadata.mode() & 0o077 != 0
    {
        return Err(format!(
            "refusing unsafe launcher state root {}",
            root.display()
        ));
    }
    let file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    let metadata = file
        .metadata()
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(format!(
            "refusing unsafe launcher state file {}",
            path.display()
        ));
    }
    if metadata.len() > MAX_STATE_BYTES {
        return Err(format!(
            "launcher state file {} exceeds {MAX_STATE_BYTES} bytes",
            path.display()
        ));
    }
    let mut text = String::with_capacity(metadata.len() as usize);
    file.take(MAX_STATE_BYTES + 1)
        .read_to_string(&mut text)
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if text.len() as u64 > MAX_STATE_BYTES {
        return Err(format!(
            "launcher state file {} exceeds {MAX_STATE_BYTES} bytes",
            path.display()
        ));
    }
    Ok(Some(text))
}

/// A MISSING file is an empty arrangement; a file that exists and does not parse is not, and the
/// difference matters because the next drag rewrites whatever we decide it was. Say so rather than
/// silently starting from nothing and overwriting the user's real arrangement with the assumption.
pub fn load_placement() -> (Placement, Option<String>) {
    let Some(path) = placement_path() else {
        return (Placement::new(), None);
    };
    match read_state(&path) {
        Ok(None) => (Placement::new(), None),
        Err(error) => (Placement::new(), Some(error)),
        Ok(Some(text)) => match serde_json::from_str(&text) {
            Ok(p) => (p, None),
            Err(e) => (Placement::new(), Some(format!("{}: {e}", path.display()))),
        },
    }
}

/// Best-effort about FAILING, never about corrupting. A launcher that refused to open because it
/// could not write its placement file would trade the whole feature for one of its conveniences --
/// but a half-written file is worse than no file, because it parses as an empty arrangement and
/// the next drag makes that permanent. Write beside it and rename, which is atomic on any POSIX
/// filesystem, so a reader sees the old file or the new one and never a truncated one.
pub fn save_placement(p: &Placement) {
    // NEVER FROM A TEST. `place_app` saves as part of doing its job, so every placement test was
    // writing the fixture arrangement -- machine "m", apps "Foot" and "Brand New" -- straight over
    // the real file in the developer's own state directory. Inside isolated build environments HOME is a
    // throwaway and it went unnoticed; anyone running `cargo test` on their own machine lost their
    // arrangement to it, silently, and this is how one such file was found.
    //
    // A cfg guard rather than a redirected path: tests share one process, so pointing
    // XDG_STATE_HOME somewhere else would race between parallel test threads and leave the
    // protection depending on which test ran first.
    if cfg!(test) {
        return;
    }
    if let Some(path) = placement_path() {
        write_atomic(&path, p);
    }
}

/// Visibility follows the same fail-closed persistence rule as placement: a malformed existing
/// file is an error, not permission to replace the user's hidden set with an empty one.
pub fn load_visibility() -> (Visibility, Option<String>) {
    let Some(path) = visibility_path() else {
        return (Visibility::new(), None);
    };
    match read_state(&path) {
        Ok(None) => (Visibility::new(), None),
        Err(error) => (Visibility::new(), Some(error)),
        Ok(Some(text)) => match serde_json::from_str(&text) {
            Ok(v) => (v, None),
            Err(e) => (Visibility::new(), Some(format!("{}: {e}", path.display()))),
        },
    }
}

pub fn save_visibility(v: &Visibility) {
    if cfg!(test) {
        return;
    }
    if let Some(path) = visibility_path() {
        write_atomic(&path, v);
    }
}

pub fn write_atomic<T>(path: &std::path::Path, value: &T)
where
    T: serde::Serialize + Clone + Send + 'static,
{
    let value = value.clone();
    let job: WriteJob = Box::new(move |path| {
        let bytes = serde_json::to_vec_pretty(&value)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if bytes.len() > MAX_STATE_BYTES as usize {
            return Err(format!(
                "launcher state for {} exceeds {MAX_STATE_BYTES} bytes",
                path.display()
            ));
        }
        write_atomic_bytes(path, &bytes)
    });
    if cfg!(test) {
        let _ = job(path);
        return;
    }
    enqueue_write(path.to_path_buf(), job);
}

type WriteJob = Box<dyn FnOnce(&Path) -> Result<(), String> + Send>;

#[derive(Default)]
struct PendingWrite {
    latest: Option<(u64, WriteJob)>,
    active: bool,
    submitted: u64,
    completed: u64,
    last_error: Option<String>,
}

struct WriteRegistry {
    pending: Mutex<HashMap<PathBuf, PendingWrite>>,
    changed: Condvar,
}

fn write_registry() -> &'static WriteRegistry {
    static WRITES: OnceLock<WriteRegistry> = OnceLock::new();
    WRITES.get_or_init(|| WriteRegistry {
        pending: Mutex::new(HashMap::new()),
        changed: Condvar::new(),
    })
}

fn enqueue_write(path: PathBuf, job: WriteJob) {
    static NEXT_WRITE: AtomicU64 = AtomicU64::new(1);
    let generation = NEXT_WRITE.fetch_add(1, Ordering::Relaxed);
    let writes = write_registry();
    let Ok(mut pending) = writes.pending.lock() else {
        return;
    };
    let entry = pending.entry(path.clone()).or_default();
    entry.latest = Some((generation, job));
    entry.submitted = generation;
    if entry.active {
        writes.changed.notify_all();
        return;
    }
    entry.active = true;
    drop(pending);

    let worker_path = path.clone();
    if std::thread::Builder::new()
        .name("cbar-launcher-state".into())
        .spawn(move || {
            loop {
                let next = {
                    let Ok(mut pending) = writes.pending.lock() else {
                        return;
                    };
                    let Some(active) = pending.get_mut(&worker_path) else {
                        return;
                    };
                    match active.latest.take() {
                        Some(job) => job,
                        None => {
                            active.active = false;
                            writes.changed.notify_all();
                            return;
                        }
                    }
                };
                let (generation, job) = next;
                let result = job(&worker_path);
                let Ok(mut pending) = writes.pending.lock() else {
                    return;
                };
                let Some(active) = pending.get_mut(&worker_path) else {
                    return;
                };
                active.completed = active.completed.max(generation);
                if generation == active.submitted {
                    active.last_error = result.err();
                }
                writes.changed.notify_all();
            }
        })
        .is_err()
    {
        mark_writer_spawn_failed(writes, &path);
    }
}

fn mark_writer_spawn_failed(writes: &WriteRegistry, path: &Path) {
    if let Ok(mut pending) = writes.pending.lock()
        && let Some(active) = pending.get_mut(path)
    {
        active.active = false;
        active.latest.take();
        active.completed = active.submitted;
        active.last_error = Some("unable to start launcher state writer".to_string());
        writes.changed.notify_all();
    }
}

/// Wait only on a blocking worker, and only for a bounded interval, before re-reading state for a
/// replacement UI. The common case observes the just-saved snapshot; a stuck filesystem merely
/// times out and the GTK shell transfers its live snapshot instead of waiting or rolling back.
pub fn wait_for_state_writes(timeout: std::time::Duration) -> bool {
    let writes = write_registry();
    let Ok(mut pending) = writes.pending.lock() else {
        return false;
    };
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if pending
            .values()
            .all(|state| state.latest.is_none() && state.completed >= state.submitted)
        {
            return true;
        }
        let Some(remaining) = deadline.checked_duration_since(std::time::Instant::now()) else {
            return false;
        };
        let Ok((next, result)) = writes.changed.wait_timeout(pending, remaining) else {
            return false;
        };
        pending = next;
        if result.timed_out() {
            return pending
                .values()
                .all(|state| state.latest.is_none() && state.completed >= state.submitted);
        }
    }
}

fn pending_write_error(path: &Path) -> Option<String> {
    let state = write_registry().pending.lock().ok()?;
    let pending = state.get(path)?;
    (pending.latest.is_none() && pending.completed >= pending.submitted)
        .then(|| pending.last_error.clone())
        .flatten()
}

fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(dir)
            .map_err(|error| format!("{}: {error}", dir.display()))?;
        let metadata = std::fs::symlink_metadata(dir)
            .map_err(|error| format!("{}: {error}", dir.display()))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return Err(format!(
                "refusing unsafe launcher state root {}",
                dir.display()
            ));
        }
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("{}: {error}", dir.display()))?;
    }
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "json.{}.{}.tmp",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed)
    ));
    if let Ok(existing) = std::fs::symlink_metadata(path)
        && (existing.file_type().is_symlink()
            || !existing.is_file()
            || existing.uid() != unsafe { libc::geteuid() })
    {
        return Err(format!(
            "refusing unsafe launcher state file {}",
            path.display()
        ));
    }
    let result = (|| -> Result<(), String> {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)
            .map_err(|error| format!("{}: {error}", tmp.display()))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("{}: {error}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|error| format!("{}: {error}", path.display()))?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|error| format!("{}: {error}", path.display()))
    })();
    let _ = std::fs::remove_file(tmp);
    result
}

#[derive(PartialEq, Clone, Copy, Debug)]
pub enum Focus {
    Outside,
    Inside,
}

pub struct State {
    /// The row set as CONFIG declared it, in the same order as every machine's `cells`.
    /// Frecency never permutes rows: configured order is the launcher's spatial contract.
    pub folders: Vec<String>,
    /// Presentation policy for every ordered row vector. It lives in the model because search,
    /// visibility and drag/drop all change vectors; reflowing only during inventory would leave
    /// stale gaps until the next refresh.
    pub layout: Layout,
    /// How often each thing is reached for. See usage.rs.
    pub usage: Usage,
    /// False when the on-disk usage file existed but could not be read or parsed. In-memory use may
    /// continue, but writing an empty replacement would turn a transient read failure into data loss.
    pub usage_writable: bool,
    /// Standard errors an item must lead by before it may move. See `significantly_greater`.
    pub z: f64,
    pub half_life_days: f64,
    /// The inventory EXACTLY as the category table produced it. Never mutated: filing an app is
    /// recorded in `placement` and re-applied, so a re-inventory can replace this wholesale and
    /// every filing survives. Mutating the grid in place instead would make the user's decisions
    /// indistinguishable from the guesses they were correcting.
    pub base: Vec<Machine>,
    /// The user's own arrangement. Persisted; see `placement_path`.
    pub placement: Placement,
    /// The placement analogue of `usage_writable`; see `load_placement`.
    pub placement_writable: bool,
    /// Per-machine application ids the user explicitly hid. This filters the derived grid only;
    /// `base` and `placement` stay pristine so reset can restore both membership and position.
    pub visibility: Visibility,
    /// False when an existing visibility file could not be read or parsed. In-memory hiding may
    /// continue for the session, but must not overwrite state whose contents we could not recover.
    pub visibility_writable: bool,
    /// `base` with `placement` applied. Filtered into `view`.
    pub machines: Vec<Machine>,
    /// `machines` with the query applied. Navigation and rendering BOTH read this, so the cursor
    /// can never point at something the user cannot see.
    pub view: Vec<Machine>,
    pub col: usize,
    pub row: usize,
    pub line: usize,
    pub item: usize,
    /// The column the user last chose DELIBERATELY, with left/right. Moving up or down aims for
    /// it rather than for wherever the previous line happened to end -- so a cell is a real 2D
    /// grid: at line 1 item 2, Down lands on line 2 item 2. Crossing a SHORT line clamps for that
    /// line only and does not forget the goal, which is what makes down-then-up return you to
    /// where you started instead of drifting left. Same model a text editor uses for a column.
    pub item_goal: usize,
    pub focus: Focus,
    pub query: String,
}

/// Used when there are no machines at all, so `cell()` can answer without indexing into nothing.
/// A config with an empty machine list is a shape the module can legitimately render (every
/// machine toggled off), and it must open empty rather than panic on startup.
static NO_LINES: Vec<Line> = Vec::new();

impl State {
    pub fn cell(&self) -> &Vec<Line> {
        match self.view.get(self.col).and_then(|m| m.cells.get(self.row)) {
            Some(c) => c,
            None => &NO_LINES,
        }
    }

    pub fn current_line(&self) -> Option<&Line> {
        self.cell().get(self.line)
    }

    /// Test-only: bump a score without writing to disk.
    #[cfg(test)]
    pub fn record_launch_for_test(&mut self, machine: &str, app: &str) {
        usage::record(
            &mut self.usage,
            machine,
            app,
            usage::now_secs(),
            self.half_life_days,
        );
    }

    /// Record a launch in memory. Batch callers persist once after recording every successful app.
    pub fn record_launch(&mut self, machine: &str, app: &str) {
        usage::record(
            &mut self.usage,
            machine,
            app,
            usage::now_secs(),
            self.half_life_days,
        );
    }

    pub fn save_usage(&self) {
        if self.usage_writable {
            usage::save(&self.usage);
        }
    }

    /// Hide one real application on one machine. Identity is machine + provider id, the same pair
    /// placement and usage use; display names are neither stable nor necessarily unique.
    pub fn hide_app(&mut self, machine: &str, app: &str) -> bool {
        let known = self.base.iter().any(|m| {
            m.name == machine
                && m.cells
                    .iter()
                    .flatten()
                    .flat_map(|line| line.apps.iter())
                    .any(|candidate| candidate.id == app)
        });
        if !known {
            return false;
        }
        let changed = self
            .visibility
            .entry(machine.to_string())
            .or_default()
            .insert(app.to_string());
        if !changed {
            return false;
        }
        if self.visibility_writable {
            save_visibility(&self.visibility);
        }
        self.rebuild();
        true
    }

    /// Reveal every hidden application. One reset rather than machine/folder variants keeps the
    /// recovery gesture absolute: it is impossible to forget which scope still hides something.
    pub fn reset_visibility(&mut self) -> bool {
        if self.visibility.is_empty() {
            return false;
        }
        self.visibility.clear();
        if self.visibility_writable {
            save_visibility(&self.visibility);
        }
        self.rebuild();
        true
    }

    pub fn hidden_count(&self) -> usize {
        self.visibility.values().map(HashSet::len).sum()
    }

    /// Rewrite state written when placement and usage were keyed on the DISPLAY NAME.
    ///
    /// Changing a key without migrating is the same thing as deleting the file: every entry stops
    /// matching, the grid falls back to the computed grouping, and the user's arrangement is gone
    /// with no error and nothing to restore it from. So the change and the migration ship together.
    ///
    /// The test for "is this token an old one" is deliberately not a guess about its shape -- an id
    /// is opaque by contract, and "it ends in .desktop" is one provider's habit rather than a rule.
    /// A token is old when it is NOT a known id AND IS a known name. That is self-limiting: after
    /// the rewrite nothing matches it any more, so running it again is a no-op, and a token that is
    /// neither (an app since uninstalled) is left exactly where it is, because those entries are
    /// carried on purpose -- see `materialise`.
    ///
    /// AMBIGUITY RESOLVES TO NOTHING. If two apps share a display name, an old entry naming it
    /// cannot say which was meant, and picking one would silently hand the other's arrangement to
    /// the wrong app. The entry is left alone instead: the apps then appear in their computed
    /// folders, which is visible and fixable with a drag, where a wrong guess is neither.
    pub fn migrate_names_to_ids(&mut self) {
        let mut touched = false;
        for m in &self.base {
            let mut ids: std::collections::HashSet<&str> = std::collections::HashSet::new();
            let mut by_name: HashMap<&str, Vec<&str>> = HashMap::new();
            for lines in &m.cells {
                for l in lines {
                    for a in &l.apps {
                        ids.insert(a.id.as_str());
                        by_name
                            .entry(a.name.as_str())
                            .or_default()
                            .push(a.id.as_str());
                    }
                }
            }
            let rename = |token: &String| -> Option<String> {
                if ids.contains(token.as_str()) {
                    return None;
                }
                match by_name.get(token.as_str()) {
                    Some(candidates) if candidates.len() == 1 => Some(candidates[0].to_string()),
                    _ => None,
                }
            };

            if let Some(folders) = self.placement.get_mut(&m.name) {
                for lines in folders.values_mut() {
                    for l in lines.iter_mut() {
                        for token in l.apps_mut().iter_mut() {
                            if let Some(id) = rename(token) {
                                *token = id;
                                touched = true;
                            }
                        }
                    }
                }
            }

            // Usage keys are "<machine>\u{1}<app>", so the same rewrite applies to the second half.
            let prefix = format!("{}\u{1}", m.name);
            let moves: Vec<(String, String)> = self
                .usage
                .keys()
                .filter_map(|k| {
                    let app = k.strip_prefix(&prefix)?;
                    rename(&app.to_string()).map(|id| (k.clone(), usage::key(&m.name, &id)))
                })
                .collect();
            for (from, to) in moves {
                if let Some(entry) = self.usage.remove(&from) {
                    self.usage.insert(to, entry);
                    touched = true;
                }
            }
        }

        // Written once, so the next start has nothing to do. Skipped entirely when nothing moved,
        // which is every start after the first -- a migration that rewrote both state files on
        // every launch would be a needless write on the path this program is judged by.
        if touched {
            if self.placement_writable {
                save_placement(&self.placement);
            }
            self.save_usage();
        }
    }

    /// Re-derive the grid from the pristine inventory plus the user's filings, then re-filter.
    /// Called on startup and after every drop, so there is exactly one path from
    /// (inventory, placement) to what is on screen.
    pub fn rebuild(&mut self) {
        self.machines = apply_placement(&self.base, &self.placement, &self.folders);
        apply_visibility(&mut self.machines, &self.visibility);
        // Placement decides MEMBERSHIP -- which folder, which line. Usage decides ORDER within
        // that, and only where the evidence justifies a move. Running it here rather than baking
        // it into the placement file keeps the two separable: the file stays a record of what the
        // user arranged, never of what the statistics did to it.
        apply_usage(
            &mut self.machines,
            &self.usage,
            usage::now_secs(),
            self.z,
            self.half_life_days,
        );
        self.refilter();
    }

    /// Replace the model inputs returned by a background inventory refresh.
    ///
    /// A resident launcher normally receives the same inventory on every reveal. Rebuilding an
    /// identical model allocates the derived grid and then makes GTK replace every widget in it,
    /// even though not one visible byte changed. Equality here is exact -- including order,
    /// errors, launch prefixes and layout -- so `false` is safe evidence that the cached grid is
    /// already current, while any real change still takes the one ordinary rebuild path.
    pub fn replace_inventory(
        &mut self,
        folders: Vec<String>,
        layout: Layout,
        base: Vec<Machine>,
    ) -> bool {
        if self.folders == folders && self.layout == layout && self.base == base {
            return false;
        }
        self.folders = folders;
        self.layout = layout;
        self.base = base;
        self.rebuild();
        self.clamp();
        true
    }

    /// Freeze a machine's CURRENT grid into the placement verbatim.
    ///
    /// Called before every move, and the reason is subtle: until the user has touched a machine,
    /// most of its apps have no placement entry at all and are drawn from the computed grouping.
    /// Moving one app without first recording the rest would leave its neighbours to be re-derived
    /// and re-appended, so they would visibly jump. Snapshotting first means a drop changes exactly
    /// the one thing the user dragged.
    pub fn materialise(&mut self, mi: usize) {
        // FROM THE PLACEMENT GRID, NOT THE RENDERED ONE, and the difference is the whole reason
        // this is not simply `&self.machines[mi]`.
        //
        // `self.machines` has been through `apply_usage`, so its lines are in FRECENCY
        // order. Snapshotting that would write the statistics' opinion into placement.json as
        // though the user had arranged it -- and once written it is indistinguishable from a real
        // arrangement, so it never decays and never moves again. Two drags in, the file is a
        // fossil of whatever the scores happened to say at the moment of the first one.
        //
        // `rebuild`'s own comment promises exactly the opposite: placement decides membership,
        // usage decides order, and the file "stays a record of what the user arranged, never of
        // what the statistics did to it". This recomputes the placement-only grid so that promise
        // is true rather than merely stated.
        let placed = apply_placement(&self.base, &self.placement, &self.folders);
        let m = &placed[mi];
        let mut folders: HashMap<String, Vec<StoredLine>> = HashMap::new();
        for (r, lines) in m.cells.iter().enumerate() {
            if lines.is_empty() {
                continue;
            }
            folders.insert(
                // The configured label at the same row index. Folder order never changes at
                // runtime, so labels and cells remain parallel through every rebuild.
                self.folders[r].clone(),
                lines
                    .iter()
                    .map(|l| {
                        StoredLine::new(
                            l.name.clone(),
                            l.apps.iter().map(|a| a.id.clone()).collect(),
                        )
                    })
                    .collect(),
            );
        }
        // MERGE, never replace. An app named in the placement but absent from today's inventory
        // has no cell to be snapshotted from, so a wholesale insert would drop it -- silently
        // discarding the arrangement of everything that happened to be missing (uninstalled, or a
        // backend that timed out and returned a partial list) the moment the user dragged anything
        // else. Entries for apps we cannot currently see are carried through untouched, which is
        // what makes "uninstall it and it returns to where you put it" true.
        let seen: std::collections::HashSet<String> = folders
            .values()
            .flatten()
            .flat_map(|l| l.apps().iter().cloned())
            .collect();
        let previous = self.placement.get(&m.name).cloned().unwrap_or_default();
        for (folder, lines) in previous {
            // Carried through with their NAMES intact: an app that is not in today's inventory
            // has not stopped belonging to the row somebody filed it in.
            let kept: Vec<StoredLine> = lines
                .into_iter()
                .map(|l| {
                    let name = l.name().map(|n| n.to_string());
                    let apps: Vec<String> = l
                        .apps()
                        .iter()
                        .filter(|n| !seen.contains(*n))
                        .cloned()
                        .collect();
                    StoredLine::new(name, apps)
                })
                .filter(|l| !l.apps().is_empty())
                .collect();
            if !kept.is_empty() {
                folders.entry(folder).or_default().extend(kept);
            }
        }
        self.placement.insert(m.name.clone(), folders);
    }

    /// Move `app` on machine `mi` into `folder`, joining the line that `join` names, immediately
    /// before the app that `before` names. This one call covers filing into a folder, joining an
    /// appset at a chosen position, and reordering within a line -- they differ only in where the
    /// drop landed, which is the point of recording arrangements rather than folders.
    ///
    /// ── EVERYTHING HERE IS NAMED, NOTHING IS NUMBERED, AND THAT IS THE FIX ────────────────────
    ///
    /// This used to take a line INDEX and a position INDEX, read straight off the rendered grid,
    /// and it was wrong in two independent ways at once:
    ///
    ///   * The rendered grid is FILTERED. With a query active, "the second line" on screen might
    ///     be the fifth line in the placement -- every line whose apps all failed to match is
    ///     simply absent. Dropping onto a visible appset therefore joined a different one, and the
    ///     user had no way to see which.
    ///   * The rendered grid is USAGE-ORDERED. `apply_usage` permutes lines by frecency, so even
    ///     with no query the on-screen order is not the stored order once anything has been
    ///     launched a few times.
    ///
    /// An index is only meaningful in the space it was computed in, and the drop happened in a
    /// space that is two transformations away from the one being written to. Names survive both
    /// transformations, because filtering and reordering move apps around without renaming them.
    ///
    /// `join` is the rendered line's membership rather than one chosen app: the first name on it
    /// may BE the app being dragged, which is gone by the time the target is looked up, so the
    /// anchor has to be picked as one that will still be there.
    pub fn place_app(
        &mut self,
        mi: usize,
        app: &str,
        folder: usize,
        join: Option<&[String]>,
        before: Option<&str>,
    ) {
        // Dropping something into the gap it already occupies is not a move. Worth an early return
        // rather than letting it fall through: after the removal below, "insert before `app`"
        // names a position that no longer exists, and the fallback for an unfindable neighbour is
        // "append to the end" -- so the no-op would silently become a jump to the far right.
        if before == Some(app) {
            return;
        }

        self.materialise(mi);
        let machine = self.machines[mi].name.clone();
        let target_label = self.folders[folder].clone();
        // An anchor that survives the removal: any name on the target line that is not the one
        // being moved. A line holding only the dragged app has none, and correctly yields None --
        // moving an app out of its own single-app line and back onto it is a no-op either way.
        let anchor = join.and_then(|names| names.iter().find(|n| n.as_str() != app).cloned());
        let folders = self.placement.entry(machine).or_default();

        // Remove it from wherever it currently is, and drop any line that empties as a result.
        for lines in folders.values_mut() {
            for l in lines.iter_mut() {
                l.apps_mut().retain(|n| n != app);
            }
            // An emptied line disappears UNLESS it was named: a declared row is part of the
            // taxonomy, not a by-product of what happens to be in it, and it has to survive being
            // emptied or you could never drag the last thing out and put something else back.
            lines.retain(|l| !l.apps().is_empty() || l.name().is_some());
        }

        // Find the target AFTER the removal, never before: taking the app out can empty a line and
        // delete it, which shifts every later line down one. A target located beforehand would
        // then name the wrong line -- the same class of bug as the rendered indices this replaced.
        let target_line = anchor.as_ref().and_then(|a| {
            folders
                .get(&target_label)
                .and_then(|ls| ls.iter().position(|l| l.apps().iter().any(|n| n == a)))
        });

        let lines = folders.entry(target_label).or_default();
        match target_line {
            Some(li) => {
                // An unfindable neighbour appends rather than panicking. It happens legitimately:
                // `before` names what was rendered next, and that app may be filtered out of the
                // stored line, or on a different one entirely.
                let at = before
                    .and_then(|b| lines[li].apps().iter().position(|n| n == b))
                    .unwrap_or(lines[li].apps().len());
                lines[li].apps_mut().insert(at, app.to_string());
            }
            // No anchor, or the line it named is gone: give the app a line of its own, which is
            // also what dropping on a cell's background means.
            None => lines.push(StoredLine::Bare(vec![app.to_string()])),
        }

        if self.placement_writable {
            save_placement(&self.placement);
        }
        self.rebuild();
        // The drop can shrink the line the cursor was on. Nothing else in this path re-checks it.
        self.clamp();
    }

    /// Change the query, and put the cursor back at the top of what is now on screen.
    ///
    /// SEPARATE FROM `refilter`, and the distinction is the bug this fixes. `refilter` runs for two
    /// unrelated reasons -- the query changed, or the grid was rebuilt after a drag -- and only one
    /// of them should move the cursor. Resetting inside `refilter` would drag the selection away
    /// from the app the user just dropped; not resetting at all leaves the goal column pointing at
    /// a position that now holds a different app.
    ///
    /// `item_goal` is the column that up/down preserves, and it is only meaningful while a line's
    /// membership is stable. A query rewrites that membership, so the goal is stale by definition:
    /// keeping it selects the Nth surviving app rather than the first match, and the first match is
    /// the one the user is looking at while they type. `snap_to_content` already reasoned this out
    /// for the case where the cell empties entirely; this is the same rule for the case where it
    /// does not.
    pub fn set_query(&mut self, q: String) {
        self.query = q;
        self.line = 0;
        self.item = 0;
        self.item_goal = 0;
        self.refilter();
    }

    /// FUZZY, not substring, and not hand-rolled: `nucleo`, the matcher Helix uses. A substring
    /// filter is the obvious thing to write and the wrong thing to ship -- typing "e" matched
    /// almost every application here, because nearly every name contains one. Fuzzy matching with
    /// a real score is what makes "cod" find "Code - OSS" and "gimp" not match "Manage Printing",
    /// and it is a solved problem with a maintained crate behind it.
    ///
    /// Applied to EVERY cell at once -- the whole point of a matrix is seeing "where does this
    /// thing exist" across machines, and a filter that only searched the current column would
    /// throw that away. Lines that lose every app disappear; a cell that loses every line renders
    /// as empty, which is itself the answer to "does that machine have this?".
    pub fn refilter(&mut self) {
        // "which machine" is a thing you can TYPE, not only a thing you arrow to.
        //
        // The grid's whole premise is that a machine is a position -- but the search box was
        // machine-blind, so the moment you started typing you lost the one axis the layout exists
        // for, and `foot@srvhome` searched for an application literally called that. Naming the
        // machine in the query keeps both halves of the idea available at once.
        let names: Vec<(String, Vec<String>)> = self
            .machines
            .iter()
            .map(|m| (m.name.clone(), m.aliases.clone()))
            .collect();
        let (only, pattern_text) = split_query(&self.query, &names);

        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = (!pattern_text.is_empty())
            .then(|| Pattern::parse(&pattern_text, CaseMatching::Ignore, Normalization::Smart));
        let mut buf = Vec::new();
        let mut keep = |name: &str| -> bool {
            match &pattern {
                None => true,
                Some(p) => p
                    .score(Utf32Str::new(name, &mut buf), &mut matcher)
                    .is_some(),
            }
        };
        self.view = self
            .machines
            .iter()
            .map(|m| Machine {
                name: m.name.clone(),
                aliases: m.aliases.clone(),
                accent: m.accent.clone(),
                launch: m.launch.clone(),
                error: m.error.clone(),
                // A named machine empties the others rather than removing their columns. The
                // column is the machine's identity in this layout; dropping it would make the grid
                // reflow under the query and move everything the user had learned the position of.
                cells: if only.as_deref().is_some_and(|w| w != m.name) {
                    vec![Vec::new(); m.cells.len()]
                } else {
                    m.cells
                        .iter()
                        .map(|lines| {
                            lines
                                .iter()
                                .filter_map(|l| {
                                    let apps: Vec<Arc<App>> =
                                        l.apps.iter().filter(|a| keep(&a.name)).cloned().collect();
                                    // A NAMED row survives having no matches while there is no
                                    // query, because that is precisely when you need to see it: an
                                    // empty declared row is a place to drag something INTO, and one
                                    // that is invisible until it already has contents can never
                                    // acquire any. Under a query it goes, like any line with nothing
                                    // in it -- a search should show what matched, not the taxonomy.
                                    let keeps_place = l.name.is_some() && pattern.is_none();
                                    if apps.is_empty() && !keeps_place {
                                        None
                                    } else {
                                        Some(Line {
                                            name: l.name.clone(),
                                            apps,
                                        })
                                    }
                                })
                                .collect()
                        })
                        .collect()
                },
            })
            .collect();
        // Shape the CURRENT vectors, not the inventory snapshot. Search and visibility can change
        // a row from 7 items to 5 (two lines to one), while a refresh or reset can cross the rail
        // threshold in the other direction. This count-only pass is cheap and deterministic.
        reflow_vectors(&mut self.view, &self.folders, &self.layout);
        self.snap_to_content();
    }

    /// After a filter, the cursor is very likely sitting on a cell that no longer has anything in
    /// it. Rather than leaving it stranded, walk to the first cell that DOES -- reading order,
    /// row-major -- so typing always leaves something selected and Enter always means something.
    pub fn snap_to_content(&mut self) {
        if self.view.is_empty() || !self.cell().is_empty() {
            return;
        }
        for r in 0..self.folders.len() {
            for c in 0..self.view.len() {
                if !self.view[c].cells[r].is_empty() {
                    self.row = r;
                    self.col = c;
                    self.line = 0;
                    self.item = 0;
                    // The goal column belongs to the cell it was chosen in. Carrying it across a
                    // snap selects the Nth item of an unrelated cell rather than the first match,
                    // which for a search result is the one thing the user is looking at.
                    self.item_goal = 0;
                    return;
                }
            }
        }
    }

    /// Every cursor move funnels through here, so no key handler can leave the cursor pointing
    /// into a cell or line that shrank under it -- the class of bug that only appears once real
    /// inventories differ in length between machines, or the moment a filter empties something.
    /// Does any machine have anything on this row?
    ///
    /// A row nobody has anything on is not information, it is a gap with a label. It arises
    /// honestly: every folder emits a catch-all row alongside its subcategories, so sorting every
    /// chat client into biz/leis/priv leaves the bare `Chat` row empty everywhere -- and a
    /// subcategory whose members are installed on no machine does the same.
    pub fn row_has_content(&self, r: usize) -> bool {
        self.view
            .iter()
            .any(|m| m.cells.get(r).is_some_and(|c| !c.is_empty()))
    }

    /// The next row in `dir` that has something on it, or the current one if none does.
    ///
    /// Skipping rather than hiding-and-reindexing: row indices stay aligned with `folders` and the
    /// placement.
    /// An empty row is simply never landed on.
    pub fn next_row(&self, from: usize, dir: i32) -> usize {
        let last = self.folders.len().saturating_sub(1);
        let mut r = from;
        loop {
            let next = if dir < 0 {
                r.checked_sub(1)
            } else {
                (r + 1).le(&last).then_some(r + 1)
            };
            let Some(next) = next else { return from };
            r = next;
            if self.row_has_content(r) {
                return r;
            }
        }
    }

    pub fn clamp(&mut self) {
        self.col = self.col.min(self.view.len().saturating_sub(1));
        self.row = self.row.min(self.folders.len().saturating_sub(1));
        let lines = self.cell().len();
        if lines == 0 {
            self.line = 0;
            self.item = 0;
            self.focus = Focus::Outside;
            return;
        }
        self.line = self.line.min(lines - 1);
        let items = self.cell()[self.line].apps.len();
        // Aim for the goal column, clamped to THIS line -- never overwrite the goal itself.
        self.item = if items == 0 {
            0
        } else {
            self.item_goal.min(items - 1)
        };
    }
}

/// Split a typed query into an optional machine and the pattern to match applications against.
///
/// ── WHY SEVERAL SPELLINGS, AND WHY EITHER ORDER ──────────────────────────────────────────────
///
/// Because people already have a habit and it is not the same habit. `foot@srvhome` is the shape
/// SSH and email taught; `server.foot` is the shape a namespace or an object path teaches;
/// `foot.worklxc` is the shape a hostname teaches. All three mean one thing, none is more correct,
/// and a launcher that accepted only one would be teaching its own convention for no reason.
///
/// So the separator may be any of `@ . : /`, and the machine may be on either side. Which side is
/// which is decided by the DATA rather than by the syntax: whichever side names a machine is the
/// machine. That is what lets one rule serve every spelling instead of one rule per spelling.
///
/// A prefix is enough as long as it is unambiguous -- `foot@ser` reaches a machine called
/// `server` -- because the point is to type less, and a qualifier you must spell in full is barely
/// faster than arrowing to the column. An empty pattern (`server.`) means "everything on that
/// machine", which falls out of the same rule rather than needing its own.
///
/// AMBIGUITY RESOLVES TO NO MACHINE. If neither side names one, or the query has no separator, the
/// whole query is the pattern -- so an application whose name happens to contain an `@` or a dot
/// still searches normally, and the feature can never make an ordinary search stop working.
pub fn split_query(query: &str, machines: &[(String, Vec<String>)]) -> (Option<String>, String) {
    let resolve = |token: &str| -> Option<String> {
        if token.is_empty() {
            return None;
        }
        let lower = token.to_lowercase();
        // EXACT FIRST, and an alias counts as exact. A declared nickname is a deliberate statement
        // that this word means this machine, so it must beat any accident of spelling -- otherwise
        // adding a machine whose name happens to start with someone's alias would quietly break
        // a shortcut they had been using for months.
        if let Some((n, _)) = machines.iter().find(|(n, al)| {
            n.to_lowercase() == lower || al.iter().any(|a| a.to_lowercase() == lower)
        }) {
            return Some(n.clone());
        }
        // Then a unique prefix of a name or an alias. Two candidates starting the same way is not
        // an invitation to guess -- it is a reason to treat the token as ordinary search text.
        let mut hits = machines.iter().filter(|(n, al)| {
            n.to_lowercase().starts_with(&lower)
                || al.iter().any(|a| a.to_lowercase().starts_with(&lower))
        });
        match (hits.next(), hits.next()) {
            (Some((n, _)), None) => Some(n.clone()),
            _ => None,
        }
    };

    for sep in ['@', ':', '/', '.'] {
        let Some((left, right)) = query.split_once(sep) else {
            continue;
        };
        // The right side first for `@`, which universally means "the thing after the at is where":
        // it is the one spelling where the order is not a matter of taste.
        let (first, second) = if sep == '@' {
            (right, left)
        } else {
            (left, right)
        };
        if let Some(m) = resolve(first) {
            return (Some(m), second.trim().to_string());
        }
        if let Some(m) = resolve(second) {
            return (Some(m), first.trim().to_string());
        }
    }
    (None, query.trim().to_string())
}

/// Re-derive a machine's grid from the pristine inventory plus the user's arrangement.
///
/// Two passes, and the order is the contract: the user's own lines first, in their own order, then
/// everything the arrangement does not mention, in the folder the category table computed for it.
/// That is what makes a newly-installed app appear in "Other" without any entry existing for it,
/// while nothing the user has already arranged moves to make room.
///
/// Names that no longer resolve are skipped rather than removed from the arrangement -- uninstall
/// something and reinstall it later and it returns to where you put it.
pub fn apply_placement(base: &[Machine], p: &Placement, folders: &[String]) -> Vec<Machine> {
    base.iter()
        .map(|m| {
            let mut by_id: HashMap<&str, &Arc<App>> = HashMap::new();
            for lines in &m.cells {
                for l in lines {
                    for app in &l.apps {
                        by_id.insert(app.id.as_str(), app);
                    }
                }
            }

            let arranged = p.get(&m.name);
            let mut placed: std::collections::HashSet<&str> = std::collections::HashSet::new();
            if let Some(folders) = arranged {
                for lines in folders.values() {
                    for l in lines {
                        for n in l.apps() {
                            placed.insert(n.as_str());
                        }
                    }
                }
            }

            let mut cells: Vec<Vec<Line>> = vec![Vec::new(); folders.len()];

            if let Some(arranged_folders) = arranged {
                for (fi, fname) in folders.iter().enumerate() {
                    let Some(lines) = arranged_folders.get(fname.as_str()) else {
                        continue;
                    };
                    for l in lines {
                        let apps: Vec<Arc<App>> = l
                            .apps()
                            .iter()
                            .filter_map(|n| by_id.get(n.as_str()).map(|a| Arc::clone(a)))
                            .collect();
                        // A NAMED row is drawn even when empty. It is part of the taxonomy the
                        // operator declared, and a row you cannot see is a row you cannot drag
                        // anything into -- which would make declaring one pointless.
                        let name = l.name().map(|n| n.to_string());
                        if !apps.is_empty() || name.is_some() {
                            cells[fi].push(Line { name, apps });
                        }
                    }
                }
            }

            for (r, lines) in m.cells.iter().enumerate() {
                for l in lines {
                    let apps: Vec<Arc<App>> = l
                        .apps
                        .iter()
                        .filter(|a| !placed.contains(a.id.as_str()))
                        .cloned()
                        .collect();
                    // The name comes along. A row keeps what it is called even when the pass that
                    // rebuilt it was only concerned with which apps had not been filed yet.
                    if !apps.is_empty() || l.name.is_some() {
                        cells[r].push(Line {
                            name: l.name.clone(),
                            apps,
                        });
                    }
                }
            }

            Machine {
                name: m.name.clone(),
                aliases: m.aliases.clone(),
                accent: m.accent.clone(),
                launch: m.launch.clone(),
                error: m.error.clone(),
                cells,
            }
        })
        .collect()
}

/// Remove hidden ids from the derived grid without touching inventory or placement.
///
/// This pass owns membership only. `reflow_vectors` owns presentation and immediately flattens
/// each cell into the configured balanced lines or rail, so packing here as well merely allocated
/// a temporary layout that the next pass discarded.
pub fn apply_visibility(machines: &mut [Machine], visibility: &Visibility) {
    for machine in machines {
        let Some(hidden) = visibility.get(&machine.name) else {
            continue;
        };
        for cell in &mut machine.cells {
            for line in cell.iter_mut() {
                line.apps.retain(|app| !hidden.contains(&app.id));
            }
            // Named rows remain valid placement targets even when their final app is hidden.
            cell.retain(|line| !line.apps.is_empty() || line.name.is_some());
        }
    }
}

/// Present every cell as one ordered vector, split into balanced visual lines or one bounded rail.
///
/// Placement may still contain several stored lines because it records ordering independently of
/// the current display. Flattening happens only in the derived view: a label cutoff, a search, or
/// hiding an application can therefore reflow immediately without rewriting user state.
pub fn reflow_vectors(machines: &mut [Machine], folders: &[String], layout: &Layout) {
    for machine in machines {
        for (row, cell) in machine.cells.iter_mut().enumerate() {
            let Some(label) = folders.get(row) else {
                continue;
            };
            let apps: Vec<Arc<App>> = std::mem::take(cell)
                .into_iter()
                .flat_map(|line| line.apps)
                .collect();
            if apps.is_empty() {
                continue;
            }
            let lengths = layout
                .line_lengths(label, apps.len())
                .unwrap_or_else(|| vec![apps.len()]);
            let mut apps = apps.into_iter();
            for length in lengths {
                cell.push(Line {
                    name: None,
                    apps: apps.by_ref().take(length).collect(),
                });
            }
        }
    }
}

/// Build the argv that actually starts `app` on `machine`.
///
/// Expand the freedesktop Exec field codes. File/URL codes disappear because this launcher was
/// invoked without files; name/icon/desktop-path codes come from inventory metadata, and `%%` is a
/// literal percent sign. Unknown codes are invalid by the spec and are removed rather than passed
/// as surprising literal arguments.
///
/// Splitting is `shlex`, not `split_whitespace`: an Exec may legitimately quote an argument
/// containing spaces, and naive splitting turns one path into two broken ones.
pub fn launch_argv(machine: &Machine, app: &App, terminal: &[String]) -> Option<Vec<String>> {
    // Empty means READ-ONLY. Treating it as an ordinary empty prefix launches the inventory's Exec
    // on the local machine while claiming it started on the selected remote column. A local column
    // is explicit: `launch = ["{}"]`.
    if machine.launch.is_empty() {
        return None;
    }
    // SPLIT FIRST, then drop field codes. Stripping on whitespace beforehand reached INSIDE
    // quoted arguments and collapsed runs of spaces, so `prog "a  b"` became `prog "a b"` -- a
    // path silently altered on its way to exec.
    let tokens: Vec<String> = shlex::split(&app.exec).unwrap_or_else(|| vec![app.exec.clone()]);
    let tokens = expand_exec_tokens(tokens, app);

    let mut inner = Vec::new();
    // ORDER MATTERS, and it is the non-obvious part: the machine prefix comes FIRST, then the
    // terminal, then the program. A terminal emulator for a remote program has to run on the
    // remote machine -- `<forward> foot -e helix`, never `foot -e <forward> helix`, which would
    // open a local window and then try to forward from inside it.
    if app.terminal {
        inner.extend(terminal.iter().cloned());
    }
    inner.extend(tokens);
    // `{}` is a PLACEHOLDER when present, a prefix when absent. config.rs documented the
    // placeholder and nothing ever substituted it, so a launch template containing one passed the
    // two braces to exec as a literal argument.
    if machine.launch.iter().any(|a| a == "{}") {
        let mut out = Vec::new();
        for a in &machine.launch {
            if a == "{}" {
                out.extend(inner.iter().cloned());
            } else {
                out.push(a.clone());
            }
        }
        return Some(out);
    }
    let mut argv = machine.launch.clone();
    argv.extend(inner);
    Some(argv)
}

fn expand_exec_tokens(tokens: Vec<String>, app: &App) -> Vec<String> {
    let mut expanded = Vec::new();
    for token in tokens {
        if token == "%i" {
            if !app.icon.is_empty() {
                expanded.push("--icon".to_string());
                expanded.push(app.icon.clone());
            }
            continue;
        }
        let mut output = String::new();
        let mut chars = token.chars();
        while let Some(character) = chars.next() {
            if character != '%' {
                output.push(character);
                continue;
            }
            match chars.next() {
                Some('%') => output.push('%'),
                Some('c') => output.push_str(&app.name),
                Some('k') => output.push_str(&app.desktop_file),
                Some('i') => output.push_str(&app.icon),
                Some('f' | 'F' | 'u' | 'U') => {}
                Some(_) => {}
                None => output.push('%'),
            }
        }
        if !output.is_empty() {
            expanded.push(output);
        }
    }
    expanded
}

/// Order each cell by how often things are actually used -- WITHOUT sorting it.
///
/// Two levels, both through the same significance gate, so nothing moves on noise:
///   * items on a line, by their own score;
///   * lines within a cell, by the sum of their apps -- an appset you start often should rise as
///     a unit, since starting the line is one action;
///
/// Folder order never participates. It is explicitly declared configuration and therefore part of
/// the launcher's learnable spatial layout; statistics may arrange a folder's contents, not move
/// the folder itself.
pub fn apply_usage(machines: &mut [Machine], u: &Usage, now: u64, z: f64, hl: f64) {
    for m in machines.iter_mut() {
        let name = m.name.clone();
        for cell in m.cells.iter_mut() {
            for line in cell.iter_mut() {
                usage::reorder_stable(
                    &mut line.apps,
                    |a| usage::score_of(u, &name, &a.id, now, hl),
                    z,
                );
            }
            usage::reorder_stable(
                cell,
                |l| {
                    l.apps
                        .iter()
                        .map(|a| usage::score_of(u, &name, &a.id, now, hl))
                        .sum::<f64>()
                },
                z,
            );
        }
    }
}

#[cfg(test)]
pub fn a(name: &str, icon: &str) -> App {
    App {
        id: name.into(),
        name: name.into(),
        icon: icon.into(),
        exec: name.to_lowercase(),
        terminal: false,
        desktop_file: String::new(),
    }
}

#[cfg(test)]
pub fn line(apps: Vec<App>) -> Line {
    Line {
        name: None,
        apps: apps.into_iter().map(Arc::new).collect(),
    }
}

/// FIXTURE. Deliberately uneven -- different line counts per cell, different item counts per
/// line, and some cells genuinely empty -- because an even grid hides exactly the layout bugs a
/// real multi-machine configuration produces.
#[cfg(test)]
pub fn fixture() -> Vec<Machine> {
    vec![
        Machine {
            name: "laptop".into(),
            aliases: vec![],
            accent: "#166534".into(),
            launch: vec!["{}".into()],
            error: None,
            cells: vec![
                vec![
                    line(vec![a("Foot", "foot"), a("Foot Client", "foot")]),
                    line(vec![a("Foot Server", "foot")]),
                ],
                vec![
                    line(vec![
                        a("Helix", "helix"),
                        a("Code - OSS", "com.visualstudio.code.oss"),
                    ]),
                    line(vec![
                        a("Builder", "org.gnome.Builder"),
                        a("IntelliJ IDEA", "idea"),
                    ]),
                ],
                vec![line(vec![
                    a("Firefox", "firefox"),
                    a("Chromium", "chromium"),
                ])],
                // The appset that started this whole conversation: all the messengers, one line.
                vec![line(vec![
                    a("Telegram", "org.telegram.desktop"),
                    a("ZapZap", "com.rtosta.zapzap"),
                    a("Teams", "teams-for-linux"),
                ])],
                vec![line(vec![
                    a("Thunar", "thunar"),
                    a("Czkawka", "com.github.qarmin.czkawka"),
                ])],
                vec![line(vec![a("mpv", "mpv"), a("VLC", "vlc")])],
                // Other: the inbox. Two apps the category table did not recognise.
                vec![
                    line(vec![a("Czkawka", "com.github.qarmin.czkawka")]),
                    line(vec![a("Bottles", "com.usebottles.bottles")]),
                ],
            ],
        },
        Machine {
            name: "workstation".into(),
            aliases: vec![],
            accent: "#B45309".into(),
            launch: vec!["{}".into()],
            error: None,
            cells: vec![
                vec![line(vec![a("Foot", "foot"), a("Alacritty", "Alacritty")])],
                vec![line(vec![a("Helix", "helix"), a("Zed", "dev.zed.Zed")])],
                vec![line(vec![a("Firefox", "firefox")])],
                vec![],
                vec![line(vec![a("Thunar", "thunar")])],
                vec![
                    line(vec![a("mpv", "mpv"), a("VLC", "vlc")]),
                    line(vec![a("Kdenlive", "kdenlive"), a("GIMP", "gimp")]),
                ],
                vec![line(vec![a("Steam", "steam")])],
            ],
        },
        Machine {
            name: "console".into(),
            aliases: vec![],
            accent: "#9F1239".into(),
            launch: vec!["{}".into()],
            error: None,
            cells: vec![
                vec![line(vec![a("Foot", "foot")])],
                vec![line(vec![a("Helix", "helix"), a("Nano", "nano")])],
                vec![],
                vec![line(vec![a("aerc", "utilities-terminal")])],
                vec![line(vec![a("Engrampa", "engrampa")])],
                vec![line(vec![a("mpv", "mpv")])],
                // An EMPTY inbox -- the steady state, and still drawn.
                vec![],
            ],
        },
    ]
}

// ── tests ───────────────────────────────────────────────────────────────────────────────────
//
// These cover the four things that were actually wrong at some point while building this, plus
// the invariants that stop them coming back. Every one of them runs without a display.
#[cfg(test)]
mod tests {
    use super::*;

    static TEST_TMP: AtomicU64 = AtomicU64::new(0);

    /// A rendered line's membership, as `place_app` takes it. Tests name the line by what is on
    /// it for the same reason the drop handler does -- an index would not survive a filter.
    fn on(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn app(n: &str) -> Arc<App> {
        Arc::new(App {
            id: n.into(),
            name: n.into(),
            icon: "x".into(),
            exec: n.to_lowercase(),
            terminal: false,
            desktop_file: String::new(),
        })
    }

    /// One machine, one folder, with the given lines. `folder` indexes DEFAULT_FOLDERS.
    fn machine(name: &str, folder: usize, lines: Vec<Vec<&str>>) -> Machine {
        let mut cells: Vec<Vec<Line>> = vec![Vec::new(); DEFAULT_FOLDERS.len()];
        cells[folder] = lines
            .into_iter()
            .map(|l| Line {
                name: None,
                apps: l.into_iter().map(app).collect(),
            })
            .collect();
        Machine {
            name: name.into(),
            aliases: vec![],
            accent: "#fff".into(),
            // An explicit identity template is the local-machine spelling. Empty is read-only.
            launch: vec!["{}".into()],
            error: None,
            cells,
        }
    }

    fn state(machines: Vec<Machine>) -> State {
        let mut s = State {
            folders: DEFAULT_FOLDERS.iter().map(|f| f.to_string()).collect(),
            layout: Layout::default(),
            // No usage in the model tests: they are about placement and navigation, and a live
            // reordering pass would make their expectations depend on statistics they are not
            // testing. usage.rs owns those, with its own suite.
            usage: Usage::new(),
            usage_writable: true,
            z: 2.0,
            half_life_days: crate::usage::HALF_LIFE_DAYS,
            base: machines,
            placement: Placement::new(),
            placement_writable: true,
            visibility: Visibility::new(),
            visibility_writable: true,
            machines: Vec::new(),
            view: Vec::new(),
            col: 0,
            row: 0,
            line: 0,
            item: 0,
            item_goal: 0,
            focus: Focus::Outside,
            query: String::new(),
        };
        s.machines = apply_placement(&s.base, &s.placement, &s.folders);
        s.refilter();
        s
    }

    #[test]
    fn an_unchanged_inventory_refresh_is_a_noop() {
        let mut s = state(vec![machine("m", 0, vec![vec!["alpha", "beta"]])]);
        let before = s.view.clone();

        assert!(!s.replace_inventory(s.folders.clone(), s.layout.clone(), s.base.clone()));
        assert!(s.view == before, "the cached derived grid was retained");

        let replacement = vec![machine("m", 0, vec![vec!["alpha", "gamma"]])];
        assert!(s.replace_inventory(s.folders.clone(), s.layout.clone(), replacement));
        assert_eq!(s.view[0].cells[0][0].apps[1].id, "gamma");
    }

    #[test]
    fn derived_grids_share_immutable_app_payloads() {
        let s = state(vec![machine("m", 0, vec![vec!["shared"]])]);
        let base = &s.base[0].cells[0][0].apps[0];
        let arranged = &s.machines[0].cells[0][0].apps[0];
        let visible = &s.view[0].cells[0][0].apps[0];
        assert!(Arc::ptr_eq(base, arranged));
        assert!(Arc::ptr_eq(base, visible));
    }

    #[test]
    fn vectors_are_reflowed_from_current_item_count() {
        let folders = vec!["Games/strategy".to_string()];
        let mut machines = vec![machine(
            "m",
            0,
            vec![
                vec!["Alpha", "Beta", "Gamma"],
                vec!["Delta", "Epsilon", "Zeta", "Eta"],
            ],
        )];

        reflow_vectors(&mut machines, &folders, &Layout::default());

        assert_eq!(machines[0].cells[0].len(), 2);
        assert_eq!(machines[0].cells[0][0].apps.len(), 4);
        assert_eq!(
            machines[0].cells[0]
                .iter()
                .flat_map(|line| line.apps.iter())
                .map(|app| app.name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "Beta", "Gamma", "Delta", "Epsilon", "Zeta", "Eta"]
        );
    }

    /// A cell is a real 2D grid, not a list of lists: moving DOWN keeps the column.
    #[test]
    fn down_preserves_the_column() {
        let mut s = state(vec![machine(
            "m",
            0,
            vec![vec!["a", "b", "c"], vec!["d", "e", "f"]],
        )]);
        s.focus = Focus::Inside;
        s.item = 1;
        s.item_goal = 1;
        s.line = 1;
        s.clamp();
        assert_eq!(
            s.cell()[s.line].apps[s.item].name,
            "e",
            "line 1 item 2 -> down -> line 2 item 2"
        );
    }

    /// Crossing a SHORT line clamps for that line only. Coming back returns to the goal column
    /// instead of drifting left -- the bug a naive `item = min(item, len-1)` produces.
    #[test]
    fn a_short_line_does_not_eat_the_goal_column() {
        let mut s = state(vec![machine("m", 0, vec![vec!["placeholder"]])]);
        s.view[0].cells[0] = vec![
            Line {
                name: None,
                apps: vec![app("a"), app("b"), app("c")],
            },
            Line {
                name: None,
                apps: vec![app("solo")],
            },
            Line {
                name: None,
                apps: vec![app("x"), app("y"), app("z")],
            },
        ];
        s.focus = Focus::Inside;
        s.item = 2;
        s.item_goal = 2;

        s.line = 1;
        s.clamp();
        assert_eq!(s.item, 0, "clamped onto the one-item line");

        s.line = 2;
        s.clamp();
        assert_eq!(
            s.cell()[s.line].apps[s.item].name,
            "z",
            "goal column survived the short line"
        );
    }

    /// The filter reaches every machine, not just the focused column.
    #[test]
    fn search_spans_all_machines() {
        let mut s = state(vec![
            machine("one", 0, vec![vec!["Firefox", "Helix"]]),
            machine("two", 0, vec![vec!["Helix"]]),
        ]);
        s.query = "fire".into();
        s.refilter();
        assert_eq!(s.view[0].cells[0].len(), 1);
        assert_eq!(s.view[0].cells[0][0].apps.len(), 1);
        assert!(
            s.view[1].cells[0].is_empty(),
            "the other machine filtered down to nothing"
        );
    }

    /// Fuzzy, not substring: gaps are allowed, so an acronym-ish prefix finds the real name.
    #[test]
    fn search_matches_fuzzily() {
        let mut s = state(vec![machine(
            "m",
            0,
            vec![vec!["Code - OSS", "Manage Printing"]],
        )]);
        s.query = "cod".into();
        s.refilter();
        let hits: Vec<&str> = s.view[0].cells[0]
            .iter()
            .flat_map(|l| l.apps.iter().map(|a| a.name.as_str()))
            .collect();
        assert_eq!(
            hits,
            vec!["Code - OSS"],
            "matched across the gap, and did not match the other"
        );
    }

    /// The regression that motivated dropping substring matching: a single common letter must not
    /// match nearly everything, which is exactly what `contains` did.
    #[test]
    fn a_single_letter_is_not_a_wildcard() {
        let mut s = state(vec![machine("m", 0, vec![vec!["Zed", "Krita"]])]);
        s.query = "zd".into();
        s.refilter();
        let hits: Vec<&str> = s.view[0].cells[0]
            .iter()
            .flat_map(|l| l.apps.iter().map(|a| a.name.as_str()))
            .collect();
        assert_eq!(hits, vec!["Zed"]);
    }

    /// Typing must never strand the cursor on a cell the filter emptied.
    #[test]
    fn the_cursor_snaps_to_surviving_content() {
        let mut s = state(vec![
            machine("one", 0, vec![vec!["Firefox"]]),
            machine("two", 1, vec![vec!["Kdenlive"]]),
        ]);
        s.query = "kden".into();
        s.refilter();
        assert!(
            !s.cell().is_empty(),
            "cursor moved to a cell that still has something"
        );
        assert_eq!(s.cell()[0].apps[0].name, "Kdenlive");
    }

    /// Dropping on a cell background gives the app a line of its own, in the target folder.
    #[test]
    fn filing_into_a_folder_moves_the_app() {
        let mut s = state(vec![machine("m", 6, vec![vec!["Bottles"]])]); // 6 = Other, the inbox
        s.place_app(0, "Bottles", 4, None, None); // 4 = Files
        assert!(s.machines[0].cells[6].is_empty(), "left the inbox");
        assert_eq!(s.machines[0].cells[4][0].apps[0].name, "Bottles");
    }

    /// Dropping ON a line inserts INTO it, at the requested position -- this is appset building.
    #[test]
    fn dropping_on_a_line_inserts_at_position() {
        let mut m = machine("m", 0, vec![vec!["a", "b", "c"]]);
        m.cells[6] = vec![Line {
            name: None,
            apps: vec![app("new")],
        }];
        let mut s = state(vec![m]);
        s.place_app(0, "new", 0, Some(&on(&["a", "b", "c"])), Some("b"));
        let names: Vec<&str> = s.machines[0].cells[0][0]
            .apps
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "new", "b", "c"], "landed between a and b");
    }

    /// Same call, same line: a reorder.
    #[test]
    fn reordering_within_a_line() {
        let mut s = state(vec![machine("m", 0, vec![vec!["a", "b", "c"]])]);
        s.place_app(0, "c", 0, Some(&on(&["a", "b", "c"])), Some("a"));
        let names: Vec<&str> = s.machines[0].cells[0][0]
            .apps
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(names, vec!["c", "a", "b"]);
    }

    /// An app nobody arranged keeps the folder the category table computed for it, so a newly
    /// installed app appears in the inbox without any placement entry existing.
    #[test]
    fn unplaced_apps_keep_their_computed_folder() {
        let mut m = machine("m", 0, vec![vec!["Foot"]]);
        m.cells[6] = vec![Line {
            name: None,
            apps: vec![app("Brand New")],
        }];
        let mut s = state(vec![m]);
        s.place_app(0, "Foot", 1, None, None); // arrange something else entirely
        assert_eq!(
            s.machines[0].cells[6][0].apps[0].name, "Brand New",
            "still in the inbox"
        );
    }

    /// A placement naming an app that no longer exists must not corrupt its neighbours.
    #[test]
    fn a_vanished_app_is_skipped_not_fatal() {
        let base = vec![machine("m", 0, vec![vec!["a", "b"]])];
        let mut p = Placement::new();
        let mut folders = HashMap::new();
        folders.insert(
            DEFAULT_FOLDERS[0].to_string(),
            vec![StoredLine::Bare(vec![
                "a".into(),
                "ghost".into(),
                "b".into(),
            ])],
        );
        p.insert("m".to_string(), folders);
        let rows: Vec<String> = DEFAULT_FOLDERS.iter().map(|f| f.to_string()).collect();
        let out = apply_placement(&base, &p, &rows);
        let names: Vec<&str> = out[0].cells[0][0]
            .apps
            .iter()
            .map(|a| a.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["a", "b"],
            "ghost skipped, order of the survivors kept"
        );
    }

    fn appx(name: &str, exec: &str) -> App {
        App {
            id: name.into(),
            name: name.into(),
            icon: "x".into(),
            exec: exec.into(),
            terminal: false,
            desktop_file: String::new(),
        }
    }

    /// Unhandled field codes must be REMOVED, not passed through. Left in place, "%U" becomes a
    /// literal argument and the browser opens a tab for a file called "%U".
    #[test]
    fn field_codes_are_stripped() {
        let m = machine("m", 0, vec![vec!["x"]]);
        assert_eq!(
            launch_argv(&m, &appx("Firefox", "firefox %u"), &[]),
            Some(vec!["firefox".into()])
        );
        assert_eq!(
            launch_argv(&m, &appx("Files", "thunar %F"), &[]),
            Some(vec!["thunar".into()])
        );
        assert_eq!(
            launch_argv(&m, &appx("Foot", "foot --server %i"), &[]),
            Some(vec![
                "foot".into(),
                "--server".into(),
                "--icon".into(),
                "x".into()
            ]),
            "icon expansion and real flags both survive"
        );
    }

    #[test]
    fn non_file_exec_codes_follow_the_desktop_entry_contract() {
        let m = machine("m", 0, vec![vec!["x"]]);
        let mut app = appx("Friendly Name", "program %% %c %k");
        app.desktop_file = "/usr/share/applications/friendly.desktop".into();
        assert_eq!(
            launch_argv(&m, &app, &[]),
            Some(vec![
                "program".into(),
                "%".into(),
                "Friendly Name".into(),
                "/usr/share/applications/friendly.desktop".into()
            ])
        );
    }

    /// A percent sign that is not a field code is an ordinary argument.
    #[test]
    fn a_bare_percent_is_not_a_field_code() {
        let m = machine("m", 0, vec![vec!["x"]]);
        assert_eq!(
            launch_argv(&m, &appx("odd", "prog 100%"), &[]),
            Some(vec!["prog".into(), "100%".into()])
        );
    }

    /// Splitting is shlex, so a quoted argument containing spaces stays ONE argument. Naive
    /// whitespace splitting turns one path into two broken ones.
    #[test]
    fn quoted_arguments_survive_splitting() {
        let m = machine("m", 0, vec![vec!["x"]]);
        assert_eq!(
            launch_argv(&m, &appx("q", "prog --path \"/a b/c\" --flag"), &[]),
            Some(vec![
                "prog".into(),
                "--path".into(),
                "/a b/c".into(),
                "--flag".into()
            ])
        );
    }

    /// A Terminal=true program is wrapped, and the ORDER is the part worth pinning: machine
    /// prefix, then terminal, then program. The other order opens a local window and tries to
    /// forward from inside it.
    #[test]
    fn a_terminal_program_is_wrapped_in_the_right_order() {
        let mut m = machine("remote", 0, vec![vec!["x"]]);
        m.launch = vec!["fwd@remote".to_string()];
        let mut app = appx("Helix", "helix %F");
        app.terminal = true;
        assert_eq!(
            launch_argv(&m, &app, &["foot".to_string(), "-e".to_string()]),
            Some(vec![
                "fwd@remote".into(),
                "foot".into(),
                "-e".into(),
                "helix".into()
            ])
        );
    }

    /// A graphical program must NOT be wrapped even when a terminal is configured.
    #[test]
    fn a_graphical_program_is_not_wrapped() {
        let m = machine("m", 0, vec![vec!["x"]]);
        assert_eq!(
            launch_argv(
                &m,
                &appx("Firefox", "firefox %u"),
                &["foot".to_string(), "-e".to_string()]
            ),
            Some(vec!["firefox".into()])
        );
    }

    /// The machine's prefix is what turns "run this" into "run this THERE".
    #[test]
    fn the_launch_prefix_leads() {
        let mut m = machine("remote", 0, vec![vec!["x"]]);
        m.launch = vec!["waypipe@remote".to_string()];
        assert_eq!(
            launch_argv(&m, &appx("Helix", "helix %F"), &[]),
            Some(vec!["waypipe@remote".into(), "helix".into()])
        );
    }

    /// A category is one contiguous visual block and its heading is rendered once. Even
    /// overwhelming usage in one Code subrow must not pull it away from the other Code subrows or
    /// move the block past AI.
    #[test]
    fn rebuilding_keeps_each_category_one_contiguous_block() {
        let declared: Vec<String> = [
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
        .into_iter()
        .map(str::to_string)
        .collect();
        let cells = [
            "Claude", "OpenCode", "", "Foot", "Zed", "CMake", "Sysprof", "", "",
        ]
        .into_iter()
        .map(|name| {
            if name.is_empty() {
                vec![]
            } else {
                vec![line(vec![(*app(name)).clone()])]
            }
        })
        .collect();
        let mut s = state(vec![Machine {
            name: "m".into(),
            aliases: vec![],
            accent: "#fff".into(),
            launch: vec!["{}".into()],
            error: None,
            cells,
        }]);
        s.folders = declared.clone();
        for _ in 0..80 {
            s.record_launch_for_test("m", "Foot");
        }
        s.rebuild();
        s.rebuild();
        s.rebuild();
        assert_eq!(s.folders, declared);
        let visible_categories: Vec<&str> = s
            .folders
            .iter()
            .enumerate()
            .filter(|(r, _)| s.row_has_content(*r))
            .map(|(_, row)| row.split_once('/').map_or(row.as_str(), |(f, _)| f))
            .collect();
        assert_eq!(
            visible_categories,
            ["AI", "AI", "Code", "Code", "Code", "Code"],
            "a category may appear in one contiguous block only"
        );
        assert_eq!(s.machines[0].cells[3][0].apps[0].name, "Foot");
    }

    /// An empty machine list is a shape the config module can legitimately render -- every machine
    /// toggled off -- and must open empty rather than panic on the first cursor read.
    #[test]
    fn no_machines_does_not_panic() {
        let mut s = state(vec![]);
        s.rebuild();
        assert!(s.cell().is_empty());
        s.clamp();
    }

    /// An app named in the placement but absent from today's inventory must survive a drag
    /// elsewhere -- otherwise uninstalling something, or one partial inventory, silently discards
    /// the arrangement of everything that was missing at that moment.
    #[test]
    fn materialise_keeps_entries_for_apps_it_cannot_see() {
        let mut s = state(vec![machine("m", 0, vec![vec!["here"]])]);
        let mut folders = HashMap::new();
        folders.insert(
            "Media".to_string(),
            vec![StoredLine::Bare(vec!["gone".into()])],
        );
        s.placement.insert("m".to_string(), folders);
        s.rebuild();

        s.place_app(0, "here", 1, None, None);

        let after = &s.placement["m"];
        assert!(
            after.contains_key("Media"),
            "the absent app's folder survived: {after:?}"
        );
        assert_eq!(after["Media"], vec![StoredLine::Bare(vec!["gone".into()])]);
    }

    /// Dragging rightward within a line must land where the pointer was, not one place further.
    ///
    /// This used to need explicit correction: with a numeric position, removing the dragged item
    /// first shifted every later gap down one, so a rightward drag overshot its neighbour. Naming
    /// the neighbour instead makes the arithmetic disappear rather than get fixed -- "before c"
    /// means the same thing whether or not anything left the line in between.
    #[test]
    fn rightward_reorder_lands_where_it_was_dropped() {
        let mut s = state(vec![machine("m", 0, vec![vec!["a", "b", "c", "d"]])]);
        s.place_app(0, "a", 0, Some(&on(&["a", "b", "c", "d"])), Some("c")); // the gap after b
        let names: Vec<&str> = s.machines[0].cells[0][0]
            .apps
            .iter()
            .map(|x| x.name.as_str())
            .collect();
        assert_eq!(
            names,
            vec!["b", "a", "c", "d"],
            "landed after b, not after c"
        );
    }

    /// A drop while a QUERY is active must join the appset the user is looking at.
    ///
    /// The rendered grid is filtered, so a line's position on screen is not its position in the
    /// placement: here the only visible line in the folder is the placement's SECOND one, because
    /// the first matched nothing. Addressing it by rendered index joined the wrong appset -- and
    /// silently, since the line it actually joined was the one hidden by the filter.
    #[test]
    fn a_drop_under_a_filter_joins_the_line_that_is_visible() {
        let mut m = machine("m", 0, vec![vec!["alpha", "beta"], vec!["gamma"]]);
        m.cells[6] = vec![Line {
            name: None,
            apps: vec![app("delta")],
        }];
        let mut s = state(vec![m]);

        // Only the gamma line survives this: on screen it is line 0, in the placement it is line 1.
        s.query = "gamma".into();
        s.refilter();
        assert_eq!(
            s.machines[0].cells[0].len(),
            2,
            "the model still holds both lines"
        );

        s.place_app(0, "delta", 0, Some(&on(&["gamma"])), None);

        let lines = &s.placement["m"]["Terminals"];
        let with_gamma = lines
            .iter()
            .find(|l| l.apps().contains(&"gamma".to_string()))
            .expect("the gamma line is still there");
        assert!(
            with_gamma.apps().contains(&"delta".to_string()),
            "joined the visible appset: {lines:?}"
        );
        let with_alpha = lines
            .iter()
            .find(|l| l.apps().contains(&"alpha".to_string()))
            .unwrap();
        assert!(
            !with_alpha.apps().contains(&"delta".to_string()),
            "did NOT join the filtered-away one: {lines:?}"
        );
    }

    /// Frecency must never be written into the arrangement.
    ///
    /// `rebuild` orders the rendered grid by usage; `materialise` freezes the grid into
    /// placement.json before a move. Snapshotting the RENDERED grid therefore recorded the
    /// statistics' order as though the user had chosen it -- and once written it is
    /// indistinguishable from a real arrangement, so it stops decaying and never moves again.
    #[test]
    fn a_drag_does_not_fossilise_the_frecency_order() {
        let mut m = machine("m", 0, vec![vec!["rare"], vec!["popular"]]);
        m.cells[6] = vec![Line {
            name: None,
            apps: vec![app("unrelated")],
        }];
        let mut s = state(vec![m]);

        // Enough launches that the significance gate is cleared and the order really does move.
        for _ in 0..60 {
            s.record_launch_for_test("m", "popular");
        }
        s.rebuild();
        assert_eq!(
            s.machines[0].cells[0][0].apps[0].name, "popular",
            "precondition: usage put the popular line first ON SCREEN"
        );

        // A drag ANYWHERE on this machine materialises the whole of it.
        s.place_app(0, "unrelated", 1, None, None);

        assert_eq!(
            s.placement["m"]["Terminals"],
            vec![
                StoredLine::Bare(vec!["rare".into()]),
                StoredLine::Bare(vec!["popular".into()])
            ],
            "the FILE kept the arrangement, not the ranking"
        );
    }

    /// Typing puts the cursor on the first match, not on whatever the goal column happens to hit.
    ///
    /// The goal column is what makes up/down preserve your position, and it is only meaningful
    /// while a line's membership holds still. A query rewrites that membership, so a surviving
    /// goal selects the Nth survivor -- which is an arbitrary app, while the one the user is
    /// looking at as they type is the first.
    #[test]
    fn a_query_puts_the_cursor_on_the_first_match() {
        let mut s = state(vec![machine(
            "m",
            0,
            vec![vec!["alpha", "beta", "gamma", "delta"]],
        )]);
        s.focus = Focus::Inside;
        s.item = 3;
        s.item_goal = 3;

        // Every name contains an "a", so nothing is filtered out and only the cursor can move.
        s.set_query("a".into());
        s.clamp();

        assert_eq!(s.item, 0, "landed on the first match");
        assert_eq!(s.item_goal, 0, "and the stale goal column went with it");
        assert_eq!(s.view[0].cells[0][0].apps[s.item].name, "alpha");
    }

    /// ...but a REBUILD must not move the cursor, which is why the reset lives in `set_query` and
    /// not in `refilter`. A drag re-derives the whole grid, and a cursor that jumped to the top
    /// afterwards would leave the app the user just dropped.
    #[test]
    fn a_rebuild_leaves_the_cursor_where_it_was() {
        let mut s = state(vec![machine("m", 0, vec![vec!["a", "b", "c", "d"]])]);
        s.focus = Focus::Inside;
        s.item = 2;
        s.item_goal = 2;

        s.rebuild();
        s.clamp();

        assert_eq!(s.item, 2, "a rebuild is not a query");
        assert_eq!(s.item_goal, 2);
    }

    /// An app with an id distinct from its name, for the identity tests below.
    fn app_id(id: &str, name: &str) -> Arc<App> {
        Arc::new(App {
            id: id.into(),
            name: name.into(),
            icon: "x".into(),
            exec: id.into(),
            terminal: false,
            desktop_file: String::new(),
        })
    }

    /// TWO APPS, ONE DISPLAY NAME. This is the whole reason ids exist.
    ///
    /// `org.kde.foo.desktop` and `foo.desktop` both calling themselves "Foo" is ordinary. Keyed on
    /// the name they were one object: filing one filed both, and whichever the lookup did not
    /// return became unreachable.
    #[test]
    fn two_apps_sharing_a_name_stay_distinct() {
        let mut m = machine("m", 0, vec![]);
        m.cells[0] = vec![Line {
            name: None,
            apps: vec![app_id("a.desktop", "Foo"), app_id("b.desktop", "Foo")],
        }];
        let mut s = state(vec![m]);

        // File ONE of them somewhere else, by its id.
        s.place_app(0, "b.desktop", 4, None, None);

        let terminals: Vec<&str> = s.machines[0].cells[0]
            .iter()
            .flat_map(|l| l.apps.iter())
            .map(|a| a.id.as_str())
            .collect();
        let files: Vec<&str> = s.machines[0].cells[4]
            .iter()
            .flat_map(|l| l.apps.iter())
            .map(|a| a.id.as_str())
            .collect();
        assert_eq!(terminals, vec!["a.desktop"], "the other one stayed put");
        assert_eq!(files, vec!["b.desktop"], "and only the named one moved");
    }

    /// An arrangement written before ids existed must survive, or the change is a silent delete.
    #[test]
    fn an_arrangement_written_under_names_is_migrated() {
        let mut m = machine("m", 0, vec![]);
        m.cells[0] = vec![Line {
            name: None,
            apps: vec![app_id("helix.desktop", "Helix")],
        }];
        let mut s = state(vec![m]);

        // What the old code would have written: the DISPLAY NAME, in a different folder.
        let mut folders = HashMap::new();
        folders.insert(
            "Editors".to_string(),
            vec![StoredLine::Bare(vec!["Helix".into()])],
        );
        s.placement.insert("m".to_string(), folders);
        s.usage.insert(
            usage::key("m", "Helix"),
            usage::Entry {
                score: 9.0,
                last: 0,
            },
        );

        s.migrate_names_to_ids();
        s.rebuild();

        assert_eq!(
            s.placement["m"]["Editors"],
            vec![StoredLine::Bare(vec!["helix.desktop".into()])]
        );
        assert!(
            s.usage.contains_key(&usage::key("m", "helix.desktop")),
            "the score came too"
        );
        assert!(
            !s.usage.contains_key(&usage::key("m", "Helix")),
            "and did not stay behind"
        );
        assert_eq!(
            s.machines[0].cells[1][0].apps[0].id, "helix.desktop",
            "and the arrangement still applies: it is in Editors, not its computed folder"
        );
    }

    /// Running it twice must change nothing the second time, or a later start could rewrite an id
    /// that happens to collide with some other app's display name.
    #[test]
    fn the_migration_is_idempotent() {
        let mut m = machine("m", 0, vec![]);
        m.cells[0] = vec![Line {
            name: None,
            apps: vec![app_id("helix.desktop", "Helix")],
        }];
        let mut s = state(vec![m]);
        let mut folders = HashMap::new();
        folders.insert(
            "Editors".to_string(),
            vec![StoredLine::Bare(vec!["Helix".into()])],
        );
        s.placement.insert("m".to_string(), folders);

        s.migrate_names_to_ids();
        let once = s.placement.clone();
        s.migrate_names_to_ids();
        assert_eq!(s.placement, once);
    }

    /// An AMBIGUOUS old entry is left alone rather than guessed.
    ///
    /// If two apps share the display name an old entry records, nothing in the file says which was
    /// meant. Picking one hands the other's arrangement to the wrong app, silently and with no way
    /// to notice; leaving it drops both back to their computed folders, which is visible and one
    /// drag from fixed.
    #[test]
    fn an_ambiguous_name_is_not_guessed() {
        let mut m = machine("m", 0, vec![]);
        m.cells[0] = vec![Line {
            name: None,
            apps: vec![app_id("a.desktop", "Foo"), app_id("b.desktop", "Foo")],
        }];
        let mut s = state(vec![m]);
        let mut folders = HashMap::new();
        folders.insert(
            "Editors".to_string(),
            vec![StoredLine::Bare(vec!["Foo".into()])],
        );
        s.placement.insert("m".to_string(), folders);

        s.migrate_names_to_ids();

        assert_eq!(
            s.placement["m"]["Editors"],
            vec![StoredLine::Bare(vec!["Foo".into()])],
            "untouched -- an unresolvable entry is left, not resolved to a coin flip"
        );
    }

    fn hosts() -> Vec<(String, Vec<String>)> {
        vec![
            ("laptop".into(), vec![]),
            ("workstation".into(), vec![]),
            ("server".into(), vec![]),
        ]
    }

    fn hosts_with_aliases() -> Vec<(String, Vec<String>)> {
        vec![
            ("laptop".into(), vec!["lap".into()]),
            ("workstation".into(), vec!["work".into()]),
            ("server".into(), vec!["srv".into()]),
        ]
    }

    /// Every spelling a person might already have the habit of, meaning the same thing.
    #[test]
    fn a_machine_can_be_named_in_any_of_the_usual_shapes() {
        let h = hosts();
        for q in [
            "foot@server",
            "foot.server",
            "foot:server",
            "foot/server",
            "server.foot",
        ] {
            assert_eq!(
                split_query(q, &h),
                (Some("server".into()), "foot".into()),
                "{q}"
            );
        }
    }

    /// A prefix is enough, because a qualifier you must spell in full saves nothing over arrowing.
    #[test]
    fn an_unambiguous_prefix_is_enough() {
        assert_eq!(
            split_query("foot@ser", &hosts()),
            (Some("server".into()), "foot".into())
        );
        assert_eq!(
            split_query("foot@l", &hosts()),
            (Some("laptop".into()), "foot".into())
        );
    }

    /// A declared alias is just another name for the machine, in every spelling.
    #[test]
    fn an_alias_names_the_machine() {
        let h = hosts_with_aliases();
        for q in ["foot@srv", "foot.srv", "srv.foot", "foot@work", "foot@lap"] {
            let (m, pat) = split_query(q, &h);
            assert!(m.is_some(), "{q} should name a machine");
            assert_eq!(pat, "foot", "{q}");
        }
        assert_eq!(split_query("foot@work", &h).0, Some("workstation".into()));
        assert_eq!(split_query("foot@lap", &h).0, Some("laptop".into()));
        assert_eq!(split_query("foot@srv", &h).0, Some("server".into()));
    }

    /// AN ALIAS BEATS A PREFIX, which is the reason to declare one at all: without this, adding a
    /// machine whose name merely starts with somebody's shortcut would silently break it.
    #[test]
    fn a_declared_alias_survives_a_new_machine_that_collides() {
        let h: Vec<(String, Vec<String>)> = vec![
            ("workstation".into(), vec!["work".into()]),
            // Added later, and its NAME begins with the same letters as the alias above.
            ("workspace-box".into(), vec![]),
        ];
        assert_eq!(
            split_query("foot@work", &h).0,
            Some("workstation".into()),
            "the declared alias still wins"
        );
    }

    /// ...and an ambiguous one is not a licence to guess.
    #[test]
    fn an_ambiguous_prefix_names_no_machine() {
        let h: Vec<(String, Vec<String>)> =
            vec![("work".into(), vec![]), ("workstation".into(), vec![])];
        // "arc" could be either, so it stays ordinary search text rather than picking one.
        assert_eq!(split_query("foot@arc", &h), (None, "foot@arc".into()));
    }

    /// An exact name beats a prefix, or a machine could never be reached once another began with
    /// its whole name.
    #[test]
    fn an_exact_name_wins_over_a_longer_one() {
        let h: Vec<(String, Vec<String>)> =
            vec![("work".into(), vec![]), ("workstation".into(), vec![])];
        assert_eq!(
            split_query("foot@work", &h),
            (Some("work".into()), "foot".into())
        );
    }

    /// Naming a machine with no pattern means everything on it -- falling out of the same rule
    /// rather than being a special case.
    #[test]
    fn a_bare_machine_shows_all_of_it() {
        assert_eq!(
            split_query("server.", &hosts()),
            (Some("server".into()), String::new())
        );
        assert_eq!(
            split_query("@workstation", &hosts()),
            (Some("workstation".into()), String::new())
        );
    }

    /// THE SAFETY PROPERTY: a query that names no machine is passed through untouched, so this can
    /// never make an ordinary search stop working -- including one containing a separator.
    #[test]
    fn a_query_naming_no_machine_is_left_alone() {
        assert_eq!(split_query("foot", &hosts()), (None, "foot".into()));
        assert_eq!(split_query("some.app", &hosts()), (None, "some.app".into()));
        assert_eq!(
            split_query("user@host", &hosts()),
            (None, "user@host".into())
        );
    }

    /// And the whole point: the other machines empty, without their columns disappearing.
    #[test]
    fn a_qualified_query_empties_the_other_machines() {
        let mut s = state(vec![
            machine("laptop", 0, vec![vec!["Foot"]]),
            machine("server", 0, vec![vec!["Foot"]]),
        ]);
        s.set_query("foot@server".into());

        assert_eq!(s.view.len(), 2, "both columns still exist");
        assert!(s.view[0].cells[0].is_empty(), "the unnamed machine emptied");
        assert_eq!(
            s.view[1].cells[0][0].apps[0].name, "Foot",
            "the named machine kept its match"
        );
    }

    /// A PLACEMENT FILE WRITTEN BEFORE NAMED ROWS EXISTED STILL LOADS.
    ///
    /// This is the whole reason the stored form is an untagged enum. A schema change that cannot
    /// read what is already on disk does not fail loudly -- the file stops parsing, the grid falls
    /// back to the computed grouping, and the user's arrangement is gone with no error at all.
    #[test]
    fn the_old_placement_shape_still_parses() {
        let old = r#"{"m":{"Editors":[["helix.desktop","vim.desktop"],["zed.desktop"]]}}"#;
        let p: Placement = serde_json::from_str(old).expect("the old shape must still parse");
        let lines = &p["m"]["Editors"];
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0].apps(),
            &vec!["helix.desktop".to_string(), "vim.desktop".to_string()]
        );
        assert_eq!(
            lines[0].name(),
            None,
            "an old line has no name, and that is not an error"
        );
    }

    /// And the new shape round-trips, with unnamed lines still written in the old form so a file
    /// nobody has named anything in stays readable by an older build.
    #[test]
    fn named_and_unnamed_lines_round_trip() {
        let mut folders = HashMap::new();
        folders.insert(
            "Chat".to_string(),
            vec![
                StoredLine::new(Some("business".into()), vec!["teams.desktop".into()]),
                StoredLine::new(None, vec!["signal.desktop".into()]),
            ],
        );
        let mut p = Placement::new();
        p.insert("m".to_string(), folders);

        let text = serde_json::to_string(&p).unwrap();
        assert!(text.contains("business"));
        let back: Placement = serde_json::from_str(&text).unwrap();
        assert_eq!(back, p);
        assert_eq!(back["m"]["Chat"][1].name(), None);
    }

    #[test]
    fn hiding_is_per_machine_and_keeps_the_inventory_pristine() {
        let mut s = state(vec![
            machine("laptop", 0, vec![vec!["shared", "local"]]),
            machine("workstation", 0, vec![vec!["shared", "remote"]]),
        ]);

        assert!(s.hide_app("laptop", "shared"));

        let visible = |machine: &Machine| {
            machine
                .cells
                .iter()
                .flatten()
                .flat_map(|line| line.apps.iter())
                .map(|app| app.id.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(visible(&s.view[0]), vec!["local"]);
        assert_eq!(visible(&s.view[1]), vec!["shared", "remote"]);
        assert_eq!(
            visible(&s.base[0]),
            vec!["shared", "local"],
            "visibility is a view over inventory, never an inventory mutation"
        );
        assert_eq!(s.hidden_count(), 1);
    }

    #[test]
    fn resetting_visibility_restores_the_exact_saved_placement() {
        let mut s = state(vec![machine("m", 0, vec![vec!["alpha", "beta"]])]);
        s.place_app(0, "alpha", 1, None, None);
        let placement = s.placement.clone();

        assert!(s.hide_app("m", "alpha"));
        assert_eq!(s.placement, placement, "hiding did not rewrite the shelf");
        assert!(s.view[0].cells[1].is_empty(), "alpha is invisible");

        assert!(s.reset_visibility());
        assert_eq!(s.hidden_count(), 0);
        assert_eq!(s.placement, placement, "reset did not rewrite the shelf");
        assert_eq!(s.view[0].cells[1][0].apps[0].id, "alpha");
    }

    #[test]
    fn hiding_rebalances_the_current_vector() {
        let mut s = state(vec![machine(
            "m",
            0,
            vec![vec!["a", "b", "c", "d", "e", "f", "g"]],
        )]);

        assert!(s.hide_app("m", "b"));

        let lines = &s.view[0].cells[0];
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].apps.len(), 3);
        assert_eq!(lines[1].apps.len(), 3);
        assert_eq!(
            lines
                .iter()
                .flat_map(|line| line.apps.iter())
                .map(|app| app.id.as_str())
                .collect::<Vec<_>>(),
            ["a", "c", "d", "e", "f", "g"]
        );
    }

    /// A named row survives being emptied. It is part of the taxonomy, not a by-product of its
    /// contents -- otherwise dragging the last thing out would delete the row and you could never
    /// put anything back.
    #[test]
    fn an_emptied_named_row_is_not_deleted() {
        let mut m = machine("m", 0, vec![]);
        m.cells[0] = vec![Line {
            name: Some("business".into()),
            apps: vec![app("Teams")],
        }];
        let mut s = state(vec![m]);

        // Move the only occupant somewhere else entirely.
        s.place_app(0, "Teams", 4, None, None);

        let chat = &s.placement["m"]["Terminals"];
        assert_eq!(chat.len(), 1, "the row is still there: {chat:?}");
        assert_eq!(chat[0].name(), Some("business"));
        assert!(chat[0].apps().is_empty(), "and it is empty");
    }

    /// A row nobody has anything on is never shown and never landed on.
    ///
    /// It arises honestly rather than by accident: every folder emits a catch-all row beside its
    /// subcategories, so sorting every member into one leaves the catch-all empty everywhere.
    #[test]
    fn a_row_empty_on_every_machine_is_skipped() {
        let mut a = machine("a", 0, vec![vec!["one"]]);
        let mut b = machine("b", 0, vec![vec!["two"]]);
        // Row 2 has something; row 1 has nothing on either machine.
        a.cells[2] = vec![Line {
            name: None,
            apps: vec![app("three")],
        }];
        b.cells[2] = vec![Line {
            name: None,
            apps: vec![app("four")],
        }];
        let s = state(vec![a, b]);

        assert!(s.row_has_content(0));
        assert!(!s.row_has_content(1), "nobody has anything on row 1");
        assert!(s.row_has_content(2));

        // Moving down from row 0 lands on 2, never on 1.
        assert_eq!(s.next_row(0, 1), 2);
        assert_eq!(s.next_row(2, -1), 0);
    }

    /// And at the end there is nowhere to go, which must not wrap or run off.
    #[test]
    fn moving_past_the_last_populated_row_stays_put() {
        let s = state(vec![machine("a", 0, vec![vec!["one"]])]);
        assert_eq!(s.next_row(0, 1), 0, "no populated row below");
        assert_eq!(s.next_row(0, -1), 0, "and none above");
    }

    /// Dropping an item into the gap it already occupies is a no-op, not a shuffle.
    #[test]
    fn dropping_an_item_where_it_already_is_changes_nothing() {
        let mut s = state(vec![machine("m", 0, vec![vec!["a", "b", "c"]])]);
        s.place_app(0, "a", 0, Some(&on(&["a", "b", "c"])), Some("b")); // the gap a already occupies
        let names: Vec<&str> = s.machines[0].cells[0][0]
            .apps
            .iter()
            .map(|x| x.name.as_str())
            .collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    /// Field codes are dropped AFTER splitting, so a quoted argument keeps its internal spacing.
    #[test]
    fn stripping_does_not_reach_inside_a_quoted_argument() {
        let m = machine("m", 0, vec![vec!["x"]]);
        assert_eq!(
            launch_argv(&m, &appx("q", "prog \"/a  b\" %U"), &[]),
            Some(vec!["prog".into(), "/a  b".into()]),
            "two spaces preserved, %U gone"
        );
    }

    /// `{}` is substituted where it appears rather than reaching exec as a literal argument.
    #[test]
    fn the_launch_template_placeholder_is_substituted() {
        let mut m = machine("m", 0, vec![vec!["x"]]);
        m.launch = vec!["ssh".into(), "box".into(), "{}".into()];
        assert_eq!(
            launch_argv(&m, &appx("Helix", "helix %F"), &[]),
            Some(vec!["ssh".into(), "box".into(), "helix".into()])
        );
    }

    #[test]
    fn an_empty_launch_prefix_is_read_only_not_local() {
        let mut m = machine("remote", 0, vec![vec!["x"]]);
        m.launch.clear();
        assert_eq!(launch_argv(&m, &appx("Helix", "helix"), &[]), None);
    }

    #[test]
    fn terminal_wrapper_is_inside_a_placeholder_template() {
        let mut m = machine("remote", 0, vec![vec!["x"]]);
        m.launch = vec!["ssh".into(), "box".into(), "{}".into()];
        let mut app = appx("Helix", "helix %F");
        app.terminal = true;
        assert_eq!(
            launch_argv(&m, &app, &["foot".into(), "-e".into()]),
            Some(vec![
                "ssh".into(),
                "box".into(),
                "foot".into(),
                "-e".into(),
                "helix".into()
            ])
        );
    }

    /// clamp() is the only thing standing between a shrinking grid and an index panic.
    ///
    /// NOTE THE SHAPE OF THIS INVARIANT, which a first version of this test got wrong: resting on
    /// an EMPTY cell is legal and reachable on purpose. Empty cells are drawn rather than
    /// collapsed, because "that machine has no chat client" is an answer worth seeing, and you have to
    /// be able to arrow onto one to read it. So the guarantee is not "there is always an item" --
    /// it is that indices are in range whenever there IS something to point at, and that an empty
    /// cell cannot leave you inside it.
    #[test]
    fn clamp_never_points_out_of_range() {
        let mut s = state(vec![machine("m", 0, vec![vec!["a"]])]);
        s.col = 99;
        s.row = 99;
        s.line = 99;
        s.item = 99;
        s.item_goal = 99;
        s.clamp();
        assert!(s.col < s.view.len(), "column in range");
        assert!(s.row < s.folders.len(), "row in range");
        if s.cell().is_empty() {
            assert_eq!(
                s.focus,
                Focus::Outside,
                "an empty cell must not hold you inside it"
            );
        } else {
            assert!(
                s.cell()
                    .get(s.line)
                    .is_some_and(|l| l.apps.get(s.item).is_some())
            );
        }
    }

    /// The same guarantee where it actually bites: a NON-empty cell with wild indices.
    #[test]
    fn clamp_lands_on_a_real_item_when_one_exists() {
        let mut s = state(vec![machine("m", 0, vec![vec!["a", "b"]])]);
        s.row = 0;
        s.line = 99;
        s.item = 99;
        s.item_goal = 99;
        s.clamp();
        assert_eq!(
            s.cell()[s.line].apps[s.item].name,
            "b",
            "clamped to the last real item"
        );
    }

    #[test]
    fn persisted_state_is_private_and_atomic() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "cbar-launcher-state-test-{}-{}",
            std::process::id(),
            TEST_TMP.fetch_add(1, Ordering::Relaxed)
        ));
        let path = root.join("state.json");
        write_atomic(&path, &serde_json::json!({"safe": true}));
        let file_mode = std::fs::metadata(&path)
            .expect("state was written")
            .permissions()
            .mode()
            & 0o777;
        let dir_mode = std::fs::metadata(&root)
            .expect("state directory exists")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
        std::fs::remove_dir_all(&root).expect("remove isolated test directory");
    }

    #[test]
    fn state_reads_are_bounded_and_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "cbar-launcher-state-read-test-{}-{}",
            std::process::id(),
            TEST_TMP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).expect("create isolated state directory");
        let target = root.join("target.json");
        std::fs::write(&target, br#"{"safe":true}"#).expect("write isolated state target");
        let link = root.join("linked.json");
        symlink(&target, &link).expect("create isolated state symlink");
        assert!(
            read_state(&link).is_err(),
            "state reads must use O_NOFOLLOW"
        );

        let huge = root.join("huge.json");
        std::fs::File::create(&huge)
            .expect("create sparse oversized state")
            .set_len(MAX_STATE_BYTES + 1)
            .expect("size sparse oversized state");
        assert!(
            read_state(&huge).is_err(),
            "oversized state must be rejected"
        );

        std::fs::remove_dir_all(root).expect("remove isolated state directory");
    }

    #[test]
    fn state_reads_require_private_roots_and_files() {
        let root = std::env::temp_dir().join(format!(
            "cbar-launcher-state-mode-test-{}-{}",
            std::process::id(),
            TEST_TMP.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        let path = root.join("state.json");
        std::fs::write(&path, br#"{"safe":true}"#).unwrap();

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(read_state(&path).is_err());

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read_state(&path).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn atomic_state_write_refuses_a_symlinked_parent() {
        use std::os::unix::fs::symlink;

        let root = std::env::temp_dir().join(format!(
            "cbar-launcher-state-link-test-{}-{}",
            std::process::id(),
            TEST_TMP.fetch_add(1, Ordering::Relaxed)
        ));
        let target = root.join("target");
        let linked = root.join("state");
        std::fs::create_dir_all(&target).expect("create isolated state target");
        symlink(&target, &linked).expect("create isolated state symlink");

        write_atomic(
            &linked.join("state.json"),
            &serde_json::json!({"safe": true}),
        );
        assert!(
            !target.join("state.json").exists(),
            "a symlinked state root must not redirect writes"
        );

        std::fs::remove_file(linked).expect("remove isolated state symlink");
        std::fs::remove_dir_all(root).expect("remove isolated state root");
    }

    #[test]
    fn async_state_writer_serializes_coalesces_and_reports_latest_result() {
        use std::sync::Barrier;

        let path = std::env::temp_dir().join(format!(
            "cbar-launcher-state-writer-test-{}-{}",
            std::process::id(),
            TEST_TMP.fetch_add(1, Ordering::Relaxed)
        ));
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let writes = Arc::new(Mutex::new(Vec::new()));
        enqueue_write(
            path.clone(),
            Box::new({
                let entered = entered.clone();
                let release = release.clone();
                let writes = writes.clone();
                move |_| {
                    entered.wait();
                    release.wait();
                    writes.lock().unwrap().push(1);
                    Ok(())
                }
            }),
        );
        entered.wait();
        enqueue_write(
            path.clone(),
            Box::new({
                let writes = writes.clone();
                move |_| {
                    writes.lock().unwrap().push(2);
                    Ok(())
                }
            }),
        );
        enqueue_write(
            path.clone(),
            Box::new({
                let writes = writes.clone();
                move |_| {
                    writes.lock().unwrap().push(3);
                    Err("newest write failed".into())
                }
            }),
        );
        release.wait();
        assert!(wait_for_state_writes(std::time::Duration::from_secs(2)));
        assert_eq!(*writes.lock().unwrap(), [1, 3]);
        assert_eq!(
            pending_write_error(&path).as_deref(),
            Some("newest write failed")
        );

        enqueue_write(path.clone(), Box::new(|_| Ok(())));
        assert!(wait_for_state_writes(std::time::Duration::from_secs(2)));
        assert!(pending_write_error(&path).is_none());
    }

    #[test]
    fn writer_spawn_failure_settles_the_generation_and_is_readable() {
        let path = std::env::temp_dir().join(format!(
            "cbar-launcher-state-spawn-failure-test-{}-{}",
            std::process::id(),
            TEST_TMP.fetch_add(1, Ordering::Relaxed)
        ));
        let writes = write_registry();
        {
            let mut pending = writes.pending.lock().unwrap();
            pending.insert(
                path.clone(),
                PendingWrite {
                    latest: Some((17, Box::new(|_| Ok(())))),
                    active: true,
                    submitted: 17,
                    completed: 0,
                    last_error: None,
                },
            );
        }

        mark_writer_spawn_failed(writes, &path);

        {
            let pending = writes.pending.lock().unwrap();
            let state = pending.get(&path).unwrap();
            assert!(!state.active);
            assert!(state.latest.is_none());
            assert_eq!(state.completed, state.submitted);
            assert_eq!(
                state.last_error.as_deref(),
                Some("unable to start launcher state writer")
            );
        }
        assert_eq!(
            read_state(&path).unwrap_err(),
            "unable to start launcher state writer"
        );
        writes.pending.lock().unwrap().remove(&path);
    }
}
