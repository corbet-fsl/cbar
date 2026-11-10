use std::collections::HashMap;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use super::wayland;
use crate::channels::SyncSenderExt;
use crate::{Ironbar, arc_mut, debug, get_display, info, lock, register_client};
use gtk::gdk::Monitor;
use gtk::gdk::prelude::*;
use tokio::sync::broadcast::{self, Sender};

/// What happened to an output.
///
/// The event only says that the set of outputs changed. It deliberately carries no output
/// objects: the listener reads the outputs that exist at that moment, so a late, repeated or
/// reordered event can never make it act on a stale view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorState {
    Disconnected,
    Connected,
}

#[derive(Debug, Clone)]
pub struct MonitorEvent {
    pub connector: String,
    pub state: MonitorState,
}

/// There are two ways in which we are notified of output events:
/// 1. Through a wayland client event
/// 2. Through a GDK event
///
/// We need to collate both events before we report an output as connected, because bars need both
/// the Wayland output (its name and description) and the GDK monitor (what the window binds to).
enum InternalMonitorState {
    Disconnected,
    WaylandConnected,
    GdkConnected,
    BothConnected,
}

struct MonitorProxy {
    pub connector: String,
    state: InternalMonitorState,
}

impl MonitorProxy {
    fn disconnect(&mut self) -> &mut Self {
        self.state = InternalMonitorState::Disconnected;

        self
    }

    fn connect_wayland(&mut self) -> &mut Self {
        self.state = match &self.state {
            InternalMonitorState::GdkConnected => InternalMonitorState::BothConnected,
            _ => InternalMonitorState::WaylandConnected,
        };

        self
    }

    fn connect_gdk(&mut self) -> &mut Self {
        self.state = match &self.state {
            InternalMonitorState::WaylandConnected => InternalMonitorState::BothConnected,
            _ => InternalMonitorState::GdkConnected,
        };

        self
    }

    fn maybe_send(&self, tx: &Sender<MonitorEvent>) {
        match &self.state {
            InternalMonitorState::Disconnected => {
                info!("Monitor {} disconnected", self.connector);
                tx.send_expect(MonitorEvent {
                    connector: self.connector.clone(),
                    state: MonitorState::Disconnected,
                });
            }
            InternalMonitorState::BothConnected => {
                info!("Monitor {} connected", self.connector);
                tx.send_expect(MonitorEvent {
                    connector: self.connector.clone(),
                    state: MonitorState::Connected,
                });
            }
            _ => {}
        }
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct BroadcastChannel<T>(broadcast::Sender<T>, Arc<Mutex<broadcast::Receiver<T>>>);

impl<T> From<(broadcast::Sender<T>, broadcast::Receiver<T>)> for BroadcastChannel<T> {
    fn from(value: (broadcast::Sender<T>, broadcast::Receiver<T>)) -> Self {
        Self(value.0, arc_mut!(value.1))
    }
}

#[derive(Debug)]
pub struct Client {
    // Associates a connector ID (e.g. HDMI-1) to a state
    output_channel: BroadcastChannel<MonitorEvent>,
}

impl Client {
    pub(crate) fn new() -> Self {
        Self {
            output_channel: broadcast::channel(8).into(),
        }
    }

    pub(crate) fn start(&self, ironbar: &Rc<Ironbar>) {
        let mut rx_wl_outputs = ironbar.clients.borrow_mut().wayland().subscribe_outputs();

        let monitors = arc_mut!(HashMap::new());

        // listen to wayland events
        {
            let monitors = monitors.clone();
            let output_tx = self.output_channel.0.clone();

            glib::spawn_future_local(async move {
                while let Ok(event) = rx_wl_outputs.recv().await {
                    debug!("Wayland output event: {event:?}");
                    if let Some(name) = &event.output.name {
                        let mut guard = lock!(monitors);
                        let entry = guard.entry(name.clone()).or_insert_with(|| MonitorProxy {
                            connector: name.clone(),
                            state: InternalMonitorState::Disconnected,
                        });
                        match event.event_type {
                            wayland::OutputEventType::New => {
                                entry.connect_wayland().maybe_send(&output_tx);
                            }
                            wayland::OutputEventType::Destroyed => {
                                entry.disconnect().maybe_send(&output_tx);
                            }
                            wayland::OutputEventType::Update => {}
                        }
                    }
                }
            });
        }

        // listen to GDK events
        {
            let output_tx = self.output_channel.0.clone();
            let monitors = monitors.clone();

            let display = get_display();
            display
                .monitors()
                .connect_items_changed(move |list, position, removed, added| {
                    debug!("GDK event: +{added}, -{removed}");
                    for added_idx in position..position + added {
                        if let Some(monitor) = list.item(added_idx).and_downcast::<Monitor>() {
                            let output_tx = output_tx.clone();
                            let monitors = monitors.clone();

                            /*
                             * At this point, we have a `gdk::Monitor`. However, the object initially
                             * has all its fields `None`, including `connector`.
                             *
                             * We have to listen for the notify event for the `connector` being set.
                             */
                            monitor.connect_notify(Some("connector"), move |m, _| {
                                if let Some(connector) = m.connector() {
                                    lock!(monitors)
                                        .entry(connector.to_string())
                                        .or_insert_with(|| MonitorProxy {
                                            connector: connector.to_string(),
                                            state: InternalMonitorState::Disconnected,
                                        })
                                        .connect_gdk()
                                        .maybe_send(&output_tx);
                                }
                            });
                        }
                    }
                });
        }
    }

    /// Subscribes to events
    pub fn subscribe(&self) -> broadcast::Receiver<MonitorEvent> {
        self.output_channel.0.subscribe()
    }
}

register_client!(Client, outputs);
