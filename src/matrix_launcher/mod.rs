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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExplicitRequest {
    Start(u64),
    Pending,
}

#[derive(Default)]
struct PreparationState {
    order: PreparationOrder,
    explicit_in_flight: Option<u64>,
    explicit_pending: bool,
    warm_in_flight: Option<u64>,
}

impl PreparationState {
    fn request_explicit(&mut self) -> ExplicitRequest {
        if self.explicit_in_flight.is_some() {
            // The queued run reads its inputs only after the current worker has
            // finished, so one bit represents the newest of any number of
            // coalesced requests without retaining a stale snapshot.
            self.explicit_pending = true;
            self.order.begin();
            ExplicitRequest::Pending
        } else {
            let generation = self.order.begin();
            self.explicit_in_flight = Some(generation);
            ExplicitRequest::Start(generation)
        }
    }

    fn finish_explicit(&mut self, generation: u64) -> Option<u64> {
        if self.explicit_in_flight != Some(generation) {
            return None;
        }
        self.explicit_in_flight = None;
        if self.explicit_pending {
            self.explicit_pending = false;
            let generation = self.order.begin();
            self.explicit_in_flight = Some(generation);
            Some(generation)
        } else {
            None
        }
    }

    fn request_warm(&mut self) -> Option<u64> {
        // Startup schedules warmup after the control socket is already live.
        // It is speculation only: it must never supersede an explicit request
        // which arrived in that startup window.
        if self.explicit_in_flight.is_some()
            || self.explicit_pending
            || self.warm_in_flight.is_some()
        {
            return None;
        }
        let generation = self.order.begin();
        self.warm_in_flight = Some(generation);
        Some(generation)
    }

    fn finish_warm(&mut self, generation: u64) {
        if self.warm_in_flight == Some(generation) {
            self.warm_in_flight = None;
        }
    }

    fn invalidate(&self) {
        self.order.begin();
    }

    fn is_current(&self, generation: u64) -> bool {
        self.order.is_current(generation)
    }
}

fn record_internal_dismiss(desired_visible: &Cell<bool>, preparation: &RefCell<PreparationState>) {
    desired_visible.set(false);
    // A preparation that began for the showing which just ended must not be allowed to map its
    // result afterwards. The next explicit show/refresh receives its own newer generation.
    preparation.borrow().invalidate();
}

#[derive(Clone)]
pub struct Launcher {
    application: gtk::Application,
    ui: Rc<RefCell<Option<cbar_launcher::LauncherUi>>>,
    display: Rc<RefCell<Option<gtk::gdk::Display>>>,
    preparation: Rc<RefCell<PreparationState>>,
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
            preparation: Rc::new(RefCell::new(PreparationState::default())),
            desired_visible: Rc::new(Cell::new(false)),
        }
    }

    /// Warm the configured launcher only after cbar's bars have been constructed. An ordinary
    /// Ironbar setup therefore pays no launcher disk/theme work, and even configured setups get
    /// their bar first. Explicit show remains able to initialize on demand if it wins the race.
    pub fn warm(&self) {
        let Some(generation) = self.preparation.borrow_mut().request_warm() else {
            return;
        };
        let launcher = self.clone();
        let prepared = crate::Ironbar::runtime()
            .handle()
            .spawn_blocking(cbar_launcher::prepare_if_configured);
        gtk::glib::spawn_future_local(async move {
            if let Ok(Some(prepared)) = prepared.await
                && launcher.preparation.borrow().is_current(generation)
            {
                if let Some(display) = launcher.launcher_display() {
                    let theme = cbar_launcher::capture_icon_theme(&display);
                    let prepared = crate::Ironbar::runtime()
                        .handle()
                        .spawn_blocking(move || cbar_launcher::prepare_icons(prepared, theme));
                    if let Ok(prepared) = prepared.await
                        && launcher.preparation.borrow().is_current(generation)
                    {
                        launcher.ensure_initialized_with(prepared, &display);
                    }
                } else {
                    eprintln!("cbar launcher: unable to open a GDK display");
                }
            }
            launcher.preparation.borrow_mut().finish_warm(generation);
        });
    }

    /// Initialize on the blocking runtime when an IPC action beats speculative warmup. The action
    /// returns immediately, leaving cbar's bars responsive; no config or mutable-state file is read
    /// on GTK. The same path reloads an existing launcher: a changed config replaces the complete UI
    /// so fields captured by GTK construction (theme, keymap, terminal and focus policy included)
    /// cannot become partially/stickily applied. Repeated overlapping actions coalesce onto one
    /// newest follow-up preparation.
    fn initialize_async(&self) {
        let request = self.preparation.borrow_mut().request_explicit();
        let ExplicitRequest::Start(generation) = request else {
            return;
        };
        self.start_explicit_preparation(generation);
    }

    fn start_explicit_preparation(&self, generation: u64) {
        owner_trace(format_args!("prepare-start generation={generation}"));
        let launcher = self.clone();
        let prepared = crate::Ironbar::runtime()
            .handle()
            .spawn_blocking(cbar_launcher::prepare);
        gtk::glib::spawn_future_local(async move {
            match prepared.await {
                Ok(prepared) => {
                    if launcher.preparation.borrow().is_current(generation) {
                        if let Some(display) = launcher.launcher_display() {
                            let theme = cbar_launcher::capture_icon_theme(&display);
                            let prepared =
                                crate::Ironbar::runtime().handle().spawn_blocking(move || {
                                    cbar_launcher::prepare_icons(prepared, theme)
                                });
                            match prepared.await {
                                Ok(prepared)
                                    if launcher.preparation.borrow().is_current(generation) =>
                                {
                                    launcher.ensure_initialized_with(prepared, &display);
                                }
                                Ok(_) => {}
                                Err(error) => {
                                    eprintln!(
                                        "cbar launcher: icon preparation worker failed: {error}"
                                    )
                                }
                            }
                        } else {
                            eprintln!("cbar launcher: unable to open a GDK display");
                        }
                    }
                }
                Err(error) => eprintln!("cbar launcher: preparation worker failed: {error}"),
            }
            let next = launcher
                .preparation
                .borrow_mut()
                .finish_explicit(generation);
            owner_trace(format_args!(
                "prepare-finish generation={generation} current={} desired_visible={} status={}",
                launcher.preparation.borrow().is_current(generation),
                launcher.desired_visible.get(),
                launcher.status()
            ));
            if let Some(generation) = next {
                launcher.start_explicit_preparation(generation);
            }
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
            let preparation = self.preparation.clone();
            Rc::new(move || record_internal_dismiss(&desired_visible, &preparation))
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
    use super::{ExplicitRequest, PreparationOrder, PreparationState, record_internal_dismiss};
    use std::cell::{Cell, RefCell};

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
        let preparation = RefCell::new(PreparationState::default());
        let desired_visible = Cell::new(true);
        let ExplicitRequest::Start(inflight_prepare) = preparation.borrow_mut().request_explicit()
        else {
            panic!("first explicit preparation should start");
        };

        record_internal_dismiss(&desired_visible, &preparation);
        assert!(!desired_visible.get());
        assert!(!preparation.borrow().is_current(inflight_prepare));

        // A refresh arriving before the stale worker finishes is retained. The
        // old implementation returned early here merely because `initializing`
        // was true, losing the refresh permanently.
        assert_eq!(
            preparation.borrow_mut().request_explicit(),
            ExplicitRequest::Pending
        );
        let refresh = preparation
            .borrow_mut()
            .finish_explicit(inflight_prepare)
            .expect("pending refresh should start after stale preparation finishes");
        assert!(preparation.borrow().is_current(refresh));
        assert!(!desired_visible.get());
    }

    #[test]
    fn startup_warm_cannot_invalidate_a_cold_explicit_show() {
        let mut preparation = PreparationState::default();
        let ExplicitRequest::Start(explicit_show) = preparation.request_explicit() else {
            panic!("cold explicit show should start");
        };

        assert_eq!(preparation.request_warm(), None);
        assert!(preparation.is_current(explicit_show));
        assert_eq!(preparation.finish_explicit(explicit_show), None);
    }

    #[test]
    fn explicit_show_supersedes_an_already_running_warmup() {
        let mut preparation = PreparationState::default();
        let warm = preparation.request_warm().expect("warmup should start");
        let ExplicitRequest::Start(explicit_show) = preparation.request_explicit() else {
            panic!("explicit show should not wait behind speculative warmup");
        };

        assert!(!preparation.is_current(warm));
        assert!(preparation.is_current(explicit_show));
        preparation.finish_warm(warm);
        assert!(preparation.is_current(explicit_show));
    }

    #[test]
    fn repeated_explicit_requests_coalesce_onto_one_newest_run() {
        let mut preparation = PreparationState::default();
        let ExplicitRequest::Start(first) = preparation.request_explicit() else {
            panic!("first request should start");
        };
        assert_eq!(preparation.request_explicit(), ExplicitRequest::Pending);
        assert_eq!(preparation.request_explicit(), ExplicitRequest::Pending);
        assert!(!preparation.is_current(first));

        let newest = preparation
            .finish_explicit(first)
            .expect("coalesced request should start exactly once");
        assert!(preparation.is_current(newest));
        assert_eq!(preparation.finish_explicit(newest), None);
    }
}
