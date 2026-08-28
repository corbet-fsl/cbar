//! Opinionated native system-history graphs.
//!
//! One process-wide sampler feeds every output, and each module instance draws
//! all visible cells in one GTK4 `DrawingArea`. It intentionally does not use
//! the generic Lua/Cairo module, runtime cache files, or per-cell polling.

mod area;
mod hub;
mod model;
mod renderer;
mod sampler;

use crate::config::CommonConfig;
use crate::modules::{Module, ModuleInfo, ModuleParts, WidgetContext};
use crate::{module_impl, spawn};
use area::ResponsiveGraphArea;
use gtk::DrawingArea;
use gtk::gdk::{BUTTON_PRIMARY, BUTTON_SECONDARY};
use gtk::prelude::*;
use gtk::{GestureClick, Tooltip};
use hub::{GraphDemand, GraphHub};
use ironbar_launch_service::{submit_detached_argv, warm_launch_service};
use model::{GRAPH_HEIGHT, GraphFrame, Layout, Metric, MetricSet};
use serde::Deserialize;
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use std::sync::Arc;
use tokio::sync::mpsc::Receiver;
use tokio::sync::oneshot;
use tracing::error;

#[derive(Debug, Default, Clone, Deserialize)]
#[cfg_attr(feature = "extras", derive(schemars::JsonSchema))]
pub struct NetworkActions {
    /// Argument vector run when the displayed LAN cell is clicked.
    pub lan: Option<Vec<String>>,
    /// Argument vector run when the displayed WLAN cell is clicked.
    pub wlan: Option<Vec<String>>,
    /// Argument vector run when the displayed WWAN cell is clicked.
    pub wwan: Option<Vec<String>>,
    /// Argument vector run when the displayed VPN cell is clicked.
    pub vpn: Option<Vec<String>>,
}

impl NetworkActions {
    fn get(&self, metric: Metric) -> Option<&[String]> {
        match metric {
            Metric::Lan => self.lan.as_deref(),
            Metric::Wlan => self.wlan.as_deref(),
            Metric::Wwan => self.wwan.as_deref(),
            Metric::Vpn => self.vpn.as_deref(),
            _ => None,
        }
        .filter(|argv| !argv.is_empty())
    }

    fn is_empty(&self) -> bool {
        Metric::NETWORK
            .into_iter()
            .all(|metric| self.get(metric).is_none())
    }
}

fn default_demand() -> Arc<GraphDemand> {
    GraphDemand::core()
}

#[derive(Debug, Deserialize)]
#[cfg_attr(feature = "extras", derive(schemars::JsonSchema))]
#[serde(default)]
/// Responsive horizontal system-history canvas.
///
/// Vertical bars leave this optional module hidden rather than forcing its
/// horizontal history into the bar's cross-axis.
pub struct SystemGraphModule {
    /// Optional category-specific primary-click actions. Each value is an
    /// argument vector, never a shell string. Exact `{category}` and
    /// `{interface}` arguments are replaced at launch time.
    network_actions: NetworkActions,

    /// Optional category-specific secondary-click actions, with the same
    /// exact argument-vector contract as `network_actions`.
    network_secondary_actions: NetworkActions,

    /// See [common options](module-level-options#common-options).
    pub common: Option<CommonConfig>,

    #[serde(skip, default = "default_demand")]
    #[cfg_attr(feature = "extras", schemars(skip))]
    demand: Arc<GraphDemand>,
}

impl Default for SystemGraphModule {
    fn default() -> Self {
        Self {
            network_actions: NetworkActions::default(),
            network_secondary_actions: NetworkActions::default(),
            common: Some(CommonConfig::default()),
            demand: default_demand(),
        }
    }
}

impl Clone for SystemGraphModule {
    fn clone(&self) -> Self {
        // BarConfig is cloned once per output. Demand is per rendered widget,
        // while GraphHub is process-wide; sharing this Arc would let one
        // output's allocation or unmap overwrite another output's demand.
        Self {
            network_actions: self.network_actions.clone(),
            network_secondary_actions: self.network_secondary_actions.clone(),
            common: self.common.clone(),
            demand: default_demand(),
        }
    }
}

impl Module<gtk::Box> for SystemGraphModule {
    type SendMessage = ();
    type ReceiveMessage = ();

    module_impl!("system_graph");

    fn spawn_controller(
        &self,
        info: &ModuleInfo,
        _context: &WidgetContext<Self::SendMessage, Self::ReceiveMessage>,
        _rx: Receiver<Self::ReceiveMessage>,
    ) -> color_eyre::Result<()> {
        if info.bar_position.orientation() != gtk::Orientation::Horizontal {
            return Ok(());
        }
        if !self.network_actions.is_empty() || !self.network_secondary_actions.is_empty() {
            warm_launch_service();
        }
        let hub = GraphHub::global();
        hub.register(&self.demand);
        hub.start();
        Ok(())
    }

    fn into_widget(
        self,
        _context: WidgetContext<Self::SendMessage, Self::ReceiveMessage>,
        info: &ModuleInfo,
    ) -> color_eyre::Result<ModuleParts<gtk::Box>> {
        let container = gtk::Box::new(info.bar_position.orientation(), 0);
        if info.bar_position.orientation() != gtk::Orientation::Horizontal {
            container.set_visible(false);
            return Ok(ModuleParts {
                widget: container,
                popup: None,
            });
        }
        {
            let demand = self.demand.clone();
            container.connect_map(move |_| demand.set_mapped(true));
        }
        {
            let demand = self.demand.clone();
            container.connect_unmap(move |_| demand.set_mapped(false));
        }
        let minimum: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        let area = ResponsiveGraphArea::new(
            Layout::allocation_floor_width(minimum),
            Layout::preferred_width(minimum),
            GRAPH_HEIGHT,
        );

        let frame = Rc::new(RefCell::new(Arc::new(GraphFrame::default())));
        let (cancel_tx, cancel_rx) = oneshot::channel();
        {
            let frame = frame.clone();
            let demand = self.demand.clone();
            let subscription_guard = UiSubscriptionGuard::new(cancel_tx);
            area.set_draw_func(move |area, cairo, width, height| {
                let _keep_subscription_alive = &subscription_guard;
                let frame = frame.borrow();
                let requested = Layout::requested(frame.available);
                let layout = Layout::fit(requested, width);
                let mut probe_candidates = frame.available;
                for metric in frame.probeable.iter() {
                    probe_candidates.insert(metric);
                }
                let probe_layout = Layout::fit(Layout::requested(probe_candidates), width);
                let probe: MetricSet = probe_layout
                    .demand
                    .iter()
                    .filter(|metric| frame.probeable.contains(*metric))
                    .collect();
                demand.store(layout.demand, probe);
                let font = area.pango_context().font_description();
                let font_family = font
                    .as_ref()
                    .and_then(|font| font.family())
                    .and_then(|family| {
                        family
                            .split(',')
                            .next()
                            .map(str::trim)
                            .filter(|family| !family.is_empty())
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| "monospace".to_string());
                let font_size = font
                    .as_ref()
                    .map(|font| f64::from(font.size()) / f64::from(gtk::pango::SCALE))
                    .filter(|size| *size > 0.0)
                    .unwrap_or(12.0);
                if let Err(err) =
                    renderer::draw(cairo, &frame, &layout, height, &font_family, font_size)
                {
                    error!(?err, "failed to draw native system graphs");
                }
            });
        }
        container.append(&area);

        let drawing_area: &DrawingArea = area.upcast_ref();
        install_network_tooltips(
            drawing_area,
            &frame,
            &self.network_actions,
            &self.network_secondary_actions,
        );
        if !self.network_actions.is_empty() {
            install_network_actions(drawing_area, &frame, self.network_actions, BUTTON_PRIMARY);
        }
        if !self.network_secondary_actions.is_empty() {
            install_network_actions(
                drawing_area,
                &frame,
                self.network_secondary_actions,
                BUTTON_SECONDARY,
            );
        }

        subscribe_canvas(&area, &frame, cancel_rx);

        Ok(ModuleParts {
            widget: container,
            popup: None,
        })
    }
}

struct UiSubscriptionGuard(Option<oneshot::Sender<()>>);

impl UiSubscriptionGuard {
    const fn new(cancel: oneshot::Sender<()>) -> Self {
        Self(Some(cancel))
    }
}

impl Drop for UiSubscriptionGuard {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            let _ = cancel.send(());
        }
    }
}

fn subscribe_canvas(
    area: &ResponsiveGraphArea,
    frame: &Rc<RefCell<Arc<GraphFrame>>>,
    mut cancel: oneshot::Receiver<()>,
) {
    let area = area.downgrade();
    let frame = Rc::downgrade(frame);
    let mut updates = GraphHub::global().subscribe();
    glib::spawn_future_local(async move {
        let initial = updates.borrow_and_update().clone();
        if !apply_weak_update(&area, &frame, initial) {
            return;
        }

        loop {
            tokio::select! {
                result = updates.changed() => {
                    if result.is_err() {
                        break;
                    }
                    let update = updates.borrow_and_update().clone();
                    if !apply_weak_update(&area, &frame, update) {
                        break;
                    }
                }
                _ = &mut cancel => break,
            }
        }
    });
}

fn apply_weak_update(
    area: &glib::WeakRef<ResponsiveGraphArea>,
    frame: &Weak<RefCell<Arc<GraphFrame>>>,
    update: Arc<GraphFrame>,
) -> bool {
    let Some(area) = area.upgrade() else {
        return false;
    };
    let Some(frame) = frame.upgrade() else {
        return false;
    };
    let requested = Layout::requested(update.available);
    area.set_widths(
        Layout::allocation_floor_width(requested),
        Layout::preferred_width(requested),
    );
    area.set_visible(requested.bits() != 0);
    *frame.borrow_mut() = update;
    area.queue_draw();
    true
}

fn network_at_x(frame: &GraphFrame, width: i32, x: f64) -> Option<(Metric, &model::NetworkView)> {
    let layout = Layout::fit(Layout::requested(frame.available), width);
    let metric = layout
        .cells
        .iter()
        .find(|cell| x >= cell.x && x < cell.x + cell.width)
        .map(|cell| cell.metric)?;
    frame.network.get(&metric).map(|view| (metric, view))
}

fn install_network_tooltips(
    area: &DrawingArea,
    frame: &Rc<RefCell<Arc<GraphFrame>>>,
    primary_actions: &NetworkActions,
    secondary_actions: &NetworkActions,
) {
    area.set_has_tooltip(true);
    let frame = frame.clone();
    let primary_actions = primary_actions.clone();
    let secondary_actions = secondary_actions.clone();
    area.connect_query_tooltip(move |area, x, _y, keyboard, tooltip| {
        if keyboard {
            return false;
        }
        let frame = frame.borrow();
        let Some((metric, view)) = network_at_x(&frame, area.width(), f64::from(x)) else {
            return false;
        };
        set_network_tooltip(
            tooltip,
            metric,
            &view.current,
            primary_actions.get(metric).is_some() || secondary_actions.get(metric).is_some(),
        );
        true
    });
}

fn install_network_actions(
    area: &DrawingArea,
    frame: &Rc<RefCell<Arc<GraphFrame>>>,
    actions: NetworkActions,
    button: u32,
) {
    let click = GestureClick::builder().button(button).build();
    let frame = frame.clone();
    let area_for_click = area.downgrade();
    click.connect_released(move |_, _, x, _| {
        let Some(area_for_click) = area_for_click.upgrade() else {
            return;
        };
        let frame = frame.borrow();
        let Some((metric, view)) = network_at_x(&frame, area_for_click.width(), x) else {
            return;
        };
        let Some(action) = actions.get(metric) else {
            return;
        };
        launch_network_action(action, metric, &view.current.name);
    });
    area.add_controller(click);
}

fn set_network_tooltip(
    tooltip: &Tooltip,
    metric: Metric,
    network: &model::NetworkHistory,
    clickable: bool,
) {
    tooltip.set_text(Some(&network_tooltip_text(metric, network, clickable)));
}

fn network_tooltip_text(
    metric: Metric,
    network: &model::NetworkHistory,
    clickable: bool,
) -> String {
    let mut status = format!("{} · {}", metric.label(), network.name);
    if !network.active {
        status.push_str(" · inactive");
    }
    if network.total > 1 {
        status.push_str(&format!(" · {}/{}", network.index, network.total));
    }
    status.push_str(&format!(
        "\n↓ {}  ↑ {}",
        format_network_rate(network.rx.current().unwrap_or(0.0)),
        format_network_rate(network.tx.current().unwrap_or(0.0))
    ));
    if clickable {
        status.push_str("\nClick to manage");
    }
    status
}

fn format_network_rate(bytes: f64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;
    if bytes >= GIB {
        format_rate_unit(bytes, GIB, "GiB/s")
    } else if bytes >= MIB {
        format_rate_unit(bytes, MIB, "MiB/s")
    } else if bytes >= KIB {
        format_rate_unit(bytes, KIB, "KiB/s")
    } else {
        format!("{} B/s", bytes.max(0.0) as u64)
    }
}

fn format_rate_unit(bytes: f64, unit: f64, suffix: &str) -> String {
    let tenths = (bytes.max(0.0) * 10.0 / unit) as u64;
    format!("{}.{:01} {suffix}", tenths / 10, tenths % 10)
}

fn launch_network_action(action: &[String], metric: Metric, interface: &str) {
    let Some((program, arguments)) = prepare_network_action(action, metric, interface) else {
        return;
    };
    let mut argv = Vec::with_capacity(arguments.len() + 1);
    argv.push(program.clone());
    argv.extend(arguments);
    let ticket = match submit_detached_argv(argv) {
        Ok(ticket) => ticket,
        Err(error) => {
            error!(?error, %program, "failed to queue native graph network action");
            return;
        }
    };
    spawn(async move {
        if let Err(error) = ticket.await {
            error!(?error, %program, "failed to hand off native graph network action");
        }
    });
}

fn prepare_network_action(
    action: &[String],
    metric: Metric,
    interface: &str,
) -> Option<(String, Vec<String>)> {
    let (program, arguments) = action.split_first()?;
    if program.is_empty() {
        return None;
    }
    let category = metric.label().to_ascii_lowercase();
    let arguments: Vec<_> = arguments
        .iter()
        .map(|argument| match argument.as_str() {
            "{category}" => category.clone(),
            "{interface}" => interface.to_string(),
            _ => argument.clone(),
        })
        .collect();
    let program = program.clone();
    Some((program, arguments))
}

#[cfg(test)]
mod interaction_tests {
    use super::*;

    #[cfg(any(
        feature = "config+corn",
        feature = "config+json",
        feature = "config+yaml",
        feature = "config+toml"
    ))]
    fn assert_config_fixture(content: &'static str, format: ::config::FileFormat) {
        let parsed: crate::config::Config = ::config::Config::builder()
            .add_source(::config::File::from_str(content, format))
            .build()
            .expect("system graph config fixture should build")
            .try_deserialize()
            .expect("system graph config fixture should deserialize");
        let modules = parsed
            .bar
            .center
            .expect("system graph config fixture should contain center modules");
        let [crate::config::ModuleConfig::SystemGraph(module)] = modules.as_slice() else {
            panic!("system graph config fixture should contain exactly one native graph module");
        };
        assert_eq!(
            module.network_actions.get(Metric::Wlan),
            Some(
                ["network-ui", "{category}", "{interface}"]
                    .map(str::to_string)
                    .as_slice()
            )
        );
        assert_eq!(
            module.network_secondary_actions.get(Metric::Wlan),
            Some(["network-editor"].map(str::to_string).as_slice())
        );
    }

    #[test]
    fn category_actions_are_optional_and_data_driven() {
        assert!(NetworkActions::default().is_empty());
        let actions = NetworkActions {
            wlan: Some(vec![
                "network-ui".to_string(),
                "{category}".to_string(),
                "{interface}".to_string(),
            ]),
            ..NetworkActions::default()
        };
        assert!(!actions.is_empty());
        assert!(actions.get(Metric::Lan).is_none());
        let expected = ["network-ui", "{category}", "{interface}"].map(str::to_string);
        assert_eq!(actions.get(Metric::Wlan), Some(expected.as_slice()));

        let prepared = prepare_network_action(
            actions.get(Metric::Wlan).expect("WLAN action should exist"),
            Metric::Wlan,
            "radio;still-one-argument",
        )
        .expect("non-empty WLAN action should prepare");
        assert_eq!(prepared.0, "network-ui");
        assert_eq!(prepared.1, ["wlan", "radio;still-one-argument"]);
        assert!(prepare_network_action(&[String::new()], Metric::Wlan, "radio").is_none());
    }

    #[test]
    fn primary_and_secondary_network_actions_are_independently_optional() {
        let module = SystemGraphModule {
            network_actions: NetworkActions {
                wlan: Some(vec!["radio-menu".to_string()]),
                ..NetworkActions::default()
            },
            network_secondary_actions: NetworkActions {
                wlan: Some(vec!["connection-editor".to_string()]),
                ..NetworkActions::default()
            },
            ..SystemGraphModule::default()
        };

        assert_eq!(
            module.network_actions.get(Metric::Wlan),
            Some(["radio-menu".to_string()].as_slice())
        );
        assert_eq!(
            module.network_secondary_actions.get(Metric::Wlan),
            Some(["connection-editor".to_string()].as_slice())
        );
        assert!(module.network_secondary_actions.get(Metric::Lan).is_none());
    }

    #[test]
    fn tooltip_rate_format_preserves_the_existing_status_contract() {
        assert_eq!(format_network_rate(0.0), "0 B/s");
        assert_eq!(format_network_rate(2048.0), "2.0 KiB/s");
        assert_eq!(format_network_rate(3.5 * 1024.0 * 1024.0), "3.5 MiB/s");

        let mut network = model::NetworkHistory::new("radio-é".to_string(), 4);
        network.active = false;
        assert_eq!(
            network_tooltip_text(Metric::Wlan, &network, true),
            "WLAN · radio-é · inactive\n↓ 0 B/s  ↑ 0 B/s\nClick to manage"
        );
    }

    #[test]
    fn dropping_recreated_canvas_guards_cancels_each_subscription() {
        for _ in 0..2 {
            let (cancel_tx, mut cancel_rx) = oneshot::channel();
            let guard = UiSubscriptionGuard::new(cancel_tx);
            drop(guard);
            assert_eq!(cancel_rx.try_recv(), Ok(()));
        }
    }

    #[test]
    fn cloned_output_modules_have_independent_demand_state() {
        let first = SystemGraphModule::default();
        let second = first.clone();
        assert!(!Arc::ptr_eq(&first.demand, &second.demand));

        first.demand.set_mapped(true);
        first
            .demand
            .store([Metric::Cpu].into_iter().collect(), MetricSet::empty());
        assert_eq!(first.demand.test_state(), (true, 1 << Metric::Cpu as u8, 0));
        assert_eq!(
            second.demand.test_state(),
            (
                false,
                (1 << Metric::Cpu as u8) | (1 << Metric::Ram as u8),
                0,
            )
        );
    }

    #[cfg(feature = "config+corn")]
    #[test]
    fn corn_example_deserializes_the_native_module_and_argv_action() {
        assert_config_fixture(
            include_str!("../../../test-configs/system-graph.corn"),
            ::config::FileFormat::Corn,
        );
    }

    #[cfg(feature = "config+json")]
    #[test]
    fn json_example_deserializes_the_native_module_and_argv_action() {
        assert_config_fixture(
            include_str!("../../../test-configs/system-graph.json"),
            ::config::FileFormat::Json,
        );
    }

    #[cfg(feature = "config+yaml")]
    #[test]
    fn yaml_example_deserializes_the_native_module_and_argv_action() {
        assert_config_fixture(
            include_str!("../../../test-configs/system-graph.yaml"),
            ::config::FileFormat::Yaml,
        );
    }

    #[cfg(feature = "config+toml")]
    #[test]
    fn toml_example_deserializes_the_native_module_and_argv_action() {
        assert_config_fixture(
            include_str!("../../../test-configs/system-graph.toml"),
            ::config::FileFormat::Toml,
        );
    }
}
