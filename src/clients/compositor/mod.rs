use crate::register_fallible_client;
use cfg_if::cfg_if;
use std::ffi::OsString;
use std::fmt::{Debug, Display, Formatter};
use std::sync::Arc;
use thiserror::Error;
use tokio::sync::broadcast;
use tracing::debug;

#[cfg(feature = "hyprland")]
pub mod hyprland;
#[cfg(feature = "niri")]
pub mod niri;
#[cfg(feature = "sway")]
pub mod sway;

#[derive(Debug, Error)]
pub enum Error {
    #[error("{0} is unsupported by compositor. The following are supported: {1:?}")]
    Unsupported(&'static str, &'static [&'static str]),
    #[error("{0} feature flag is disabled for compositor")]
    Disabled(&'static str),
    #[error(transparent)]
    Other(#[from] Box<dyn std::error::Error + Send + Sync>),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compositor {
    #[cfg(feature = "sway")]
    Scroll,
    #[cfg(feature = "sway")]
    Sway,
    #[cfg(feature = "sway")]
    I3,
    #[cfg(feature = "hyprland")]
    Hyprland,
    #[cfg(feature = "niri")]
    Niri,
    Unsupported,
}

impl Display for Compositor {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                #[cfg(any(feature = "sway"))]
                Self::Scroll => "Scroll",
                #[cfg(any(feature = "sway"))]
                Self::Sway => "Sway",
                #[cfg(feature = "sway")]
                Self::I3 => "i3",
                #[cfg(any(feature = "hyprland"))]
                Self::Hyprland => "Hyprland",
                #[cfg(feature = "workspaces+niri")]
                Self::Niri => "Niri",
                Self::Unsupported => "Unsupported",
            }
        )
    }
}

impl Compositor {
    /// Attempts to get the current compositor.
    /// This is done by checking system env vars.
    pub(crate) fn current() -> Self {
        if let Some(compositor) = sway_ipc_compositor_from_values(
            std::env::var_os("SCROLLSOCK"),
            std::env::var_os("SWAYSOCK"),
            std::env::var_os("I3SOCK"),
        ) {
            cfg_if! {
                if #[cfg(feature = "sway")] {
                    match compositor {
                        SwayIpcCompositor::Scroll => Self::Scroll,
                        SwayIpcCompositor::Sway => Self::Sway,
                        SwayIpcCompositor::I3 => Self::I3,
                    }
                }
                else {
                    tracing::error!("Not compiled with Sway/i3 IPC support");
                    Self::Unsupported
                }
            }
        } else if std::env::var("HYPRLAND_INSTANCE_SIGNATURE").is_ok() {
            cfg_if! {
                if #[cfg(feature = "hyprland")] { Self::Hyprland }
                else { tracing::error!("Not compiled with Hyprland support"); Self::Unsupported }
            }
        } else if std::env::var("NIRI_SOCKET").is_ok() {
            cfg_if! {
                if #[cfg(feature = "niri")] { Self::Niri }
                else {tracing::error!("Not compiled with Niri support"); Self::Unsupported }
            }
        } else {
            Self::Unsupported
        }
    }

    /// Whether the current compositor exposes a keyboard-layout client.
    ///
    /// i3 shares Sway's workspace and binding-mode IPC, but not Sway's
    /// `GET_INPUTS`, `input` event, or `input ... xkb_switch_layout` extension.
    #[cfg(feature = "keyboard")]
    pub(crate) const fn supports_keyboard_layout_client(&self) -> bool {
        match self {
            #[cfg(feature = "keyboard+sway")]
            Self::Scroll | Self::Sway => true,
            #[cfg(feature = "keyboard+hyprland")]
            Self::Hyprland => true,
            _ => false,
        }
    }

    #[cfg(feature = "workspaces")]
    pub(crate) const fn supports_workspace_client(&self) -> bool {
        match self {
            #[cfg(feature = "workspaces+sway")]
            Self::Scroll | Self::Sway | Self::I3 => true,
            #[cfg(feature = "workspaces+hyprland")]
            Self::Hyprland => true,
            #[cfg(feature = "workspaces+niri")]
            Self::Niri => true,
            _ => false,
        }
    }

    #[cfg(feature = "bindmode")]
    pub(crate) const fn supports_bindmode_client(&self) -> bool {
        match self {
            #[cfg(feature = "bindmode+sway")]
            Self::Scroll | Self::Sway | Self::I3 => true,
            #[cfg(feature = "bindmode+hyprland")]
            Self::Hyprland => true,
            _ => false,
        }
    }

    #[cfg(feature = "bindmode")]
    pub fn create_bindmode_client(
        clients: &mut super::Clients,
    ) -> Result<Arc<dyn BindModeClient + Send + Sync>> {
        let current = Self::current();
        debug!("Getting keyboard_layout client for: {current}");
        match current {
            #[cfg(feature = "bindmode+sway")]
            Self::Scroll | Self::Sway | Self::I3 => {
                debug_assert!(current.supports_bindmode_client());
                Ok(clients.sway().map_err(|err| Error::Other(err.into()))?)
            }
            #[cfg(feature = "bindmode+hyprland")]
            Self::Hyprland => {
                debug_assert!(current.supports_bindmode_client());
                Ok(clients.hyprland())
            }
            #[cfg(feature = "niri")]
            Self::Niri => Err(Error::Unsupported("bindmode", &["sway", "hyprland"])),
            Self::Unsupported => Err(Error::Unsupported("bindmode", &["sway", "hyprland"])),
            #[allow(unreachable_patterns)]
            _ => Err(Error::Disabled("bindmode")),
        }
    }

    #[cfg(feature = "keyboard")]
    pub fn create_keyboard_layout_client(
        clients: &mut super::Clients,
    ) -> Result<Arc<dyn KeyboardLayoutClient + Send + Sync>> {
        let current = Self::current();
        debug!("Getting keyboard_layout client for: {current}");
        match current {
            #[cfg(feature = "keyboard+sway")]
            Self::Scroll | Self::Sway => {
                debug_assert!(current.supports_keyboard_layout_client());
                Ok(clients.sway().map_err(|err| Error::Other(err.into()))?)
            }
            #[cfg(feature = "sway")]
            Self::I3 => {
                debug_assert!(!current.supports_keyboard_layout_client());
                Err(Error::Unsupported("keyboard", &["sway", "hyprland"]))
            }
            #[cfg(feature = "keyboard+hyprland")]
            Self::Hyprland => {
                debug_assert!(current.supports_keyboard_layout_client());
                Ok(clients.hyprland())
            }
            #[cfg(feature = "niri")]
            Self::Niri => Err(Error::Unsupported("keyboard", &["sway", "hyprland"])),
            Self::Unsupported => Err(Error::Unsupported("keyboard", &["sway", "hyprland"])),
            #[allow(unreachable_patterns)]
            _ => Err(Error::Disabled("keyboard")),
        }
    }

    /// Creates a new instance of
    /// the workspace client for the current compositor.
    #[cfg(feature = "workspaces")]
    pub fn create_workspace_client(
        clients: &mut super::Clients,
    ) -> Result<Arc<dyn WorkspaceClient + Send + Sync>> {
        let current = Self::current();
        debug!("Getting workspace client for: {current}");
        match current {
            #[cfg(feature = "workspaces+sway")]
            Self::Scroll | Self::Sway | Self::I3 => {
                debug_assert!(current.supports_workspace_client());
                Ok(clients.sway().map_err(|err| Error::Other(err.into()))?)
            }
            #[cfg(feature = "workspaces+hyprland")]
            Self::Hyprland => {
                debug_assert!(current.supports_workspace_client());
                Ok(clients.hyprland())
            }
            #[cfg(feature = "workspaces+niri")]
            Self::Niri => {
                debug_assert!(current.supports_workspace_client());
                Ok(Arc::new(niri::Client::new()))
            }
            Self::Unsupported => Err(Error::Unsupported(
                "workspaces",
                &["sway", "hyprland", "niri"],
            )),
            #[allow(unreachable_patterns)]
            _ => Err(Error::Disabled("workspaces")),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SwayIpcCompositor {
    Scroll,
    Sway,
    I3,
}

fn sway_ipc_compositor_from_values(
    scrollsock: Option<OsString>,
    swaysock: Option<OsString>,
    i3sock: Option<OsString>,
) -> Option<SwayIpcCompositor> {
    if scrollsock.is_some_and(|path| !path.is_empty()) {
        Some(SwayIpcCompositor::Scroll)
    } else if swaysock.is_some_and(|path| !path.is_empty()) {
        Some(SwayIpcCompositor::Sway)
    } else if i3sock.is_some_and(|path| !path.is_empty()) {
        Some(SwayIpcCompositor::I3)
    } else {
        None
    }
}

#[cfg(test)]
mod compositor_detection_tests {
    use super::*;

    #[test]
    fn compositor_socket_precedence_preserves_scroll_sway_and_i3_identity() {
        assert_eq!(
            sway_ipc_compositor_from_values(
                Some(OsString::from("/run/scroll.sock")),
                Some(OsString::from("/run/sway.sock")),
                Some(OsString::from("/run/i3.sock")),
            ),
            Some(SwayIpcCompositor::Scroll)
        );
        assert_eq!(
            sway_ipc_compositor_from_values(
                None,
                Some(OsString::from("/run/sway.sock")),
                Some(OsString::from("/run/i3.sock")),
            ),
            Some(SwayIpcCompositor::Sway)
        );
        assert_eq!(
            sway_ipc_compositor_from_values(None, None, Some(OsString::from("/run/i3.sock")),),
            Some(SwayIpcCompositor::I3)
        );
        assert_eq!(
            sway_ipc_compositor_from_values(
                Some(OsString::new()),
                Some(OsString::new()),
                Some(OsString::new()),
            ),
            None
        );
    }

    #[cfg(feature = "keyboard+sway")]
    #[test]
    fn i3_does_not_advertise_sway_keyboard_layout_extensions() {
        assert!(!Compositor::I3.supports_keyboard_layout_client());
        assert!(Compositor::Scroll.supports_keyboard_layout_client());
        assert!(Compositor::Sway.supports_keyboard_layout_client());
    }

    #[cfg(all(feature = "workspaces+sway", feature = "bindmode+sway"))]
    #[test]
    fn i3_retains_the_shared_workspace_and_bindmode_clients() {
        assert!(Compositor::I3.supports_workspace_client());
        assert!(Compositor::I3.supports_bindmode_client());
    }
}

#[derive(Debug, Clone)]
pub struct Workspace {
    /// Unique identifier
    pub id: i64,
    /// The workspace index (e.g. for sorting)
    pub index: i64,
    /// Workspace friendly name
    pub name: String,
    /// Name of the monitor (output) the workspace is located on
    pub monitor: String,
    /// How visible the workspace is
    pub visibility: Visibility,
}

/// Identifies a workspace to focus.
///
/// Open workspaces use their compositor-assigned ID. A persistent favourite
/// retains both its configured name and, when numeric, its configured index so
/// each compositor adapter can preserve its native creation semantics.
#[derive(Debug, Clone, Eq, PartialEq)]
#[cfg(feature = "workspaces")]
pub enum WorkspaceTarget {
    Id(i64),
    Name(String),
    Persistent { name: String, index: Option<i64> },
}

#[cfg(feature = "workspaces")]
impl WorkspaceTarget {
    pub(crate) fn persistent_by_name(&self) -> Self {
        match self {
            Self::Persistent { name, .. } => Self::Name(name.clone()),
            target => target.clone(),
        }
    }

    pub(crate) fn persistent_by_index(&self, max: i64) -> Self {
        match self {
            Self::Persistent {
                index: Some(index), ..
            } if *index > 0 && *index <= max => Self::Id(*index),
            Self::Persistent { name, .. } => Self::Name(name.clone()),
            target => target.clone(),
        }
    }
}

/// Indicates workspace visibility.
/// Visible workspaces have a boolean flag to indicate if they are also focused.
#[derive(Debug, Copy, Clone)]
pub enum Visibility {
    Visible { focused: bool },
    Hidden,
}

impl Visibility {
    pub fn visible() -> Self {
        Self::Visible { focused: false }
    }

    pub fn focused() -> Self {
        Self::Visible { focused: true }
    }

    pub fn is_visible(self) -> bool {
        matches!(self, Self::Visible { .. })
    }

    pub fn is_focused(self) -> bool {
        if let Self::Visible { focused } = self {
            focused
        } else {
            false
        }
    }
}

#[derive(Debug, Clone)]
#[cfg(feature = "keyboard")]
pub struct KeyboardLayoutUpdate(pub String);

#[derive(Debug, Clone)]
#[cfg(feature = "workspaces")]
pub enum WorkspaceUpdate {
    /// Provides an initial list of workspaces.
    /// This is re-sent to all subscribers when a new subscription is created.
    Init(Vec<Workspace>),
    /// Replaces the current workspace state after a compositor reconnect.
    ///
    /// Unlike [`Self::Init`], this is intended for existing subscribers and
    /// must reconcile workspaces that disappeared while the event stream was
    /// unavailable.
    Resync(Vec<Workspace>),
    Add(Workspace),
    Remove(i64),
    Move(Workspace),
    /// Declares focus moved from the old workspace to the new.
    Focus {
        old: Option<Workspace>,
        new: Workspace,
    },

    Rename {
        id: i64,
        name: String,
    },

    /// The urgent state of a node changed.
    Urgent {
        id: i64,
        urgent: bool,
    },

    /// An update was triggered by the compositor but this was not mapped by Ironbar.
    ///
    /// This is purely used for ergonomics within the compositor clients
    /// and should be ignored by consumers.
    Unknown,
}

#[derive(Clone, Debug)]
#[cfg(feature = "bindmode")]
pub struct BindModeUpdate {
    /// The binding mode that became active.
    pub name: String,
    /// Whether the mode should be parsed as pango markup.
    pub pango_markup: bool,
}

#[cfg(feature = "workspaces")]
pub trait WorkspaceClient: Debug + Send + Sync {
    /// Requests the identified workspace is focused or created.
    fn focus(&self, target: WorkspaceTarget);

    /// Creates a new to workspace event receiver.
    fn subscribe(&self) -> broadcast::Receiver<WorkspaceUpdate>;
}

#[cfg(feature = "workspaces")]
register_fallible_client!(dyn WorkspaceClient, workspaces);

#[cfg(feature = "keyboard")]
pub trait KeyboardLayoutClient: Debug + Send + Sync {
    /// Switches to the next layout.
    fn set_next_active(&self);

    /// Creates a new to keyboard layout event receiver.
    fn subscribe(&self) -> broadcast::Receiver<KeyboardLayoutUpdate>;
}

#[cfg(feature = "keyboard")]
register_fallible_client!(dyn KeyboardLayoutClient, keyboard_layout);

#[cfg(feature = "bindmode")]
pub trait BindModeClient: Debug + Send + Sync {
    /// Add a callback for bindmode updates.
    fn subscribe(&self) -> Result<broadcast::Receiver<BindModeUpdate>>;
}

#[cfg(feature = "bindmode")]
register_fallible_client!(dyn BindModeClient, bindmode);
