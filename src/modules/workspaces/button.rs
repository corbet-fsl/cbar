use super::open_state::OpenState;
use crate::channels::AsyncSenderExt;
use crate::clients::compositor::WorkspaceTarget;
use crate::image::IconButton;
use crate::modules::workspaces::WorkspaceItemContext;
use glib::signal::SignalHandlerId;
use gtk::Button as GtkButton;
use gtk::prelude::*;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct Button {
    button: IconButton,
    binding: WorkspaceBinding,
    monitor: String,
    open_state: OpenState,
    conn_id: Option<SignalHandlerId>,
    tx: mpsc::Sender<WorkspaceTarget>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
struct WorkspaceBinding {
    workspace_id: Option<i64>,
    favorite_name: Option<String>,
}

impl WorkspaceBinding {
    fn workspace(id: i64) -> Self {
        Self {
            workspace_id: Some(id),
            favorite_name: None,
        }
    }

    fn favorite(name: &str) -> Self {
        Self {
            workspace_id: None,
            favorite_name: Some(name.to_string()),
        }
    }

    fn focus_target(&self) -> WorkspaceTarget {
        self.workspace_id.map_or_else(
            || {
                let name = self
                    .favorite_name
                    .clone()
                    .expect("closed workspace button to be a favourite");
                let index = name
                    .parse::<i64>()
                    .ok()
                    .filter(|index| *index > 0 && index.to_string() == name);
                WorkspaceTarget::Persistent { index, name }
            },
            WorkspaceTarget::Id,
        )
    }

    fn open(&mut self, id: i64) {
        self.workspace_id = Some(id);
    }

    fn workspace_removed(&mut self, id: i64) -> bool {
        if self.workspace_id != Some(id) || self.favorite_name.is_none() {
            return false;
        }

        self.workspace_id = None;
        true
    }
}

impl Button {
    pub fn new(
        id: i64,
        index: i64,
        name: &str,
        monitor: &str,
        open_state: OpenState,
        context: &WorkspaceItemContext,
    ) -> Self {
        let label = context.format_label(name, index);

        let button = IconButton::new(&label, context.icon_size, &context.image_provider);
        button.set_widget_name(name);
        button.add_css_class("item");

        let mut btn = Self {
            button,
            binding: WorkspaceBinding::workspace(id),
            monitor: monitor.to_string(),
            open_state,
            conn_id: None,
            tx: context.tx.clone(),
        };

        btn.reconnect_focus();
        btn.apply_open_state();
        btn
    }

    pub fn new_favorite(
        index: i64,
        name: &str,
        monitor: &str,
        context: &WorkspaceItemContext,
    ) -> Self {
        let label = context.format_label(name, index);

        let button = IconButton::new(&label, context.icon_size, &context.image_provider);
        button.set_widget_name(name);
        button.add_css_class("item");

        let mut btn = Self {
            button,
            binding: WorkspaceBinding::favorite(name),
            monitor: monitor.to_string(),
            open_state: OpenState::Closed,
            conn_id: None,
            tx: context.tx.clone(),
        };

        btn.reconnect_focus();
        btn.apply_open_state();
        btn
    }

    pub fn button(&self) -> &GtkButton {
        &self.button
    }

    pub fn set_label(&self, label: &str) {
        self.button.set_label(label);
    }

    pub fn open_state(&self) -> OpenState {
        self.open_state
    }

    pub fn set_open_state(&mut self, open_state: OpenState) {
        if self.open_state == open_state {
            return;
        }
        self.open_state = open_state;
        self.apply_open_state();
    }

    fn apply_open_state(&self) {
        let open_state = self.open_state;

        if open_state.is_visible() {
            self.button.add_css_class("visible");
        } else {
            self.button.remove_css_class("visible");
        }

        if open_state == OpenState::Focused {
            self.button.add_css_class("focused");
        } else {
            self.button.remove_css_class("focused");
        }

        if open_state == OpenState::Closed {
            self.button.add_css_class("inactive");
        } else {
            self.button.remove_css_class("inactive");
        }
    }

    pub fn set_urgent(&self, urgent: bool) {
        if urgent {
            self.button.add_css_class("urgent");
        } else {
            self.button.remove_css_class("urgent");
        }
    }

    pub fn workspace_id(&self) -> Option<i64> {
        self.binding.workspace_id
    }

    pub fn set_workspace_id(&mut self, id: i64) {
        self.binding.open(id);
        self.reconnect_focus();
    }

    pub fn set_workspace_closed(&mut self, id: i64) {
        assert!(
            self.binding.workspace_removed(id),
            "removed workspace to match an open favourite"
        );
        self.reconnect_focus();
    }

    fn reconnect_focus(&mut self) {
        if let Some(conn_id) = self.conn_id.take() {
            self.button.disconnect(conn_id);
        }
        let tx = self.tx.clone();
        let target = self.binding.focus_target();
        let conn_id = self.button.connect_clicked(move |_item| {
            tx.send_spawn(target.clone());
        });
        self.conn_id = Some(conn_id);
    }

    pub fn monitor(&self) -> &str {
        &self.monitor
    }

    pub fn set_monitor(&mut self, monitor: &str) {
        self.monitor = monitor.to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn favourite_restores_its_persistent_identity_after_empty_event() {
        let mut binding = WorkspaceBinding::favorite("2");
        assert_eq!(
            binding.focus_target(),
            WorkspaceTarget::Persistent {
                name: "2".to_string(),
                index: Some(2),
            }
        );

        binding.open(42);
        assert_eq!(binding.focus_target(), WorkspaceTarget::Id(42));

        assert!(!binding.workspace_removed(41));
        assert_eq!(binding.focus_target(), WorkspaceTarget::Id(42));

        assert!(binding.workspace_removed(42));
        assert_eq!(
            binding.focus_target(),
            WorkspaceTarget::Persistent {
                name: "2".to_string(),
                index: Some(2),
            }
        );
    }

    #[test]
    fn named_favourite_has_no_invented_numeric_identity() {
        assert_eq!(
            WorkspaceBinding::favorite("dev").focus_target(),
            WorkspaceTarget::Persistent {
                name: "dev".to_string(),
                index: None,
            }
        );
    }

    #[test]
    fn noncanonical_numeric_favourites_keep_their_exact_names() {
        for name in ["02", "0", "-1", "+2", "9223372036854775808"] {
            assert_eq!(
                WorkspaceBinding::favorite(name).focus_target(),
                WorkspaceTarget::Persistent {
                    name: name.to_string(),
                    index: None,
                }
            );
        }
    }
}
