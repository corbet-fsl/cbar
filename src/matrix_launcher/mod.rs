//! The launcher is another window of cbar's existing GTK application.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::OnceLock;

fn owner_tracing_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("CBAR_LAUNCHER_TRACE").is_some())
}

fn owner_trace(fields: std::fmt::Arguments<'_>) {
    if owner_tracing_on() {
        eprintln!("cbar-launcher-owner-trace {fields}");
    }
}

#[derive(Default)]
struct PreparationOrder(Cell<u64>);

impl PreparationOrder {
    fn begin(&self) -> u64 {
        let mut generation = self.0.get().wrapping_add(1);
        if generation == 0 {
            generation = 1;
        }
        self.0.set(generation);
        generation
    }

    fn is_current(&self, generation: u64) -> bool {
        self.0.get() == generation
    }
}

fn record_internal_dismiss(desired_visible: &Cell<bool>, order: &PreparationOrder) {
    desired_visible.set(false);
    // A preparation that began for the showing which just ended must not be allowed to map its
    // result afterwards. The next explicit show/refresh receives its own newer generation.
    order.begin();
}

#[derive(Clone)]
pub struct Launcher {
    application: gtk::Application,
    ui: Rc<RefCell<Option<cbar_launcher::LauncherUi>>>,
    display: Rc<RefCell<Option<gtk::gdk::Display>>>,
    initializing: Rc<Cell<bool>>,
    preparation_order: Rc<PreparationOrder>,
    desired_visible: Rc<Cell<bool>>,
}

impl std::fmt::Debug for Launcher {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Launcher")
            .field("initialized", &self.ui.borrow().is_some())
            .finish_non_exhaustive()
    }
}

impl Launcher {
    pub fn new(application: &gtk::Application) -> Self {
        Self {
            application: application.clone(),
            ui: Rc::new(RefCell::new(None)),
            display: Rc::new(RefCell::new(None)),
            initializing: Rc::new(Cell::new(false)),
            preparation_order: Rc::new(PreparationOrder::default()),
            desired_visible: Rc::new(Cell::new(false)),
        }
    }

    /// Warm the configured launcher only after cbar's bars have been constructed. An ordinary
    /// Ironbar setup therefore pays no launcher disk/theme work, and even configured setups get
    /// their bar first. Explicit show remains able to initialize on demand if it wins the race.
    pub fn warm(&self) {
        let generation = self.preparation_order.begin();
        let launcher = self.clone();
        let prepared = crate::Ironbar::runtime()
            .handle()
            .spawn_blocking(cbar_launcher::prepare_if_configured);
        gtk::glib::spawn_future_local(async move {
            if let Ok(Some(prepared)) = prepared.await {
                if !launcher.preparation_order.is_current(generation) {
                    return;
                }
                let Some(display) = launcher.launcher_display() else {
                    eprintln!("cbar launcher: unable to open a GDK display");
                    return;
                };
                let theme = cbar_launcher::capture_icon_theme(&display);
                let prepared = crate::Ironbar::runtime()
                    .handle()
                    .spawn_blocking(move || cbar_launcher::prepare_icons(prepared, theme));
                if let Ok(prepared) = prepared.await
                    && launcher.preparation_order.is_current(generation)
                {
                    launcher.ensure_initialized_with(prepared, &display);
                }
            }
        });
    }

    /// Initialize on the blocking runtime when an IPC action beats speculative warmup. The action
    /// returns immediately, leaving cbar's bars responsive; no config or mutable-state file is read
    /// on GTK. The same path reloads an existing launcher: a changed config replaces the complete UI
    /// so fields captured by GTK construction (theme, keymap, terminal and focus policy included)
    /// cannot become partially/stickily applied. Repeated actions coalesce onto one preparation.
    fn initialize_async(&self) {
        if self.initializing.replace(true) {
            return;
        }
        let generation = self.preparation_order.begin();
        owner_trace(format_args!("prepare-start generation={generation}"));
        let launcher = self.clone();
        let prepared = crate::Ironbar::runtime()
            .handle()
            .spawn_blocking(cbar_launcher::prepare);
        gtk::glib::spawn_future_local(async move {
            match prepared.await {
                Ok(prepared) => {
                    if !launcher.preparation_order.is_current(generation) {
                        launcher.initializing.set(false);
                        return;
                    }
                    let Some(display) = launcher.launcher_display() else {
                        eprintln!("cbar launcher: unable to open a GDK display");
                        launcher.initializing.set(false);
                        return;
                    };
                    let theme = cbar_launcher::capture_icon_theme(&display);
                    let prepared = crate::Ironbar::runtime()
                        .handle()
                        .spawn_blocking(move || cbar_launcher::prepare_icons(prepared, theme));
                    match prepared.await {
                        Ok(prepared) if launcher.preparation_order.is_current(generation) => {
                            launcher.ensure_initialized_with(prepared, &display);
                        }
                        Ok(_) => {}
                        Err(error) => {
                            eprintln!("cbar launcher: icon preparation worker failed: {error}")
                        }
                    }
                }
                Err(error) => eprintln!("cbar launcher: preparation worker failed: {error}"),
            }
            launcher.initializing.set(false);
            owner_trace(format_args!(
                "prepare-finish generation={generation} current={} desired_visible={} status={}",
                launcher.preparation_order.is_current(generation),
                launcher.desired_visible.get(),
                launcher.status()
            ));
        });
    }

    fn launcher_display(&self) -> Option<gtk::gdk::Display> {
        if let Some(display) = self.display.borrow().as_ref() {
            return Some(display.clone());
        }
        // gtk4-layer-shell owns one process-global layer-shell proxy bound to the first Wayland
        // display which initializes it. cbar's bar has already done that, so a launcher wl_surface
        // from a second GDK connection would be passed to a proxy from the first connection and the
        // compositor must reject the cross-connection request. Embedded layer surfaces therefore
        // share the application's default display. Launcher CSS remains isolated by its scoped
        // selectors, explicit reset, and USER+1 priority in cbar-launcher.
        let display = gtk::gdk::Display::default()?;
        self.display.replace(Some(display.clone()));
        Some(display)
    }

    fn ensure_initialized_with(
        &self,
        mut prepared: cbar_launcher::PreparedLauncher,
        display: &gtk::gdk::Display,
    ) {
        let on_dismiss: Rc<dyn Fn()> = {
            let desired_visible = self.desired_visible.clone();
            let preparation_order = self.preparation_order.clone();
            Rc::new(move || record_internal_dismiss(&desired_visible, &preparation_order))
        };
        let current = self.ui.borrow().as_ref().cloned();
        match current {
            None => {
                self.ui.replace(Some(
                    cbar_launcher::LauncherUi::attach_prepared_with_dismiss(
                        &self.application,
                        crate::Ironbar::runtime().handle(),
                        prepared,
                        display,
                        on_dismiss,
                    ),
                ));
            }
            Some(current) if current.prepare_replacement(&mut prepared) => {
                let replacement = cbar_launcher::LauncherUi::attach_prepared_with_dismiss(
                    &self.application,
                    crate::Ironbar::runtime().handle(),
                    prepared,
                    display,
                    on_dismiss,
                );
                self.ui.replace(Some(replacement));
                current.retire();
            }
            Some(current) => current.refresh(),
        }
        self.apply_desired_visibility();
    }

    fn apply_desired_visibility(&self) {
        let ui = self.ui.borrow().as_ref().cloned();
        let Some(ui) = ui else {
            return;
        };
        if self.desired_visible.get() && !ui.is_visible() {
            ui.show();
        } else if !self.desired_visible.get() && ui.is_visible() {
            ui.hide();
        }
    }

    pub fn show(&self) -> Result<(), String> {
        self.desired_visible.set(true);
        // A coherent resident UI is the launcher's local last-known-good state. Map it before
        // asking the blocking pool to check config/state again: pool pressure, a slow filesystem,
        // or a half-written declarative update must never make an already-resident launcher look
        // dead. A prepared replacement is still swapped atomically when it arrives.
        self.apply_desired_visibility();
        self.initialize_async();
        Ok(())
    }

    pub fn hide(&self) {
        self.desired_visible.set(false);
        self.apply_desired_visibility();
    }

    pub fn toggle(&self) -> Result<(), String> {
        let visible = self
            .ui
            .borrow()
            .as_ref()
            .map_or_else(|| self.desired_visible.get(), |ui| ui.is_visible());
        self.desired_visible.set(!visible);
        if visible {
            self.apply_desired_visibility();
        } else {
            self.apply_desired_visibility();
            self.initialize_async();
        }
        Ok(())
    }

    pub fn refresh(&self) -> Result<(), String> {
        self.initialize_async();
        Ok(())
    }

    pub fn status(&self) -> String {
        match self.ui.borrow().as_ref() {
            Some(ui) if ui.is_visible() => "visible".to_string(),
            Some(_) => "resident".to_string(),
            None => "uninitialized".to_string(),
        }
    }

    pub fn owns_window(&self, window: &gtk::Window) -> bool {
        self.ui
            .borrow()
            .as_ref()
            .is_some_and(|ui| ui.owns_window(window))
    }
}

#[cfg(test)]
mod tests {
    use super::{PreparationOrder, record_internal_dismiss};
    use std::cell::Cell;

    #[test]
    fn newest_started_preparation_wins_even_when_it_finishes_first() {
        let order = PreparationOrder::default();
        let slow_warm = order.begin();
        let explicit_show = order.begin();

        assert!(order.is_current(explicit_show));
        assert!(!order.is_current(slow_warm));

        // Finishing the stale warmup later cannot make it current again.
        assert!(!order.is_current(slow_warm));
        assert!(order.is_current(explicit_show));
    }

    #[test]
    fn internal_dismiss_invalidates_inflight_prepare_and_stays_hidden_on_refresh() {
        let order = PreparationOrder::default();
        let desired_visible = Cell::new(true);
        let inflight_prepare = order.begin();

        record_internal_dismiss(&desired_visible, &order);
        assert!(!desired_visible.get());
        assert!(!order.is_current(inflight_prepare));

        // A later refresh is allowed to prepare newer state, but it does not change visibility
        // intent and therefore cannot turn a normal Escape/blur/launch dismissal into a reopen.
        let refresh = order.begin();
        assert!(order.is_current(refresh));
        assert!(!desired_visible.get());
    }
}
