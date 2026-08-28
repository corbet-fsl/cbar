use notify::event::{ModifyKind, RenameMode};
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher, recommended_watcher};
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self as std_mpsc, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tracing::{debug, error};

const COMMAND_QUEUE_CAPACITY: usize = 256;
const SUBSCRIBER_QUEUE_CAPACITY: usize = 1;
const MAINTENANCE_INTERVAL: Duration = Duration::from_millis(250);

static SERVICE: OnceLock<FileWatchService> = OnceLock::new();
static SERVICE_INIT: Mutex<()> = Mutex::new(());

/// Subscribes to mutations of one file.
///
/// All subscriptions in the process share one platform watcher. The returned
/// channel has capacity for one pending notification, so filesystem event
/// bursts are coalesced until the consumer catches up.
pub async fn subscribe(path: impl AsRef<Path>) -> notify::Result<mpsc::Receiver<()>> {
    let path = path.as_ref().to_path_buf();
    tokio::task::spawn_blocking(move || subscribe_blocking(&path))
        .await
        .map_err(|_| notify::Error::generic("file watch subscription worker stopped"))?
}

fn subscribe_blocking(path: &Path) -> notify::Result<mpsc::Receiver<()>> {
    service()?.subscribe(path)
}

fn service() -> notify::Result<&'static FileWatchService> {
    if let Some(service) = SERVICE.get() {
        return Ok(service);
    }

    let _init = SERVICE_INIT
        .lock()
        .map_err(|_| notify::Error::generic("file watch service initialization lock poisoned"))?;

    if let Some(service) = SERVICE.get() {
        return Ok(service);
    }

    let service = FileWatchService::start()?;
    if SERVICE.set(service).is_err() {
        return Err(notify::Error::generic(
            "file watch service initialized more than once",
        ));
    }

    SERVICE
        .get()
        .ok_or_else(|| notify::Error::generic("file watch service failed to initialize"))
}

struct FileWatchService {
    commands: SyncSender<Command>,
}

impl FileWatchService {
    fn start() -> notify::Result<Self> {
        let (commands, command_rx) = std_mpsc::sync_channel(COMMAND_QUEUE_CAPACITY);
        let event_tx = commands.clone();
        let overflowed = Arc::new(AtomicBool::new(false));
        let callback_overflowed = overflowed.clone();
        let (ready_tx, ready_rx) = std_mpsc::channel();

        let _thread = thread::Builder::new()
            .name("file-watch-service".to_string())
            .spawn(move || {
                let watcher = recommended_watcher(move |result| {
                    match event_tx.try_send(Command::Event(result)) {
                        Ok(()) => {}
                        Err(std_mpsc::TrySendError::Full(_)) => {
                            callback_overflowed.store(true, Ordering::Release);
                        }
                        Err(std_mpsc::TrySendError::Disconnected(_)) => {}
                    }
                });

                match watcher {
                    Ok(watcher) => {
                        if ready_tx.send(Ok(())).is_ok() {
                            run_service(
                                WatchRegistry::new(NotifyDirectoryWatcher(watcher)),
                                command_rx,
                                overflowed,
                            );
                        }
                    }
                    Err(err) => {
                        let _result = ready_tx.send(Err(err));
                    }
                }
            })
            .map_err(notify::Error::io)?;

        ready_rx.recv().map_err(|_| {
            notify::Error::generic("file watch service stopped during initialization")
        })??;

        Ok(Self { commands })
    }

    fn subscribe(&self, path: &Path) -> notify::Result<mpsc::Receiver<()>> {
        let (updates_tx, updates_rx) = mpsc::channel(SUBSCRIBER_QUEUE_CAPACITY);
        let (result_tx, result_rx) = std_mpsc::channel();

        self.commands
            .send(Command::Subscribe {
                path: path.to_path_buf(),
                updates: updates_tx,
                result: result_tx,
            })
            .map_err(|_| notify::Error::generic("file watch service stopped"))?;

        result_rx
            .recv()
            .map_err(|_| notify::Error::generic("file watch service stopped"))??;

        Ok(updates_rx)
    }
}

enum Command {
    Subscribe {
        path: PathBuf,
        updates: mpsc::Sender<()>,
        result: std_mpsc::Sender<notify::Result<()>>,
    },
    Event(notify::Result<Event>),
}

fn run_service(
    mut registry: WatchRegistry<NotifyDirectoryWatcher>,
    commands: Receiver<Command>,
    overflowed: Arc<AtomicBool>,
) {
    let mut last_maintenance = Instant::now();

    loop {
        match commands.recv_timeout(MAINTENANCE_INTERVAL) {
            Ok(Command::Subscribe {
                path,
                updates,
                result,
            }) => {
                let _result = result.send(registry.subscribe(&path, updates));
            }
            Ok(Command::Event(Ok(event))) => {
                if registry.dispatch(&event) {
                    debug!(?event, "file watch event");
                }
            }
            Ok(Command::Event(Err(err))) => {
                error!("Error occurred while watching files: {err:?}");
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        if overflowed.swap(false, Ordering::AcqRel) {
            debug!("file watch event queue overflowed; refreshing all subscribers");
            registry.notify_all();
        }

        if last_maintenance.elapsed() >= MAINTENANCE_INTERVAL {
            registry.prune_closed();
            last_maintenance = Instant::now();
        }
    }
}

trait DirectoryWatcher {
    fn watch(&mut self, path: &Path) -> notify::Result<()>;
    fn unwatch(&mut self, path: &Path) -> notify::Result<()>;
}

struct NotifyDirectoryWatcher(RecommendedWatcher);

impl DirectoryWatcher for NotifyDirectoryWatcher {
    fn watch(&mut self, path: &Path) -> notify::Result<()> {
        self.0.watch(path, RecursiveMode::NonRecursive)
    }

    fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
        self.0.unwatch(path)
    }
}

struct Subscribers {
    directory: PathBuf,
    senders: Vec<mpsc::Sender<()>>,
}

struct WatchRegistry<W> {
    watcher: W,
    subscribers: HashMap<PathBuf, Subscribers>,
    directories: HashMap<PathBuf, usize>,
}

impl<W: DirectoryWatcher> WatchRegistry<W> {
    fn new(watcher: W) -> Self {
        Self {
            watcher,
            subscribers: HashMap::new(),
            directories: HashMap::new(),
        }
    }

    fn subscribe(&mut self, path: &Path, sender: mpsc::Sender<()>) -> notify::Result<()> {
        let path = normalize_path(path)?;

        if let Some(subscribers) = self.subscribers.get_mut(&path) {
            subscribers.senders.push(sender);
            return Ok(());
        }

        let directory = path
            .parent()
            .ok_or_else(|| notify::Error::generic("watched file has no parent directory"))?
            .to_path_buf();

        if !self.directories.contains_key(&directory) {
            self.watcher.watch(&directory)?;
            self.directories.insert(directory.clone(), 0);
        }

        if let Some(count) = self.directories.get_mut(&directory) {
            *count += 1;
        }

        self.subscribers.insert(
            path,
            Subscribers {
                directory,
                senders: vec![sender],
            },
        );

        Ok(())
    }

    fn dispatch(&mut self, event: &Event) -> bool {
        if event.need_rescan() {
            return self.notify_all();
        }

        let event_paths: &[PathBuf] = match event.kind {
            EventKind::Any
            | EventKind::Create(_)
            | EventKind::Modify(
                ModifyKind::Any
                | ModifyKind::Data(_)
                | ModifyKind::Name(RenameMode::Any | RenameMode::Other)
                | ModifyKind::Other,
            )
            | EventKind::Other => &event.paths,
            EventKind::Modify(ModifyKind::Name(RenameMode::To | RenameMode::Both)) => event
                .paths
                .last()
                .map(std::slice::from_ref)
                .unwrap_or_default(),
            EventKind::Access(_)
            | EventKind::Modify(ModifyKind::Metadata(_) | ModifyKind::Name(RenameMode::From))
            | EventKind::Remove(_) => return false,
        };

        let mut targets = HashSet::new();

        for event_path in event_paths {
            let Ok(event_path) = normalize_path(event_path) else {
                continue;
            };

            if self.subscribers.contains_key(&event_path) {
                targets.insert(event_path.clone());
            }

            if self.directories.contains_key(&event_path) {
                targets.extend(
                    self.subscribers
                        .iter()
                        .filter(|(_, subscribers)| subscribers.directory == event_path)
                        .map(|(path, _)| path.clone()),
                );
            }
        }

        let matched = !targets.is_empty();
        for target in targets {
            self.notify_target(&target);
        }

        matched
    }

    fn notify_all(&mut self) -> bool {
        let targets = self.subscribers.keys().cloned().collect::<Vec<_>>();
        let matched = !targets.is_empty();

        for target in targets {
            self.notify_target(&target);
        }

        matched
    }

    fn notify_target(&mut self, path: &Path) {
        let remove = self.subscribers.get_mut(path).is_some_and(|subscribers| {
            subscribers
                .senders
                .retain(|sender| match sender.try_send(()) {
                    Ok(()) | Err(mpsc::error::TrySendError::Full(())) => true,
                    Err(mpsc::error::TrySendError::Closed(())) => false,
                });
            subscribers.senders.is_empty()
        });

        if remove {
            self.remove_target(path);
        }
    }

    fn prune_closed(&mut self) {
        let mut empty = Vec::new();

        for (path, subscribers) in &mut self.subscribers {
            subscribers.senders.retain(|sender| !sender.is_closed());
            if subscribers.senders.is_empty() {
                empty.push(path.clone());
            }
        }

        for path in empty {
            self.remove_target(&path);
        }
    }

    fn remove_target(&mut self, path: &Path) {
        let Some(subscribers) = self.subscribers.remove(path) else {
            return;
        };

        let remove_directory = self
            .directories
            .get_mut(&subscribers.directory)
            .is_some_and(|count| {
                debug_assert!(*count > 0);
                *count = count.saturating_sub(1);
                *count == 0
            });

        if remove_directory {
            self.directories.remove(&subscribers.directory);
            if let Err(err) = self.watcher.unwatch(&subscribers.directory) {
                error!(
                    "Failed to stop watching directory '{}': {err:?}",
                    subscribers.directory.display()
                );
            }
        }
    }
}

fn normalize_path(path: &Path) -> notify::Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        env::current_dir().map_err(notify::Error::io)?.join(path)
    };

    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir | Component::Normal(_) => normalized.push(component.as_os_str()),
        }
    }

    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{DataChange, ModifyKind, RemoveKind, RenameMode};

    #[derive(Default)]
    struct RecordingWatcher {
        watched: Vec<PathBuf>,
        unwatched: Vec<PathBuf>,
    }

    impl DirectoryWatcher for RecordingWatcher {
        fn watch(&mut self, path: &Path) -> notify::Result<()> {
            self.watched.push(path.to_path_buf());
            Ok(())
        }

        fn unwatch(&mut self, path: &Path) -> notify::Result<()> {
            self.unwatched.push(path.to_path_buf());
            Ok(())
        }
    }

    fn add_subscriber(
        registry: &mut WatchRegistry<RecordingWatcher>,
        path: &Path,
    ) -> notify::Result<mpsc::Receiver<()>> {
        let (sender, receiver) = mpsc::channel(SUBSCRIBER_QUEUE_CAPACITY);
        registry.subscribe(path, sender)?;
        Ok(receiver)
    }

    fn data_change(path: &Path) -> Event {
        Event::new(EventKind::Modify(ModifyKind::Data(DataChange::Content)))
            .add_path(path.to_path_buf())
    }

    #[test]
    fn routes_multiple_files_to_all_matching_subscribers() -> notify::Result<()> {
        let first_path = Path::new("/watch-tests/config/style.css");
        let second_path = Path::new("/watch-tests/config/graph.lua");
        let mut registry = WatchRegistry::new(RecordingWatcher::default());
        let mut first_a = add_subscriber(&mut registry, first_path)?;
        let mut first_b = add_subscriber(&mut registry, first_path)?;
        let mut second = add_subscriber(&mut registry, second_path)?;

        assert!(registry.dispatch(&data_change(first_path)));
        assert_eq!(first_a.try_recv(), Ok(()));
        assert_eq!(first_b.try_recv(), Ok(()));
        assert_eq!(second.try_recv(), Err(mpsc::error::TryRecvError::Empty));

        assert!(registry.dispatch(&data_change(second_path)));
        assert_eq!(second.try_recv(), Ok(()));

        Ok(())
    }

    #[test]
    fn deduplicates_parent_directory_watches() -> notify::Result<()> {
        let directory = PathBuf::from("/watch-tests/config");
        let mut registry = WatchRegistry::new(RecordingWatcher::default());
        let _first = add_subscriber(&mut registry, &directory.join("style.css"))?;
        let _second = add_subscriber(&mut registry, &directory.join("graph.lua"))?;
        let _third = add_subscriber(&mut registry, &directory.join("style.css"))?;

        assert_eq!(registry.watcher.watched, vec![directory]);
        assert_eq!(registry.directories.len(), 1);

        Ok(())
    }

    #[test]
    fn ignores_unrelated_and_access_events() -> notify::Result<()> {
        let path = Path::new("/watch-tests/config/style.css");
        let mut registry = WatchRegistry::new(RecordingWatcher::default());
        let mut receiver = add_subscriber(&mut registry, path)?;

        assert!(!registry.dispatch(&data_change(Path::new("/watch-tests/config/other.css",))));
        assert!(
            !registry.dispatch(
                &Event::new(EventKind::Access(notify::event::AccessKind::Read))
                    .add_path(path.to_path_buf()),
            )
        );
        assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));

        Ok(())
    }

    #[test]
    fn prunes_closed_subscribers_and_unwatches_unused_directories() -> notify::Result<()> {
        let directory = PathBuf::from("/watch-tests/config");
        let first_path = directory.join("style.css");
        let second_path = directory.join("graph.lua");
        let mut registry = WatchRegistry::new(RecordingWatcher::default());
        let first = add_subscriber(&mut registry, &first_path)?;
        let mut second = add_subscriber(&mut registry, &second_path)?;

        drop(first);
        registry.prune_closed();
        assert!(!registry.subscribers.contains_key(&first_path));
        assert!(registry.watcher.unwatched.is_empty());
        assert!(registry.dispatch(&data_change(&second_path)));
        assert_eq!(second.try_recv(), Ok(()));

        drop(second);
        registry.prune_closed();
        assert!(registry.subscribers.is_empty());
        assert_eq!(registry.watcher.unwatched, vec![directory]);

        Ok(())
    }

    #[test]
    fn matches_atomic_replace_rename_target() -> notify::Result<()> {
        let target = Path::new("/watch-tests/config/style.css");
        let temporary = Path::new("/watch-tests/config/.style.css.tmp");
        let mut registry = WatchRegistry::new(RecordingWatcher::default());
        let mut receiver = add_subscriber(&mut registry, target)?;
        let event = Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::Both)))
            .add_path(temporary.to_path_buf())
            .add_path(target.to_path_buf());

        assert!(registry.dispatch(&event));
        assert_eq!(receiver.try_recv(), Ok(()));

        Ok(())
    }

    #[test]
    fn ignores_rename_away_and_remove_until_a_replacement_arrives() -> notify::Result<()> {
        let target = Path::new("/watch-tests/config/style.css");
        let mut registry = WatchRegistry::new(RecordingWatcher::default());
        let mut receiver = add_subscriber(&mut registry, target)?;

        assert!(
            !registry.dispatch(
                &Event::new(EventKind::Modify(ModifyKind::Name(RenameMode::From)))
                    .add_path(target.to_path_buf()),
            )
        );
        assert!(!registry.dispatch(
            &Event::new(EventKind::Remove(RemoveKind::File)).add_path(target.to_path_buf()),
        ));
        assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));

        assert!(
            registry.dispatch(
                &Event::new(EventKind::Create(notify::event::CreateKind::File))
                    .add_path(target.to_path_buf()),
            )
        );
        assert_eq!(receiver.try_recv(), Ok(()));

        Ok(())
    }

    #[test]
    fn coalesces_pending_events_per_subscriber() -> notify::Result<()> {
        let path = Path::new("/watch-tests/config/style.css");
        let mut registry = WatchRegistry::new(RecordingWatcher::default());
        let mut receiver = add_subscriber(&mut registry, path)?;

        assert!(registry.dispatch(&data_change(path)));
        assert!(registry.dispatch(&data_change(path)));
        assert_eq!(receiver.try_recv(), Ok(()));
        assert_eq!(receiver.try_recv(), Err(mpsc::error::TryRecvError::Empty));

        Ok(())
    }
}
