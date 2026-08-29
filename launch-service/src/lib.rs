//! Portable, bounded application handoff for UI callers.
//!
//! The public submission path never invokes a shell and never waits on the
//! caller. A small bounded worker pool performs launches, and the returned
//! ticket reports whether ownership moved to a user service or to the portable
//! direct backend. Direct children are owned by one bounded reaper until they
//! exit. Linux uses pidfd notifications when available; other systems and older
//! kernels use a low-frequency bounded fallback.

#![deny(clippy::unwrap_used)]

use futures_channel::oneshot;
use std::collections::VecDeque;
#[cfg(any(target_os = "linux", all(test, unix)))]
use std::ffi::OsStr;
#[cfg(any(target_os = "linux", test))]
use std::ffi::OsString;
use std::future::Future;
#[cfg(any(target_os = "linux", all(test, unix)))]
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, ExitStatus, Stdio};
#[cfg(any(target_os = "linux", test))]
use std::sync::MutexGuard;
#[cfg(any(target_os = "linux", test))]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex, OnceLock, mpsc};
use std::task::{Context, Poll};
use std::thread;
use std::time::Duration;
#[cfg(any(target_os = "linux", test))]
use std::time::Instant;
use thiserror::Error;

const MIN_LAUNCH_WORKERS: usize = 2;
const MAX_LAUNCH_WORKERS: usize = 32;
const QUEUED_REQUESTS_PER_WORKER: usize = 4;
const MIN_QUEUED_REQUESTS: usize = 64;
const MAX_QUEUED_REQUESTS: usize = 128;
const MAX_ARGV_ENTRIES: usize = 4_096;
const MAX_ARGV_BYTES: usize = 256 * 1024;
const MAX_BATCH_REQUESTS: usize = 64;
const MAX_BATCH_ARGV_ENTRIES: usize = 32_768;
const MAX_BATCH_ARGV_BYTES: usize = 4 * 1024 * 1024;
const MAX_QUEUED_ARGV_ENTRIES: usize = MAX_BATCH_ARGV_ENTRIES;
const MAX_QUEUED_ARGV_BYTES: usize = MAX_BATCH_ARGV_BYTES;
const MAX_DIRECT_CHILDREN: usize = 512;
const REAPER_FALLBACK_TICK: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const USER_SERVICE_ACK_TIMEOUT: Duration = Duration::from_secs(2);
#[cfg(target_os = "linux")]
const USER_SERVICE_PROBE_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(target_os = "linux")]
const USER_SERVICE_RESOLUTION_TIMEOUT: Duration = Duration::from_secs(1);
#[cfg(any(target_os = "linux", test))]
const USER_SERVICE_RETRY_AFTER: Duration = Duration::from_secs(30);

static DIRECT_CHILDREN: AtomicUsize = AtomicUsize::new(0);
static LAUNCH_WORKER: OnceLock<Option<LaunchWorker>> = OnceLock::new();
static DIRECT_REAPER: OnceLock<Option<DirectReaper>> = OnceLock::new();
#[cfg(any(target_os = "linux", test))]
static USER_SERVICE_CAPABILITY: OnceLock<UserServiceCapability> = OnceLock::new();

/// Successful ownership transfer. A direct child remains owned and reaped by
/// this crate; callers must not attempt to wait for it themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchReceipt {
    UserService,
    Direct,
}

/// A nonblocking submission outcome. Awaiting it does not occupy a runtime
/// blocking worker and is safe from an asynchronous UI controller.
#[derive(Debug)]
pub struct LaunchTicket {
    outcome: oneshot::Receiver<Result<LaunchReceipt, LaunchError>>,
}

impl Future for LaunchTicket {
    type Output = Result<LaunchReceipt, LaunchError>;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.outcome).poll(context) {
            Poll::Ready(Ok(outcome)) => Poll::Ready(outcome),
            Poll::Ready(Err(_)) => Poll::Ready(Err(LaunchError::WorkerStopped)),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[derive(Debug, Error)]
pub enum LaunchError {
    #[error("launch argument vector is empty or has an empty program")]
    EmptyArgv,
    #[error(
        "launch argument vector exceeds the limit of {max_entries} entries or {max_bytes} bytes"
    )]
    ArgvTooLarge {
        max_entries: usize,
        max_bytes: usize,
    },
    #[error(
        "launch batch exceeds the limit of {max_requests} requests, {max_entries} entries, or {max_bytes} bytes"
    )]
    BatchTooLarge {
        max_requests: usize,
        max_entries: usize,
        max_bytes: usize,
    },
    #[error(
        "detached launch queue cannot atomically reserve this batch (limits: {max_requests} requests, {max_entries} entries, {max_bytes} bytes)"
    )]
    QueueFull {
        max_requests: usize,
        max_entries: usize,
        max_bytes: usize,
    },
    #[error("detached launch worker is unavailable")]
    WorkerUnavailable,
    #[error("detached launch worker stopped before reporting an outcome")]
    WorkerStopped,
    #[error("direct child limit reached ({limit})")]
    DirectCapacity { limit: usize },
    #[error("direct child reaper is unavailable")]
    ReaperUnavailable,
    #[error("failed to start {program}: {source}")]
    Spawn {
        program: String,
        #[source]
        source: std::io::Error,
    },
    #[error("transient service handoff exited with {status}")]
    HandoffRejected { status: ExitStatus },
    #[error(
        "transient service handoff acknowledgement timed out after {millis} ms; the service may already have been accepted and was not retried"
    )]
    HandoffTimeout { millis: u128 },
    #[error("transient service handoff failed: {source}")]
    HandoffWait {
        #[source]
        source: std::io::Error,
    },
}

#[derive(Debug)]
struct LaunchJob {
    argv: Vec<String>,
    cost: ArgvCost,
    outcome: oneshot::Sender<Result<LaunchReceipt, LaunchError>>,
}

#[derive(Debug)]
struct LaunchWorker {
    queue: Arc<LaunchQueue>,
}

impl LaunchWorker {
    fn start() -> Option<Self> {
        let worker_count = launch_worker_count(
            std::thread::available_parallelism()
                .ok()
                .map(std::num::NonZeroUsize::get),
        );
        let capacity = launch_queue_capacity(worker_count);
        let queue = Arc::new(LaunchQueue::new(capacity));
        for index in 0..worker_count {
            let thread_queue = queue.clone();
            if thread::Builder::new()
                .name(format!("cbar-launch-worker-{index}"))
                .spawn(move || launch_worker(thread_queue))
                .is_err()
            {
                queue.stop();
                return None;
            }
        }
        Some(Self { queue })
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct ArgvCost {
    requests: usize,
    entries: usize,
    bytes: usize,
}

impl ArgvCost {
    fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            requests: self.requests.checked_add(other.requests)?,
            entries: self.entries.checked_add(other.entries)?,
            bytes: self.bytes.checked_add(other.bytes)?,
        })
    }
}

#[derive(Debug, Default)]
struct LaunchQueueState {
    jobs: VecDeque<LaunchJob>,
    cost: ArgvCost,
    stopped: bool,
}

#[derive(Debug)]
struct LaunchQueue {
    state: Mutex<LaunchQueueState>,
    ready: Condvar,
    max_requests: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl LaunchQueue {
    fn new(max_requests: usize) -> Self {
        Self {
            state: Mutex::new(LaunchQueueState::default()),
            ready: Condvar::new(),
            max_requests,
            max_entries: MAX_QUEUED_ARGV_ENTRIES,
            max_bytes: MAX_QUEUED_ARGV_BYTES,
        }
    }

    fn enqueue_batch(&self, jobs: Vec<LaunchJob>, cost: ArgvCost) -> Result<(), LaunchError> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.stopped {
            return Err(LaunchError::WorkerStopped);
        }
        let Some(reserved) = state.cost.checked_add(cost) else {
            return Err(self.full_error());
        };
        if reserved.requests > self.max_requests
            || reserved.entries > self.max_entries
            || reserved.bytes > self.max_bytes
        {
            return Err(self.full_error());
        }

        // The whole batch and its accounting become visible under one lock.
        // No worker can consume an earlier item if admission later fails.
        state.cost = reserved;
        state.jobs.extend(jobs);
        drop(state);
        self.ready.notify_all();
        Ok(())
    }

    fn pop(&self) -> Option<LaunchJob> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(job) = state.jobs.pop_front() {
                state.cost.requests = state.cost.requests.saturating_sub(job.cost.requests);
                state.cost.entries = state.cost.entries.saturating_sub(job.cost.entries);
                state.cost.bytes = state.cost.bytes.saturating_sub(job.cost.bytes);
                return Some(job);
            }
            if state.stopped {
                return None;
            }
            state = self
                .ready
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    fn stop(&self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .stopped = true;
        self.ready.notify_all();
    }

    fn full_error(&self) -> LaunchError {
        LaunchError::QueueFull {
            max_requests: self.max_requests,
            max_entries: self.max_entries,
            max_bytes: self.max_bytes,
        }
    }
}

fn launch_worker(queue: Arc<LaunchQueue>) {
    while let Some(job) = queue.pop() {
        let result = spawn_detached_argv(&job.argv);
        let _ = job.outcome.send(result);
    }
}

fn launch_worker_count(parallelism: Option<usize>) -> usize {
    parallelism
        .unwrap_or(1)
        .saturating_mul(2)
        .clamp(MIN_LAUNCH_WORKERS, MAX_LAUNCH_WORKERS)
}

fn launch_queue_capacity(worker_count: usize) -> usize {
    worker_count
        .saturating_mul(QUEUED_REQUESTS_PER_WORKER)
        .clamp(MIN_QUEUED_REQUESTS, MAX_QUEUED_REQUESTS)
}

/// Enqueues one exact owned argv without blocking the caller.
///
/// This is a one-item wrapper around [`submit_detached_batch`].
pub fn submit_detached_argv(argv: Vec<String>) -> Result<LaunchTicket, LaunchError> {
    let mut tickets = submit_detached_batch(vec![argv])?;
    tickets.pop().ok_or(LaunchError::WorkerStopped)
}

/// Atomically validates and enqueues a batch of exact owned argument vectors.
///
/// Either every request is reserved and returned in input order, or none can
/// be observed by a launch worker. Each ticket independently reports its own
/// manager/direct handoff result.
pub fn submit_detached_batch(batch: Vec<Vec<String>>) -> Result<Vec<LaunchTicket>, LaunchError> {
    let cost = validate_batch(&batch)?;
    let worker = LAUNCH_WORKER
        .get_or_init(LaunchWorker::start)
        .as_ref()
        .ok_or(LaunchError::WorkerUnavailable)?;

    let mut jobs = Vec::with_capacity(batch.len());
    let mut tickets = Vec::with_capacity(batch.len());
    for argv in batch {
        let job_cost = argv_cost(&argv)?;
        let (outcome, ticket) = oneshot::channel();
        jobs.push(LaunchJob {
            argv,
            cost: job_cost,
            outcome,
        });
        tickets.push(LaunchTicket { outcome: ticket });
    }
    worker.queue.enqueue_batch(jobs, cost)?;
    Ok(tickets)
}

/// Prepares the bounded worker and starts capability discovery during module
/// initialization, keeping even first-click setup away from the event path.
pub fn warm_launch_service() {
    let _ = LAUNCH_WORKER.get_or_init(LaunchWorker::start);
    warm_user_service_capability();
}

/// Synchronous worker primitive. Prefer [`submit_detached_argv`] from UI code.
/// A manager acknowledgement is hard-bounded; direct handoff returns as soon
/// as the child is safely owned by the reaper.
pub fn spawn_detached_argv(argv: &[String]) -> Result<LaunchReceipt, LaunchError> {
    let (program, arguments) = validate_argv(argv)?;

    #[cfg(target_os = "linux")]
    {
        let Some(environment_names) = service_environment_names() else {
            // An exotic environment cannot be represented losslessly by the
            // manager helper, so direct inheritance is definitively required.
            return spawn_direct(program, arguments);
        };
        let Some(manager_program) = resolve_manager_program(program) else {
            return spawn_direct(program, arguments);
        };
        if resolve_user_service_capability(environment_names.clone()) {
            return spawn_user_service(&manager_program, program, arguments, &environment_names);
        }
    }

    spawn_direct(program, arguments)
}

fn validate_argv(argv: &[String]) -> Result<(&str, &[String]), LaunchError> {
    let (program, arguments) = argv.split_first().ok_or(LaunchError::EmptyArgv)?;
    if program.is_empty() {
        return Err(LaunchError::EmptyArgv);
    }
    argv_cost(argv)?;
    Ok((program, arguments))
}

fn argv_cost(argv: &[String]) -> Result<ArgvCost, LaunchError> {
    let bytes = argv
        .iter()
        .try_fold(0usize, |total, argument| total.checked_add(argument.len()));
    if argv.is_empty()
        || argv.first().is_none_or(String::is_empty)
        || argv.len() > MAX_ARGV_ENTRIES
        || bytes.is_none_or(|bytes| bytes > MAX_ARGV_BYTES)
    {
        if argv.is_empty() || argv.first().is_none_or(String::is_empty) {
            return Err(LaunchError::EmptyArgv);
        }
        return Err(LaunchError::ArgvTooLarge {
            max_entries: MAX_ARGV_ENTRIES,
            max_bytes: MAX_ARGV_BYTES,
        });
    }
    Ok(ArgvCost {
        requests: 1,
        entries: argv.len(),
        bytes: bytes.unwrap_or_default(),
    })
}

fn validate_batch(batch: &[Vec<String>]) -> Result<ArgvCost, LaunchError> {
    let mut cost = ArgvCost::default();
    for argv in batch {
        cost = cost
            .checked_add(argv_cost(argv)?)
            .ok_or(LaunchError::BatchTooLarge {
                max_requests: MAX_BATCH_REQUESTS,
                max_entries: MAX_BATCH_ARGV_ENTRIES,
                max_bytes: MAX_BATCH_ARGV_BYTES,
            })?;
    }
    if cost.requests == 0 {
        return Err(LaunchError::EmptyArgv);
    }
    if cost.requests > MAX_BATCH_REQUESTS
        || cost.entries > MAX_BATCH_ARGV_ENTRIES
        || cost.bytes > MAX_BATCH_ARGV_BYTES
    {
        return Err(LaunchError::BatchTooLarge {
            max_requests: MAX_BATCH_REQUESTS,
            max_entries: MAX_BATCH_ARGV_ENTRIES,
            max_bytes: MAX_BATCH_ARGV_BYTES,
        });
    }
    Ok(cost)
}

/// Starts one cached, bounded capability probe. Unknown and expired states
/// start one off-thread probe; concurrent callers coalesce behind it. A cached
/// unavailable result selects direct launch only until its retry deadline.
pub fn warm_user_service_capability() {
    #[cfg(target_os = "linux")]
    {
        let Some(environment_names) = service_environment_names() else {
            capability().set_unavailable();
            return;
        };
        let Some(probe_executable) = resolve_probe_executable() else {
            capability().set_unavailable();
            return;
        };
        start_capability_probe(environment_names, probe_executable);
    }
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapabilityState {
    Unknown,
    Probing,
    Unavailable { retry_at: Instant },
    Available,
}

#[cfg(any(target_os = "linux", test))]
#[derive(Debug)]
struct UserServiceCapability {
    state: Mutex<CapabilityState>,
    changed: Condvar,
    probe_running: AtomicBool,
    probe_generation: AtomicUsize,
}

#[cfg(any(target_os = "linux", test))]
impl UserServiceCapability {
    fn new(state: CapabilityState) -> Self {
        Self {
            state: Mutex::new(state),
            changed: Condvar::new(),
            probe_running: AtomicBool::new(false),
            probe_generation: AtomicUsize::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, CapabilityState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_unavailable(&self) {
        if !self.probe_running.load(Ordering::Acquire) {
            self.probe_generation.fetch_add(1, Ordering::AcqRel);
        }
        *self.lock() = CapabilityState::Unavailable {
            retry_at: Instant::now() + USER_SERVICE_RETRY_AFTER,
        };
        self.changed.notify_all();
    }

    fn invalidate(&self) {
        if !self.probe_running.load(Ordering::Acquire) {
            self.probe_generation.fetch_add(1, Ordering::AcqRel);
        }
        *self.lock() = CapabilityState::Unknown;
        self.changed.notify_all();
    }

    fn begin_probe(&self) -> Option<usize> {
        if self
            .probe_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return None;
        }

        let mut state = self.lock();
        let eligible = match *state {
            CapabilityState::Available | CapabilityState::Probing => false,
            CapabilityState::Unavailable { retry_at } => Instant::now() >= retry_at,
            CapabilityState::Unknown => true,
        };
        if !eligible {
            self.probe_running.store(false, Ordering::Release);
            return None;
        }

        let generation = self.probe_generation.fetch_add(1, Ordering::AcqRel) + 1;
        *state = CapabilityState::Probing;
        Some(generation)
    }

    fn finish_probe(&self, generation: usize, available: bool) {
        if self.probe_generation.load(Ordering::Acquire) != generation {
            return;
        }
        let mut state = self.lock();
        if self.probe_generation.load(Ordering::Acquire) != generation {
            return;
        }
        *state = if available {
            CapabilityState::Available
        } else {
            CapabilityState::Unavailable {
                retry_at: Instant::now() + USER_SERVICE_RETRY_AFTER,
            }
        };
        self.probe_running.store(false, Ordering::Release);
        self.changed.notify_all();
    }

    fn mark_probe_stalled(&self, generation: usize) {
        if self.probe_generation.load(Ordering::Acquire) != generation
            || !self.probe_running.load(Ordering::Acquire)
        {
            return;
        }
        let mut state = self.lock();
        if self.probe_generation.load(Ordering::Acquire) == generation
            && self.probe_running.load(Ordering::Acquire)
            && matches!(*state, CapabilityState::Probing)
        {
            *state = CapabilityState::Unavailable {
                retry_at: Instant::now() + USER_SERVICE_RETRY_AFTER,
            };
            // Keep probe_running set: at most one wedged Command::spawn can
            // exist, while launches receive a definitive portable fallback.
            self.changed.notify_all();
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn capability() -> &'static UserServiceCapability {
    USER_SERVICE_CAPABILITY.get_or_init(|| UserServiceCapability::new(CapabilityState::Unknown))
}

#[cfg(target_os = "linux")]
fn start_capability_probe(environment_names: Vec<String>, probe_executable: String) {
    let shared = capability();
    let Some(generation) = shared.begin_probe() else {
        return;
    };

    let started = thread::Builder::new()
        .name("cbar-user-service-probe".to_string())
        .spawn(move || {
            let mut command = user_service_command(&probe_executable, &[], &environment_names);
            configure_child(&mut command);
            let supported = command
                .spawn()
                .ok()
                .and_then(|mut child| wait_handoff(&mut child, USER_SERVICE_PROBE_TIMEOUT).ok())
                .is_some_and(|status| status.success());
            shared.finish_probe(generation, supported);
        })
        .is_ok();
    if !started {
        shared.finish_probe(generation, false);
        return;
    }

    let watchdog_started = thread::Builder::new()
        .name("cbar-user-service-watchdog".to_string())
        .spawn(move || {
            thread::sleep(USER_SERVICE_RESOLUTION_TIMEOUT);
            shared.mark_probe_stalled(generation);
        })
        .is_ok();
    if !watchdog_started {
        shared.mark_probe_stalled(generation);
    }
}

#[cfg(target_os = "linux")]
fn resolve_user_service_capability(environment_names: Vec<String>) -> bool {
    let Some(probe_executable) = resolve_probe_executable() else {
        capability().set_unavailable();
        return false;
    };
    start_capability_probe(environment_names, probe_executable);
    wait_for_capability(capability(), USER_SERVICE_RESOLUTION_TIMEOUT)
}

#[cfg(any(target_os = "linux", test))]
fn wait_for_capability(capability: &UserServiceCapability, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    let mut state = capability.lock();
    loop {
        match *state {
            CapabilityState::Available => return true,
            CapabilityState::Unavailable { .. } => return false,
            CapabilityState::Unknown | CapabilityState::Probing => {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    *state = CapabilityState::Unavailable {
                        retry_at: Instant::now() + USER_SERVICE_RETRY_AFTER,
                    };
                    capability.changed.notify_all();
                    return false;
                };
                let waited = capability.changed.wait_timeout(state, remaining);
                let (next, timed_out) = match waited {
                    Ok((state, timeout)) => (state, timeout.timed_out()),
                    Err(poisoned) => {
                        let (state, timeout) = poisoned.into_inner();
                        (state, timeout.timed_out())
                    }
                };
                state = next;
                if timed_out && matches!(*state, CapabilityState::Probing) {
                    // Only the harmless capability probe has been submitted;
                    // the requested app has not. Selecting the direct backend
                    // here is duplicate-safe and prevents a stuck user manager
                    // from swallowing the click.
                    *state = CapabilityState::Unavailable {
                        retry_at: Instant::now() + USER_SERVICE_RETRY_AFTER,
                    };
                    capability.changed.notify_all();
                    return false;
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn spawn_user_service(
    manager_program: &str,
    direct_program: &str,
    arguments: &[String],
    environment_names: &[String],
) -> Result<LaunchReceipt, LaunchError> {
    let mut command = user_service_command(manager_program, arguments, environment_names);
    configure_child(&mut command);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(source) => {
            capability().set_unavailable();
            // No helper process exists, so no manager request can have been
            // accepted. This pre-accept failure is the one safe same-click
            // fallback and keeps systemd absence from swallowing launches.
            tracing::warn!(
                ?source,
                "user-service helper disappeared; using direct launch"
            );
            return spawn_direct(direct_program, arguments);
        }
    };
    let result = wait_handoff(&mut child, USER_SERVICE_ACK_TIMEOUT);
    finish_user_service_handoff(capability(), result)
}

#[cfg(any(target_os = "linux", test))]
fn finish_user_service_handoff(
    shared: &UserServiceCapability,
    result: Result<ExitStatus, LaunchError>,
) -> Result<LaunchReceipt, LaunchError> {
    match result {
        Ok(status) if status.success() => Ok(LaunchReceipt::UserService),
        Ok(status) => {
            // A nonzero helper status can describe this payload (for example
            // an ExecStart permission failure), not the manager capability.
            // Reprobe for the next click instead of forcing unrelated apps to
            // the direct backend for the negative capability TTL.
            shared.invalidate();
            Err(LaunchError::HandoffRejected { status })
        }
        Err(error) => {
            // Never retry this click directly: a timed-out or failed helper may
            // already have delivered StartTransientUnit to the manager. Make
            // the next click reprobe rather than caching a generic transport
            // failure as definitive manager absence.
            shared.invalidate();
            Err(error)
        }
    }
}

#[cfg(any(target_os = "linux", test))]
fn user_service_command(
    program: &str,
    arguments: &[String],
    environment_names: &[String],
) -> Command {
    let mut command = Command::new("systemd-run");
    command.args([
        "--user",
        "--collect",
        "--quiet",
        "--service-type=exec",
        "--expand-environment=no",
        "--same-dir",
    ]);
    for name in environment_names {
        command.arg(format!("--setenv={name}"));
    }
    command.args(["--", program]);
    command.args(arguments);
    command
}

fn direct_command(program: &str, arguments: &[String]) -> Command {
    let mut command = Command::new(program);
    command.args(arguments);
    command
}

fn configure_child(command: &mut Command) {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
}

#[cfg(any(target_os = "linux", test))]
fn wait_handoff(child: &mut Child, timeout: Duration) -> Result<ExitStatus, LaunchError> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if started.elapsed() < timeout => thread::sleep(Duration::from_millis(5)),
            Ok(None) => {
                terminate_and_reap(child);
                return Err(LaunchError::HandoffTimeout {
                    millis: timeout.as_millis(),
                });
            }
            Err(source) => {
                terminate_and_reap(child);
                return Err(LaunchError::HandoffWait { source });
            }
        }
    }
}

fn terminate_and_reap(child: &mut Child) {
    #[cfg(unix)]
    {
        // The leader has not been reaped, so its process-group id cannot have
        // been recycled before this best-effort cleanup signal.
        unsafe {
            libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
    }
    let _ = child.wait();
}

fn spawn_direct(program: &str, arguments: &[String]) -> Result<LaunchReceipt, LaunchError> {
    let reaper = DIRECT_REAPER
        .get_or_init(DirectReaper::start)
        .as_ref()
        .ok_or(LaunchError::ReaperUnavailable)?;
    let slot = DirectSlot::reserve()?;
    let mut command = direct_command(program, arguments);
    configure_child(&mut command);
    let child = command.spawn().map_err(|source| LaunchError::Spawn {
        program: program.to_string(),
        source,
    })?;
    let reaped = ReapedChild::new(child, program.to_string());
    match reaper.submit(reaped) {
        Ok(()) => {
            slot.transfer();
            Ok(LaunchReceipt::Direct)
        }
        Err(TrySendError::Full(error)) | Err(TrySendError::Disconnected(error)) => {
            // Capacity is reserved before spawn, so this is an internal reaper
            // failure rather than ordinary saturation. Do not abandon a Child.
            let mut child = error.child;
            terminate_and_reap(&mut child);
            Err(LaunchError::ReaperUnavailable)
        }
    }
}

#[derive(Debug)]
struct DirectSlot {
    owned: bool,
}

impl DirectSlot {
    fn reserve() -> Result<Self, LaunchError> {
        reserve_counter(&DIRECT_CHILDREN, MAX_DIRECT_CHILDREN)?;
        Ok(Self { owned: true })
    }

    fn transfer(mut self) {
        self.owned = false;
    }
}

impl Drop for DirectSlot {
    fn drop(&mut self) {
        if self.owned {
            DIRECT_CHILDREN.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn reserve_counter(counter: &AtomicUsize, limit: usize) -> Result<(), LaunchError> {
    counter
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            (current < limit).then_some(current + 1)
        })
        .map(|_| ())
        .map_err(|_| LaunchError::DirectCapacity { limit })
}

#[derive(Debug)]
struct ReapedChild {
    child: Child,
    program: String,
    #[cfg(target_os = "linux")]
    pidfd: Option<std::os::fd::OwnedFd>,
}

impl ReapedChild {
    fn new(child: Child, program: String) -> Self {
        #[cfg(target_os = "linux")]
        let pidfd = open_pidfd(&child);
        Self {
            child,
            program,
            #[cfg(target_os = "linux")]
            pidfd,
        }
    }
}

#[derive(Debug)]
struct DirectReaper {
    tx: SyncSender<ReapedChild>,
    #[cfg(target_os = "linux")]
    wake: Option<Arc<std::os::fd::OwnedFd>>,
}

impl DirectReaper {
    fn start() -> Option<Self> {
        let (tx, rx) = sync_channel::<ReapedChild>(MAX_DIRECT_CHILDREN);
        #[cfg(target_os = "linux")]
        let wake = open_eventfd().map(Arc::new);
        #[cfg(target_os = "linux")]
        let thread_wake = wake.clone();
        thread::Builder::new()
            .name("cbar-child-reaper".to_string())
            .spawn(move || {
                reap_children(
                    rx,
                    #[cfg(target_os = "linux")]
                    thread_wake,
                )
            })
            .ok()?;
        Some(Self {
            tx,
            #[cfg(target_os = "linux")]
            wake,
        })
    }

    fn submit(&self, child: ReapedChild) -> Result<(), TrySendError<ReapedChild>> {
        self.tx.try_send(child)?;
        #[cfg(target_os = "linux")]
        if let Some(wake) = &self.wake {
            signal_eventfd(wake);
        }
        Ok(())
    }
}

fn reap_children(
    rx: Receiver<ReapedChild>,
    #[cfg(target_os = "linux")] wake: Option<Arc<std::os::fd::OwnedFd>>,
) {
    let mut children = Vec::new();
    let mut disconnected = false;
    loop {
        if children.is_empty() {
            if disconnected {
                break;
            }
            match rx.recv() {
                Ok(child) => children.push(child),
                Err(_) => break,
            }
        } else {
            #[cfg(target_os = "linux")]
            wait_for_linux_child_event(&children, wake.as_deref());
            #[cfg(not(target_os = "linux"))]
            match rx.recv_timeout(REAPER_FALLBACK_TICK) {
                Ok(child) => children.push(child),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => disconnected = true,
            }
        }

        #[cfg(target_os = "linux")]
        if let Some(wake) = wake.as_deref() {
            drain_eventfd(wake);
        }
        loop {
            match rx.try_recv() {
                Ok(child) => children.push(child),
                Err(mpsc::TryRecvError::Empty) => break,
                Err(mpsc::TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }

        let mut index = children.len();
        while index > 0 {
            index -= 1;
            let remove = match children[index].child.try_wait() {
                Ok(Some(status)) => {
                    if !status.success() {
                        tracing::warn!(
                            ?status,
                            program = %children[index].program,
                            "detached direct child exited unsuccessfully"
                        );
                    }
                    true
                }
                Ok(None) => false,
                Err(error) => {
                    tracing::error!(
                        ?error,
                        program = %children[index].program,
                        "detached child is no longer waitable; releasing reaper ownership"
                    );
                    // ECHILD is expected if a host globally ignores SIGCHLD or
                    // another process-wide handler reaped this PID. Other
                    // persistent wait errors cannot be repaired by hot polling;
                    // releasing the bounded slot avoids permanently wedging all
                    // future launches.
                    true
                }
            };
            if remove {
                children.swap_remove(index);
                DIRECT_CHILDREN.fetch_sub(1, Ordering::AcqRel);
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn open_pidfd(child: &Child) -> Option<std::os::fd::OwnedFd> {
    // pidfd_open is intentionally a best-effort optimization. ENOSYS, EPERM,
    // old seccomp policies, and kernels without pidfds all retain the periodic
    // portable fallback.
    rustix::process::pidfd_open(
        rustix::process::Pid::from_child(child),
        rustix::process::PidfdFlags::empty(),
    )
    .ok()
}

#[cfg(target_os = "linux")]
fn open_eventfd() -> Option<std::os::fd::OwnedFd> {
    rustix::event::eventfd(
        0,
        rustix::event::EventfdFlags::CLOEXEC | rustix::event::EventfdFlags::NONBLOCK,
    )
    .ok()
}

#[cfg(target_os = "linux")]
fn signal_eventfd(fd: &std::os::fd::OwnedFd) {
    let value = 1_u64.to_ne_bytes();
    let _ = rustix::io::write(fd, &value);
}

#[cfg(target_os = "linux")]
fn drain_eventfd(fd: &std::os::fd::OwnedFd) {
    let mut value = [0_u8; std::mem::size_of::<u64>()];
    while rustix::io::read(fd, &mut value).is_ok() {}
}

#[cfg(target_os = "linux")]
fn wait_for_linux_child_event(children: &[ReapedChild], wake: Option<&std::os::fd::OwnedFd>) {
    use rustix::event::{PollFd, PollFlags, Timespec, poll};

    let mut pollfds = Vec::with_capacity(children.len() + if wake.is_some() { 1 } else { 0 });
    if let Some(wake) = wake {
        pollfds.push(PollFd::new(wake, PollFlags::IN));
    }
    pollfds.extend(children.iter().filter_map(|child| {
        child
            .pidfd
            .as_ref()
            .map(|pidfd| PollFd::new(pidfd, PollFlags::IN))
    }));

    if pollfds.is_empty() {
        thread::sleep(REAPER_FALLBACK_TICK);
        return;
    }
    let timeout = Timespec {
        tv_sec: REAPER_FALLBACK_TICK.as_secs() as i64,
        tv_nsec: 0,
    };
    let _ = poll(&mut pollfds, Some(&timeout));
}

#[cfg(target_os = "linux")]
fn resolve_probe_executable() -> Option<String> {
    resolve_manager_program("true")
}

#[cfg(target_os = "linux")]
fn resolve_manager_program(program: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;

    let path = std::env::var_os("PATH");
    let cwd = std::env::current_dir().ok()?;
    resolve_manager_program_from(program, path.as_deref(), &cwd, |candidate| {
        let metadata = candidate.metadata().ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .then(|| candidate.to_path_buf())
    })
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn resolve_manager_program_from(
    program: &str,
    path: Option<&OsStr>,
    cwd: &Path,
    mut resolve: impl FnMut(&Path) -> Option<PathBuf>,
) -> Option<String> {
    let program_path = Path::new(program);
    if program.as_bytes().contains(&b'/') {
        let candidate = if program_path.is_absolute() {
            program_path.to_path_buf()
        } else {
            cwd.join(program_path)
        };
        return absolute_utf8_path(resolve(&candidate)?);
    }

    find_executable_in_path(OsStr::new(program), path?, cwd, resolve)
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn find_executable_in_path(
    program: &OsStr,
    path: &OsStr,
    cwd: &Path,
    mut resolve: impl FnMut(&Path) -> Option<PathBuf>,
) -> Option<String> {
    for directory in std::env::split_paths(path) {
        let directory = if directory.as_os_str().is_empty() {
            cwd.to_path_buf()
        } else if directory.is_absolute() {
            directory
        } else {
            cwd.join(directory)
        };
        let Some(resolved) = resolve(&directory.join(program)) else {
            continue;
        };
        if let Some(resolved) = absolute_utf8_path(resolved) {
            return Some(resolved);
        }
    }
    None
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn absolute_utf8_path(path: PathBuf) -> Option<String> {
    path.is_absolute()
        .then(|| path.into_os_string().into_string().ok())?
}

/// Names whose caller values systemd-run can copy losslessly from its inherited
/// environment. Values, including credentials, never enter helper argv. The
/// manager can still add its baseline and service metadata variables.
#[cfg(any(target_os = "linux", test))]
fn service_environment_names() -> Option<Vec<String>> {
    service_environment_names_from(std::env::vars_os())
}

#[cfg(any(target_os = "linux", test))]
fn service_environment_names_from(
    environment: impl IntoIterator<Item = (OsString, OsString)>,
) -> Option<Vec<String>> {
    const MAX_NAMES: usize = 4_096;
    const MAX_NAME_BYTES: usize = 256 * 1024;

    let mut names = Vec::new();
    let mut bytes = 0usize;
    for (name, value) in environment {
        let text = name.to_str()?;
        // D-Bus strings are UTF-8. A non-UTF-8 value cannot be copied by name
        // with the manager path without changing it, so select exact direct
        // inheritance instead.
        value.to_str()?;
        let valid = !text.is_empty()
            && text.bytes().enumerate().all(|(index, byte)| match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'_' => true,
                b'0'..=b'9' => index > 0,
                _ => false,
            });
        if !valid {
            return None;
        }
        bytes = bytes.checked_add(text.len())?;
        names.push(text.to_string());
        if names.len() > MAX_NAMES || bytes > MAX_NAME_BYTES {
            return None;
        }
    }
    names.sort_unstable();
    names.dedup();
    Some(names)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn manager_service_is_exact_argv_without_shell_or_expansion() {
        let arguments = [
            "argument with spaces",
            "$HOME",
            "%n",
            "%i",
            "%%",
            "semi;colon",
        ]
        .map(str::to_string);
        let environment = ["PATH".to_string(), "WAYLAND_DISPLAY".to_string()];
        let command = user_service_command("/usr/bin/example", &arguments, &environment);
        assert_eq!(command.get_program(), "systemd-run");
        assert_eq!(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "--user",
                "--collect",
                "--quiet",
                "--service-type=exec",
                "--expand-environment=no",
                "--same-dir",
                "--setenv=PATH",
                "--setenv=WAYLAND_DISPLAY",
                "--",
                "/usr/bin/example",
                "argument with spaces",
                "$HOME",
                "%n",
                "%i",
                "%%",
                "semi;colon",
            ]
        );
        assert!(!command.get_args().any(|argument| argument == "--scope"));

        let probe = user_service_command("/nix/store/example/bin/true", &[], &environment);
        assert_eq!(probe.get_program(), "systemd-run");
        assert_eq!(
            probe
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            [
                "--user",
                "--collect",
                "--quiet",
                "--service-type=exec",
                "--expand-environment=no",
                "--same-dir",
                "--setenv=PATH",
                "--setenv=WAYLAND_DISPLAY",
                "--",
                "/nix/store/example/bin/true",
            ]
        );
    }

    #[cfg(unix)]
    #[test]
    fn nix_profile_program_is_resolved_absolutely_in_caller_path_order() {
        let cwd = Path::new("/work");
        let preferred = Path::new("/home/user/.nix-profile/bin/media-ui");
        let path = std::env::join_paths([
            Path::new("/home/user/.nix-profile/bin"),
            Path::new("/usr/bin"),
        ])
        .expect("Nix PATH fixture should join");
        let mut candidates = Vec::new();
        let resolved = resolve_manager_program_from("media-ui", Some(&path), cwd, |candidate| {
            candidates.push(candidate.to_path_buf());
            (candidate == preferred).then(|| candidate.to_path_buf())
        });
        assert_eq!(candidates, [preferred.to_path_buf()]);
        assert_eq!(
            resolved.as_deref(),
            Some("/home/user/.nix-profile/bin/media-ui")
        );
    }

    #[cfg(unix)]
    #[test]
    fn slash_relative_program_remains_valid_for_same_dir_service() {
        let resolved =
            resolve_manager_program_from("./bin/media-ui", None, Path::new("/work"), |candidate| {
                Some(candidate.to_path_buf())
            })
            .expect("relative executable should resolve against the caller cwd");
        assert!(Path::new(&resolved).is_absolute());
        assert!(Path::new(&resolved).ends_with("bin/media-ui"));
    }

    #[test]
    fn direct_command_preserves_exact_arguments() {
        let arguments = ["a b", "$HOME", "%n", "semi;colon"].map(str::to_string);
        let command = direct_command("/usr/bin/example", &arguments);
        assert_eq!(command.get_program(), "/usr/bin/example");
        assert_eq!(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            arguments
        );
    }

    #[test]
    fn environment_name_contract_is_sorted_bounded_and_lossless() {
        let names = service_environment_names_from([
            (
                OsString::from("WAYLAND_DISPLAY"),
                OsString::from("wayland-1"),
            ),
            (OsString::from("PATH"), OsString::from("/bin")),
            (OsString::from("PATH"), OsString::from("/bin")),
        ])
        .expect("ordinary environment names should be representable");
        assert_eq!(names, ["PATH", "WAYLAND_DISPLAY"]);
        assert!(
            service_environment_names_from([(OsString::from("9INVALID"), OsString::from("x"))])
                .is_none()
        );
        assert!(
            service_environment_names_from([(OsString::from("HAS-DASH"), OsString::from("x"))])
                .is_none()
        );
        assert!(
            service_environment_names_from(
                (0..=4_096)
                    .map(|index| (OsString::from(format!("NAME_{index}")), OsString::from("x")))
            )
            .is_none()
        );
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_environment_value_selects_exact_direct_inheritance() {
        use std::os::unix::ffi::OsStringExt;

        assert!(
            service_environment_names_from([(
                OsString::from("VALID_NAME"),
                OsString::from_vec(vec![0xff]),
            )])
            .is_none()
        );
    }

    #[test]
    fn argv_admission_bounds_aggregate_queue_memory() {
        let boundary = vec!["x".repeat(MAX_ARGV_BYTES)];
        validate_argv(&boundary).expect("byte boundary should be accepted");
        assert!(matches!(
            validate_argv(&["x".repeat(MAX_ARGV_BYTES + 1)]),
            Err(LaunchError::ArgvTooLarge { .. })
        ));

        let mut too_many = (0..=MAX_ARGV_ENTRIES)
            .map(|_| String::new())
            .collect::<Vec<_>>();
        too_many[0] = "program".to_string();
        assert!(matches!(
            validate_argv(&too_many),
            Err(LaunchError::ArgvTooLarge { .. })
        ));

        let ordinary_appset = (0..MAX_BATCH_REQUESTS)
            .map(|index| vec![format!("program-{index}")])
            .collect::<Vec<_>>();
        assert_eq!(
            validate_batch(&ordinary_appset)
                .expect("a 64-app launcher set should fit")
                .requests,
            MAX_BATCH_REQUESTS
        );
        let too_many_apps = (0..=MAX_BATCH_REQUESTS)
            .map(|index| vec![format!("program-{index}")])
            .collect::<Vec<_>>();
        assert!(matches!(
            validate_batch(&too_many_apps),
            Err(LaunchError::BatchTooLarge { .. })
        ));

        let bytes_per_app = MAX_BATCH_ARGV_BYTES / MAX_BATCH_REQUESTS;
        let byte_boundary = (0..MAX_BATCH_REQUESTS)
            .map(|_| vec!["x".repeat(bytes_per_app)])
            .collect::<Vec<_>>();
        assert_eq!(
            validate_batch(&byte_boundary)
                .expect("64 launcher-limit argv should fit exactly")
                .bytes,
            MAX_BATCH_ARGV_BYTES
        );
        let mut too_many_batch_bytes = byte_boundary;
        too_many_batch_bytes[0].push("x".to_string());
        assert!(matches!(
            validate_batch(&too_many_batch_bytes),
            Err(LaunchError::BatchTooLarge { .. })
        ));

        let entries_per_app = MAX_BATCH_ARGV_ENTRIES / MAX_BATCH_REQUESTS;
        let entry_boundary = (0..MAX_BATCH_REQUESTS)
            .map(|index| {
                let mut argv = vec![format!("program-{index}")];
                argv.resize(entries_per_app, String::new());
                argv
            })
            .collect::<Vec<_>>();
        assert_eq!(
            validate_batch(&entry_boundary)
                .expect("aggregate entry boundary should fit exactly")
                .entries,
            MAX_BATCH_ARGV_ENTRIES
        );
        let mut too_many_batch_entries = entry_boundary;
        too_many_batch_entries[0].push(String::new());
        assert!(matches!(
            validate_batch(&too_many_batch_entries),
            Err(LaunchError::BatchTooLarge { .. })
        ));
    }

    #[test]
    fn batch_reservation_is_atomic_and_tickets_preserve_input_order() {
        fn job(program: &str) -> (LaunchJob, LaunchTicket) {
            let argv = vec![program.to_string()];
            let cost = argv_cost(&argv).expect("fixture argv should be valid");
            let (outcome, ticket) = oneshot::channel();
            (
                LaunchJob {
                    argv,
                    cost,
                    outcome,
                },
                LaunchTicket { outcome: ticket },
            )
        }

        let queue = LaunchQueue::new(2);
        let (first, first_ticket) = job("first");
        let (second, second_ticket) = job("second");
        let cost = first
            .cost
            .checked_add(second.cost)
            .expect("fixture cost should fit");
        queue
            .enqueue_batch(vec![first, second], cost)
            .expect("whole first batch should be reserved");

        let (rejected, _rejected_ticket) = job("rejected");
        assert!(matches!(
            queue.enqueue_batch(
                vec![rejected],
                ArgvCost {
                    requests: 1,
                    entries: 1,
                    bytes: "rejected".len(),
                }
            ),
            Err(LaunchError::QueueFull { .. })
        ));
        {
            let state = queue
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert_eq!(state.jobs.len(), 2, "rejected batch must enqueue nothing");
            assert_eq!(state.cost, cost);
        }

        let first = queue.pop().expect("first job should remain queued");
        let second = queue.pop().expect("second job should remain queued");
        assert_eq!(first.argv[0], "first");
        assert_eq!(second.argv[0], "second");
        first
            .outcome
            .send(Ok(LaunchReceipt::Direct))
            .expect("first receiver should remain alive");
        second
            .outcome
            .send(Ok(LaunchReceipt::UserService))
            .expect("second receiver should remain alive");
        assert_eq!(
            futures_executor::block_on(first_ticket).expect("first ticket should resolve"),
            LaunchReceipt::Direct
        );
        assert_eq!(
            futures_executor::block_on(second_ticket).expect("second ticket should resolve"),
            LaunchReceipt::UserService
        );
    }

    #[test]
    fn queue_accounting_rejects_aggregate_bytes_without_partial_admission() {
        fn oversized_cost_job() -> LaunchJob {
            let (outcome, _ticket) = oneshot::channel();
            LaunchJob {
                argv: vec!["program".to_string()],
                cost: ArgvCost {
                    requests: 1,
                    entries: 1,
                    bytes: MAX_QUEUED_ARGV_BYTES,
                },
                outcome,
            }
        }

        let queue = LaunchQueue::new(MAX_BATCH_REQUESTS);
        let first = oversized_cost_job();
        queue
            .enqueue_batch(
                vec![first],
                ArgvCost {
                    requests: 1,
                    entries: 1,
                    bytes: MAX_QUEUED_ARGV_BYTES,
                },
            )
            .expect("queue byte boundary should fit");
        let second = oversized_cost_job();
        assert!(matches!(
            queue.enqueue_batch(
                vec![second],
                ArgvCost {
                    requests: 1,
                    entries: 1,
                    bytes: MAX_QUEUED_ARGV_BYTES,
                }
            ),
            Err(LaunchError::QueueFull { .. })
        ));
        let state = queue
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert_eq!(state.jobs.len(), 1);
        assert_eq!(state.cost.bytes, MAX_QUEUED_ARGV_BYTES);
    }

    #[test]
    fn launch_pool_is_adaptive_and_safely_capped() {
        assert_eq!(launch_worker_count(None), MIN_LAUNCH_WORKERS);
        assert_eq!(launch_worker_count(Some(1)), MIN_LAUNCH_WORKERS);
        assert_eq!(launch_worker_count(Some(3)), 6);
        assert_eq!(launch_worker_count(Some(8)), 16);
        assert_eq!(launch_worker_count(Some(16)), MAX_LAUNCH_WORKERS);
        assert_eq!(launch_worker_count(Some(usize::MAX)), MAX_LAUNCH_WORKERS);
        assert_eq!(launch_queue_capacity(MIN_LAUNCH_WORKERS), 64);
        assert_eq!(launch_queue_capacity(16), 64);
        assert_eq!(launch_queue_capacity(17), 68);
        assert_eq!(launch_queue_capacity(MAX_LAUNCH_WORKERS), 128);
        assert_eq!(launch_queue_capacity(usize::MAX), 128);
    }

    #[test]
    fn direct_capacity_is_rejected_before_spawn() {
        let counter = AtomicUsize::new(0);
        for _ in 0..3 {
            reserve_counter(&counter, 3).expect("capacity should remain");
        }
        assert!(matches!(
            reserve_counter(&counter, 3),
            Err(LaunchError::DirectCapacity { limit: 3 })
        ));
    }

    #[test]
    fn probing_launches_wait_for_one_shared_result_instead_of_falling_back() {
        let capability = Arc::new(UserServiceCapability::new(CapabilityState::Probing));
        let update = capability.clone();
        let worker = thread::spawn(move || {
            thread::sleep(Duration::from_millis(20));
            update.set_unavailable();
        });
        assert!(!wait_for_capability(&capability, Duration::from_secs(1)));
        worker.join().expect("probe fixture should join");
        assert!(matches!(
            *capability.lock(),
            CapabilityState::Unavailable { retry_at } if retry_at > Instant::now()
        ));
    }

    #[test]
    fn unavailable_capability_retries_only_after_its_ttl() {
        let capability = UserServiceCapability::new(CapabilityState::Unavailable {
            retry_at: Instant::now() + Duration::from_secs(1),
        });
        assert!(capability.begin_probe().is_none());

        *capability.lock() = CapabilityState::Unavailable {
            retry_at: Instant::now() - Duration::from_millis(1),
        };
        let generation = capability
            .begin_probe()
            .expect("expired failure should permit one retry");
        assert!(matches!(*capability.lock(), CapabilityState::Probing));
        capability.finish_probe(generation, false);
        assert!(matches!(
            *capability.lock(),
            CapabilityState::Unavailable { retry_at } if retry_at > Instant::now()
        ));
    }

    #[cfg(unix)]
    #[test]
    fn rejected_payload_reprobes_without_forcing_next_app_direct() {
        use std::os::unix::process::ExitStatusExt;

        let capability = UserServiceCapability::new(CapabilityState::Available);
        let rejected = ExitStatus::from_raw(1 << 8);
        assert!(matches!(
            finish_user_service_handoff(&capability, Ok(rejected)),
            Err(LaunchError::HandoffRejected { .. })
        ));
        assert_eq!(*capability.lock(), CapabilityState::Unknown);

        let generation = capability
            .begin_probe()
            .expect("the next app should reprobe manager capability");
        capability.finish_probe(generation, true);
        assert!(wait_for_capability(&capability, Duration::from_millis(20)));
    }

    #[test]
    fn stalled_probe_becomes_direct_without_leaking_more_probe_threads() {
        let capability = UserServiceCapability::new(CapabilityState::Unknown);
        let generation = capability
            .begin_probe()
            .expect("unknown state should start a probe");
        capability.mark_probe_stalled(generation);
        assert!(matches!(
            *capability.lock(),
            CapabilityState::Unavailable { .. }
        ));
        *capability.lock() = CapabilityState::Unavailable {
            retry_at: Instant::now() - Duration::from_millis(1),
        };
        assert!(
            capability.begin_probe().is_none(),
            "a still-running probe must bound thread growth"
        );
        capability.finish_probe(generation, false);
        assert!(!capability.probe_running.load(Ordering::Acquire));
    }

    #[test]
    fn probe_resolution_timeout_selects_duplicate_safe_direct_backend() {
        let capability = UserServiceCapability::new(CapabilityState::Probing);
        assert!(!wait_for_capability(&capability, Duration::ZERO));
        assert!(matches!(
            *capability.lock(),
            CapabilityState::Unavailable { retry_at } if retry_at > Instant::now()
        ));
    }

    #[test]
    fn empty_submission_is_rejected_without_starting_worker_work() {
        assert!(matches!(
            submit_detached_argv(Vec::new()),
            Err(LaunchError::EmptyArgv)
        ));
        assert!(matches!(
            submit_detached_argv(vec![String::new()]),
            Err(LaunchError::EmptyArgv)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_handoff_timeout_reaps_its_helper_group() {
        use std::os::unix::process::CommandExt;

        let mut command = Command::new("sh");
        command.args(["-c", "sleep 10"]).process_group(0);
        let mut child = command
            .spawn()
            .expect("handoff timeout fixture should start");
        let started = Instant::now();
        let error = wait_handoff(&mut child, Duration::from_millis(50))
            .expect_err("sleeping helper should time out");
        assert!(matches!(error, LaunchError::HandoffTimeout { millis: 50 }));
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(
            child
                .try_wait()
                .expect("reaped helper should remain queryable")
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn submitted_direct_child_reports_success_and_is_reaped_internally() {
        // Force the portable path without mutating process environment.
        capability().set_unavailable();
        let before = DIRECT_CHILDREN.load(Ordering::Acquire);
        let ticket = submit_detached_argv(vec!["sh".into(), "-c".into(), "exit 0".into()])
            .expect("direct fixture should enqueue");
        assert_eq!(
            futures_executor::block_on(ticket).expect("direct fixture should hand off"),
            LaunchReceipt::Direct
        );
        let deadline = Instant::now() + Duration::from_secs(4);
        while DIRECT_CHILDREN.load(Ordering::Acquire) != before && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(DIRECT_CHILDREN.load(Ordering::Acquire), before);
    }

    #[test]
    fn ticket_future_reports_worker_disconnect() {
        let (tx, rx) = oneshot::channel();
        drop(tx);
        assert!(matches!(
            futures_executor::block_on(LaunchTicket { outcome: rx }),
            Err(LaunchError::WorkerStopped)
        ));
    }
}
