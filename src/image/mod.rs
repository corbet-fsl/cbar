#[cfg(any(
    feature = "battery",
    feature = "bluetooth",
    feature = "clipboard",
    feature = "keyboard",
    feature = "launcher",
    feature = "menu",
    feature = "music",
    feature = "notifications",
    feature = "workspaces",
    feature = "network_manager"
))]
mod gtk;
mod provider;

#[cfg(any(
    feature = "battery",
    feature = "bluetooth",
    feature = "clipboard",
    feature = "keyboard",
    feature = "launcher",
    feature = "menu",
    feature = "music",
    feature = "notifications",
    feature = "workspaces",
    feature = "network_manager"
))]
pub use self::gtk::*;
pub use provider::Provider;
