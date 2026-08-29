//! Minimal asynchronous client for the Sway/i3 IPC wire protocol.
//!
//! Ironbar only needs commands, workspace/input snapshots, and three event
//! streams. Keeping that surface local avoids pulling the compositor's full
//! recursive tree schema into the bar and lets Scroll add harmless fields (or
//! orientation spellings) without breaking event decoding.

use crate::spawn;
use serde::Deserialize;
use std::ffi::OsString;
use std::future::Future;
use std::io;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{Mutex, mpsc};
use tokio::time::Instant;
use tracing::{info, trace, warn};

const IPC_MAGIC: &[u8; 6] = b"i3-ipc";
const HEADER_LEN: usize = 14;
const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
const EVENT_BIT: u32 = 1 << 31;
const RUN_COMMAND: u32 = 0;
const GET_WORKSPACES: u32 = 1;
const SUBSCRIBE: u32 = 2;
const GET_BINDING_STATE: u32 = 12;
const GET_INPUTS: u32 = 100;
const WORKSPACE_EVENT: u32 = EVENT_BIT;
const MODE_EVENT: u32 = EVENT_BIT | 2;
const INPUT_EVENT: u32 = EVENT_BIT | 21;
const RECONNECT_INITIAL: Duration = Duration::from_millis(100);
const RECONNECT_MAX: Duration = Duration::from_secs(5);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const SUBSCRIPTION_ACK_TIMEOUT: Duration = Duration::from_secs(2);
const LISTENER_MAINTENANCE: Duration = Duration::from_secs(1);
const LISTENER_QUEUE_CAPACITY: usize = 64;
const EVENT_QUEUE_CAPACITY: usize = 64;

type Result<T> = std::result::Result<T, Error>;
type SyncFn<T> = dyn Fn(&T) -> bool + Sync + Send;
type AliveFn = dyn Fn() -> bool + Sync + Send;
type ConnectFuture = Pin<Box<dyn Future<Output = Result<Connection>> + Send>>;
type Connector = Arc<dyn Fn() -> ConnectFuture + Send + Sync>;

trait AsyncIo: AsyncRead + AsyncWrite + Send + Unpin {}
impl<T> AsyncIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

#[derive(Debug, Error)]
pub enum Error {
    #[error("none of SCROLLSOCK, SWAYSOCK, or I3SOCK is set")]
    MissingSocket,
    #[error("failed to connect to Sway IPC socket {path:?}: {source}")]
    Connect {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("Sway IPC I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("Sway IPC frame has invalid magic")]
    InvalidMagic,
    #[error("Sway IPC frame is {length} bytes; the limit is {max}")]
    FrameTooLarge { length: usize, max: usize },
    #[error("Sway IPC replied with message type {actual}, expected {expected}")]
    UnexpectedMessageType { expected: u32, actual: u32 },
    #[error("invalid {context} JSON from Sway: {source}")]
    InvalidJson {
        context: &'static str,
        #[source]
        source: serde_json::Error,
    },
    #[error("Sway rejected command: {reason}")]
    CommandRejected { reason: String },
    #[error("Sway rejected event subscription: {reason}")]
    SubscriptionRejected { reason: String },
    #[error("Sway IPC {operation} timed out after {millis} ms")]
    RequestTimeout {
        operation: &'static str,
        millis: u128,
    },
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Workspace {
    pub id: i64,
    pub num: i64,
    pub name: String,
    pub output: String,
    #[serde(default)]
    pub visible: bool,
    #[serde(default)]
    pub focused: bool,
}

/// The deliberately small subset of a compositor node used by workspace UI.
/// All recursive tree fields, including `layout`, are ignored by serde.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Node {
    pub id: i64,
    #[serde(default)]
    pub num: Option<i64>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub visible: Option<bool>,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub urgent: bool,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Input {
    pub identifier: String,
    #[serde(default)]
    pub xkb_active_layout_name: Option<String>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceChange {
    Init,
    Empty,
    Focus,
    Move,
    Rename,
    Urgent,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct WorkspaceEvent {
    pub change: WorkspaceChange,
    #[serde(default)]
    pub current: Option<Node>,
    #[serde(default)]
    pub old: Option<Node>,
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum InputChange {
    XkbLayout,
    XkbKeymap,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct InputEvent {
    pub change: InputChange,
    pub input: Input,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct ModeEvent {
    pub change: String,
    #[serde(default)]
    pub pango_markup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkspaceMessage {
    Snapshot(Vec<Workspace>),
    Event(WorkspaceEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputMessage {
    Snapshot(Vec<Input>),
    Event(InputEvent),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModeMessage {
    Snapshot(ModeEvent),
    Event(ModeEvent),
}

#[derive(Debug)]
struct Frame {
    message_type: u32,
    payload: Vec<u8>,
}

#[derive(Debug)]
enum Event {
    Workspace(WorkspaceEvent),
    Input(InputEvent),
    Mode(ModeEvent),
}

impl Event {
    const fn event_type(&self) -> EventType {
        match self {
            Self::Workspace(_) => EventType::Workspace,
            Self::Input(_) => EventType::Input,
            Self::Mode(_) => EventType::Mode,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventType {
    Workspace,
    Input,
    Mode,
}

impl EventType {
    const fn wire_name(self) -> &'static str {
        match self {
            Self::Workspace => "workspace",
            Self::Input => "input",
            Self::Mode => "mode",
        }
    }

    const fn index(self) -> usize {
        match self {
            Self::Workspace => 0,
            Self::Input => 1,
            Self::Mode => 2,
        }
    }
}

pub(crate) struct Connection {
    io: Box<dyn AsyncIo>,
}

impl std::fmt::Debug for Connection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Connection").finish_non_exhaustive()
    }
}

impl Connection {
    async fn new() -> Result<Self> {
        let path = sway_socket_path()?;
        let stream = UnixStream::connect(&path)
            .await
            .map_err(|source| Error::Connect {
                path: path.clone(),
                source,
            })?;
        Ok(Self {
            io: Box::new(stream),
        })
    }

    #[cfg(test)]
    fn from_io(io: impl AsyncIo + 'static) -> Self {
        Self { io: Box::new(io) }
    }

    pub(crate) async fn run_command(&mut self, command: String) -> Result<()> {
        let payload = transact(&mut self.io, RUN_COMMAND, command.as_bytes()).await?;
        let outcomes: Vec<CommandOutcome> = decode_json(&payload, "command response")?;
        let failures = outcomes
            .into_iter()
            .filter(|outcome| !outcome.success)
            .map(|outcome| {
                outcome.error.unwrap_or_else(|| {
                    if outcome.parse_error {
                        "command parse error".to_string()
                    } else {
                        "command rejected".to_string()
                    }
                })
            })
            .collect::<Vec<_>>();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(Error::CommandRejected {
                reason: failures.join("; "),
            })
        }
    }

    pub(crate) async fn get_workspaces(&mut self) -> Result<Vec<Workspace>> {
        let payload = transact(&mut self.io, GET_WORKSPACES, &[]).await?;
        decode_json(&payload, "workspace response")
    }

    pub(crate) async fn get_inputs(&mut self) -> Result<Vec<Input>> {
        let payload = transact(&mut self.io, GET_INPUTS, &[]).await?;
        decode_json(&payload, "input response")
    }

    pub(crate) async fn get_binding_state(&mut self) -> Result<String> {
        let payload = transact(&mut self.io, GET_BINDING_STATE, &[]).await?;
        let state: BindingState = decode_json(&payload, "binding state response")?;
        Ok(state.name)
    }

    async fn subscribe(&mut self, event_types: &[EventType]) -> Result<()> {
        let names = event_types
            .iter()
            .map(|event_type| event_type.wire_name())
            .collect::<Vec<_>>();
        let request = serde_json::to_vec(&names).map_err(|source| Error::InvalidJson {
            context: "subscription request",
            source,
        })?;
        let payload = transact(&mut self.io, SUBSCRIBE, &request).await?;
        let response: SubscriptionOutcome = decode_json(&payload, "subscription response")?;
        if response.success {
            Ok(())
        } else {
            Err(Error::SubscriptionRejected {
                reason: response
                    .error
                    .unwrap_or_else(|| "subscription rejected".to_string()),
            })
        }
    }

    async fn next_event(&mut self) -> Result<Option<Event>> {
        let frame = read_frame(&mut self.io).await?;
        decode_event(frame)
    }
}

pub(crate) struct RequestConnection {
    current: Option<Connection>,
    connect: Connector,
}

impl std::fmt::Debug for RequestConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RequestConnection")
            .field("connected", &self.current.is_some())
            .finish()
    }
}

impl RequestConnection {
    fn new(connect: Connector) -> Self {
        Self {
            current: None,
            connect,
        }
    }

    #[cfg(test)]
    fn with_current(current: Connection, connect: Connector) -> Self {
        Self {
            current: Some(current),
            connect,
        }
    }

    async fn ensure_connected(&mut self) -> Result<&mut Connection> {
        if self.current.is_none() {
            self.current = Some((self.connect)().await?);
        }
        self.current.as_mut().ok_or_else(|| {
            Error::Io(io::Error::new(
                io::ErrorKind::NotConnected,
                "Sway IPC reconnect returned no connection",
            ))
        })
    }

    pub(crate) async fn run_command(&mut self, command: String) -> Result<()> {
        self.run_command_with_timeout(command, REQUEST_TIMEOUT)
            .await
    }

    async fn run_command_with_timeout(
        &mut self,
        command: String,
        request_timeout: Duration,
    ) -> Result<()> {
        let result = match tokio::time::timeout(request_timeout, async {
            self.ensure_connected().await?.run_command(command).await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Error::RequestTimeout {
                operation: "command",
                millis: request_timeout.as_millis(),
            }),
        };
        self.finish_request(result)
    }

    pub(crate) async fn get_workspaces(&mut self) -> Result<Vec<Workspace>> {
        self.get_workspaces_with_timeout(REQUEST_TIMEOUT).await
    }

    async fn get_workspaces_with_timeout(
        &mut self,
        request_timeout: Duration,
    ) -> Result<Vec<Workspace>> {
        let result = match tokio::time::timeout(request_timeout, async {
            self.ensure_connected().await?.get_workspaces().await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Error::RequestTimeout {
                operation: "workspace request",
                millis: request_timeout.as_millis(),
            }),
        };
        self.finish_request(result)
    }

    pub(crate) async fn get_inputs(&mut self) -> Result<Vec<Input>> {
        self.get_inputs_with_timeout(REQUEST_TIMEOUT).await
    }

    async fn get_inputs_with_timeout(&mut self, request_timeout: Duration) -> Result<Vec<Input>> {
        let result = match tokio::time::timeout(request_timeout, async {
            self.ensure_connected().await?.get_inputs().await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Error::RequestTimeout {
                operation: "input request",
                millis: request_timeout.as_millis(),
            }),
        };
        self.finish_request(result)
    }

    pub(crate) async fn get_binding_state(&mut self) -> Result<String> {
        self.get_binding_state_with_timeout(REQUEST_TIMEOUT).await
    }

    async fn get_binding_state_with_timeout(
        &mut self,
        request_timeout: Duration,
    ) -> Result<String> {
        let result = match tokio::time::timeout(request_timeout, async {
            self.ensure_connected().await?.get_binding_state().await
        })
        .await
        {
            Ok(result) => result,
            Err(_) => Err(Error::RequestTimeout {
                operation: "binding state request",
                millis: request_timeout.as_millis(),
            }),
        };
        self.finish_request(result)
    }

    fn finish_request<T>(&mut self, result: Result<T>) -> Result<T> {
        if result.as_ref().is_err_and(Error::breaks_request_connection) {
            // Never retry the operation that just failed: a command may have
            // been accepted before its reply was lost. Retire only the broken
            // transport so the *next* independent operation reconnects.
            self.current = None;
        }
        result
    }
}

impl Error {
    fn breaks_request_connection(&self) -> bool {
        !matches!(self, Self::CommandRejected { .. })
    }
}

fn socket_connector() -> Connector {
    Arc::new(|| Box::pin(Connection::new()))
}

async fn connect_and_subscribe_with_timeout(
    connect: &Connector,
    event_types: &[EventType],
    acknowledgement_timeout: Duration,
) -> Result<Connection> {
    tokio::time::timeout(acknowledgement_timeout, async {
        let mut connection = connect().await?;
        connection.subscribe(event_types).await?;
        Ok(connection)
    })
    .await
    .map_err(|_| Error::RequestTimeout {
        operation: "subscription connection and acknowledgement",
        millis: acknowledgement_timeout.as_millis(),
    })?
}

#[derive(Debug, Deserialize)]
struct CommandOutcome {
    success: bool,
    #[serde(default)]
    parse_error: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SubscriptionOutcome {
    success: bool,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BindingState {
    name: String,
}

struct ListenerEntry<T> {
    alive: Arc<AliveFn>,
    callback: Arc<SyncFn<T>>,
}

enum Listener {
    Workspace(ListenerEntry<WorkspaceMessage>),
    Input(ListenerEntry<InputMessage>),
    Mode(ListenerEntry<ModeMessage>),
}

impl Listener {
    const fn event_type(&self) -> EventType {
        match self {
            Self::Workspace(_) => EventType::Workspace,
            Self::Input(_) => EventType::Input,
            Self::Mode(_) => EventType::Mode,
        }
    }

    fn is_alive(&self) -> bool {
        match self {
            Self::Workspace(listener) => (listener.alive)(),
            Self::Input(listener) => (listener.alive)(),
            Self::Mode(listener) => (listener.alive)(),
        }
    }

    fn dispatch_event(&self, event: &Event) -> bool {
        if !self.is_alive() {
            return false;
        }

        match (self, event) {
            (Self::Workspace(listener), Event::Workspace(event)) => {
                (listener.callback)(&WorkspaceMessage::Event(event.clone()))
            }
            (Self::Input(listener), Event::Input(event)) => {
                (listener.callback)(&InputMessage::Event(event.clone()))
            }
            (Self::Mode(listener), Event::Mode(event)) => {
                (listener.callback)(&ModeMessage::Event(event.clone()))
            }
            _ => true,
        }
    }

    fn dispatch_snapshot(&self, snapshot: &Snapshot) -> bool {
        if !self.is_alive() {
            return false;
        }

        match (self, snapshot) {
            (Self::Workspace(listener), Snapshot::Workspace(workspaces)) => {
                (listener.callback)(&WorkspaceMessage::Snapshot(workspaces.clone()))
            }
            (Self::Input(listener), Snapshot::Input(inputs)) => {
                (listener.callback)(&InputMessage::Snapshot(inputs.clone()))
            }
            (Self::Mode(listener), Snapshot::Mode(mode)) => {
                (listener.callback)(&ModeMessage::Snapshot(mode.clone()))
            }
            _ => true,
        }
    }
}

#[derive(Debug)]
enum Snapshot {
    Workspace(Vec<Workspace>),
    Input(Vec<Input>),
    Mode(ModeEvent),
}

struct SupervisorHandle {
    registrations: mpsc::Sender<Listener>,
    join_handle: tokio::task::JoinHandle<()>,
}

struct EventReaderTask(tokio::task::JoinHandle<()>);

impl EventReaderTask {
    fn abort(&self) {
        self.0.abort();
    }
}

impl Drop for EventReaderTask {
    fn drop(&mut self) {
        self.abort();
    }
}

struct TaskState {
    supervisors: [Option<SupervisorHandle>; 3],
}

pub struct Client {
    connection: Arc<Mutex<RequestConnection>>,
    task_state: Mutex<TaskState>,
    connect: Connector,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("connection", &"Connection")
            .field("task_state", &format_args!("<...>"))
            .finish()
    }
}

impl Client {
    pub(crate) async fn new() -> Result<Self> {
        let connect = socket_connector();
        let connection = Arc::new(Mutex::new(RequestConnection::new(connect.clone())));
        info!("Sway IPC client initialised");

        Ok(Self {
            connection,
            task_state: Mutex::new(TaskState {
                supervisors: [None, None, None],
            }),
            connect,
        })
    }

    #[cfg(test)]
    fn with_connector(connect: Connector) -> Self {
        Self {
            connection: Arc::new(Mutex::new(RequestConnection::new(connect.clone()))),
            task_state: Mutex::new(TaskState {
                supervisors: [None, None, None],
            }),
            connect,
        }
    }

    pub(crate) fn connection(&self) -> &Arc<Mutex<RequestConnection>> {
        &self.connection
    }

    pub async fn add_workspace_listener(
        &self,
        alive: impl Fn() -> bool + Sync + Send + 'static,
        listener: impl Fn(&WorkspaceMessage) -> bool + Sync + Send + 'static,
    ) -> Result<()> {
        self.add_listener(Listener::Workspace(ListenerEntry {
            alive: Arc::new(alive),
            callback: Arc::new(listener),
        }))
        .await
    }

    pub async fn add_input_listener(
        &self,
        alive: impl Fn() -> bool + Sync + Send + 'static,
        listener: impl Fn(&InputMessage) -> bool + Sync + Send + 'static,
    ) -> Result<()> {
        self.add_listener(Listener::Input(ListenerEntry {
            alive: Arc::new(alive),
            callback: Arc::new(listener),
        }))
        .await
    }

    pub async fn add_mode_listener(
        &self,
        alive: impl Fn() -> bool + Sync + Send + 'static,
        listener: impl Fn(&ModeMessage) -> bool + Sync + Send + 'static,
    ) -> Result<()> {
        self.add_listener(Listener::Mode(ListenerEntry {
            alive: Arc::new(alive),
            callback: Arc::new(listener),
        }))
        .await
    }

    async fn add_listener(&self, mut listener: Listener) -> Result<()> {
        let event_type = listener.event_type();

        loop {
            let registrations = {
                let mut state = self.task_state.lock().await;
                if let Some(supervisor) = state.supervisors[event_type.index()].as_ref()
                    && !supervisor.join_handle.is_finished()
                {
                    supervisor.registrations.clone()
                } else {
                    if let Some(supervisor) = state.supervisors[event_type.index()].take() {
                        supervisor.join_handle.abort();
                    }
                    let (registrations, registration_rx) = mpsc::channel(LISTENER_QUEUE_CAPACITY);
                    let join_handle = spawn(subscription_supervisor(
                        event_type,
                        registration_rx,
                        self.connection.clone(),
                        self.connect.clone(),
                        RECONNECT_INITIAL,
                        SUBSCRIPTION_ACK_TIMEOUT,
                        LISTENER_MAINTENANCE,
                    ));
                    state.supervisors[event_type.index()] = Some(SupervisorHandle {
                        registrations: registrations.clone(),
                        join_handle,
                    });
                    registrations
                }
            };

            match registrations.send(listener).await {
                Ok(()) => return Ok(()),
                Err(error) => {
                    listener = error.0;
                    let mut state = self.task_state.lock().await;
                    if state.supervisors[event_type.index()]
                        .as_ref()
                        .is_some_and(|supervisor| supervisor.registrations.is_closed())
                        && let Some(supervisor) = state.supervisors[event_type.index()].take()
                    {
                        supervisor.join_handle.abort();
                    }
                }
            }
        }
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        for supervisor in &mut self.task_state.get_mut().supervisors {
            if let Some(supervisor) = supervisor.take() {
                supervisor.join_handle.abort();
            }
        }
    }
}

async fn subscription_supervisor(
    event_type: EventType,
    mut registrations: mpsc::Receiver<Listener>,
    request_connection: Arc<Mutex<RequestConnection>>,
    connect: Connector,
    initial_retry: Duration,
    acknowledgement_timeout: Duration,
    maintenance_period: Duration,
) {
    let mut listeners = Vec::new();
    let mut event_rx = None;
    let mut event_reader = None;
    let mut reconnect_retry = initial_retry;
    let mut next_connect = Instant::now();
    let mut snapshot_retry = initial_retry;
    let mut snapshot_due = None;
    let mut maintenance = tokio::time::interval(maintenance_period);
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        listeners.retain(Listener::is_alive);

        if listeners.is_empty() {
            stop_event_reader(&mut event_reader, &mut event_rx);
            snapshot_due = None;
            reconnect_retry = initial_retry;
            snapshot_retry = initial_retry;

            match registrations.recv().await {
                Some(listener) if listener.is_alive() => {
                    listeners.push(listener);
                    next_connect = Instant::now();
                }
                Some(_) => {}
                None => return,
            }
            continue;
        }

        if event_rx.is_none() && Instant::now() >= next_connect {
            match connect_and_subscribe_with_timeout(
                &connect,
                &[event_type],
                acknowledgement_timeout,
            )
            .await
            {
                Ok(connection) => {
                    info!(?event_type, "Sway IPC subscription connected");
                    let (event_tx, events) = mpsc::channel(EVENT_QUEUE_CAPACITY);
                    event_reader = Some(EventReaderTask(spawn(event_reader_loop(
                        connection, event_type, event_tx,
                    ))));
                    event_rx = Some(events);
                    reconnect_retry = initial_retry;

                    match read_snapshot(event_type, &request_connection).await {
                        Ok(snapshot) => {
                            dispatch_snapshot(&mut listeners, &snapshot);
                            snapshot_due = None;
                            snapshot_retry = initial_retry;
                        }
                        Err(error) => {
                            warn!(?error, ?event_type, "Sway IPC initial snapshot failed");
                            snapshot_due = Some(Instant::now() + snapshot_retry);
                            snapshot_retry = next_retry(snapshot_retry);
                        }
                    }
                    continue;
                }
                Err(error) => {
                    warn!(?error, ?event_type, "Sway IPC subscription connect failed");
                    next_connect = Instant::now() + reconnect_retry;
                    reconnect_retry = next_retry(reconnect_retry);
                }
            }
        }

        if event_rx.is_none() {
            enum DisconnectedAction {
                Register(Option<Listener>),
                Connect,
                Maintain,
            }

            let action = tokio::select! {
                listener = registrations.recv() => DisconnectedAction::Register(listener),
                _ = tokio::time::sleep_until(next_connect) => DisconnectedAction::Connect,
                _ = maintenance.tick() => DisconnectedAction::Maintain,
            };
            match action {
                DisconnectedAction::Register(Some(listener)) if listener.is_alive() => {
                    listeners.push(listener);
                }
                DisconnectedAction::Register(Some(_)) | DisconnectedAction::Maintain => {}
                DisconnectedAction::Register(None) => return,
                DisconnectedAction::Connect => next_connect = Instant::now(),
            }
            continue;
        }

        enum ActiveAction {
            Register(Option<Listener>),
            Event(Option<Result<Event>>),
            Maintain,
        }

        let action = {
            let events = event_rx.as_mut().expect("active event receiver");
            tokio::select! {
                listener = registrations.recv() => ActiveAction::Register(listener),
                event = events.recv() => ActiveAction::Event(event),
                _ = maintenance.tick() => ActiveAction::Maintain,
            }
        };

        match action {
            ActiveAction::Register(Some(listener)) if listener.is_alive() => {
                match read_snapshot(event_type, &request_connection).await {
                    Ok(snapshot) => {
                        if listener.dispatch_snapshot(&snapshot) {
                            listeners.push(listener);
                        }
                    }
                    Err(error) => {
                        warn!(?error, ?event_type, "Sway IPC listener snapshot failed");
                        listeners.push(listener);
                        snapshot_due.get_or_insert(Instant::now() + snapshot_retry);
                        snapshot_retry = next_retry(snapshot_retry);
                    }
                }
            }
            ActiveAction::Register(Some(_)) => {}
            ActiveAction::Register(None) => return,
            ActiveAction::Event(Some(Ok(event))) => {
                trace!(?event, "Sway IPC event");
                listeners.retain(|listener| listener.dispatch_event(&event));
            }
            ActiveAction::Event(Some(Err(error))) => {
                warn!(?error, ?event_type, "Sway IPC subscription disconnected");
                stop_event_reader(&mut event_reader, &mut event_rx);
                next_connect = Instant::now() + reconnect_retry;
                reconnect_retry = next_retry(reconnect_retry);
                snapshot_due = None;
            }
            ActiveAction::Event(None) => {
                warn!(?event_type, "Sway IPC event reader stopped");
                stop_event_reader(&mut event_reader, &mut event_rx);
                next_connect = Instant::now() + reconnect_retry;
                reconnect_retry = next_retry(reconnect_retry);
                snapshot_due = None;
            }
            ActiveAction::Maintain => {
                listeners.retain(Listener::is_alive);
                if snapshot_due.is_some_and(|due| due <= Instant::now()) {
                    match read_snapshot(event_type, &request_connection).await {
                        Ok(snapshot) => {
                            dispatch_snapshot(&mut listeners, &snapshot);
                            snapshot_due = None;
                            snapshot_retry = initial_retry;
                        }
                        Err(error) => {
                            warn!(?error, ?event_type, "Sway IPC snapshot retry failed");
                            snapshot_due = Some(Instant::now() + snapshot_retry);
                            snapshot_retry = next_retry(snapshot_retry);
                        }
                    }
                }
            }
        }
    }
}

async fn read_snapshot(
    event_type: EventType,
    request_connection: &Arc<Mutex<RequestConnection>>,
) -> Result<Snapshot> {
    let mut connection = request_connection.lock().await;
    match event_type {
        EventType::Workspace => connection.get_workspaces().await.map(Snapshot::Workspace),
        EventType::Input => connection.get_inputs().await.map(Snapshot::Input),
        EventType::Mode => connection.get_binding_state().await.map(|change| {
            Snapshot::Mode(ModeEvent {
                change,
                pango_markup: false,
            })
        }),
    }
}

fn dispatch_snapshot(listeners: &mut Vec<Listener>, snapshot: &Snapshot) {
    listeners.retain(|listener| listener.dispatch_snapshot(snapshot));
}

fn stop_event_reader(
    event_reader: &mut Option<EventReaderTask>,
    event_rx: &mut Option<mpsc::Receiver<Result<Event>>>,
) {
    if let Some(reader) = event_reader.take() {
        reader.abort();
    }
    event_rx.take();
}

async fn event_reader_loop(
    mut connection: Connection,
    event_type: EventType,
    events: mpsc::Sender<Result<Event>>,
) {
    loop {
        match connection.next_event().await {
            Ok(Some(event)) if event.event_type() == event_type => {
                if events.send(Ok(event)).await.is_err() {
                    return;
                }
            }
            Ok(Some(event)) => {
                trace!(
                    ?event,
                    ?event_type,
                    "ignoring event from unrelated subscription"
                );
            }
            Ok(None) => trace!("ignoring unsupported Sway IPC event"),
            Err(Error::InvalidJson { context, source }) => {
                // The complete frame has already been consumed, so malformed
                // data for one known event cannot desynchronise later frames.
                warn!(?source, context, "ignoring malformed Sway IPC event");
            }
            Err(error) => {
                let _ = events.send(Err(error)).await;
                return;
            }
        }
    }
}

fn next_retry(current: Duration) -> Duration {
    current.saturating_mul(2).min(RECONNECT_MAX)
}

fn sway_socket_path() -> Result<PathBuf> {
    socket_path_from_values(
        std::env::var_os("SCROLLSOCK"),
        std::env::var_os("SWAYSOCK"),
        std::env::var_os("I3SOCK"),
    )
}

fn socket_path_from_values(
    scrollsock: Option<OsString>,
    swaysock: Option<OsString>,
    i3sock: Option<OsString>,
) -> Result<PathBuf> {
    scrollsock
        .filter(|path| !path.is_empty())
        .or_else(|| swaysock.filter(|path| !path.is_empty()))
        .or_else(|| i3sock.filter(|path| !path.is_empty()))
        .map(PathBuf::from)
        .ok_or(Error::MissingSocket)
}

async fn transact<S>(stream: &mut S, message_type: u32, payload: &[u8]) -> Result<Vec<u8>>
where
    S: AsyncRead + AsyncWrite + Unpin + ?Sized,
{
    write_frame(stream, message_type, payload).await?;
    let frame = read_frame(stream).await?;
    if frame.message_type != message_type {
        return Err(Error::UnexpectedMessageType {
            expected: message_type,
            actual: frame.message_type,
        });
    }
    Ok(frame.payload)
}

fn frame_header(payload_length: usize, message_type: u32) -> Result<[u8; HEADER_LEN]> {
    if payload_length > MAX_FRAME_BYTES {
        return Err(Error::FrameTooLarge {
            length: payload_length,
            max: MAX_FRAME_BYTES,
        });
    }
    let payload_length = u32::try_from(payload_length).map_err(|_| Error::FrameTooLarge {
        length: payload_length,
        max: MAX_FRAME_BYTES,
    })?;
    let mut header = [0_u8; HEADER_LEN];
    header[..IPC_MAGIC.len()].copy_from_slice(IPC_MAGIC);
    header[6..10].copy_from_slice(&payload_length.to_ne_bytes());
    header[10..14].copy_from_slice(&message_type.to_ne_bytes());
    Ok(header)
}

async fn write_frame<W>(writer: &mut W, message_type: u32, payload: &[u8]) -> Result<()>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let header = frame_header(payload.len(), message_type)?;
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame<R>(reader: &mut R) -> Result<Frame>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut header = [0_u8; HEADER_LEN];
    reader.read_exact(&mut header).await?;
    if &header[..IPC_MAGIC.len()] != IPC_MAGIC {
        return Err(Error::InvalidMagic);
    }
    let length = u32::from_ne_bytes(header[6..10].try_into().expect("four-byte length")) as usize;
    if length > MAX_FRAME_BYTES {
        return Err(Error::FrameTooLarge {
            length,
            max: MAX_FRAME_BYTES,
        });
    }
    let message_type =
        u32::from_ne_bytes(header[10..14].try_into().expect("four-byte message type"));
    let mut payload = vec![0_u8; length];
    reader.read_exact(&mut payload).await?;
    Ok(Frame {
        message_type,
        payload,
    })
}

fn decode_json<T>(payload: &[u8], context: &'static str) -> Result<T>
where
    T: for<'de> Deserialize<'de>,
{
    serde_json::from_slice(payload).map_err(|source| Error::InvalidJson { context, source })
}

fn decode_event(frame: Frame) -> Result<Option<Event>> {
    match frame.message_type {
        WORKSPACE_EVENT => decode_json(&frame.payload, "workspace event")
            .map(Event::Workspace)
            .map(Some),
        INPUT_EVENT => decode_json(&frame.payload, "input event")
            .map(Event::Input)
            .map(Some),
        MODE_EVENT => decode_json(&frame.payload, "mode event")
            .map(Event::Mode)
            .map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex as StdMutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::io::{AsyncWriteExt, DuplexStream, duplex};
    use tokio::sync::mpsc;
    use tokio::time::timeout;

    fn encoded_frame(message_type: u32, payload: &[u8]) -> Vec<u8> {
        let mut bytes = frame_header(payload.len(), message_type)
            .expect("fixture frame should fit")
            .to_vec();
        bytes.extend_from_slice(payload);
        bytes
    }

    async fn acknowledge_subscription(server: &mut DuplexStream, expected: &[&str]) {
        let request = read_frame(server)
            .await
            .expect("subscription request should arrive");
        assert_eq!(request.message_type, SUBSCRIBE);
        assert_eq!(
            serde_json::from_slice::<Vec<String>>(&request.payload)
                .expect("subscription payload should be JSON"),
            expected
        );
        write_frame(server, SUBSCRIBE, br#"{"success":true}"#)
            .await
            .expect("subscription acknowledgement should write");
    }

    #[test]
    fn protocol_header_bytes_match_i3_wire_format() {
        let header = frame_header(3, GET_WORKSPACES).expect("small frame should fit");
        assert_eq!(&header[..6], IPC_MAGIC);
        assert_eq!(&header[6..10], &3_u32.to_ne_bytes());
        assert_eq!(&header[10..14], &GET_WORKSPACES.to_ne_bytes());
        assert!(matches!(
            frame_header(MAX_FRAME_BYTES + 1, RUN_COMMAND),
            Err(Error::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn socket_path_prefers_scroll_then_sway_then_i3() {
        assert_eq!(
            socket_path_from_values(
                Some(OsString::from("/run/scroll.sock")),
                Some(OsString::from("/run/sway.sock")),
                Some(OsString::from("/run/i3.sock")),
            )
            .expect("SCROLLSOCK should win"),
            PathBuf::from("/run/scroll.sock")
        );
        assert_eq!(
            socket_path_from_values(
                None,
                Some(OsString::from("/run/sway.sock")),
                Some(OsString::from("/run/i3.sock")),
            )
            .expect("SWAYSOCK should win over I3SOCK"),
            PathBuf::from("/run/sway.sock")
        );
        assert_eq!(
            socket_path_from_values(None, None, Some(OsString::from("/run/i3.sock")))
                .expect("I3SOCK should be accepted"),
            PathBuf::from("/run/i3.sock")
        );
        assert!(matches!(
            socket_path_from_values(Some(OsString::new()), Some(OsString::new()), None,),
            Err(Error::MissingSocket)
        ));
    }

    #[tokio::test]
    async fn fragmented_header_and_payload_are_reassembled() {
        let payload = br#"[{"id":7,"num":2,"name":"2:web","output":"DP-1"}]"#;
        let bytes = encoded_frame(GET_WORKSPACES, payload);
        let (mut writer, mut reader) = duplex(bytes.len());
        let task = tokio::spawn(async move {
            for chunk in bytes.chunks(3) {
                writer
                    .write_all(chunk)
                    .await
                    .expect("fragment should write");
                tokio::task::yield_now().await;
            }
        });

        let frame = read_frame(&mut reader)
            .await
            .expect("fragmented frame should decode");
        assert_eq!(frame.message_type, GET_WORKSPACES);
        assert_eq!(frame.payload, payload);
        task.await.expect("fragment writer should finish");
    }

    #[tokio::test]
    async fn oversized_frame_is_rejected_before_payload_allocation() {
        let mut header = frame_header(0, GET_INPUTS).expect("empty header should build");
        header[6..10].copy_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_ne_bytes());
        let (mut writer, mut reader) = duplex(HEADER_LEN);
        writer
            .write_all(&header)
            .await
            .expect("header should write");
        assert!(matches!(
            read_frame(&mut reader).await,
            Err(Error::FrameTooLarge { .. })
        ));
    }

    #[tokio::test]
    async fn run_command_preserves_exact_payload_and_reports_rejection() {
        let (client, mut server) = duplex(4096);
        let mut connection = Connection::from_io(client);
        let exact = "workspace name with spaces; $HOME %n".to_string();
        let expected = exact.clone();
        let task = tokio::spawn(async move {
            let request = read_frame(&mut server)
                .await
                .expect("command request should arrive");
            assert_eq!(request.message_type, RUN_COMMAND);
            assert_eq!(request.payload, expected.as_bytes());
            write_frame(
                &mut server,
                RUN_COMMAND,
                br#"[{"success":false,"parse_error":true,"error":"bad command"}]"#,
            )
            .await
            .expect("command rejection should write");
        });

        assert!(matches!(
            connection.run_command(exact).await,
            Err(Error::CommandRejected { reason }) if reason == "bad command"
        ));
        task.await.expect("command server should finish");
    }

    #[tokio::test]
    async fn malformed_and_wrong_type_command_responses_are_errors() {
        for (response_type, response, expected_wrong_type) in [
            (RUN_COMMAND, b"not-json".as_slice(), false),
            (GET_INPUTS, br#"[{"success":true}]"#.as_slice(), true),
        ] {
            let (client, mut server) = duplex(4096);
            let mut connection = Connection::from_io(client);
            let task = tokio::spawn(async move {
                let _ = read_frame(&mut server)
                    .await
                    .expect("command request should arrive");
                write_frame(&mut server, response_type, response)
                    .await
                    .expect("response should write");
            });
            let error = connection
                .run_command("nop".to_string())
                .await
                .expect_err("invalid response should fail");
            assert_eq!(
                matches!(error, Error::UnexpectedMessageType { .. }),
                expected_wrong_type
            );
            task.await.expect("response server should finish");
        }
    }

    #[tokio::test]
    async fn query_replies_decode_only_required_fields() {
        let (client, mut server) = duplex(8192);
        let mut connection = Connection::from_io(client);
        let task = tokio::spawn(async move {
            let workspaces = read_frame(&mut server)
                .await
                .expect("workspace request should arrive");
            assert_eq!(workspaces.message_type, GET_WORKSPACES);
            write_frame(
                &mut server,
                GET_WORKSPACES,
                br#"[{"id":42,"num":3,"name":"3","output":"eDP-1","visible":true,"focused":false,"rect":{"x":0}}]"#,
            )
            .await
            .expect("workspace response should write");

            let inputs = read_frame(&mut server)
                .await
                .expect("input request should arrive");
            assert_eq!(inputs.message_type, GET_INPUTS);
            write_frame(
                &mut server,
                GET_INPUTS,
                br#"[{"identifier":"1:2:keyboard","type":"keyboard","xkb_active_layout_name":"English (US)"}]"#,
            )
            .await
            .expect("input response should write");

            let binding_state = read_frame(&mut server)
                .await
                .expect("binding state request should arrive");
            assert_eq!(binding_state.message_type, GET_BINDING_STATE);
            write_frame(&mut server, GET_BINDING_STATE, br#"{"name":"resize"}"#)
                .await
                .expect("binding state response should write");
        });

        assert_eq!(
            connection
                .get_workspaces()
                .await
                .expect("workspaces should decode")[0]
                .id,
            42
        );
        assert_eq!(
            connection.get_inputs().await.expect("inputs should decode")[0]
                .xkb_active_layout_name
                .as_deref(),
            Some("English (US)")
        );
        assert_eq!(
            connection
                .get_binding_state()
                .await
                .expect("binding state should decode"),
            "resize"
        );
        task.await.expect("query server should finish");
    }

    #[test]
    fn scroll_workspace_event_ignores_orientation_layouts_and_unknown_fields() {
        let frame = Frame {
            message_type: WORKSPACE_EVENT,
            payload: br#"{
                "change":"focus",
                "current":{"id":17,"num":4,"name":"4:web","output":"eDP-1","focused":true,"visible":true,"urgent":false,"layout":"horizontal","nodes":[]},
                "old":{"id":9,"num":2,"name":"2:term","output":"eDP-1","focused":false,"visible":true,"urgent":false,"layout":"vertical","floating_nodes":[]},
                "scroll_extension":{"future":true}
            }"#
            .to_vec(),
        };
        let Some(Event::Workspace(event)) = decode_event(frame).expect("event should decode")
        else {
            panic!("expected workspace event");
        };
        assert_eq!(event.change, WorkspaceChange::Focus);
        assert_eq!(event.current.expect("current workspace").id, 17);
        assert_eq!(event.old.expect("old workspace").id, 9);

        assert!(
            decode_event(Frame {
                message_type: EVENT_BIT | 127,
                payload: b"not even json".to_vec(),
            })
            .expect("unknown event should be ignored")
            .is_none()
        );
    }

    fn queued_connector(
        connections: Vec<Result<Connection>>,
        connect_count: Arc<AtomicUsize>,
    ) -> Connector {
        let connections = Arc::new(StdMutex::new(VecDeque::from(connections)));
        Arc::new(move || {
            connect_count.fetch_add(1, Ordering::SeqCst);
            let next = connections
                .lock()
                .expect("test connection queue lock")
                .pop_front()
                .unwrap_or_else(|| {
                    Err(Error::Io(io::Error::new(
                        io::ErrorKind::NotConnected,
                        "no test connection remains",
                    )))
                });
            Box::pin(std::future::ready(next))
        })
    }

    #[tokio::test]
    async fn startup_without_socket_keeps_supervisor_until_snapshot_and_event_arrive() {
        let (subscription_client, mut subscription_server) = duplex(8192);
        let (request_client, mut request_server) = duplex(8192);
        let connect_count = Arc::new(AtomicUsize::new(0));
        let connector = queued_connector(
            vec![
                Err(Error::Io(io::Error::new(
                    io::ErrorKind::NotFound,
                    "fixture socket is not available yet",
                ))),
                Ok(Connection::from_io(subscription_client)),
                Ok(Connection::from_io(request_client)),
            ],
            connect_count.clone(),
        );
        let client = Client::with_connector(connector);

        let subscription_task = tokio::spawn(async move {
            acknowledge_subscription(&mut subscription_server, &["workspace"]).await;
            write_frame(
                &mut subscription_server,
                WORKSPACE_EVENT,
                br#"{"change":"focus","current":{"id":2,"num":2,"name":"event","output":"DP-1"}}"#,
            )
            .await
            .expect("queued workspace event should write");
            std::future::pending::<()>().await;
        });
        let request_task = tokio::spawn(async move {
            let request = read_frame(&mut request_server)
                .await
                .expect("workspace snapshot request should arrive");
            assert_eq!(request.message_type, GET_WORKSPACES);
            write_frame(
                &mut request_server,
                GET_WORKSPACES,
                br#"[{"id":1,"num":1,"name":"snapshot","output":"DP-1"}]"#,
            )
            .await
            .expect("workspace snapshot should write");
        });

        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (messages_tx, mut messages_rx) = mpsc::unbounded_channel();
        client
            .add_workspace_listener(
                {
                    let alive = alive.clone();
                    move || alive.load(Ordering::SeqCst)
                },
                move |message| messages_tx.send(message.clone()).is_ok(),
            )
            .await
            .expect("workspace listener should register while disconnected");

        let first = timeout(Duration::from_secs(2), messages_rx.recv())
            .await
            .expect("startup snapshot timeout")
            .expect("startup snapshot channel should stay open");
        assert!(matches!(
            first,
            WorkspaceMessage::Snapshot(workspaces)
                if workspaces.iter().map(|workspace| workspace.id).collect::<Vec<_>>() == [1]
        ));
        let second = timeout(Duration::from_secs(1), messages_rx.recv())
            .await
            .expect("queued event timeout")
            .expect("queued event channel should stay open");
        assert!(matches!(
            second,
            WorkspaceMessage::Event(WorkspaceEvent {
                current: Some(Node { id: 2, .. }),
                ..
            })
        ));
        assert_eq!(connect_count.load(Ordering::SeqCst), 3);

        alive.store(false, Ordering::SeqCst);
        drop(client);
        request_task.await.expect("snapshot server should finish");
        subscription_task.abort();
        let _ = subscription_task.await;
    }

    #[tokio::test]
    async fn dropping_client_aborts_active_event_reader_and_closes_socket() {
        let (subscription_client, mut subscription_server) = duplex(8192);
        let (request_client, mut request_server) = duplex(8192);
        let connect_count = Arc::new(AtomicUsize::new(0));
        let connector = queued_connector(
            vec![
                Ok(Connection::from_io(subscription_client)),
                Ok(Connection::from_io(request_client)),
            ],
            connect_count.clone(),
        );
        let client = Client::with_connector(connector);

        let subscription_task = tokio::spawn(async move {
            acknowledge_subscription(&mut subscription_server, &["workspace"]).await;
            let mut byte = [0_u8; 1];
            subscription_server
                .read_exact(&mut byte)
                .await
                .expect_err("dropping the client should close the active event socket");
        });
        let request_task = tokio::spawn(async move {
            let request = read_frame(&mut request_server)
                .await
                .expect("workspace snapshot request should arrive");
            assert_eq!(request.message_type, GET_WORKSPACES);
            write_frame(&mut request_server, GET_WORKSPACES, b"[]")
                .await
                .expect("workspace snapshot should write");
        });

        let (messages_tx, mut messages_rx) = mpsc::unbounded_channel();
        client
            .add_workspace_listener(
                || true,
                move |message| messages_tx.send(message.clone()).is_ok(),
            )
            .await
            .expect("workspace listener should register");
        assert!(matches!(
            timeout(Duration::from_secs(1), messages_rx.recv())
                .await
                .expect("snapshot timeout")
                .expect("snapshot channel should stay open"),
            WorkspaceMessage::Snapshot(workspaces) if workspaces.is_empty()
        ));

        drop(client);
        timeout(Duration::from_secs(1), subscription_task)
            .await
            .expect("active event socket close timeout")
            .expect("subscription server should finish");
        request_task.await.expect("snapshot server should finish");
        assert_eq!(connect_count.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn eof_reconnect_resnapshots_before_queued_event() {
        let (initial_client, mut initial_server) = duplex(8192);
        let (request_client, mut request_server) = duplex(8192);
        let (reconnect_client, mut reconnect_server) = duplex(8192);
        let connect_count = Arc::new(AtomicUsize::new(0));
        let connector = queued_connector(
            vec![
                Ok(Connection::from_io(initial_client)),
                Ok(Connection::from_io(request_client)),
                Ok(Connection::from_io(reconnect_client)),
            ],
            connect_count.clone(),
        );
        let client = Client::with_connector(connector);

        let (close_tx, close_rx) = tokio::sync::oneshot::channel();
        let initial_task = tokio::spawn(async move {
            acknowledge_subscription(&mut initial_server, &["workspace"]).await;
            close_rx.await.expect("initial socket close signal");
        });
        let request_task = tokio::spawn(async move {
            for payload in [
                br#"[{"id":1,"num":1,"name":"old","output":"DP-1"},{"id":3,"num":3,"name":"gone","output":"DP-1"}]"#.as_slice(),
                br#"[{"id":1,"num":1,"name":"renamed","output":"DP-1"},{"id":2,"num":2,"name":"new","output":"DP-1","focused":true}]"#.as_slice(),
            ] {
                let request = read_frame(&mut request_server)
                    .await
                    .expect("workspace snapshot request should arrive");
                assert_eq!(request.message_type, GET_WORKSPACES);
                write_frame(&mut request_server, GET_WORKSPACES, payload)
                    .await
                    .expect("workspace snapshot should write");
            }
        });
        let reconnect_task = tokio::spawn(async move {
            acknowledge_subscription(&mut reconnect_server, &["workspace"]).await;
            write_frame(
                &mut reconnect_server,
                WORKSPACE_EVENT,
                br#"{"change":"focus","current":{"id":2,"num":2,"name":"new","output":"DP-1","focused":true}}"#,
            )
            .await
            .expect("reconnected event should write");
            std::future::pending::<()>().await;
        });

        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (messages_tx, mut messages_rx) = mpsc::unbounded_channel();
        client
            .add_workspace_listener(
                {
                    let alive = alive.clone();
                    move || alive.load(Ordering::SeqCst)
                },
                move |message| messages_tx.send(message.clone()).is_ok(),
            )
            .await
            .expect("workspace listener should register");

        let initial = timeout(Duration::from_secs(1), messages_rx.recv())
            .await
            .expect("initial snapshot timeout")
            .expect("initial snapshot channel should stay open");
        assert!(matches!(
            initial,
            WorkspaceMessage::Snapshot(workspaces)
                if workspaces.iter().map(|workspace| (workspace.id, workspace.name.as_str())).collect::<Vec<_>>()
                    == [(1, "old"), (3, "gone")]
        ));
        close_tx
            .send(())
            .expect("initial subscription should be closed");

        let resnapshot = timeout(Duration::from_secs(2), messages_rx.recv())
            .await
            .expect("reconnect snapshot timeout")
            .expect("reconnect snapshot channel should stay open");
        assert!(matches!(
            resnapshot,
            WorkspaceMessage::Snapshot(workspaces)
                if workspaces.iter().map(|workspace| (workspace.id, workspace.name.as_str())).collect::<Vec<_>>()
                    == [(1, "renamed"), (2, "new")]
        ));
        let queued_event = timeout(Duration::from_secs(1), messages_rx.recv())
            .await
            .expect("reconnected event timeout")
            .expect("reconnected event channel should stay open");
        assert!(matches!(
            queued_event,
            WorkspaceMessage::Event(WorkspaceEvent {
                current: Some(Node { id: 2, .. }),
                ..
            })
        ));
        assert_eq!(connect_count.load(Ordering::SeqCst), 3);

        alive.store(false, Ordering::SeqCst);
        drop(client);
        initial_task.await.expect("initial server should finish");
        request_task.await.expect("request server should finish");
        reconnect_task.abort();
        let _ = reconnect_task.await;
    }

    #[tokio::test]
    async fn failed_reconnect_snapshot_retains_state_and_retries_without_losing_event() {
        let (initial_client, mut initial_server) = duplex(8192);
        let (request_client, mut request_server) = duplex(8192);
        let (reconnect_client, mut reconnect_server) = duplex(8192);
        let (retry_request_client, mut retry_request_server) = duplex(8192);
        let connect_count = Arc::new(AtomicUsize::new(0));
        let connector = queued_connector(
            vec![
                Ok(Connection::from_io(initial_client)),
                Ok(Connection::from_io(request_client)),
                Ok(Connection::from_io(reconnect_client)),
                Ok(Connection::from_io(retry_request_client)),
            ],
            connect_count.clone(),
        );
        let request_connection = Arc::new(Mutex::new(RequestConnection::new(connector.clone())));
        let (registrations, registration_rx) = mpsc::channel(1);
        let supervisor = tokio::spawn(subscription_supervisor(
            EventType::Workspace,
            registration_rx,
            request_connection,
            connector,
            Duration::from_millis(5),
            Duration::from_millis(100),
            Duration::from_millis(10),
        ));

        let (close_tx, close_rx) = tokio::sync::oneshot::channel();
        let initial_task = tokio::spawn(async move {
            acknowledge_subscription(&mut initial_server, &["workspace"]).await;
            close_rx.await.expect("initial socket close signal");
        });
        let reconnect_task = tokio::spawn(async move {
            acknowledge_subscription(&mut reconnect_server, &["workspace"]).await;
            write_frame(
                &mut reconnect_server,
                WORKSPACE_EVENT,
                br#"{"change":"focus","current":{"id":7,"num":7,"name":"queued","output":"DP-1"}}"#,
            )
            .await
            .expect("event during failed snapshot should write");
            std::future::pending::<()>().await;
        });
        let request_task = tokio::spawn(async move {
            let initial = read_frame(&mut request_server)
                .await
                .expect("initial snapshot request should arrive");
            assert_eq!(initial.message_type, GET_WORKSPACES);
            write_frame(
                &mut request_server,
                GET_WORKSPACES,
                br#"[{"id":4,"num":4,"name":"retained","output":"DP-1","focused":true}]"#,
            )
            .await
            .expect("initial snapshot should write");

            let failed = read_frame(&mut request_server)
                .await
                .expect("reconnect snapshot request should arrive");
            assert_eq!(failed.message_type, GET_WORKSPACES);
            // Drop the request socket without replying. The already displayed
            // snapshot must remain intact while a later request retries.
        });
        let retry_request_task = tokio::spawn(async move {
            let request = read_frame(&mut retry_request_server)
                .await
                .expect("retried snapshot request should arrive");
            assert_eq!(request.message_type, GET_WORKSPACES);
            write_frame(
                &mut retry_request_server,
                GET_WORKSPACES,
                br#"[{"id":7,"num":7,"name":"queued","output":"DP-1","focused":true}]"#,
            )
            .await
            .expect("retried snapshot should write");
        });

        let alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let (messages_tx, mut messages_rx) = mpsc::unbounded_channel();
        registrations
            .send(Listener::Workspace(ListenerEntry {
                alive: {
                    let alive = alive.clone();
                    Arc::new(move || alive.load(Ordering::SeqCst))
                },
                callback: Arc::new(move |message| messages_tx.send(message.clone()).is_ok()),
            }))
            .await
            .expect("listener should register");

        let initial = timeout(Duration::from_secs(1), messages_rx.recv())
            .await
            .expect("initial snapshot timeout")
            .expect("initial snapshot channel should stay open");
        assert!(matches!(
            initial,
            WorkspaceMessage::Snapshot(workspaces)
                if workspaces.len() == 1
                    && workspaces[0].id == 4
                    && workspaces[0].name == "retained"
        ));
        close_tx
            .send(())
            .expect("initial subscription should close");

        let next = timeout(Duration::from_secs(1), messages_rx.recv())
            .await
            .expect("post-failure update timeout")
            .expect("post-failure channel should stay open");
        let last = timeout(Duration::from_secs(1), messages_rx.recv())
            .await
            .expect("post-failure update timeout")
            .expect("post-failure channel should stay open");
        assert!(
            [&next, &last].iter().any(|message| matches!(
                message,
                WorkspaceMessage::Event(WorkspaceEvent {
                    current: Some(Node { id: 7, .. }),
                    ..
                })
            )),
            "the event queued during snapshot failure must be delivered"
        );
        assert!(
            [&next, &last].iter().all(|message| !matches!(
                message,
                WorkspaceMessage::Snapshot(workspaces) if workspaces.is_empty()
            )),
            "a failed reconnect snapshot must not fabricate an empty reset"
        );
        assert!(
            [&next, &last].iter().any(|message| matches!(
                message,
                WorkspaceMessage::Snapshot(workspaces)
                    if workspaces.len() == 1 && workspaces[0].id == 7
            )),
            "a successful snapshot retry must eventually replace retained state"
        );
        assert_eq!(connect_count.load(Ordering::SeqCst), 4);

        alive.store(false, Ordering::SeqCst);
        drop(registrations);
        supervisor.abort();
        let _ = supervisor.await;
        initial_task.await.expect("initial server should finish");
        request_task.await.expect("request server should finish");
        retry_request_task
            .await
            .expect("retry request server should finish");
        reconnect_task.abort();
        let _ = reconnect_task.await;
    }

    #[tokio::test]
    async fn vanished_receiver_prunes_listener_and_stops_socket_without_reconnect() {
        let (subscription_client, mut subscription_server) = duplex(8192);
        let (request_client, mut request_server) = duplex(8192);
        let connect_count = Arc::new(AtomicUsize::new(0));
        let connector = queued_connector(
            vec![
                Ok(Connection::from_io(subscription_client)),
                Ok(Connection::from_io(request_client)),
            ],
            connect_count.clone(),
        );
        let request_connection = Arc::new(Mutex::new(RequestConnection::new(connector.clone())));
        let (registrations, registration_rx) = mpsc::channel(1);
        let supervisor = tokio::spawn(subscription_supervisor(
            EventType::Workspace,
            registration_rx,
            request_connection,
            connector,
            Duration::from_millis(5),
            Duration::from_millis(100),
            Duration::from_millis(10),
        ));

        let subscription_task = tokio::spawn(async move {
            acknowledge_subscription(&mut subscription_server, &["workspace"]).await;
            let mut byte = [0_u8; 1];
            subscription_server
                .read_exact(&mut byte)
                .await
                .expect_err("pruned listener should close event socket");
        });
        let request_task = tokio::spawn(async move {
            let request = read_frame(&mut request_server)
                .await
                .expect("workspace snapshot request should arrive");
            assert_eq!(request.message_type, GET_WORKSPACES);
            write_frame(&mut request_server, GET_WORKSPACES, b"[]")
                .await
                .expect("workspace snapshot should write");
        });

        let (updates, mut receiver) = tokio::sync::broadcast::channel(4);
        let alive_updates = updates.clone();
        let callback_updates = updates.clone();
        registrations
            .send(Listener::Workspace(ListenerEntry {
                alive: Arc::new(move || alive_updates.receiver_count() > 0),
                callback: Arc::new(move |message| callback_updates.send(message.clone()).is_ok()),
            }))
            .await
            .expect("listener should register");
        assert!(matches!(
            timeout(Duration::from_secs(1), receiver.recv())
                .await
                .expect("snapshot timeout")
                .expect("snapshot should broadcast"),
            WorkspaceMessage::Snapshot(workspaces) if workspaces.is_empty()
        ));
        drop(receiver);

        timeout(Duration::from_secs(1), subscription_task)
            .await
            .expect("event socket close timeout")
            .expect("subscription server should finish");
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(connect_count.load(Ordering::SeqCst), 2);

        request_task.await.expect("request server should finish");
        drop(registrations);
        supervisor.abort();
        let _ = supervisor.await;
    }

    #[tokio::test]
    async fn failed_request_is_not_retried_and_next_operation_reconnects() {
        let (first_client, mut first_server) = duplex(4096);
        let (second_client, mut second_server) = duplex(4096);
        let reconnects = Arc::new(StdMutex::new(VecDeque::from([Connection::from_io(
            second_client,
        )])));
        let connector: Connector = {
            let reconnects = reconnects.clone();
            Arc::new(move || {
                let next = reconnects
                    .lock()
                    .expect("request reconnect queue lock")
                    .pop_front()
                    .ok_or_else(|| {
                        Error::Io(io::Error::new(
                            io::ErrorKind::NotConnected,
                            "no request reconnect remains",
                        ))
                    });
                Box::pin(std::future::ready(next))
            })
        };
        let mut request =
            RequestConnection::with_current(Connection::from_io(first_client), connector);

        let first = tokio::spawn(async move {
            let command = read_frame(&mut first_server)
                .await
                .expect("first command should arrive exactly once");
            assert_eq!(command.message_type, RUN_COMMAND);
            assert_eq!(command.payload, b"exec maybe-started");
            // Losing the reply makes acceptance ambiguous. Closing this socket
            // must not cause the same command to be sent to the next one.
        });
        assert!(
            request
                .run_command_with_timeout(
                    "exec maybe-started".to_string(),
                    Duration::from_millis(100),
                )
                .await
                .is_err()
        );
        assert!(request.current.is_none());

        let second = tokio::spawn(async move {
            let next = read_frame(&mut second_server)
                .await
                .expect("next independent request should reconnect");
            assert_eq!(next.message_type, GET_INPUTS);
            write_frame(
                &mut second_server,
                GET_INPUTS,
                br#"[{"identifier":"keyboard","xkb_active_layout_name":"US"}]"#,
            )
            .await
            .expect("reconnected input response should write");
        });
        assert_eq!(
            request
                .get_inputs_with_timeout(Duration::from_millis(100))
                .await
                .expect("next operation should use a fresh connection")[0]
                .identifier,
            "keyboard"
        );
        first.await.expect("first request server should finish");
        second.await.expect("second request server should finish");
        assert!(
            reconnects
                .lock()
                .expect("request reconnect queue lock")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn pending_connector_is_bounded_for_requests_and_subscriptions() {
        let connector: Connector = Arc::new(|| Box::pin(std::future::pending()));
        let mut request = RequestConnection {
            current: None,
            connect: connector.clone(),
        };
        assert!(matches!(
            request
                .get_inputs_with_timeout(Duration::from_millis(10))
                .await,
            Err(Error::RequestTimeout {
                operation: "input request",
                ..
            })
        ));
        assert!(request.current.is_none());

        assert!(matches!(
            connect_and_subscribe_with_timeout(
                &connector,
                &[EventType::Workspace],
                Duration::from_millis(10),
            )
            .await,
            Err(Error::RequestTimeout {
                operation: "subscription connection and acknowledgement",
                ..
            })
        ));
    }

    #[tokio::test]
    async fn initial_subscription_acknowledgement_is_bounded() {
        let (client, mut server) = duplex(1024);
        let connections = Arc::new(StdMutex::new(VecDeque::from([Connection::from_io(client)])));
        let connector: Connector = {
            let connections = connections.clone();
            Arc::new(move || {
                let next = connections
                    .lock()
                    .expect("subscription connection queue lock")
                    .pop_front()
                    .ok_or_else(|| {
                        Error::Io(io::Error::new(
                            io::ErrorKind::NotConnected,
                            "no subscription connection remains",
                        ))
                    });
                Box::pin(std::future::ready(next))
            })
        };
        let server = tokio::spawn(async move {
            let request = read_frame(&mut server)
                .await
                .expect("subscription request should arrive");
            assert_eq!(request.message_type, SUBSCRIBE);
            std::future::pending::<()>().await;
        });
        assert!(matches!(
            connect_and_subscribe_with_timeout(
                &connector,
                &[EventType::Mode],
                Duration::from_millis(10),
            )
            .await,
            Err(Error::RequestTimeout { .. })
        ));
        server.abort();
        let _ = server.await;
    }

    #[tokio::test]
    async fn rejected_subscription_never_starts_event_loop() {
        let (client, mut server) = duplex(1024);
        let mut connection = Connection::from_io(client);
        let task = tokio::spawn(async move {
            let request = read_frame(&mut server)
                .await
                .expect("subscription should arrive");
            assert_eq!(request.message_type, SUBSCRIBE);
            write_frame(
                &mut server,
                SUBSCRIBE,
                br#"{"success":false,"error":"unsupported event"}"#,
            )
            .await
            .expect("rejection should write");
        });
        assert!(matches!(
            connection.subscribe(&[EventType::Input]).await,
            Err(Error::SubscriptionRejected { reason }) if reason == "unsupported event"
        ));
        task.await.expect("subscription server should finish");
    }
}
