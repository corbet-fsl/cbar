mod r#box;
mod button;
mod image;
mod label;
mod progress;
mod slider;

use self::r#box::BoxWidget;
use self::image::ImageWidget;
use self::label::LabelWidget;
use self::slider::SliderWidget;
use crate::channels::AsyncSenderExt;
use crate::config::{CommonConfig, ModuleConfig};
use crate::module_impl;
use crate::modules::custom::button::ButtonWidget;
use crate::modules::custom::progress::ProgressWidget;
use crate::modules::{
    AnyModuleFactory, BarModuleFactory, Module, ModuleInfo, ModuleParts, ModulePopup,
    ModuleUpdateEvent, PopupButton, PopupModuleFactory, WidgetContext, add_events,
};
use crate::script::Script;
use color_eyre::Result;
use gtk::prelude::*;
use gtk::{Button, Orientation};
use serde::Deserialize;
use std::cell::RefCell;
use std::rc::Rc;
use tokio::sync::mpsc;
use tracing::{debug, error};

#[derive(Debug, Deserialize, Clone)]
#[cfg_attr(feature = "extras", derive(schemars::JsonSchema))]
pub struct CustomModule {
    /// Modules and widgets to add to the bar container.
    ///
    /// **Default**: `[]`
    bar: Vec<WidgetConfig>,

    /// Modules and widgets to add to the popup container.
    ///
    /// **Default**: `null`
    popup: Option<Vec<WidgetConfig>>,

    /// See [common options](module-level-options#common-options).
    #[serde(flatten)]
    pub common: Option<CommonConfig>,
}

#[derive(Debug, Deserialize, Clone)]
#[cfg_attr(feature = "extras", derive(schemars::JsonSchema))]
pub struct WidgetConfig {
    /// One of a custom module native Ironbar module.
    #[serde(flatten)]
    widget: WidgetOrModule,

    /// See [common options](module-level-options#common-options).
    #[serde(flatten)]
    common: CommonConfig,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
#[cfg_attr(feature = "extras", derive(schemars::JsonSchema))]
pub enum WidgetOrModule {
    /// A custom-module specific basic widget
    Widget(Widget),
    /// A native Ironbar module, such as `clock` or `focused`.
    /// All widgets are supported, including their popups.
    Module(ModuleConfig),
}

#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "extras", derive(schemars::JsonSchema))]
pub enum Widget {
    /// A container to place nested widgets inside.
    Box(BoxWidget),
    /// A text label. Pango markup is supported.
    Label(LabelWidget),
    /// A clickable button, which can run a command when clicked.
    Button(ButtonWidget),
    /// An image or icon from disk or http.
    Image(ImageWidget),
    /// A draggable slider.
    Slider(SliderWidget),
    /// A progress bar.
    Progress(ProgressWidget),
}

#[derive(Clone)]
struct CustomWidgetContext<'a> {
    info: &'a ModuleInfo<'a>,
    tx: &'a mpsc::Sender<ExecEvent>,
    bar_orientation: Orientation,
    popup_buttons: Rc<RefCell<Vec<Button>>>,
    module_factory: AnyModuleFactory,
    image_provider: crate::image::Provider,
}

trait CustomWidget {
    type Widget;

    fn into_widget(self, context: CustomWidgetContext) -> Self::Widget;
}

/// Creates a new widget of type `ty`,
/// setting its name and class based on
/// the values available on `self`.
#[macro_export]
macro_rules! build {
    ($self:ident, $ty:ty) => {{
        let mut builder = <$ty>::builder();

        if let Some(name) = &$self.name {
            builder = builder.name(name);
        }

        let widget = builder.build();

        if let Some(class) = &$self.class {
            for part in class.split(' ') {
                widget.add_css_class(part);
            }
        }

        widget
    }};
}

/// Sets the widget length,
/// using either a width or height request
/// based on the bar's orientation.
pub fn set_length<W: WidgetExt>(widget: &W, length: i32, bar_orientation: Orientation) {
    match bar_orientation {
        Orientation::Horizontal => widget.set_width_request(length),
        Orientation::Vertical => widget.set_height_request(length),
        _ => {}
    }
}

impl WidgetOrModule {
    fn add_to(self, parent: &gtk::Box, context: &CustomWidgetContext, common: CommonConfig) {
        match self {
            WidgetOrModule::Widget(widget) => widget.add_to(parent, context, common),
            WidgetOrModule::Module(config) => {
                if let Err(err) = config.create(&context.module_factory, parent, context.info) {
                    error!("{err:?}");
                }
            }
        }
    }
}

impl Widget {
    /// Creates this widget and adds it to the parent container
    fn add_to(self, parent: &gtk::Box, context: &CustomWidgetContext, common: CommonConfig) {
        macro_rules! create {
            ($widget:expr) => {
                add_events(
                    &$widget.into_widget(context.clone()),
                    common,
                    context.bar_orientation,
                )
            };
        }

        let event_box = match self {
            Self::Box(widget) => create!(widget),
            Self::Label(widget) => create!(widget),
            Self::Button(widget) => create!(widget),
            Self::Image(widget) => create!(widget),
            Self::Slider(widget) => create!(widget),
            Self::Progress(widget) => create!(widget),
        };

        parent.append(&event_box);
    }
}

#[derive(Debug)]
pub struct ExecEvent {
    cmd: String,
    args: Option<Vec<String>>,
    id: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuiltinCommand {
    TogglePopup,
    OpenPopup,
    ClosePopup,
    #[cfg(feature = "matrix_launcher")]
    ToggleLauncher,
}

fn builtin_command(command: &str) -> Option<BuiltinCommand> {
    match command {
        "popup:toggle" => Some(BuiltinCommand::TogglePopup),
        "popup:open" => Some(BuiltinCommand::OpenPopup),
        "popup:close" => Some(BuiltinCommand::ClosePopup),
        #[cfg(feature = "matrix_launcher")]
        "launcher:toggle" => Some(BuiltinCommand::ToggleLauncher),
        _ => None,
    }
}

impl Module<gtk::Box> for CustomModule {
    type SendMessage = ();
    type ReceiveMessage = ExecEvent;

    module_impl!("custom");

    fn spawn_controller(
        &self,
        _info: &ModuleInfo,
        context: &WidgetContext<Self::SendMessage, Self::ReceiveMessage>,
        mut rx: mpsc::Receiver<Self::ReceiveMessage>,
    ) -> Result<()> {
        let tx = context.tx.clone();
        #[cfg(feature = "matrix_launcher")]
        let ironbar = context.ironbar.clone();
        gtk::glib::spawn_future_local(async move {
            while let Some(event) = rx.recv().await {
                if event.cmd.starts_with('!') {
                    let script = Script::from(&event.cmd[1..]);

                    debug!("executing command: '{}'", script.cmd);

                    let args = event.args.unwrap_or_default();
                    script.run_as_oneshot(Some(&args));
                } else {
                    match builtin_command(&event.cmd) {
                        Some(BuiltinCommand::TogglePopup) => {
                            tx.send_expect(ModuleUpdateEvent::TogglePopup(event.id))
                                .await;
                        }
                        Some(BuiltinCommand::OpenPopup) => {
                            tx.send_expect(ModuleUpdateEvent::OpenPopup(event.id)).await;
                        }
                        Some(BuiltinCommand::ClosePopup) => {
                            tx.send_expect(ModuleUpdateEvent::ClosePopup).await;
                        }
                        #[cfg(feature = "matrix_launcher")]
                        Some(BuiltinCommand::ToggleLauncher) => {
                            let result = ironbar
                                .matrix_launcher()
                                .ok_or_else(|| "launcher is not initialized".to_string())
                                .and_then(|launcher| launcher.toggle());
                            if let Err(error) = result {
                                error!("Failed to toggle launcher: {error}");
                            }
                        }
                        None => error!("Received invalid command: '{}'", event.cmd),
                    }
                }
            }
        });

        Ok(())
    }

    fn into_widget(
        self,
        mut context: WidgetContext<Self::SendMessage, Self::ReceiveMessage>,
        info: &ModuleInfo,
    ) -> Result<ModuleParts<gtk::Box>> {
        let orientation = info.bar_position.orientation();
        let container = gtk::Box::builder().orientation(orientation).build();

        let popup_buttons = Rc::new(RefCell::new(Vec::new()));

        let custom_context = CustomWidgetContext {
            info,
            tx: &context.controller_tx,
            bar_orientation: orientation,
            popup_buttons: popup_buttons.clone(),
            module_factory: BarModuleFactory::new(
                context.ironbar.clone(),
                context.bar.clone(),
                context.popup.clone(),
            )
            .into(),
            image_provider: context.ironbar.image_provider(),
        };

        self.bar.clone().into_iter().for_each(|widget| {
            widget
                .widget
                .add_to(&container, &custom_context, widget.common);
        });

        for button in popup_buttons.borrow().iter() {
            button.ensure_popup_id();
        }

        context.button_id = popup_buttons
            .borrow()
            .first()
            .map_or(usize::MAX, PopupButton::popup_id);

        let popup = self
            .into_popup(context, info)
            .into_popup_parts_owned(popup_buttons.take());

        Ok(ModuleParts {
            widget: container,
            popup,
        })
    }

    fn into_popup(
        self,
        context: WidgetContext<Self::SendMessage, Self::ReceiveMessage>,
        info: &ModuleInfo,
    ) -> Option<gtk::Box>
    where
        Self: Sized,
    {
        let container = gtk::Box::new(Orientation::Horizontal, 0);

        if let Some(popup) = self.popup {
            let custom_context = CustomWidgetContext {
                info,
                tx: &context.controller_tx,
                bar_orientation: Orientation::Horizontal,
                popup_buttons: Rc::new(RefCell::new(vec![])),
                image_provider: context.ironbar.image_provider(),
                module_factory: PopupModuleFactory::new(
                    context.ironbar,
                    context.bar,
                    context.popup,
                    context.button_id,
                )
                .into(),
            };

            for widget in popup {
                widget
                    .widget
                    .add_to(&container, &custom_context, widget.common);
            }
        }

        Some(container)
    }
}

#[cfg(test)]
mod tests {
    use super::{BuiltinCommand, builtin_command};

    #[test]
    fn builtin_controller_commands_are_exact_and_preserve_popup_actions() {
        assert_eq!(
            builtin_command("popup:toggle"),
            Some(BuiltinCommand::TogglePopup)
        );
        assert_eq!(
            builtin_command("popup:open"),
            Some(BuiltinCommand::OpenPopup)
        );
        assert_eq!(
            builtin_command("popup:close"),
            Some(BuiltinCommand::ClosePopup)
        );

        #[cfg(feature = "matrix_launcher")]
        assert_eq!(
            builtin_command("launcher:toggle"),
            Some(BuiltinCommand::ToggleLauncher)
        );
        #[cfg(not(feature = "matrix_launcher"))]
        assert_eq!(builtin_command("launcher:toggle"), None);

        for invalid in [
            "launcher:show",
            "launcher:toggle ",
            "Launcher:toggle",
            "popup:toggle ",
            "not-a-command",
        ] {
            assert_eq!(builtin_command(invalid), None, "{invalid}");
        }
    }
}
