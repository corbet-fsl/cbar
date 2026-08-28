// icons.rs — decode each icon once in the life of the machine, not once per launch.
//
// WHY THIS EXISTS. Measured headlessly on a 191-application inventory, with the inventory served
// from a file so no SSH is in the path:
//
//   191 apps, icons blanked    0.095s   <- exec, dynamic link, GTK init, window, grid
//   191 apps, with icons       0.170s   <- the same, plus icons
//
// Icons are three quarters of everything above the floor, and the grid itself is about ten
// milliseconds of it. That cost is paid on EVERY launch to produce a result that is identical
// every time: the same 95 distinct names resolve to the same files and rasterise to the same
// twenty-pixel images until somebody installs software.
//
// So it is paid once and written down. What lands here is the finished pixels -- not the PNG, not
// the SVG, not a path to either -- so a warm launch does no theme lookup, opens no image file,
// runs no rasteriser, and never loads librsvg at all. 95 icons at 20px is 152kB on disk.
//
// ── WHY A WHOLE-CACHE STAMP RATHER THAN PER-ENTRY VALIDATION ────────────────────────────────
//
// Checking each icon against its source file would mean asking the icon theme where every icon
// lives, and that lookup is a large part of the cost this exists to avoid -- a cache that has to
// do the expensive thing to find out whether it is valid saves nothing. Instead the whole file
// carries one stamp taken from the icon THEME's own index files. Installing or removing software
// rewrites those indexes, so the stamp moves and the cache is discarded wholesale. Between
// installs it is trusted completely.
//
// The failure mode this chooses is the right way round: a stale cache shows a stale icon until the
// next package operation, while an over-eager check costs latency on every launch forever.
//
// ONE CLASS OF ENTRY IS EXEMPT, and stating it is the point of this paragraph. An `Icon=` may name
// an absolute FILE rather than a theme icon, and such a file is reachable by no icon theme index --
// so the theme stamp cannot see it change, and the argument above simply does not hold for it.
// Those entries carry the opened target descriptor's device, inode, size, mtime and ctime in their
// key instead. Validation and any decode run in the bounded source lane, so they are self-validating
// without putting filesystem work back onto GTK.
use gtk4::prelude::*;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Bumped whenever the on-disk layout changes, so an old file is discarded rather than
/// misread. A cache that cannot be parsed is simply absent -- never a reason to fail to start.
const MAGIC: &[u8; 6] = b"CBLI05";
const MAX_ICON_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESIDENT_ICON_BYTES: usize = 128 * 1024 * 1024;
const MAX_ICON_CACHE_ENTRIES: usize = 32_768;
const MAX_ICON_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ICON_THEME_ROOTS: usize = 256;
const MAX_ICON_THEME_DIRECTORIES: usize = 4_096;
const MAX_PENDING_ICON_REQUESTS: usize = 512;
const MAX_ICON_WAITERS: usize = 512;
const ICON_WORK_QUEUE_CAPACITY: usize = 256;
const MAX_ICON_WORKERS: usize = 4;
const ICON_RESULT_POLL: Duration = Duration::from_millis(16);
const ICON_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Separates an absolute icon path from the descriptor identity that validates it. Splitting from
/// the right keeps even an unusual Unix path containing this byte representable.
const STAMP_SEPARATOR: char = '\u{1}';

static NEXT_ICON_GENERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
enum IconSource {
    Absolute(PathBuf),
    Themed(PathBuf),
}

impl IconSource {
    const fn is_absolute(&self) -> bool {
        matches!(self, Self::Absolute(_))
    }

    fn path(&self) -> &std::path::Path {
        match self {
            Self::Absolute(path) | Self::Themed(path) => path,
        }
    }
}

#[derive(Debug)]
struct IconRequest {
    generation: u64,
    request_id: u64,
    stamp: u64,
    px: i32,
    name: String,
    source: IconSource,
    cached_key: Option<String>,
    result: SyncSender<IconResult>,
}

#[derive(Debug)]
struct IconResult {
    generation: u64,
    request_id: u64,
    stamp: u64,
    px: i32,
    name: String,
    absolute: bool,
    outcome: LoadOutcome,
}

#[derive(Debug)]
enum LoadOutcome {
    Cached { key: String },
    Decoded { key: String, pixels: Arc<[u8]> },
    Missing,
}

struct PendingIcon {
    request_id: u64,
    request: Option<IconRequest>,
    waiters: Vec<gtk4::glib::WeakRef<gtk4::Image>>,
    started: Instant,
    absolute: bool,
}

struct IconWorkerPool {
    requests: SyncSender<IconRequest>,
}

pub struct Icons {
    generation: u64,
    next_request_id: u64,
    px: i32,
    stamp: u64,
    /// WHERE this cache lives, carried rather than recomputed. Tests need their own file, and the
    /// obvious way to give them one -- pointing XDG_CACHE_HOME somewhere else -- is a race: tests
    /// share a process, so one setting the variable changes it under every other running thread.
    /// That is the same trap the state writers are guarded against, and writing it here anyway is
    /// how this was found. An injected path cannot race.
    path: Option<PathBuf>,
    /// name -> RGBA pixels at `px` square. The persisted form.
    pixels: HashMap<String, Arc<[u8]>>,
    /// name -> the texture handed to GTK, built on first use and kept for the process's life.
    /// Separate from `pixels` because a texture cannot be written to a file and pixels cannot be
    /// drawn; keeping both costs one copy of 1.6kB per icon and saves rebuilding either.
    textures: HashMap<String, Option<gtk4::gdk::Texture>>,
    /// The latest persisted descriptor-versioned key for each absolute path. It is only a worker
    /// candidate: GTK never trusts it until the source descriptor has been opened and validated.
    absolute_candidates: HashMap<String, String>,
    /// Descriptor versions validated during this process and therefore safe for immediate redraws.
    absolute_current: HashMap<String, String>,
    pending: HashMap<String, PendingIcon>,
    failed: HashSet<String>,
    waiter_count: usize,
    pump_active: bool,
    result_tx: SyncSender<IconResult>,
    result_rx: Receiver<IconResult>,
    /// Set when a name was decoded that the file did not have, i.e. the file is worth rewriting.
    dirty: bool,
}

fn next_icon_generation() -> u64 {
    let generation = NEXT_ICON_GENERATION.fetch_add(1, Ordering::Relaxed);
    if generation == 0 {
        NEXT_ICON_GENERATION.fetch_add(1, Ordering::Relaxed)
    } else {
        generation
    }
}

fn absolute_cache_key(name: &str, metadata: &std::fs::Metadata) -> String {
    format!(
        "{name}{STAMP_SEPARATOR}{}:{}:{}:{}:{}:{}:{}",
        metadata.dev(),
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

fn absolute_name_from_key(key: &str) -> Option<&str> {
    let (name, identity) = key.rsplit_once(STAMP_SEPARATOR)?;
    if !std::path::Path::new(name).is_absolute()
        || identity.split(':').count() != 7
        || identity
            .split(':')
            .any(|field| field.parse::<i128>().is_err())
    {
        return None;
    }
    Some(name)
}

#[cfg(test)]
thread_local! {
    static SOURCE_OPEN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static SOURCE_DECODE_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn open_icon_source(path: &std::path::Path) -> Option<(std::fs::File, std::fs::Metadata)> {
    #[cfg(test)]
    SOURCE_OPEN_COUNT.with(|count| count.set(count.get() + 1));

    // Source paths deliberately follow ordinary symlinks. The descriptor, rather than a path
    // metadata check followed by a second open, is the authority from here onward: special files
    // and oversized inputs are rejected with fstat and the exact same descriptor is read below.
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ICON_SOURCE_BYTES {
        return None;
    }
    Some((file, metadata))
}

fn decode_opened_source(
    file: std::fs::File,
    metadata: &std::fs::Metadata,
    px: i32,
    absolute: bool,
) -> Option<Arc<[u8]>> {
    let mut encoded = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_ICON_SOURCE_BYTES + 1)
        .read_to_end(&mut encoded)
        .ok()?;
    if encoded.len() as u64 > MAX_ICON_SOURCE_BYTES {
        return None;
    }

    #[cfg(test)]
    SOURCE_DECODE_COUNT.with(|count| count.set(count.get() + 1));

    let pixbuf = if absolute {
        // Preserve the established absolute-file path exactly: its loader has always been given a
        // target size before bytes, so raster and vector inputs retain the same final geometry.
        let loader = gtk4::gdk_pixbuf::PixbufLoader::new();
        loader.set_size(px, px);
        loader.write(&encoded).ok()?;
        loader.close().ok()?;
        loader.pixbuf()?
    } else {
        // `from_file_at_size` (the previous theme path) preserves aspect ratio. Decode the bytes
        // already read from the validated descriptor through the equivalent stream API.
        let bytes = gtk4::glib::Bytes::from_owned(encoded);
        let stream = gtk4::gio::MemoryInputStream::from_bytes(&bytes);
        gtk4::gdk_pixbuf::Pixbuf::from_stream_at_scale(
            &stream,
            px,
            px,
            true,
            None::<&gtk4::gio::Cancellable>,
        )
        .ok()?
    };
    rgba_square(&pixbuf, px).map(Arc::from)
}

fn load_icon(request: &IconRequest) -> LoadOutcome {
    let Some((file, metadata)) = open_icon_source(request.source.path()) else {
        return LoadOutcome::Missing;
    };
    let key = if request.source.is_absolute() {
        absolute_cache_key(&request.name, &metadata)
    } else {
        request.name.clone()
    };
    if request.cached_key.as_deref() == Some(&key) {
        return LoadOutcome::Cached { key };
    }
    match decode_opened_source(file, &metadata, request.px, request.source.is_absolute()) {
        Some(pixels) => LoadOutcome::Decoded { key, pixels },
        None => LoadOutcome::Missing,
    }
}

fn run_icon_request(request: IconRequest) {
    let outcome = load_icon(&request);
    let result = IconResult {
        generation: request.generation,
        request_id: request.request_id,
        stamp: request.stamp,
        px: request.px,
        name: request.name,
        absolute: request.source.is_absolute(),
        outcome,
    };
    let _ = request.result.send(result);
}

fn icon_worker_count(parallelism: Option<usize>) -> usize {
    parallelism
        .unwrap_or(1)
        .max(1)
        .div_ceil(2)
        .clamp(1, MAX_ICON_WORKERS)
}

impl IconWorkerPool {
    fn start() -> Option<Self> {
        let (requests, receiver) = sync_channel(ICON_WORK_QUEUE_CAPACITY);
        let receiver = Arc::new(Mutex::new(receiver));
        let workers = icon_worker_count(
            std::thread::available_parallelism()
                .ok()
                .map(std::num::NonZeroUsize::get),
        );
        let mut started = 0usize;
        for index in 0..workers {
            let receiver = receiver.clone();
            if std::thread::Builder::new()
                .name(format!("cbar-icon-source-{index}"))
                .spawn(move || {
                    loop {
                        let request = {
                            let Ok(receiver) = receiver.lock() else {
                                return;
                            };
                            receiver.recv()
                        };
                        let Ok(request) = request else {
                            return;
                        };
                        run_icon_request(request);
                    }
                })
                .is_ok()
            {
                started += 1;
            }
        }
        (started > 0).then_some(Self { requests })
    }
}

fn icon_worker_pool() -> Option<&'static IconWorkerPool> {
    static WORKERS: OnceLock<Option<IconWorkerPool>> = OnceLock::new();
    WORKERS.get_or_init(IconWorkerPool::start).as_ref()
}

fn cache_path(px: i32) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })?;
    Some(
        base.join("cbar")
            .join("launcher")
            .join(format!("icons-{px}.bin")),
    )
}

/// One number for "has the set of installed icons changed".
///
/// The newest mtime across the icon themes' own index files. Package operations rewrite those --
/// it is what `gtk-update-icon-cache` exists to do -- so this moves exactly when the answer to an
/// icon lookup could have changed, and stays still the rest of the time.
#[derive(Clone, Debug)]
pub(crate) struct ThemeSnapshot {
    name: String,
    roots: Vec<PathBuf>,
}

pub(crate) fn capture_theme(theme: &gtk4::IconTheme) -> Option<ThemeSnapshot> {
    let mut roots = theme.search_path();
    roots.sort();
    (roots.len() <= MAX_ICON_THEME_ROOTS).then(|| ThemeSnapshot {
        name: theme.theme_name().to_string(),
        roots,
    })
}

fn theme_stamp(theme: &ThemeSnapshot) -> Option<u64> {
    let mut stamp = std::collections::hash_map::DefaultHasher::new();
    // Switching the active theme changes every lookup without touching any theme index. The old
    // stamp missed that completely and served the previous theme's finished pixels forever.
    theme.name.hash(&mut stamp);
    let mut visited_directories = 0usize;
    for root in &theme.roots {
        root.hash(&mut stamp);
        if let Ok(modified) = std::fs::metadata(root).and_then(|m| m.modified()) {
            modified.hash(&mut stamp);
        }
        let Ok(themes) = std::fs::read_dir(root) else {
            continue;
        };
        for theme in themes.flatten() {
            if visited_directories >= MAX_ICON_THEME_DIRECTORIES {
                // A partial stamp could trust stale pixels when an unvisited theme changes. A
                // pathological theme tree therefore disables persistence instead of weakening
                // invalidation correctness or monopolising GTK's event thread.
                return None;
            }
            visited_directories += 1;
            for f in ["icon-theme.cache", "index.theme"] {
                if let Ok(md) = std::fs::metadata(theme.path().join(f))
                    && let Ok(t) = md.modified()
                {
                    t.hash(&mut stamp);
                }
            }
        }
    }
    Some(stamp.finish())
}

pub(crate) struct PreparedIcons {
    px: i32,
    stamp: u64,
    path: Option<PathBuf>,
    pixels: HashMap<String, Arc<[u8]>>,
}

impl PreparedIcons {
    pub(crate) fn empty(path: Option<PathBuf>, px: i32, stamp: u64) -> Self {
        Self {
            px,
            stamp,
            path,
            pixels: HashMap::new(),
        }
    }

    pub(crate) fn identity(&self) -> (i32, u64) {
        (self.px, self.stamp)
    }

    pub(crate) fn prepare(px: i32, theme: ThemeSnapshot) -> Self {
        let Some(stamp) = theme_stamp(&theme) else {
            return Self::empty(None, px, 0);
        };
        match cache_path(px) {
            Some(path) => Self::load_from(path, px, stamp),
            None => Self::empty(None, px, stamp),
        }
    }

    fn load_from(path: PathBuf, px: i32, stamp: u64) -> Self {
        let mut me = Self::empty(Some(path.clone()), px, stamp);
        let Some(root) = path.parent() else {
            return me;
        };
        let Ok(root_metadata) = std::fs::symlink_metadata(root) else {
            return me;
        };
        if root_metadata.file_type().is_symlink()
            || !root_metadata.is_dir()
            || root_metadata.uid() != unsafe { libc::geteuid() }
            || root_metadata.mode() & 0o077 != 0
        {
            return me;
        }
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => file,
            Err(_) => return me,
        };
        let Ok(metadata) = file.metadata() else {
            return me;
        };
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.len() > MAX_ICON_CACHE_BYTES as u64
        {
            return me;
        }
        let mut buf = Vec::with_capacity(metadata.len() as usize);
        if file
            .take((MAX_ICON_CACHE_BYTES + 1) as u64)
            .read_to_end(&mut buf)
            .is_err()
            || buf.len() > MAX_ICON_CACHE_BYTES
        {
            return me;
        }
        if buf.len() < MAGIC.len() + 12 || &buf[..MAGIC.len()] != MAGIC {
            return me;
        }
        let mut at = MAGIC.len();
        let take = |b: &[u8], at: &mut usize, n: usize| -> Option<Vec<u8>> {
            if *at + n > b.len() {
                return None;
            }
            let v = b[*at..*at + n].to_vec();
            *at += n;
            Some(v)
        };
        let Some(st) = take(&buf, &mut at, 8) else {
            return me;
        };
        if u64::from_le_bytes(st.try_into().unwrap_or([0; 8])) != stamp {
            return me;
        }
        let Some(cnt) = take(&buf, &mut at, 4) else {
            return me;
        };
        let count = u32::from_le_bytes(cnt.try_into().unwrap_or([0; 4])) as usize;
        let Some(bytes) = usize::try_from(px)
            .ok()
            .and_then(|size| size.checked_mul(size))
            .and_then(|pixels| pixels.checked_mul(4))
        else {
            return me;
        };
        let resident_entries = MAX_RESIDENT_ICON_BYTES / bytes.max(256);
        let structurally_possible = buf.len().saturating_sub(at) / bytes.saturating_add(2).max(1);
        if count
            > MAX_ICON_CACHE_ENTRIES
                .min(resident_entries)
                .min(structurally_possible)
        {
            return me;
        }
        let mut parsed = HashMap::with_capacity(count);
        for _ in 0..count {
            let Some(nl) = take(&buf, &mut at, 2) else {
                return me;
            };
            let n = u16::from_le_bytes(nl.try_into().unwrap_or([0; 2])) as usize;
            let Some(name) = take(&buf, &mut at, n) else {
                return me;
            };
            let Some(px_data) = take(&buf, &mut at, bytes) else {
                return me;
            };
            let Ok(name) = String::from_utf8(name) else {
                return me;
            };
            if parsed.insert(name, Arc::from(px_data)).is_some() {
                return me;
            }
        }
        me.pixels = parsed;
        me
    }
}

impl Icons {
    pub(crate) fn from_prepared(prepared: PreparedIcons) -> Self {
        let mut absolute_candidates = HashMap::new();
        for key in prepared.pixels.keys() {
            if let Some(name) = absolute_name_from_key(key) {
                absolute_candidates
                    .entry(name.to_string())
                    .and_modify(|candidate: &mut String| {
                        if key > candidate {
                            *candidate = key.clone();
                        }
                    })
                    .or_insert_with(|| key.clone());
            }
        }
        let (result_tx, result_rx) = sync_channel(MAX_PENDING_ICON_REQUESTS);
        Self {
            generation: next_icon_generation(),
            next_request_id: 0,
            px: prepared.px,
            stamp: prepared.stamp,
            path: prepared.path,
            pixels: prepared.pixels,
            textures: HashMap::new(),
            absolute_candidates,
            absolute_current: HashMap::new(),
            pending: HashMap::new(),
            failed: HashSet::new(),
            waiter_count: 0,
            pump_active: false,
            result_tx,
            result_rx,
            dirty: false,
        }
    }

    #[cfg(test)]
    fn load_from(path: PathBuf, px: i32, stamp: u64) -> Self {
        Self::from_prepared(PreparedIcons::load_from(path, px, stamp))
    }

    /// A warm-only lookup. This function performs no source metadata, file access, theme lookup or
    /// decoding; a miss is handed to [`Self::image`] and its bounded worker lane.
    pub fn texture(&mut self, name: &str) -> Option<gtk4::gdk::Texture> {
        if name.is_empty() {
            return None;
        }
        let key = if std::path::Path::new(name).is_absolute() {
            self.absolute_current.get(name)?.clone()
        } else {
            name.to_string()
        };
        self.texture_for_key(&key)
    }

    fn texture_for_key(&mut self, key: &str) -> Option<gtk4::gdk::Texture> {
        if let Some(hit) = self.textures.get(key) {
            return hit.clone();
        }
        let data = self.pixels.get(key)?.clone();
        let made = Some(Self::texture_from_pixels(self.px, &data));
        if self.within_resident_budget(0, 1) {
            self.textures.insert(key.to_string(), made.clone());
        }
        made
    }

    fn texture_from_pixels(px: i32, data: &[u8]) -> gtk4::gdk::Texture {
        gtk4::gdk::MemoryTexture::new(
            px,
            px,
            gtk4::gdk::MemoryFormat::R8g8b8a8,
            &gtk4::glib::Bytes::from(data),
            (px * 4) as usize,
        )
        .upcast()
    }

    /// Creates the exact image GTK should show now and arranges an in-place upgrade on a cold
    /// cache miss. Theme icons use GTK's own resolved paintable immediately; absolute files remain
    /// blank only until their descriptor has been validated and decoded off the main thread.
    pub fn image(cache: &Rc<RefCell<Self>>, name: &str, theme: &gtk4::IconTheme) -> gtk4::Image {
        let warm = cache.borrow_mut().texture(name);
        if std::path::Path::new(name).is_absolute() {
            let image = warm.as_ref().map_or_else(gtk4::Image::new, |texture| {
                gtk4::Image::from_paintable(Some(texture))
            });
            Self::request(
                cache,
                name,
                IconSource::Absolute(PathBuf::from(name)),
                &image,
            );
            return image;
        }
        if let Some(texture) = warm {
            return gtk4::Image::from_paintable(Some(&texture));
        }
        if name.is_empty() {
            return gtk4::Image::new();
        }

        let paintable = theme.lookup_icon(
            name,
            &[],
            cache.borrow().px,
            1,
            gtk4::TextDirection::None,
            gtk4::IconLookupFlags::empty(),
        );
        let image = gtk4::Image::from_paintable(Some(&paintable));
        if let Some(path) = paintable.file().and_then(|file| file.path()) {
            Self::request(cache, name, IconSource::Themed(path), &image);
        }
        image
    }

    fn request(cache: &Rc<RefCell<Self>>, name: &str, source: IconSource, image: &gtk4::Image) {
        let mut state = cache.borrow_mut();
        if state.failed.contains(name) {
            return;
        }
        if state.waiter_count >= MAX_ICON_WAITERS {
            state.prune_waiters();
        }
        if state.waiter_count >= MAX_ICON_WAITERS {
            return;
        }
        if let Some(pending) = state.pending.get_mut(name) {
            pending.waiters.push(image.downgrade());
            state.waiter_count += 1;
            drop(state);
            Self::ensure_pump(cache);
            return;
        }
        if state.pending.len() >= MAX_PENDING_ICON_REQUESTS {
            return;
        }

        state.next_request_id = state.next_request_id.wrapping_add(1);
        if state.next_request_id == 0 {
            state.next_request_id = 1;
        }
        let request_id = state.next_request_id;
        let cached_key = source.is_absolute().then(|| {
            state
                .absolute_current
                .get(name)
                .or_else(|| state.absolute_candidates.get(name))
                .cloned()
        });
        let cached_key = cached_key.flatten();
        let request = IconRequest {
            generation: state.generation,
            request_id,
            stamp: state.stamp,
            px: state.px,
            name: name.to_string(),
            source,
            cached_key,
            result: state.result_tx.clone(),
        };
        state.pending.insert(
            name.to_string(),
            PendingIcon {
                request_id,
                absolute: request.source.is_absolute(),
                request: Some(request),
                waiters: vec![image.downgrade()],
                started: Instant::now(),
            },
        );
        state.waiter_count += 1;
        state.flush_requests();
        drop(state);
        Self::ensure_pump(cache);
    }

    fn ensure_pump(cache: &Rc<RefCell<Self>>) {
        {
            let mut state = cache.borrow_mut();
            if state.pump_active || state.pending.is_empty() {
                return;
            }
            state.pump_active = true;
        }
        let cache = Rc::downgrade(cache);
        gtk4::glib::timeout_add_local(ICON_RESULT_POLL, move || {
            let Some(cache) = cache.upgrade() else {
                return gtk4::glib::ControlFlow::Break;
            };
            let mut state = cache.borrow_mut();
            state.drain_results();
            state.expire_requests();
            state.flush_requests();
            if state.pending.is_empty() {
                // Snapshot the cache once at the end of this request burst. `save` only hands
                // immutable data to its writer, but collecting that snapshot is proportional to
                // the resident icon count and must not run again on every 16 ms result poll.
                state.save();
                state.pump_active = false;
                gtk4::glib::ControlFlow::Break
            } else {
                gtk4::glib::ControlFlow::Continue
            }
        });
    }

    fn prune_waiters(&mut self) {
        let mut count = 0usize;
        for pending in self.pending.values_mut() {
            pending.waiters.retain(|waiter| waiter.upgrade().is_some());
            count = count.saturating_add(pending.waiters.len());
        }
        self.waiter_count = count;
    }

    fn flush_requests(&mut self) {
        let Some(pool) = icon_worker_pool() else {
            self.fail_all_pending();
            return;
        };
        let mut disconnected = false;
        for pending in self.pending.values_mut() {
            let Some(request) = pending.request.take() else {
                continue;
            };
            match pool.requests.try_send(request) {
                Ok(()) => {}
                Err(TrySendError::Full(request)) => pending.request = Some(request),
                Err(TrySendError::Disconnected(_)) => disconnected = true,
            }
        }
        if disconnected {
            self.fail_all_pending();
        }
    }

    fn fail_all_pending(&mut self) {
        if self.failed.len() < MAX_PENDING_ICON_REQUESTS {
            self.failed.extend(
                self.pending
                    .keys()
                    .take(MAX_PENDING_ICON_REQUESTS - self.failed.len())
                    .cloned(),
            );
        }
        self.pending.clear();
        self.waiter_count = 0;
    }

    fn expire_requests(&mut self) {
        let expired: Vec<_> = self
            .pending
            .iter()
            .filter(|(_, pending)| pending.started.elapsed() >= ICON_REQUEST_TIMEOUT)
            .map(|(name, _)| name.clone())
            .collect();
        for name in expired {
            if let Some(pending) = self.pending.remove(&name) {
                self.waiter_count = self.waiter_count.saturating_sub(pending.waiters.len());
                if self.failed.len() < MAX_PENDING_ICON_REQUESTS {
                    self.failed.insert(name);
                }
            }
        }
    }

    fn drain_results(&mut self) {
        while let Ok(result) = self.result_rx.try_recv() {
            let Some(pending) = self.pending.get(&result.name) else {
                continue;
            };
            if !self.result_is_current(&result, pending) {
                continue;
            }
            let pending = self
                .pending
                .remove(&result.name)
                .expect("a matching pending icon exists");
            self.waiter_count = self.waiter_count.saturating_sub(pending.waiters.len());
            self.apply_result(result, pending);
        }
    }

    fn result_is_current(&self, result: &IconResult, pending: &PendingIcon) -> bool {
        result.generation == self.generation
            && result.request_id == pending.request_id
            && result.stamp == self.stamp
            && result.px == self.px
            && result.absolute == pending.absolute
    }

    fn apply_result(&mut self, result: IconResult, pending: PendingIcon) {
        let mut texture = None;
        match result.outcome {
            LoadOutcome::Cached { key } => {
                texture = self.texture_for_key(&key);
                if result.absolute && texture.is_some() {
                    self.absolute_current.insert(result.name.clone(), key);
                }
            }
            LoadOutcome::Decoded { key, pixels } => {
                let expected = usize::try_from(self.px)
                    .ok()
                    .and_then(|px| px.checked_mul(px))
                    .and_then(|px| px.checked_mul(4));
                let replaced = result
                    .absolute
                    .then(|| self.absolute_candidates.get(&result.name).cloned())
                    .flatten()
                    .filter(|old| old != &key);
                let remove_pixels = replaced
                    .as_ref()
                    .is_some_and(|old| self.pixels.contains_key(old));
                let remove_textures = replaced
                    .as_ref()
                    .is_some_and(|old| self.textures.contains_key(old));
                let pixel_count = self
                    .pixels
                    .len()
                    .saturating_sub(usize::from(remove_pixels))
                    .saturating_add(usize::from(!self.pixels.contains_key(&key)));
                let texture_count = self
                    .textures
                    .len()
                    .saturating_sub(usize::from(remove_textures))
                    .saturating_add(usize::from(!self.textures.contains_key(&key)));
                if expected != Some(pixels.len()) {
                    if self.failed.len() < MAX_PENDING_ICON_REQUESTS {
                        self.failed.insert(result.name.clone());
                    }
                    if result.absolute {
                        self.absolute_current.remove(&result.name);
                    }
                } else if self.within_resident_budget_counts(pixel_count, texture_count) {
                    if result.absolute
                        && let Some(old) = self
                            .absolute_candidates
                            .insert(result.name.clone(), key.clone())
                            .filter(|old| old != &key)
                    {
                        self.pixels.remove(&old);
                        self.textures.remove(&old);
                    }
                    self.pixels.insert(key.clone(), pixels);
                    self.dirty = true;
                    texture = self.texture_for_key(&key);
                    if result.absolute && texture.is_some() {
                        self.absolute_current.insert(result.name.clone(), key);
                    }
                } else {
                    // The materialized grid has its own byte cap, so a one-off texture remains
                    // bounded even when the persistent resident cache is full. This preserves the
                    // established visible result without admitting another cache entry.
                    texture = Some(Self::texture_from_pixels(self.px, &pixels));
                    if result.absolute {
                        self.absolute_current.remove(&result.name);
                    }
                }
            }
            LoadOutcome::Missing => {
                if self.failed.len() < MAX_PENDING_ICON_REQUESTS {
                    self.failed.insert(result.name.clone());
                }
                if result.absolute {
                    self.absolute_current.remove(&result.name);
                }
            }
        }

        for waiter in pending.waiters {
            let Some(image) = waiter.upgrade() else {
                continue;
            };
            if let Some(texture) = texture.as_ref() {
                image.set_paintable(Some(texture));
            } else if result.absolute {
                image.set_paintable(None::<&gtk4::gdk::Texture>);
            }
        }
    }

    fn within_resident_budget(&self, add_pixels: usize, add_textures: usize) -> bool {
        let Some(pixels) = self.pixels.len().checked_add(add_pixels) else {
            return false;
        };
        let Some(textures) = self.textures.len().checked_add(add_textures) else {
            return false;
        };
        self.within_resident_budget_counts(pixels, textures)
    }

    fn within_resident_budget_counts(&self, pixels: usize, textures: usize) -> bool {
        let bytes = usize::try_from(self.px)
            .ok()
            .and_then(|size| size.checked_mul(size))
            .and_then(|pixels| pixels.checked_mul(4))
            // Even a missing-icon entry owns a key/hash bucket/Option; charge a small floor so a
            // hostile stream of unique missing names cannot bypass the pixel budget.
            .map(|bytes| bytes.max(256));
        bytes
            .and_then(|bytes| {
                pixels
                    .checked_add(textures)
                    .and_then(|entries| entries.checked_mul(bytes))
            })
            .is_some_and(|bytes| bytes <= MAX_RESIDENT_ICON_BYTES)
    }

    /// Write the file if anything new was decoded. Atomic, for the same reason the state files are:
    /// a truncated cache is worse than none, because it parses as "these icons are blank".
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let Some(bytes) = usize::try_from(self.px)
            .ok()
            .and_then(|size| size.checked_mul(size))
            .and_then(|pixels| pixels.checked_mul(4))
        else {
            return;
        };
        let usable: Vec<(String, Arc<[u8]>)> = self
            .pixels
            .iter()
            .filter(|(n, d)| d.len() == bytes && n.len() <= u16::MAX as usize)
            .map(|(name, data)| (name.clone(), data.clone()))
            .collect();
        let stamp = self.stamp;
        // Any icons decoded after this snapshot set the bit again, while ordinary redraws remain
        // a no-op. The immutable Arc-backed pixels keep serialization off GTK without copying the
        // full cache on the event thread.
        self.dirty = false;
        let write = IconWrite {
            stamp,
            bytes_per_icon: bytes,
            pixels: usable,
        };
        if cfg!(test) {
            serialize_and_write_cache(path, write);
            return;
        }
        enqueue_cache_write(path.clone(), write);
    }
}

struct IconWrite {
    stamp: u64,
    bytes_per_icon: usize,
    pixels: Vec<(String, Arc<[u8]>)>,
}

#[derive(Default)]
struct PendingIconWrite {
    latest: Option<IconWrite>,
}

/// Serialize writes per cache path and retain only the newest snapshot waiting behind an active
/// write. Provider B can therefore never rename an older snapshot after provider C's newer one;
/// intermediate snapshots are disposable, but write order is monotonic.
fn enqueue_cache_write(path: PathBuf, write: IconWrite) {
    enqueue_cache_write_with(path, write, || {}, serialize_and_write_cache);
}

fn enqueue_cache_write_with<Before, Writer>(
    path: PathBuf,
    write: IconWrite,
    before_first_write: Before,
    writer: Writer,
) where
    Before: FnOnce() + Send + 'static,
    Writer: Fn(&std::path::Path, IconWrite) + Send + 'static,
{
    static WRITES: OnceLock<Mutex<HashMap<PathBuf, PendingIconWrite>>> = OnceLock::new();
    let writes = WRITES.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut pending) = writes.lock() else {
        return;
    };
    if let Some(active) = pending.get_mut(&path) {
        active.latest = Some(write);
        return;
    }
    pending.insert(
        path.clone(),
        PendingIconWrite {
            latest: Some(write),
        },
    );
    drop(pending);

    let worker_path = path.clone();
    if std::thread::Builder::new()
        .name("cbar-launcher-icons".into())
        .spawn(move || {
            let mut before_first_write = Some(before_first_write);
            loop {
                let next = {
                    let Ok(mut pending) = writes.lock() else {
                        return;
                    };
                    let Some(active) = pending.get_mut(&worker_path) else {
                        return;
                    };
                    match active.latest.take() {
                        Some(write) => write,
                        None => {
                            pending.remove(&worker_path);
                            return;
                        }
                    }
                };
                if let Some(before) = before_first_write.take() {
                    before();
                }
                writer(&worker_path, next);
            }
        })
        .is_err()
        && let Ok(mut pending) = writes.lock()
    {
        pending.remove(&path);
    }
}

fn serialize_and_write_cache(path: &std::path::Path, write: IconWrite) {
    let total = write
        .pixels
        .iter()
        .try_fold(MAGIC.len() + 12, |total, (name, _)| {
            total
                .checked_add(2)
                .and_then(|total| total.checked_add(name.len()))
                .and_then(|total| total.checked_add(write.bytes_per_icon))
        });
    let Some(total) = total.filter(|total| *total <= MAX_ICON_CACHE_BYTES) else {
        return;
    };
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&write.stamp.to_le_bytes());
    out.extend_from_slice(&(write.pixels.len() as u32).to_le_bytes());
    for (name, data) in write.pixels {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&data);
    }
    write_cache(path, &out);
}

fn write_cache(path: &std::path::Path, out: &[u8]) {
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let Ok(metadata) = std::fs::symlink_metadata(dir) else {
            return;
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return;
        }
        if std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).is_err() {
            return;
        }
    }
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "bin.{}.{}.tmp",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed)
    ));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        && file.write_all(out).is_ok()
        && file.sync_all().is_ok()
        && std::fs::rename(&tmp, path).is_ok()
    {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    let _ = std::fs::remove_file(tmp);
}

/// Pixels as an exactly `px` by `px` RGBA block, which is what the cache format stores and what
/// `MemoryTexture` wants. A loader asked for a size gives back something that FITS it, preserving
/// aspect, so a wide icon comes back short -- centring it here means every entry is one fixed
/// length and the file needs no per-entry geometry.
fn rgba_square(pb: &gtk4::gdk_pixbuf::Pixbuf, px: i32) -> Option<Vec<u8>> {
    let (w, h) = (pb.width(), pb.height());
    if w <= 0 || h <= 0 || w > px || h > px {
        return None;
    }
    let src = pb.read_pixel_bytes();
    let stride = pb.rowstride() as usize;
    let chans = pb.n_channels() as usize;
    if chans != 3 && chans != 4 {
        return None;
    }
    let mut out = vec![0u8; (px * px * 4) as usize];
    let (ox, oy) = (((px - w) / 2) as usize, ((px - h) / 2) as usize);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let s = y * stride + x * chans;
            if s + chans > src.len() {
                return None;
            }
            let d = ((y + oy) * px as usize + (x + ox)) * 4;
            out[d] = src[s];
            out[d + 1] = src[s + 1];
            out[d + 2] = src[s + 2];
            out[d + 3] = if chans == 4 { src[s + 3] } else { 255 };
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "cbar-launcher-icons-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        d.join("icons.bin")
    }

    fn blank(path: PathBuf, px: i32, stamp: u64) -> Icons {
        let mut icons = Icons::from_prepared(PreparedIcons::empty(Some(path), px, stamp));
        icons.dirty = true;
        icons
    }

    fn request(name: &str, source: IconSource, px: i32, cached_key: Option<String>) -> IconRequest {
        let (result, _) = sync_channel(1);
        IconRequest {
            generation: 1,
            request_id: 1,
            stamp: 1,
            px,
            name: name.to_string(),
            source,
            cached_key,
            result,
        }
    }

    fn write_test_png(path: &std::path::Path) {
        let pixbuf =
            gtk4::gdk_pixbuf::Pixbuf::new(gtk4::gdk_pixbuf::Colorspace::Rgb, true, 8, 2, 2)
                .expect("test pixbuf");
        pixbuf.fill(0x3366ccff);
        std::fs::write(path, pixbuf.save_to_bufferv("png", &[]).expect("test PNG"))
            .expect("write test PNG");
    }

    fn reset_source_counters() {
        SOURCE_OPEN_COUNT.with(|count| count.set(0));
        SOURCE_DECODE_COUNT.with(|count| count.set(0));
    }

    fn source_counters() -> (usize, usize) {
        (
            SOURCE_OPEN_COUNT.with(std::cell::Cell::get),
            SOURCE_DECODE_COUNT.with(std::cell::Cell::get),
        )
    }

    #[test]
    fn cold_texture_lookup_does_no_source_or_decode_work() {
        let mut icons = blank(tmp("cold-lookup"), 20, 7);
        reset_source_counters();

        assert!(icons.texture("firefox").is_none());
        assert!(
            icons
                .texture("/nonexistent/cbar-launcher-test/icon.png")
                .is_none()
        );
        assert_eq!(
            source_counters(),
            (0, 0),
            "the GTK cache lookup must remain memory-only on every miss"
        );
    }

    #[test]
    fn a_rewritten_icon_file_gets_a_new_descriptor_key() {
        let path = tmp("rewritten").with_file_name("art.png");
        std::fs::write(&path, b"first").unwrap();
        let name = path.to_str().unwrap();
        let first = absolute_cache_key(
            name,
            &std::fs::File::open(&path).unwrap().metadata().unwrap(),
        );
        assert_eq!(absolute_name_from_key(&first), Some(name));

        std::fs::write(&path, b"second").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();

        assert_ne!(
            absolute_cache_key(
                name,
                &std::fs::File::open(&path).unwrap().metadata().unwrap()
            ),
            first,
            "a file rewritten behind an unchanged path must miss the cache"
        );
    }

    #[test]
    fn an_absolute_path_that_is_not_there_is_a_worker_miss() {
        let missing = "/nonexistent/cbar-launcher-test/icon.png";
        assert!(matches!(
            load_icon(&request(
                missing,
                IconSource::Absolute(PathBuf::from(missing)),
                20,
                None,
            )),
            LoadOutcome::Missing
        ));
    }

    /// A file this program did not write, or wrote in an older shape, reads as "no cache" rather
    /// than as garbage pixels.
    #[test]
    fn a_foreign_file_is_ignored() {
        let p = tmp("foreign");
        std::fs::write(&p, b"not a cbar launcher cache at all").unwrap();
        assert!(
            Icons::load_from(p, 20, 1).pixels.is_empty(),
            "garbage must not become icons"
        );
    }

    /// The stamp IS the validity test, so a mismatched one discards everything -- otherwise an
    /// application that was uninstalled keeps its icon forever.
    #[test]
    fn a_stale_stamp_discards_everything() {
        let p = tmp("stale");
        let px = 4;
        let mut w = blank(p.clone(), px, 12345);
        w.pixels
            .insert("thing".into(), vec![7u8; (px * px * 4) as usize].into());
        w.save();

        assert!(
            Icons::load_from(p.clone(), px, 999).pixels.is_empty(),
            "a different stamp reads as empty"
        );
        assert_eq!(
            Icons::load_from(p, px, 12345).pixels.len(),
            1,
            "the matching stamp still reads"
        );
    }

    /// Round trip: what is written comes back byte for byte.
    #[test]
    fn a_written_cache_reads_back_identical() {
        let p = tmp("roundtrip");
        let px = 4;
        let data: Vec<u8> = (0..(px * px * 4)).map(|i| (i % 251) as u8).collect();
        let mut w = blank(p.clone(), px, 77);
        w.pixels.insert("round.trip".into(), data.clone().into());
        w.save();

        assert_eq!(
            Icons::load_from(p, px, 77)
                .pixels
                .get("round.trip")
                .map(AsRef::as_ref),
            Some(data.as_slice())
        );
    }

    /// Nothing new decoded means nothing written -- a warm start must not rewrite the file it just
    /// read, which would be a needless write on the latency path every single launch.
    #[test]
    fn a_clean_cache_is_not_rewritten() {
        let p = tmp("clean");
        let mut w = blank(p.clone(), 4, 5);
        w.pixels.insert("x".into(), vec![0u8; 64].into());
        w.save();
        let before = std::fs::metadata(&p).unwrap().len();
        let mut read_back = Icons::load_from(p.clone(), 4, 5);
        read_back.dirty = false;
        std::fs::write(&p, b"sentinel").unwrap();
        read_back.save();
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"sentinel",
            "a clean cache wrote nothing"
        );
        assert!(before > 0);
    }

    #[test]
    fn cache_reads_are_bounded_and_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let target = tmp("symlink-read-target");
        std::fs::write(&target, b"not trusted through a symlink").unwrap();
        let link = target.with_file_name("icons-link.bin");
        symlink(&target, &link).unwrap();
        assert!(Icons::load_from(link, 20, 1).pixels.is_empty());

        let huge = tmp("oversized");
        std::fs::File::create(&huge)
            .unwrap()
            .set_len(MAX_ICON_CACHE_BYTES as u64 + 1)
            .unwrap();
        assert!(Icons::load_from(huge, 20, 1).pixels.is_empty());
    }

    #[test]
    fn cache_load_requires_private_roots_and_files() {
        let path = tmp("unsafe-mode");
        let root = path.parent().unwrap().to_path_buf();
        let original_root_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        let mut cache = blank(path.clone(), 4, 11);
        cache.pixels.insert("x".into(), vec![0u8; 64].into());
        cache.save();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Icons::load_from(path.clone(), 4, 11).pixels.is_empty());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Icons::load_from(path, 4, 11).pixels.is_empty());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(original_root_mode))
            .unwrap();
    }

    #[test]
    fn cache_entry_count_is_rejected_before_hostile_allocation() {
        let path = tmp("hostile-count");
        let count = MAX_ICON_CACHE_ENTRIES + 1;
        let mut encoded = Vec::with_capacity(MAGIC.len() + 12 + count * 6);
        encoded.extend_from_slice(MAGIC);
        encoded.extend_from_slice(&7u64.to_le_bytes());
        encoded.extend_from_slice(&(count as u32).to_le_bytes());
        // At one pixel an empty key record is two length bytes plus four RGBA bytes. Supplying a
        // structurally plausible body proves rejection is the explicit entry/resident cap, not a
        // coincidentally short file discovered only after allocating the advertised map.
        encoded.resize(encoded.len() + count * 6, 0);
        std::fs::write(&path, encoded).unwrap();
        assert!(Icons::load_from(path, 1, 7).pixels.is_empty());
    }

    #[test]
    fn absolute_icon_sources_are_regular_bounded_and_follow_ordinary_symlinks() {
        use std::os::unix::fs::symlink;

        let regular = tmp("absolute-regular");
        std::fs::write(&regular, b"not an image, but a bounded regular source").unwrap();
        assert!(open_icon_source(&regular).is_some());

        let link = tmp("absolute-link");
        symlink(&regular, &link).unwrap();
        let (_, linked_metadata) = open_icon_source(&link).expect("ordinary symlink source");
        assert_eq!(
            linked_metadata.ino(),
            std::fs::metadata(&regular).unwrap().ino(),
            "fstat must describe the followed target descriptor"
        );

        let huge = tmp("absolute-huge");
        std::fs::File::create(&huge)
            .unwrap()
            .set_len(MAX_ICON_SOURCE_BYTES + 1)
            .unwrap();
        assert!(open_icon_source(&huge).is_none());
    }

    #[test]
    fn legitimate_absolute_symlink_is_decoded_and_keyed_by_its_target_descriptor() {
        use std::os::unix::fs::symlink;

        let target = tmp("absolute-png-target").with_file_name("target.png");
        write_test_png(&target);
        let link = target.with_file_name("linked.png");
        symlink(&target, &link).unwrap();
        let name = link.to_string_lossy().into_owned();

        let outcome = load_icon(&request(
            &name,
            IconSource::Absolute(link.clone()),
            20,
            None,
        ));
        let LoadOutcome::Decoded { key, pixels } = outcome else {
            panic!("a legitimate absolute symlink did not decode");
        };
        assert_eq!(pixels.len(), 20 * 20 * 4);
        assert_eq!(absolute_name_from_key(&key), Some(name.as_str()));
        let target_metadata = std::fs::metadata(&target).unwrap();
        assert!(key.contains(&format!(":{}:", target_metadata.ino())));

        let replacement = target.with_file_name("replacement.png");
        write_test_png(&replacement);
        std::fs::remove_file(&link).unwrap();
        symlink(&replacement, &link).unwrap();
        let LoadOutcome::Decoded {
            key: replacement_key,
            ..
        } = load_icon(&request(
            &name,
            IconSource::Absolute(link),
            20,
            Some(key.clone()),
        ))
        else {
            panic!("retargeted symlink reused stale cached pixels");
        };
        assert_ne!(replacement_key, key);
    }

    #[test]
    fn matching_absolute_descriptor_key_skips_read_and_decode() {
        let path = tmp("absolute-cached-hit").with_file_name("cached.png");
        write_test_png(&path);
        let name = path.to_str().unwrap();
        let metadata = std::fs::File::open(&path).unwrap().metadata().unwrap();
        let key = absolute_cache_key(name, &metadata);
        reset_source_counters();

        assert!(matches!(
            load_icon(&request(
                name,
                IconSource::Absolute(path.clone()),
                20,
                Some(key.clone()),
            )),
            LoadOutcome::Cached { key: found } if found == key
        ));
        assert_eq!(
            source_counters(),
            (1, 0),
            "validation opens and fstats once but must not read or decode a cache hit"
        );
    }

    #[test]
    fn icon_worker_count_is_hardware_derived_and_bounded() {
        assert_eq!(icon_worker_count(None), 1);
        assert_eq!(icon_worker_count(Some(1)), 1);
        assert_eq!(icon_worker_count(Some(8)), 4);
        assert_eq!(icon_worker_count(Some(8_192)), MAX_ICON_WORKERS);
    }

    #[test]
    fn cache_save_refuses_a_symlinked_root() {
        use std::os::unix::fs::symlink;

        let base = tmp("symlink-root").parent().unwrap().join("root");
        let target = base.join("target");
        let link = base.join("cache");
        std::fs::create_dir_all(&target).unwrap();
        symlink(&target, &link).unwrap();
        let mut icons = blank(link.join("icons.bin"), 4, 1);
        icons.pixels.insert("safe".into(), vec![0; 64].into());
        icons.save();
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);

        std::fs::remove_file(link).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn cache_load_refuses_a_symlinked_root() {
        use std::os::unix::fs::symlink;

        let base = tmp("symlink-load-root").parent().unwrap().join("root");
        let target = base.join("target");
        let link = base.join("cache");
        std::fs::create_dir_all(&target).unwrap();
        let target_file = target.join("icons.bin");
        let mut writer = blank(target_file, 4, 5);
        writer
            .pixels
            .insert("thing".into(), vec![7u8; 4 * 4 * 4].into());
        writer.save();
        symlink(&target, &link).unwrap();

        assert!(
            Icons::load_from(link.join("icons.bin"), 4, 5)
                .pixels
                .is_empty()
        );
    }

    #[test]
    fn completion_requires_matching_generation_request_theme_and_size() {
        let icons = blank(tmp("completion-generation"), 20, 91);
        let pending = PendingIcon {
            request_id: 7,
            request: None,
            waiters: Vec::new(),
            started: Instant::now(),
            absolute: true,
        };
        let current = IconResult {
            generation: icons.generation,
            request_id: 7,
            stamp: 91,
            px: 20,
            name: "/tmp/icon.png".to_string(),
            absolute: true,
            outcome: LoadOutcome::Missing,
        };
        assert!(icons.result_is_current(&current, &pending));

        for stale in [
            IconResult {
                generation: icons.generation.wrapping_add(1),
                ..current_result_like(&current)
            },
            IconResult {
                request_id: 8,
                ..current_result_like(&current)
            },
            IconResult {
                stamp: 92,
                ..current_result_like(&current)
            },
            IconResult {
                px: 24,
                ..current_result_like(&current)
            },
            IconResult {
                absolute: false,
                ..current_result_like(&current)
            },
        ] {
            assert!(!icons.result_is_current(&stale, &pending));
        }
    }

    fn current_result_like(result: &IconResult) -> IconResult {
        IconResult {
            generation: result.generation,
            request_id: result.request_id,
            stamp: result.stamp,
            px: result.px,
            name: result.name.clone(),
            absolute: result.absolute,
            outcome: LoadOutcome::Missing,
        }
    }

    #[test]
    fn async_cache_writer_serializes_and_coalesces_to_newest_snapshot() {
        use std::sync::Barrier;

        fn snapshot(stamp: u64) -> IconWrite {
            IconWrite {
                stamp,
                bytes_per_icon: 4,
                pixels: vec![(format!("icon-{stamp}"), vec![stamp as u8; 4].into())],
            }
        }

        let path = tmp("ordered-writer");
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        enqueue_cache_write_with(
            path.clone(),
            snapshot(1),
            {
                let started = started.clone();
                let release = release.clone();
                move || {
                    started.wait();
                    release.wait();
                }
            },
            {
                let seen = seen.clone();
                move |_, write| {
                    let stamp = write.stamp;
                    seen.lock().unwrap().push(stamp);
                    if stamp == 3 {
                        done_tx.send(()).unwrap();
                    }
                }
            },
        );
        // Hold snapshot 1 in the single worker while 2 is replaced by 3 in the pending slot.
        started.wait();
        enqueue_cache_write(path.clone(), snapshot(2));
        enqueue_cache_write(path, snapshot(3));
        release.wait();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("newest cache snapshot was not written");
        assert_eq!(*seen.lock().unwrap(), [1, 3]);
    }
}
