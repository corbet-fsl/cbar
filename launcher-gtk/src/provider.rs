//! Independent, offline-first inventory providers for the resident launcher.
//!
//! Every configured machine owns one task and one retry state. The launcher never joins these
//! tasks and reveal merely broadcasts a refresh hint, so a slow SSH command cannot delay a local
//! inventory, another remote, or the GTK thread.

use std::collections::VecDeque;
use std::future::Future;
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::pin::Pin;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use cbar_launcher_core::config;
use cbar_launcher_core::model::Machine;
use sha2::{Digest, Sha256};
use tokio::io::AsyncReadExt;
use tokio::runtime::Handle;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

const MAX_INVENTORY_BYTES: usize = 16 * 1024 * 1024;
const MAX_PROVIDER_ERROR_BYTES: usize = 64 * 1024;
const MAX_TOTAL_INVENTORY_BYTES: usize = 128 * 1024 * 1024;
const MAX_TOTAL_INVENTORY_APPS: usize = 32_768;
const MAX_CONCURRENT_PROVIDER_COMMANDS: usize = 32;
const MAX_CONCURRENT_CACHE_READS: usize = 4;
const CACHE_PREPARE_WAIT: Duration = Duration::from_millis(50);
static NEXT_PROVIDER_EPOCH: AtomicU64 = AtomicU64::new(1);
static LATEST_PROVIDER_EPOCH: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
struct CacheGeneration {
    manager_epoch: u64,
    provider_revision: u64,
}

impl CacheGeneration {
    const fn new(manager_epoch: u64, provider_revision: u64) -> Self {
        Self {
            manager_epoch,
            provider_revision,
        }
    }
}

struct CacheWriteReservation {
    path: PathBuf,
    generation: CacheGeneration,
    gate: Arc<std::sync::Mutex<CacheGeneration>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderSpec {
    pub name: String,
    pub argv: Vec<String>,
    pub timeout: Duration,
    machine: config::MachineConfig,
    max_bytes: usize,
    max_apps: usize,
}

impl From<&config::MachineConfig> for ProviderSpec {
    fn from(machine: &config::MachineConfig) -> Self {
        Self {
            name: machine.name.clone(),
            argv: machine.inventory.clone(),
            timeout: Duration::from_millis(machine.inventory_timeout_ms),
            machine: machine.clone(),
            max_bytes: MAX_INVENTORY_BYTES,
            max_apps: config::MAX_INVENTORY_APPS,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderStatus {
    /// Last-known-good data loaded from the local cache before any command completed.
    Cached,
    /// A fresh answer from this provider.
    Online,
    /// This provider failed; `column` is still its last-known-good answer, if one exists.
    Offline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProviderUpdate {
    pub machine: String,
    pub column: Option<Arc<Machine>>,
    pub status: ProviderStatus,
    pub error: Option<String>,
}

type ProviderFuture = Pin<Box<dyn Future<Output = Result<Vec<u8>, String>> + Send>>;
type Runner = Arc<dyn Fn(ProviderSpec) -> ProviderFuture + Send + Sync>;

trait Cache: Send + Sync {
    fn load(&self, machine: &str, limit: usize) -> Option<Vec<u8>>;
    fn reserve_store(
        &self,
        _machine: &str,
        _generation: CacheGeneration,
    ) -> Result<Option<CacheWriteReservation>, String> {
        Ok(None)
    }
    fn store(
        &self,
        machine: &str,
        inventory: &[u8],
        generation: CacheGeneration,
        reservation: Option<CacheWriteReservation>,
    ) -> Result<(), String>;
}

#[derive(Clone)]
struct RetryPolicy {
    initial: Duration,
    maximum: Duration,
    runner_cleanup_grace: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(500),
            maximum: Duration::from_secs(60),
            runner_cleanup_grace: Duration::from_millis(250),
        }
    }
}

/// The cheap handle kept by GTK. It owns no provider result and waits for nothing.
pub struct ProviderManager {
    refresh: watch::Sender<u64>,
    tasks: Vec<JoinHandle<()>>,
}

/// A bounded newest-state stream. Each provider owns one slot, so a noisy or disconnected host
/// can replace only its own stale update and can never consume memory or back-pressure another
/// host. The GTK consumer still receives ordinary individual updates.
pub struct ProviderUpdates {
    receiver: watch::Receiver<Vec<Option<Arc<ProviderUpdate>>>>,
    seen: Vec<Option<Arc<ProviderUpdate>>>,
    pending: VecDeque<Arc<ProviderUpdate>>,
}

impl ProviderUpdates {
    fn collect_changed(&mut self) {
        let current = self.receiver.borrow_and_update().clone();
        for (index, update) in current.into_iter().enumerate() {
            let unchanged = matches!(
                (self.seen.get(index).and_then(Option::as_ref), update.as_ref()),
                (Some(previous), Some(current)) if Arc::ptr_eq(previous, current)
            ) || matches!(
                (
                    self.seen.get(index).and_then(Option::as_ref),
                    update.as_ref()
                ),
                (None, None)
            );
            if !unchanged {
                if let Some(item) = update.clone() {
                    self.pending.push_back(item);
                }
                self.seen[index] = update;
            }
        }
    }

    pub async fn recv(&mut self) -> Option<Arc<ProviderUpdate>> {
        if let Some(update) = self.pending.pop_front() {
            return Some(update);
        }
        loop {
            self.receiver.changed().await.ok()?;
            self.collect_changed();
            if let Some(update) = self.pending.pop_front() {
                return Some(update);
            }
        }
    }

    pub(crate) fn try_recv(&mut self) -> Result<Arc<ProviderUpdate>, ()> {
        if let Some(update) = self.pending.pop_front() {
            return Ok(update);
        }
        if self.receiver.has_changed().map_err(|_| ())? {
            self.collect_changed();
        }
        self.pending.pop_front().ok_or(())
    }
}

impl ProviderManager {
    pub fn start(config: &config::Config, runtime: &Handle) -> (Self, ProviderUpdates) {
        let specs = config.machines.iter().map(ProviderSpec::from).collect();
        let cache: Arc<dyn Cache> = match cache_root() {
            Some(root) => Arc::new(DiskCache::new(root)),
            None => Arc::new(NullCache),
        };
        let runner: Runner = Arc::new(move |spec| {
            Box::pin(async move {
                if spec.argv.as_slice() == ["cbar:desktop-files"] {
                    crate::desktop::inventory(&spec.name, spec.max_bytes, spec.max_apps).await
                } else {
                    run_inventory(spec).await
                }
            })
        });
        Self::start_with_shape(
            specs,
            runtime,
            cache,
            runner,
            RetryPolicy::default(),
            Arc::new(config.folder_rows()),
            Arc::new(config.subrows.clone()),
        )
    }

    #[cfg(test)]
    fn start_with(
        specs: Vec<ProviderSpec>,
        runtime: &Handle,
        cache: Arc<dyn Cache>,
        runner: Runner,
        retry: RetryPolicy,
    ) -> (Self, ProviderUpdates) {
        Self::start_with_shape(
            specs,
            runtime,
            cache,
            runner,
            retry,
            Arc::new(vec!["Other".to_string()]),
            Arc::new(Default::default()),
        )
    }

    fn start_with_shape(
        mut specs: Vec<ProviderSpec>,
        runtime: &Handle,
        cache: Arc<dyn Cache>,
        runner: Runner,
        retry: RetryPolicy,
        rows: Arc<Vec<String>>,
        subrows: Arc<std::collections::HashMap<String, Vec<config::SubRow>>>,
    ) -> (Self, ProviderUpdates) {
        let (max_bytes, max_apps) = fair_provider_limits(specs.len());
        for spec in &mut specs {
            spec.max_bytes = max_bytes;
            spec.max_apps = max_apps;
        }
        let (updates_tx, updates_rx) = watch::channel(vec![None; specs.len()]);
        let (refresh, refresh_rx) = watch::channel(0u64);
        let mut tasks = Vec::with_capacity(specs.len());
        let cache_epoch = NEXT_PROVIDER_EPOCH.fetch_add(1, Ordering::Relaxed);
        LATEST_PROVIDER_EPOCH.fetch_max(cache_epoch, Ordering::Release);
        let concurrency = Arc::new(tokio::sync::Semaphore::new(provider_concurrency(
            specs.len(),
        )));
        let cache_concurrency = cache_lane();

        for (slot, spec) in specs.into_iter().enumerate() {
            tasks.push(runtime.spawn(provider_loop(
                spec,
                refresh_rx.clone(),
                updates_tx.clone(),
                slot,
                cache.clone(),
                runner.clone(),
                retry.clone(),
                rows.clone(),
                subrows.clone(),
                concurrency.clone(),
                cache_concurrency.clone(),
                cache_epoch,
            )));
        }
        drop(updates_tx);

        let seen = vec![None; updates_rx.borrow().len()];
        (
            Self { refresh, tasks },
            ProviderUpdates {
                receiver: updates_rx,
                seen,
                pending: VecDeque::new(),
            },
        )
    }

    /// Ask every provider to refresh, without joining or polling any of them.
    pub fn refresh(&self) {
        let next = self.refresh.borrow().wrapping_add(1);
        self.refresh.send_replace(next);
    }
}

fn fair_provider_limits(machines: usize) -> (usize, usize) {
    let machines = machines.max(1);
    (
        (MAX_TOTAL_INVENTORY_BYTES / machines).clamp(1, MAX_INVENTORY_BYTES),
        (MAX_TOTAL_INVENTORY_APPS / machines).clamp(1, config::MAX_INVENTORY_APPS),
    )
}

fn provider_concurrency_for(machines: usize, parallelism: usize) -> usize {
    // Inventory discovery is mostly pipe/network I/O, so permit two independent providers per
    // available CPU while keeping a small single-core floor and a hard process-wide ceiling.
    // The machine-count minimum keeps an empty configuration harmless without manufacturing work.
    let hardware_limit = parallelism
        .max(1)
        .saturating_mul(2)
        .clamp(2, MAX_CONCURRENT_PROVIDER_COMMANDS);
    machines.max(1).min(hardware_limit)
}

fn provider_concurrency(machines: usize) -> usize {
    let parallelism = std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1);
    provider_concurrency_for(machines, parallelism)
}

fn cache_concurrency_for(parallelism: usize) -> usize {
    parallelism
        .max(1)
        .div_ceil(2)
        .clamp(1, MAX_CONCURRENT_CACHE_READS)
}

fn cache_lane() -> Arc<tokio::sync::Semaphore> {
    static CACHE_LANE: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
        std::sync::OnceLock::new();
    CACHE_LANE
        .get_or_init(|| {
            let parallelism = std::thread::available_parallelism()
                .map(std::num::NonZeroUsize::get)
                .unwrap_or(1);
            Arc::new(tokio::sync::Semaphore::new(cache_concurrency_for(
                parallelism,
            )))
        })
        .clone()
}

impl Drop for ProviderManager {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

// Each argument is one explicit piece of an independently owned machine state; packing these into
// a bag would hide rather than reduce the state-machine boundary.
#[allow(clippy::too_many_arguments)]
async fn provider_loop(
    spec: ProviderSpec,
    mut refresh: watch::Receiver<u64>,
    updates: watch::Sender<Vec<Option<Arc<ProviderUpdate>>>>,
    slot: usize,
    cache: Arc<dyn Cache>,
    runner: Runner,
    retry: RetryPolicy,
    rows: Arc<Vec<String>>,
    subrows: Arc<std::collections::HashMap<String, Vec<config::SubRow>>>,
    concurrency: Arc<tokio::sync::Semaphore>,
    cache_concurrency: Arc<tokio::sync::Semaphore>,
    cache_epoch: u64,
) {
    // Cache I/O happens inside this machine's own provider task. Starting or revealing the
    // launcher therefore performs no disk or network waits on GTK's thread.
    let cache_for_load = cache.clone();
    let machine_for_load = spec.name.clone();
    let machine_for_cache = spec.machine.clone();
    let rows_for_cache = rows.clone();
    let subrows_for_cache = subrows.clone();
    let max_bytes = spec.max_bytes;
    let max_apps = spec.max_apps;
    // Cache parsing has its own small process-global hardware-aware lane. A stalled regular/NFS
    // read can therefore neither fan out across reloads nor consume the permits which fresh local
    // and remote provider commands need to recover.
    let cached = match tokio::time::timeout(
        CACHE_PREPARE_WAIT,
        cache_concurrency.clone().acquire_owned(),
    )
    .await
    {
        Ok(Ok(cache_permit)) => {
            let task = tokio::task::spawn_blocking(move || {
                // The global permit lives in the uncancellable blocking closure. Timing out its
                // JoinHandle cannot accidentally admit more stuck NFS/regular-file reads.
                let _cache_permit = cache_permit;
                let bytes = cache_for_load.load(&machine_for_load, max_bytes)?;
                let digest = inventory_digest(&bytes);
                let column = normalized_column(
                    &machine_for_cache,
                    &bytes,
                    &rows_for_cache,
                    &subrows_for_cache,
                    max_bytes,
                    max_apps,
                )
                .ok()?;
                Some((digest, column))
            });
            match tokio::time::timeout(CACHE_PREPARE_WAIT, task).await {
                Ok(Ok(cached)) => cached,
                _ => None,
            }
        }
        _ => None,
    };
    let (mut last_digest, mut last_good) = match cached {
        Some((digest, column)) => (Some(digest), Some(column)),
        None => (None, None),
    };
    if let Some(column) = last_good.clone()
        && !publish_update(
            &updates,
            slot,
            ProviderUpdate {
                machine: spec.name.clone(),
                column: Some(column),
                status: ProviderStatus::Cached,
                error: None,
            },
        )
    {
        return;
    }
    let mut failures = 0u32;
    let mut last_was_online = false;
    let mut cache_revision = 0u64;

    loop {
        let Ok(permit) = concurrency.clone().acquire_owned().await else {
            return;
        };
        let result = tokio::time::timeout(
            spec.timeout.saturating_add(retry.runner_cleanup_grace),
            runner(spec.clone()),
        )
        .await;
        drop(permit);
        match result {
            Ok(Ok(bytes)) => {
                if bytes.len() > spec.max_bytes {
                    failures = failures.saturating_add(1);
                    last_was_online = false;
                    let error = format!(
                        "inventory exceeded this provider's fair-share limit of {} bytes ({} bytes)",
                        spec.max_bytes,
                        bytes.len(),
                    );
                    if !publish_failure(&spec, &updates, slot, &last_good, error) {
                        return;
                    }
                    if !wait_to_retry(&mut refresh, retry_delay(&retry, &spec.name, failures)).await
                    {
                        return;
                    }
                    continue;
                }
                let bytes: Arc<[u8]> = bytes.into();
                let bytes_for_digest = bytes.clone();
                let digest =
                    match tokio::task::spawn_blocking(move || inventory_digest(&bytes_for_digest))
                        .await
                    {
                        Ok(digest) => digest,
                        Err(error) => {
                            failures = failures.saturating_add(1);
                            last_was_online = false;
                            if !publish_failure(
                                &spec,
                                &updates,
                                slot,
                                &last_good,
                                format!("launcher inventory digest worker failed: {error}"),
                            ) {
                                return;
                            }
                            if !wait_to_retry(
                                &mut refresh,
                                retry_delay(&retry, &spec.name, failures),
                            )
                            .await
                            {
                                return;
                            }
                            continue;
                        }
                    };
                if last_digest == Some(digest) {
                    failures = 0;
                    if !last_was_online {
                        if !publish_update(
                            &updates,
                            slot,
                            ProviderUpdate {
                                machine: spec.name.clone(),
                                column: last_good.clone(),
                                status: ProviderStatus::Online,
                                error: None,
                            },
                        ) {
                            return;
                        }
                        last_was_online = true;
                    }
                    if refresh.changed().await.is_err() {
                        return;
                    }
                    continue;
                }
                let bytes_for_column = bytes.clone();
                let machine = spec.machine.clone();
                let rows = rows.clone();
                let subrows = subrows.clone();
                let max_bytes = spec.max_bytes;
                let max_apps = spec.max_apps;
                let column = match tokio::task::spawn_blocking(move || {
                    normalized_column(
                        &machine,
                        &bytes_for_column,
                        &rows,
                        &subrows,
                        max_bytes,
                        max_apps,
                    )
                })
                .await
                .map_err(|error| format!("launcher inventory worker failed: {error}"))
                .and_then(|column| column)
                {
                    Ok(column) => column,
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        last_was_online = false;
                        if !publish_failure(&spec, &updates, slot, &last_good, error) {
                            return;
                        }
                        if !wait_to_retry(&mut refresh, retry_delay(&retry, &spec.name, failures))
                            .await
                        {
                            return;
                        }
                        continue;
                    }
                };
                failures = 0;
                last_digest = Some(digest);
                last_good = Some(column.clone());
                last_was_online = true;
                if !publish_update(
                    &updates,
                    slot,
                    ProviderUpdate {
                        machine: spec.name.clone(),
                        column: Some(column),
                        status: ProviderStatus::Online,
                        error: None,
                    },
                ) {
                    return;
                }

                // Persistence follows publication: a slow filesystem must not delay a fresh
                // column becoming usable. The same independent bounded cache lane owns the
                // uncancellable blocking write; saturated persistence is simply skipped.
                let cache_for_store = cache.clone();
                let machine_for_store = spec.name.clone();
                let machine_for_log = machine_for_store.clone();
                let cache_concurrency = cache_concurrency.clone();
                cache_revision = cache_revision.saturating_add(1);
                let cache_generation = CacheGeneration::new(cache_epoch, cache_revision);
                let reservation = match cache_for_store
                    .reserve_store(&machine_for_store, cache_generation)
                {
                    Ok(reservation) => reservation,
                    Err(error) => {
                        warn!(machine = %machine_for_log, "unable to reserve launcher inventory cache write: {error}");
                        if refresh.changed().await.is_err() {
                            return;
                        }
                        continue;
                    }
                };
                tokio::spawn(async move {
                    let Ok(Ok(cache_permit)) =
                        tokio::time::timeout(CACHE_PREPARE_WAIT, cache_concurrency.acquire_owned())
                            .await
                    else {
                        return;
                    };
                    let stored = tokio::task::spawn_blocking(move || {
                        let _cache_permit = cache_permit;
                        cache_for_store.store(
                            &machine_for_store,
                            &bytes,
                            cache_generation,
                            reservation,
                        )
                    })
                    .await;
                    match stored {
                        Ok(Ok(())) => {}
                        Ok(Err(error)) => {
                            warn!(machine = %machine_for_log, "unable to persist launcher inventory: {error}");
                        }
                        Err(error) => {
                            warn!(machine = %machine_for_log, "launcher inventory cache worker failed: {error}");
                        }
                    }
                });

                // Fresh providers are demand-refreshed. Failed providers below retry on their own.
                if refresh.changed().await.is_err() {
                    return;
                }
            }
            Ok(Err(error)) => {
                failures = failures.saturating_add(1);
                last_was_online = false;
                if !publish_failure(&spec, &updates, slot, &last_good, error) {
                    return;
                }
                let delay = retry_delay(&retry, &spec.name, failures);
                if !wait_to_retry(&mut refresh, delay).await {
                    return;
                }
            }
            Err(_) => {
                failures = failures.saturating_add(1);
                last_was_online = false;
                let error = format!("inventory timed out after {} ms", spec.timeout.as_millis());
                if !publish_failure(&spec, &updates, slot, &last_good, error) {
                    return;
                }
                let delay = retry_delay(&retry, &spec.name, failures);
                if !wait_to_retry(&mut refresh, delay).await {
                    return;
                }
            }
        }
    }
}

/// Parse and normalize provider output away from GTK. The UI receives one bounded, ready-to-merge
/// column and never reparses JSON or regroups applications on its shared event thread.
fn normalized_column(
    machine: &config::MachineConfig,
    bytes: &[u8],
    rows: &[String],
    subrows: &std::collections::HashMap<String, Vec<config::SubRow>>,
    max_bytes: usize,
    max_apps: usize,
) -> Result<Arc<Machine>, String> {
    if bytes.len() > max_bytes {
        return Err(format!(
            "inventory exceeded this provider's fair-share limit of {max_bytes} bytes ({} bytes)",
            bytes.len(),
        ));
    }
    let inventory = config::parse_inventory_bounded(bytes, max_apps)?;
    if let Some(error) = inventory.error.as_ref() {
        return Err(error.clone());
    }
    Ok(Arc::new(crate::stream_model::machine_from(
        machine,
        Some(&inventory),
        None,
        rows,
        subrows,
    )))
}

fn inventory_digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

async fn wait_to_retry(refresh: &mut watch::Receiver<u64>, delay: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(delay) => true,
        changed = refresh.changed() => changed.is_ok(),
    }
}

fn publish_failure(
    spec: &ProviderSpec,
    updates: &watch::Sender<Vec<Option<Arc<ProviderUpdate>>>>,
    slot: usize,
    last_good: &Option<Arc<Machine>>,
    error: String,
) -> bool {
    debug!(machine = %spec.name, "launcher provider offline: {error}");
    let column = last_good.as_ref().map(|last_good| {
        let mut offline = (**last_good).clone();
        offline.error = Some(error.clone());
        Arc::new(offline)
    });
    publish_update(
        updates,
        slot,
        ProviderUpdate {
            machine: spec.name.clone(),
            column,
            status: ProviderStatus::Offline,
            error: Some(error),
        },
    )
}

fn publish_update(
    updates: &watch::Sender<Vec<Option<Arc<ProviderUpdate>>>>,
    slot: usize,
    update: ProviderUpdate,
) -> bool {
    if updates.is_closed() {
        return false;
    }
    updates.send_modify(|states| states[slot] = Some(Arc::new(update)));
    !updates.is_closed()
}

fn retry_delay(policy: &RetryPolicy, machine: &str, failures: u32) -> Duration {
    let shift = failures.saturating_sub(1).min(20);
    let factor = 1u32 << shift;
    let base = policy
        .initial
        .checked_mul(factor)
        .unwrap_or(policy.maximum)
        .min(policy.maximum);

    // Stable per-machine jitter prevents a fleet from retrying in lockstep without needing an RNG
    // (and makes the state machine deterministic under tests).
    let hash = fnv1a(machine.as_bytes()) ^ u64::from(failures);
    let spread_ms = (base.as_millis() / 4).min(u128::from(u64::MAX)) as u64;
    let jitter_ms = if spread_ms == 0 {
        0
    } else {
        hash % (spread_ms + 1)
    };
    base.saturating_sub(Duration::from_millis(spread_ms))
        .saturating_add(Duration::from_millis(jitter_ms))
}

async fn run_inventory(spec: ProviderSpec) -> Result<Vec<u8>, String> {
    let (bin, args) = spec
        .argv
        .split_first()
        .ok_or_else(|| "no inventory command configured".to_string())?;
    let mut command = tokio::process::Command::new(bin);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    command.as_std_mut().process_group(0);

    let mut child = command.spawn().map_err(|error| format!("{bin}: {error}"))?;
    let pid = child.id();
    // Declared after `child`, so cancellation drops this guard first and kills the whole process
    // group while the unreaped leader still pins its numeric PGID.
    let mut process_group = ProcessGroupGuard::new(pid);
    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| format!("{bin}: stdout pipe was not created"))?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| format!("{bin}: stderr pipe was not created"))?;
    let mut readers = ReaderTasks {
        stdout: tokio::spawn(capture_limited(stdout, "stdout", spec.max_bytes, false)),
        stderr: tokio::spawn(capture_limited(
            stderr,
            "stderr",
            MAX_PROVIDER_ERROR_BYTES,
            true,
        )),
    };

    // Drain both pipes before reaping the leader. If a descendant retains either pipe, the leader
    // remains an unreaped zombie and pins the numeric PID/PGID until the attempt timeout kills the
    // group. Reaping first would allow PGID reuse before cleanup and could signal an unrelated
    // same-user process group.
    let completed = tokio::time::timeout(spec.timeout, async {
        let (stdout, stderr) = tokio::join!(&mut readers.stdout, &mut readers.stderr);
        let stdout = stdout.map_err(|error| format!("{bin}: stdout reader failed: {error}"))??;
        let stderr = stderr.map_err(|error| format!("{bin}: stderr reader failed: {error}"))??;
        let status = child
            .wait()
            .await
            .map_err(|error| format!("{bin}: {error}"))?;
        process_group.disarm();
        Ok::<_, String>((status, stdout, stderr))
    })
    .await;

    let (status, stdout, stderr) = match completed {
        Ok(Ok(output)) => output,
        Ok(Err(error)) => {
            process_group.terminate();
            if child.wait().await.is_ok() {
                process_group.disarm();
            }
            return Err(error);
        }
        Err(_) => {
            process_group.terminate();
            if child.wait().await.is_ok() {
                process_group.disarm();
            }
            return Err(format!(
                "inventory timed out after {} ms",
                spec.timeout.as_millis()
            ));
        }
    };
    if !status.success() {
        let message = String::from_utf8_lossy(&stderr).trim().to_string();
        return Err(if message.is_empty() {
            format!("inventory exited with {status}")
        } else {
            message
        });
    }

    // Parsing and normalization happen exactly once in `normalized_column` on this provider's
    // blocking worker. The async executor owns only bounded process I/O and scheduling.
    Ok(stdout)
}

struct ProcessGroupGuard {
    pid: Option<u32>,
    armed: bool,
}

impl ProcessGroupGuard {
    fn new(pid: Option<u32>) -> Self {
        Self { pid, armed: true }
    }

    fn terminate(&self) {
        if let Some(pid) = self.pid {
            unsafe {
                libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
            }
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ProcessGroupGuard {
    fn drop(&mut self) {
        if self.armed {
            self.terminate();
        }
    }
}

/// Tokio detaches a `JoinHandle` when it is dropped. These readers instead belong to exactly one
/// inventory attempt, so every error path must cancel them; otherwise a grandchild retaining a
/// pipe could leak two tasks on every retry.
struct ReaderTasks {
    stdout: JoinHandle<Result<Vec<u8>, String>>,
    stderr: JoinHandle<Result<Vec<u8>, String>>,
}

impl Drop for ReaderTasks {
    fn drop(&mut self) {
        self.stdout.abort();
        self.stderr.abort();
    }
}

async fn capture_limited<R>(
    reader: R,
    label: &'static str,
    limit: usize,
    truncate: bool,
) -> Result<Vec<u8>, String>
where
    R: tokio::io::AsyncRead + Unpin,
{
    let mut bytes = Vec::new();
    reader
        .take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| format!("could not read inventory {label}: {error}"))?;
    if bytes.len() > limit && truncate {
        bytes.truncate(limit);
        bytes.extend_from_slice(b"\n[truncated]");
        Ok(bytes)
    } else if bytes.len() > limit {
        Err(format!("inventory {label} exceeded {limit} bytes"))
    } else {
        Ok(bytes)
    }
}

fn cache_root() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("CBAR_LAUNCHER_CACHE").filter(|path| !path.is_empty()) {
        return Some(PathBuf::from(path));
    }
    std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })
        .map(|base| base.join("cbar/launcher/inventory"))
}

struct NullCache;

impl Cache for NullCache {
    fn load(&self, _machine: &str, _limit: usize) -> Option<Vec<u8>> {
        None
    }

    fn store(
        &self,
        _machine: &str,
        _inventory: &[u8],
        _generation: CacheGeneration,
        _reservation: Option<CacheWriteReservation>,
    ) -> Result<(), String> {
        Ok(())
    }
}

struct DiskCache {
    root: PathBuf,
}

impl DiskCache {
    fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn path(&self, machine: &str) -> PathBuf {
        self.root
            .join(format!("inventory-{}.json", sha256_hex(machine.as_bytes())))
    }

    fn validate_root(&self) -> Result<(), String> {
        let metadata = std::fs::symlink_metadata(&self.root)
            .map_err(|error| format!("{}: {error}", self.root.display()))?;
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
        {
            return Err(format!(
                "refusing unsafe launcher cache root {}",
                self.root.display()
            ));
        }
        Ok(())
    }
}

impl Cache for DiskCache {
    fn load(&self, machine: &str, limit: usize) -> Option<Vec<u8>> {
        self.validate_root().ok()?;
        let path = self.path(machine);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .ok()?;
        let metadata = file.metadata().ok()?;
        let limit = limit.min(MAX_INVENTORY_BYTES);
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.len() > limit as u64
        {
            return None;
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((limit + 1) as u64).read_to_end(&mut bytes).ok()?;
        (bytes.len() <= limit).then_some(bytes)
    }

    fn reserve_store(
        &self,
        machine: &str,
        generation: CacheGeneration,
    ) -> Result<Option<CacheWriteReservation>, String> {
        reserve_cache_generation(&self.path(machine), generation).map(Some)
    }

    fn store(
        &self,
        machine: &str,
        inventory: &[u8],
        generation: CacheGeneration,
        reservation: Option<CacheWriteReservation>,
    ) -> Result<(), String> {
        if inventory.len() > MAX_INVENTORY_BYTES {
            return Err(format!("inventory exceeded {MAX_INVENTORY_BYTES} bytes"));
        }
        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(&self.root)
            .map_err(|error| format!("{}: {error}", self.root.display()))?;
        self.validate_root()?;
        std::fs::set_permissions(&self.root, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| format!("{}: {error}", self.root.display()))?;

        let path = self.path(machine);
        let reservation = match reservation {
            Some(reservation)
                if reservation.path == path && reservation.generation == generation =>
            {
                reservation
            }
            _ => reserve_cache_generation(&path, generation)?,
        };
        serialize_cache_reservation(reservation, || {
            static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
            let suffix = NEXT_TMP.fetch_add(1, Ordering::Relaxed);
            let tmp = self
                .root
                .join(format!(".inventory-{}-{suffix}.tmp", std::process::id()));
            let write = (|| -> Result<(), String> {
                let mut file = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .open(&tmp)
                    .map_err(|error| format!("{}: {error}", tmp.display()))?;
                file.write_all(inventory)
                    .and_then(|()| file.sync_all())
                    .map_err(|error| format!("{}: {error}", tmp.display()))?;
                std::fs::rename(&tmp, &path)
                    .map_err(|error| format!("{}: {error}", path.display()))?;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                    .map_err(|error| format!("{}: {error}", path.display()))
            })();
            if write.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
            write
        })
    }
}

fn reserve_cache_generation(
    path: &std::path::Path,
    generation: CacheGeneration,
) -> Result<CacheWriteReservation, String> {
    let gate = {
        let mut writes = cache_generation_registry()
            .lock()
            .map_err(|_| "launcher cache generation map was poisoned".to_string())?;
        writes.retain(|_, gate| gate.strong_count() > 0);
        match writes.get(path).and_then(std::sync::Weak::upgrade) {
            Some(gate) => gate,
            None => {
                let gate = Arc::new(std::sync::Mutex::new(CacheGeneration::default()));
                writes.insert(path.to_path_buf(), Arc::downgrade(&gate));
                gate
            }
        }
    };
    {
        let mut newest = gate.lock().map_err(|_| {
            format!(
                "launcher cache generation lock for {} was poisoned",
                path.display()
            )
        })?;
        *newest = (*newest).max(generation);
    }
    Ok(CacheWriteReservation {
        path: path.to_path_buf(),
        generation,
        gate,
    })
}

fn serialize_cache_reservation(
    reservation: CacheWriteReservation,
    write: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    serialize_cache_reservation_with(
        reservation,
        || LATEST_PROVIDER_EPOCH.load(Ordering::Acquire),
        write,
    )
}

fn serialize_cache_reservation_with(
    reservation: CacheWriteReservation,
    current_epoch: impl FnOnce() -> u64,
    write: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let CacheWriteReservation {
        path,
        generation,
        gate,
    } = reservation;
    let newest = gate.lock().map_err(|_| {
        format!(
            "launcher cache generation lock for {} was poisoned",
            path.display()
        )
    })?;
    if generation.manager_epoch < current_epoch() || generation < *newest {
        drop(newest);
        release_cache_generation_gate(&path, &gate);
        return Ok(());
    }
    let result = write();
    drop(newest);
    release_cache_generation_gate(&path, &gate);
    result
}

#[cfg(test)]
fn serialize_cache_generation_with(
    path: &std::path::Path,
    generation: CacheGeneration,
    current_epoch: impl FnOnce() -> u64,
    write: impl FnOnce() -> Result<(), String>,
) -> Result<(), String> {
    let reservation = reserve_cache_generation(path, generation)?;
    serialize_cache_reservation_with(reservation, current_epoch, write)
}

type CacheGenerationRegistry =
    std::collections::HashMap<PathBuf, std::sync::Weak<std::sync::Mutex<CacheGeneration>>>;

fn cache_generation_registry() -> &'static std::sync::Mutex<CacheGenerationRegistry> {
    static WRITES: std::sync::OnceLock<std::sync::Mutex<CacheGenerationRegistry>> =
        std::sync::OnceLock::new();
    WRITES.get_or_init(Default::default)
}

fn release_cache_generation_gate(
    path: &std::path::Path,
    gate: &Arc<std::sync::Mutex<CacheGeneration>>,
) {
    let Ok(mut writes) = cache_generation_registry().lock() else {
        return;
    };
    if Arc::strong_count(gate) == 1
        && writes
            .get(path)
            .is_some_and(|registered| std::sync::Weak::ptr_eq(registered, &Arc::downgrade(gate)))
    {
        writes.remove(path);
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let digest = Sha256::digest(bytes);
    let mut encoded = String::with_capacity(digest.len() * 2);
    for byte in digest {
        encoded.push(HEX[usize::from(byte >> 4)] as char);
        encoded.push(HEX[usize::from(byte & 0x0f)] as char);
    }
    encoded
}

fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100000001b3)
    })
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::path::Path;
    use std::sync::Mutex;

    use super::*;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(0);

    #[derive(Default)]
    struct MemoryCache(Mutex<HashMap<String, Vec<u8>>>);

    impl Cache for MemoryCache {
        fn load(&self, machine: &str, limit: usize) -> Option<Vec<u8>> {
            self.0
                .lock()
                .ok()?
                .get(machine)
                .filter(|bytes| bytes.len() <= limit)
                .cloned()
        }

        fn store(
            &self,
            machine: &str,
            inventory: &[u8],
            _generation: CacheGeneration,
            _reservation: Option<CacheWriteReservation>,
        ) -> Result<(), String> {
            self.0
                .lock()
                .map_err(|_| "cache poisoned".to_string())?
                .insert(machine.to_string(), inventory.to_vec());
            Ok(())
        }
    }

    fn valid(host: &str) -> Vec<u8> {
        format!(r#"{{"host":"{host}","folders":[]}}"#).into_bytes()
    }

    fn spec(name: &str, timeout_ms: u64) -> ProviderSpec {
        let machine = config::MachineConfig {
            name: name.into(),
            aliases: Vec::new(),
            accent: "#22C55E".into(),
            inventory: vec!["provider".into(), name.into()],
            inventory_timeout_ms: timeout_ms,
            launch: vec!["{}".into()],
        };
        ProviderSpec::from(&machine)
    }

    async fn next(rx: &mut ProviderUpdates) -> Arc<ProviderUpdate> {
        rx.recv()
            .await
            .expect("provider channel should remain open")
    }

    #[tokio::test]
    async fn zero_providers_is_a_valid_quiet_configuration() {
        let runner: Runner = Arc::new(|_| Box::pin(async { unreachable!() }));
        let (_manager, mut rx) = ProviderManager::start_with(
            Vec::new(),
            &Handle::current(),
            Arc::new(MemoryCache::default()),
            runner,
            RetryPolicy::default(),
        );
        assert!(rx.recv().await.is_none());
    }

    #[test]
    fn provider_updates_keep_only_the_newest_state_per_machine() {
        let (sender, receiver) = watch::channel(vec![None]);
        let mut updates = ProviderUpdates {
            receiver,
            seen: vec![None],
            pending: VecDeque::new(),
        };
        for attempt in 0..10_000 {
            assert!(publish_update(
                &sender,
                0,
                ProviderUpdate {
                    machine: "arbitrary-host".into(),
                    column: None,
                    status: ProviderStatus::Online,
                    error: Some(attempt.to_string()),
                }
            ));
        }
        let update = updates.try_recv().expect("newest provider state");
        assert_eq!(update.error.as_deref(), Some("9999"));
        assert!(
            updates.try_recv().is_err(),
            "intermediate states were discarded"
        );
    }

    #[tokio::test]
    async fn provider_errors_are_truncated_but_inventory_stdout_is_rejected() {
        let source = vec![b'x'; 129];
        let stderr = capture_limited(source.as_slice(), "stderr", 128, true)
            .await
            .expect("bounded diagnostic");
        assert_eq!(&stderr[..128], &source[..128]);
        assert!(stderr.ends_with(b"\n[truncated]"));

        let stdout = capture_limited(source.as_slice(), "stdout", 128, false)
            .await
            .unwrap_err();
        assert_eq!(stdout, "inventory stdout exceeded 128 bytes");
    }

    #[tokio::test(start_paused = true)]
    async fn one_timeout_never_delays_local_or_another_host_and_recovers() {
        let attempts = Arc::new(Mutex::new(HashMap::<String, usize>::new()));
        let runner: Runner = {
            let attempts = attempts.clone();
            Arc::new(move |provider| {
                let attempt = {
                    let mut attempts = attempts.lock().expect("attempt lock");
                    let count = attempts.entry(provider.name.clone()).or_default();
                    *count += 1;
                    *count
                };
                Box::pin(async move {
                    match (provider.name.as_str(), attempt) {
                        ("remote-a", 1) => {
                            tokio::time::sleep(Duration::from_secs(60)).await;
                            Ok(valid("remote-a"))
                        }
                        ("remote-b", _) => {
                            tokio::time::sleep(Duration::from_millis(2)).await;
                            Ok(valid("remote-b"))
                        }
                        ("local", _) => {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                            Ok(valid("local"))
                        }
                        ("remote-a", _) => {
                            tokio::time::sleep(Duration::from_millis(1)).await;
                            Ok(valid("remote-a"))
                        }
                        _ => unreachable!(),
                    }
                })
            })
        };
        let retry = RetryPolicy {
            initial: Duration::from_millis(10),
            maximum: Duration::from_millis(20),
            runner_cleanup_grace: Duration::ZERO,
        };
        let (_manager, mut rx) = ProviderManager::start_with(
            vec![
                spec("remote-a", 100),
                spec("local", 100),
                spec("remote-b", 100),
            ],
            &Handle::current(),
            Arc::new(MemoryCache::default()),
            runner,
            retry.clone(),
        );

        tokio::time::advance(Duration::from_millis(2)).await;
        tokio::task::yield_now().await;
        let first = next(&mut rx).await;
        let second = next(&mut rx).await;
        let ready = [first.machine.as_str(), second.machine.as_str()];
        assert!(ready.contains(&"local"), "local inventory was independent");
        assert!(ready.contains(&"remote-b"), "second remote was independent");
        assert!(
            rx.try_recv().is_err(),
            "remote-a is still pending, not a barrier"
        );

        tokio::time::advance(Duration::from_millis(98)).await;
        tokio::task::yield_now().await;
        let failed = next(&mut rx).await;
        assert_eq!(failed.machine, "remote-a");
        assert_eq!(failed.status, ProviderStatus::Offline);

        let delay = retry_delay(&retry, "remote-a", 1);
        tokio::time::advance(delay).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(1)).await;
        tokio::task::yield_now().await;
        let recovered = next(&mut rx).await;
        assert_eq!(recovered.machine, "remote-a");
        assert_eq!(recovered.status, ProviderStatus::Online);
        assert!(recovered.error.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn cached_inventory_remains_usable_and_is_marked_offline() {
        let cache = Arc::new(MemoryCache::default());
        cache
            .store("remote", &valid("remote"), CacheGeneration::new(1, 1), None)
            .expect("seed cache");
        let runner: Runner = Arc::new(|_| {
            Box::pin(async {
                tokio::time::sleep(Duration::from_secs(60)).await;
                Ok(valid("never"))
            })
        });
        let (_manager, mut rx) = ProviderManager::start_with(
            vec![spec("remote", 25)],
            &Handle::current(),
            cache,
            runner,
            RetryPolicy {
                runner_cleanup_grace: Duration::ZERO,
                ..RetryPolicy::default()
            },
        );

        let cached = next(&mut rx).await;
        assert_eq!(cached.status, ProviderStatus::Cached);
        assert!(cached.column.is_some());

        tokio::time::advance(Duration::from_millis(25)).await;
        tokio::task::yield_now().await;
        let offline = next(&mut rx).await;
        assert_eq!(offline.status, ProviderStatus::Offline);
        assert_eq!(
            offline.column.as_ref().map(|column| &column.cells),
            cached.column.as_ref().map(|column| &column.cells)
        );
        assert_eq!(
            offline
                .column
                .as_ref()
                .and_then(|column| column.error.as_ref()),
            offline.error.as_ref()
        );
        assert!(offline.error.is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn one_provider_with_an_arbitrary_name_is_not_special_cased() {
        let name = "studio / north ☃";
        let runner: Runner =
            Arc::new(|provider| Box::pin(async move { Ok(valid(&provider.name)) }));
        let (_manager, mut rx) = ProviderManager::start_with(
            vec![spec(name, 100)],
            &Handle::current(),
            Arc::new(MemoryCache::default()),
            runner,
            RetryPolicy::default(),
        );
        tokio::task::yield_now().await;
        let update = next(&mut rx).await;
        assert_eq!(update.machine, name);
        assert_eq!(update.status, ProviderStatus::Online);
    }

    #[test]
    fn backoff_is_bounded_deterministic_and_machine_jittered() {
        let policy = RetryPolicy::default();
        assert_eq!(retry_delay(&policy, "a", 4), retry_delay(&policy, "a", 4));
        assert_ne!(retry_delay(&policy, "a", 4), retry_delay(&policy, "b", 4));
        assert!(retry_delay(&policy, "a", 100) <= policy.maximum);
        assert_ne!(
            retry_delay(&policy, "a", 100),
            retry_delay(&policy, "b", 100),
            "jitter remains effective at the maximum backoff"
        );
    }

    #[test]
    fn fleet_limits_are_fair_and_process_wide_bounded() {
        for machines in [0, 1, 8, 32, 256] {
            let effective = machines.max(1);
            let (bytes, apps) = fair_provider_limits(machines);
            assert!(bytes <= MAX_INVENTORY_BYTES);
            assert!(apps <= config::MAX_INVENTORY_APPS);
            assert!(bytes.saturating_mul(effective) <= MAX_TOTAL_INVENTORY_BYTES);
            assert!(apps.saturating_mul(effective) <= MAX_TOTAL_INVENTORY_APPS);
        }
        let (_, one_apps) = fair_provider_limits(1);
        let (_, many_apps) = fair_provider_limits(256);
        assert_eq!(one_apps, config::MAX_INVENTORY_APPS);
        assert!(
            many_apps > 0,
            "every configured machine retains a fair slot"
        );
    }

    #[test]
    fn provider_concurrency_tracks_hardware_and_machine_count() {
        assert_eq!(provider_concurrency_for(0, 1), 1);
        assert_eq!(provider_concurrency_for(1, 1), 1);
        assert_eq!(provider_concurrency_for(100, 1), 2);
        assert_eq!(provider_concurrency_for(100, 8), 16);
        assert_eq!(
            provider_concurrency_for(100, usize::MAX),
            MAX_CONCURRENT_PROVIDER_COMMANDS
        );
        assert_eq!(provider_concurrency_for(3, 8), 3);
        assert_eq!(cache_concurrency_for(1), 1);
        assert_eq!(cache_concurrency_for(8), MAX_CONCURRENT_CACHE_READS);
        assert_eq!(
            cache_concurrency_for(usize::MAX),
            MAX_CONCURRENT_CACHE_READS
        );
    }

    #[tokio::test]
    async fn stalled_cache_lane_never_prevents_fresh_provider_publication() {
        struct SlowCache;
        impl Cache for SlowCache {
            fn load(&self, _machine: &str, _limit: usize) -> Option<Vec<u8>> {
                std::thread::sleep(Duration::from_millis(250));
                None
            }

            fn store(
                &self,
                _machine: &str,
                _inventory: &[u8],
                _generation: CacheGeneration,
                _reservation: Option<CacheWriteReservation>,
            ) -> Result<(), String> {
                Ok(())
            }
        }

        let runner: Runner =
            Arc::new(|provider| Box::pin(async move { Ok(valid(&provider.name)) }));
        let (_manager, mut updates) = ProviderManager::start_with(
            vec![spec("fresh-local", 500)],
            &Handle::current(),
            Arc::new(SlowCache),
            runner,
            RetryPolicy::default(),
        );
        let update = tokio::time::timeout(Duration::from_millis(150), updates.recv())
            .await
            .expect("fresh command must not wait for stuck cache I/O")
            .expect("provider update");
        assert_eq!(update.status, ProviderStatus::Online);
    }

    #[tokio::test]
    async fn stalled_cache_stores_never_backpressure_other_provider_results() {
        struct SlowStore;
        impl Cache for SlowStore {
            fn load(&self, _machine: &str, _limit: usize) -> Option<Vec<u8>> {
                None
            }

            fn store(
                &self,
                _machine: &str,
                _inventory: &[u8],
                _generation: CacheGeneration,
                _reservation: Option<CacheWriteReservation>,
            ) -> Result<(), String> {
                std::thread::sleep(Duration::from_millis(250));
                Ok(())
            }
        }

        let runner: Runner =
            Arc::new(|provider| Box::pin(async move { Ok(valid(&provider.name)) }));
        let machines = MAX_CONCURRENT_CACHE_READS * 2;
        let (_manager, mut updates) = ProviderManager::start_with(
            (0..machines)
                .map(|index| spec(&format!("store-stall-{index}"), 500))
                .collect(),
            &Handle::current(),
            Arc::new(SlowStore),
            runner,
            RetryPolicy::default(),
        );
        let all = tokio::time::timeout(Duration::from_millis(150), async {
            let mut names = HashSet::new();
            while names.len() < machines {
                names.insert(updates.recv().await.unwrap().machine.clone());
            }
            names
        })
        .await
        .expect("fresh provider results must outrun stalled cache persistence");
        assert_eq!(all.len(), machines);
    }

    #[tokio::test]
    async fn byte_identical_online_refresh_is_not_reparsed_or_republished() {
        let attempts = Arc::new(AtomicU64::new(0));
        let runner: Runner = {
            let attempts = attempts.clone();
            Arc::new(move |provider| {
                attempts.fetch_add(1, Ordering::Relaxed);
                Box::pin(async move { Ok(valid(&provider.name)) })
            })
        };
        let (manager, mut updates) = ProviderManager::start_with(
            vec![spec("stable", 500)],
            &Handle::current(),
            Arc::new(MemoryCache::default()),
            runner,
            RetryPolicy::default(),
        );
        let first = tokio::time::timeout(Duration::from_secs(2), updates.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(first.status, ProviderStatus::Online);
        manager.refresh();
        for _ in 0..100 {
            if attempts.load(Ordering::Relaxed) >= 2 {
                break;
            }
            tokio::task::yield_now().await;
        }
        assert_eq!(attempts.load(Ordering::Relaxed), 2);
        assert!(updates.try_recv().is_err(), "unchanged state stays quiet");
    }

    #[test]
    fn cache_names_do_not_expose_or_traverse_machine_names() {
        let cache = DiskCache::new(PathBuf::from("/tmp/cbar-cache-test"));
        let path = cache.path("../../remote host");
        assert_eq!(path.parent(), Some(Path::new("/tmp/cbar-cache-test")));
        assert!(path.file_name().is_some_and(|name| {
            let name = name.to_string_lossy();
            !name.contains("remote") && !name.contains("..")
        }));
        assert_ne!(
            cache.path("crafted-a"),
            cache.path("crafted-b"),
            "full SHA-256 cache keys do not cross-load independently named providers"
        );
    }

    #[test]
    fn symlinked_cache_root_is_rejected_without_writing_its_target() {
        use std::os::unix::fs::symlink;

        let base = std::env::temp_dir().join(format!(
            "cbar-cache-symlink-test-{}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let target = base.join("target");
        let root = base.join("cache");
        std::fs::create_dir_all(&target).expect("create isolated cache target");
        symlink(&target, &root).expect("create isolated cache symlink");
        let cache = DiskCache::new(root.clone());

        assert!(
            cache
                .store(
                    "machine",
                    &valid("machine"),
                    CacheGeneration::new(u64::MAX, 1),
                    None,
                )
                .is_err()
        );
        assert!(cache.load("machine", MAX_INVENTORY_BYTES).is_none());
        assert_eq!(
            std::fs::read_dir(&target).unwrap().count(),
            0,
            "the symlink target must remain untouched"
        );

        std::fs::remove_file(root).expect("remove isolated cache symlink");
        std::fs::remove_dir(target).expect("remove isolated cache target");
        std::fs::remove_dir(base).expect("remove isolated cache root");
    }

    #[test]
    fn cache_requires_private_roots_and_files() {
        let root = std::env::temp_dir().join(format!(
            "cbar-cache-mode-test-{}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        let cache = DiskCache::new(root.clone());
        assert!(
            cache
                .store(
                    "machine",
                    &valid("machine"),
                    CacheGeneration::new(u64::MAX, 1),
                    None,
                )
                .is_err()
        );

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        cache
            .store(
                "machine",
                &valid("machine"),
                CacheGeneration::new(u64::MAX, 2),
                None,
            )
            .unwrap();
        let path = cache.path("machine");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(cache.load("machine", MAX_INVENTORY_BYTES).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn older_provider_generation_cannot_overwrite_a_newer_cache() {
        use std::sync::Barrier;

        let path = std::env::temp_dir().join(format!(
            "cbar-cache-generation-test-{}-{}",
            std::process::id(),
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let writes = Arc::new(Mutex::new(Vec::new()));

        let newer = std::thread::spawn({
            let path = path.clone();
            let entered = entered.clone();
            let release = release.clone();
            let writes = writes.clone();
            move || {
                serialize_cache_generation_with(
                    &path,
                    CacheGeneration::new(2, 1),
                    || 2,
                    || {
                        entered.wait();
                        release.wait();
                        writes.lock().unwrap().push(2);
                        Ok(())
                    },
                )
                .unwrap();
            }
        });
        entered.wait();
        let older = std::thread::spawn({
            let path = path.clone();
            let writes = writes.clone();
            move || {
                serialize_cache_generation_with(
                    &path,
                    CacheGeneration::new(1, 1),
                    || 2,
                    || {
                        writes.lock().unwrap().push(1);
                        Ok(())
                    },
                )
                .unwrap();
            }
        });
        release.wait();
        newer.join().unwrap();
        older.join().unwrap();
        assert_eq!(
            *writes.lock().unwrap(),
            [2],
            "a retired manager's delayed writer must not replace a newer generation"
        );
    }

    #[test]
    fn retired_generation_is_rejected_after_newer_gate_was_evicted() {
        let path = PathBuf::from(format!(
            "/cache-generation-after-eviction-{}",
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let writes = Arc::new(Mutex::new(Vec::new()));
        serialize_cache_generation_with(&path, CacheGeneration::new(2, 1), || 2, {
            let writes = writes.clone();
            move || {
                writes.lock().unwrap().push(2);
                Ok(())
            }
        })
        .unwrap();
        assert!(
            cache_generation_registry()
                .lock()
                .unwrap()
                .get(&path)
                .is_none()
        );
        serialize_cache_generation_with(&path, CacheGeneration::new(1, 1), || 2, {
            let writes = writes.clone();
            move || {
                writes.lock().unwrap().push(1);
                Ok(())
            }
        })
        .unwrap();
        assert_eq!(*writes.lock().unwrap(), [2]);
    }

    #[test]
    fn same_manager_older_refresh_cannot_overwrite_newer_after_it_finishes() {
        let path = PathBuf::from(format!(
            "/cache-same-manager-out-of-order-{}",
            NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed)
        ));
        let writes = Arc::new(Mutex::new(Vec::new()));

        // Reservations happen when provider results are published, before either blocking cache
        // closure is admitted. The older pending reservation pins the gate while the newer write
        // runs and retires first.
        let older = reserve_cache_generation(&path, CacheGeneration::new(7, 1)).unwrap();
        let newer = reserve_cache_generation(&path, CacheGeneration::new(7, 2)).unwrap();
        serialize_cache_reservation_with(newer, || 7, {
            let writes = writes.clone();
            move || {
                writes.lock().unwrap().push(2);
                Ok(())
            }
        })
        .unwrap();
        serialize_cache_reservation_with(older, || 7, {
            let writes = writes.clone();
            move || {
                writes.lock().unwrap().push(1);
                Ok(())
            }
        })
        .unwrap();

        assert_eq!(*writes.lock().unwrap(), [2]);
        assert!(
            cache_generation_registry()
                .lock()
                .unwrap()
                .get(&path)
                .is_none()
        );
    }

    #[test]
    fn retired_cache_paths_do_not_accumulate_generation_gates() {
        for index in 0..4_096 {
            let path = PathBuf::from(format!("/cache-churn/{index}"));
            serialize_cache_generation_with(&path, CacheGeneration::new(1, 1), || 1, || Ok(()))
                .unwrap();
        }
        assert!(
            cache_generation_registry()
                .lock()
                .unwrap()
                .keys()
                .all(|path| !path.starts_with("/cache-churn")),
            "dead path gates must be evicted across arbitrary config churn"
        );
    }
}
