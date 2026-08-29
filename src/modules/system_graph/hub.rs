use super::model::{
    GraphFrame, HISTORY_LEN, Metric, MetricSet, ROTATION_SECONDS, SAMPLE_INTERVAL_MS,
};
use super::sampler::{Roots, Sampler};
use std::ffi::OsStr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::warn;

#[derive(Debug)]
pub struct GraphHub {
    tx: watch::Sender<Arc<GraphFrame>>,
    demands: Mutex<Vec<Weak<GraphDemand>>>,
    signal: Arc<DemandSignal>,
    started: AtomicBool,
    samples: AtomicU64,
    publishes: AtomicU64,
    redraws: AtomicU64,
    trace: bool,
}

#[derive(Debug)]
pub struct GraphDemand {
    mapped: AtomicBool,
    visible: AtomicU16,
    probe: AtomicU16,
    signal: OnceLock<Weak<DemandSignal>>,
}

#[derive(Debug, Default)]
struct DemandSignal {
    generation: Mutex<u64>,
    changed: Condvar,
}

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PerformanceCounts {
    samples: u64,
    publishes: u64,
    redraws: u64,
}

impl GraphDemand {
    pub fn core() -> Arc<Self> {
        Arc::new(Self {
            mapped: AtomicBool::new(false),
            visible: AtomicU16::new((1 << Metric::Cpu as u8) | (1 << Metric::Ram as u8)),
            probe: AtomicU16::new(0),
            signal: OnceLock::new(),
        })
    }

    pub fn set_mapped(&self, mapped: bool) {
        if self.mapped.swap(mapped, Ordering::AcqRel) != mapped {
            self.notify();
        }
    }

    pub fn store(&self, visible: MetricSet, probe: MetricSet) {
        let visible_changed = self.visible.swap(visible.bits(), Ordering::AcqRel) != visible.bits();
        let probe_changed = self.probe.swap(probe.bits(), Ordering::AcqRel) != probe.bits();
        if visible_changed || probe_changed {
            self.notify();
        }
    }

    fn notify(&self) {
        if let Some(signal) = self.signal.get().and_then(Weak::upgrade) {
            signal.notify();
        }
    }

    #[cfg(test)]
    pub fn test_state(&self) -> (bool, u16, u16) {
        (
            self.mapped.load(Ordering::Acquire),
            self.visible.load(Ordering::Acquire),
            self.probe.load(Ordering::Acquire),
        )
    }
}

impl Drop for GraphDemand {
    fn drop(&mut self) {
        self.notify();
    }
}

impl DemandSignal {
    fn generation(&self) -> u64 {
        *self
            .generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn notify(&self) {
        let mut generation = self
            .generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *generation = generation.wrapping_add(1);
        self.changed.notify_all();
    }

    fn wait_for_change(&self, observed: u64, timeout: Option<Duration>) {
        let generation = self
            .generation
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if *generation != observed {
            return;
        }
        if let Some(timeout) = timeout {
            let _ = self
                .changed
                .wait_timeout_while(generation, timeout, |generation| *generation == observed)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        } else {
            drop(
                self.changed
                    .wait_while(generation, |generation| *generation == observed)
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            );
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SamplingDemand {
    visible: MetricSet,
    probe: MetricSet,
}

static HUB: OnceLock<Arc<GraphHub>> = OnceLock::new();

impl GraphHub {
    pub fn global() -> Arc<Self> {
        HUB.get_or_init(|| {
            Arc::new(Self::new(trace_enabled(
                std::env::var_os("CBAR_GRAPH_TRACE").as_deref(),
            )))
        })
        .clone()
    }

    fn new(trace: bool) -> Self {
        let (tx, _) = watch::channel(Arc::new(GraphFrame::default()));
        Self {
            tx,
            demands: Mutex::new(Vec::new()),
            signal: Arc::new(DemandSignal::default()),
            started: AtomicBool::new(false),
            samples: AtomicU64::new(0),
            publishes: AtomicU64::new(0),
            redraws: AtomicU64::new(0),
            trace,
        }
    }

    pub fn register(self: &Arc<Self>, demand: &Arc<GraphDemand>) {
        let _ = demand.signal.set(Arc::downgrade(&self.signal));
        self.lock_demands().push(Arc::downgrade(demand));
        self.signal.notify();
    }

    pub fn subscribe(&self) -> watch::Receiver<Arc<GraphFrame>> {
        self.tx.subscribe()
    }

    pub fn start(self: &Arc<Self>) {
        if self.started.swap(true, Ordering::AcqRel) {
            return;
        }

        let hub = self.clone();
        if let Err(err) = thread::Builder::new()
            .name("cbar-system-graph".to_string())
            .spawn(move || hub.run())
        {
            // The graph module is optional UI. Resource exhaustion must not
            // prevent the rest of the bar from starting; a later output can
            // retry the one shared sampler thread.
            self.started.store(false, Ordering::Release);
            warn!(?err, "failed to start native graph sampler");
        }
    }

    fn run(&self) {
        let mut sampler = Sampler::production(Roots::default(), HISTORY_LEN, ROTATION_SECONDS);
        let started = Instant::now();
        let interval = Duration::from_millis(SAMPLE_INTERVAL_MS);
        let mut deadline = Instant::now();
        let mut was_active = false;
        loop {
            let observed = self.signal.generation();
            let Some(demand) = self.aggregate_demand() else {
                if was_active {
                    sampler.idle_expensive_sources();
                    if self.trace {
                        eprintln!("cbar-graph-trace event=park");
                    }
                }
                was_active = false;
                self.signal.wait_for_change(observed, None);
                deadline = Instant::now();
                continue;
            };

            let now = Instant::now();
            if !was_active {
                deadline = now;
            }
            if now >= deadline {
                let frame =
                    sampler.sample_with_probe(started.elapsed(), demand.visible, demand.probe);
                let changed = self.tx.borrow().as_ref() != &frame;
                let publish = changed || !was_active;
                if publish {
                    self.tx.send_replace(Arc::new(frame));
                }
                self.record_sample(publish);
                was_active = true;
                deadline += interval;
                if deadline <= now {
                    deadline = now + interval;
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            self.signal.wait_for_change(observed, Some(remaining));
        }
    }

    fn record_sample(&self, publish: bool) {
        if !self.trace {
            return;
        }
        let sample = self.samples.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("cbar-graph-trace event=sample count={sample}");
        if publish {
            let publish = self.publishes.fetch_add(1, Ordering::Relaxed) + 1;
            eprintln!("cbar-graph-trace event=publish count={publish}");
        }
    }

    pub(super) fn record_redraw(&self) {
        if !self.trace {
            return;
        }
        let redraw = self.redraws.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("cbar-graph-trace event=redraw count={redraw}");
    }

    #[cfg(test)]
    fn performance_counts(&self) -> PerformanceCounts {
        PerformanceCounts {
            samples: self.samples.load(Ordering::Relaxed),
            publishes: self.publishes.load(Ordering::Relaxed),
            redraws: self.redraws.load(Ordering::Relaxed),
        }
    }

    fn aggregate_demand(&self) -> Option<SamplingDemand> {
        let mut visible = 0_u16;
        let mut probe = 0_u16;
        let mut consumers = 0_usize;
        self.lock_demands().retain(|demand| {
            if let Some(demand) = demand.upgrade() {
                if demand.mapped.load(Ordering::Acquire) {
                    visible |= demand.visible.load(Ordering::Acquire);
                    probe |= demand.probe.load(Ordering::Acquire);
                    consumers += 1;
                }
                true
            } else {
                false
            }
        });

        if consumers == 0 {
            return None;
        }

        // The core pair supplies useful data before GTK performs its first
        // allocation and remains the minimum useful canvas.
        if visible == 0 {
            visible = (1 << Metric::Cpu as u8) | (1 << Metric::Ram as u8);
        }
        Some(SamplingDemand {
            visible: MetricSet::from_bits(visible),
            probe: MetricSet::from_bits(probe),
        })
    }

    fn lock_demands(&self) -> MutexGuard<'_, Vec<Weak<GraphDemand>>> {
        self.demands.lock().unwrap_or_else(|poisoned| {
            // Graph demand is disposable UI state. Recovering the last valid
            // vector isolates a failed graph consumer from the rest of the bar.
            warn!("recovering poisoned native graph demand state");
            self.demands.clear_poison();
            poisoned.into_inner()
        })
    }
}

fn trace_enabled(value: Option<&OsStr>) -> bool {
    value.is_some_and(|value| {
        !value.is_empty() && value != OsStr::new("0") && value != OsStr::new("false")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn hub() -> Arc<GraphHub> {
        Arc::new(GraphHub::new(false))
    }

    #[test]
    fn sampler_idles_without_graph_consumers() {
        let hub = hub();
        assert_eq!(hub.aggregate_demand(), None);

        let demand = Arc::new(GraphDemand {
            mapped: AtomicBool::new(true),
            visible: AtomicU16::new(1 << Metric::Gpu as u8),
            probe: AtomicU16::new(1 << Metric::Vpu as u8),
            signal: OnceLock::new(),
        });
        hub.register(&demand);
        assert_eq!(
            hub.aggregate_demand(),
            Some(SamplingDemand {
                visible: [Metric::Gpu].into_iter().collect(),
                probe: [Metric::Vpu].into_iter().collect(),
            })
        );

        drop(demand);
        assert_eq!(hub.aggregate_demand(), None);
    }

    #[test]
    fn unmapped_consumers_do_not_keep_the_sampler_active() {
        let hub = hub();
        let demand = GraphDemand::core();
        hub.register(&demand);
        assert_eq!(hub.aggregate_demand(), None);

        demand.set_mapped(true);
        assert!(hub.aggregate_demand().is_some());
        demand.set_mapped(false);
        assert_eq!(hub.aggregate_demand(), None);
    }

    #[test]
    fn poisoned_optional_demand_state_is_recovered() {
        let hub = hub();
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = hub.demands.lock().expect("test lock should start healthy");
            panic!("poison graph-only fixture state");
        }));

        assert_eq!(hub.aggregate_demand(), None);
        assert!(!hub.demands.is_poisoned());
    }

    #[test]
    fn map_and_drop_notify_a_parked_sampler_without_polling() {
        let hub = hub();
        let demand = GraphDemand::core();
        hub.register(&demand);
        let registered = hub.signal.generation();

        demand.set_mapped(true);
        let mapped = hub.signal.generation();
        assert_ne!(mapped, registered);

        demand.set_mapped(false);
        let unmapped = hub.signal.generation();
        assert_ne!(unmapped, mapped);

        drop(demand);
        assert_ne!(hub.signal.generation(), unmapped);
        assert_eq!(hub.aggregate_demand(), None);
    }

    #[test]
    fn opt_in_trace_does_not_change_sample_or_redraw_accounting() {
        let quiet = GraphHub::new(false);
        let traced = GraphHub::new(true);
        for hub in [&quiet, &traced] {
            hub.record_sample(true);
            hub.record_sample(false);
            hub.record_redraw();
        }
        assert_eq!(
            quiet.performance_counts(),
            PerformanceCounts {
                samples: 0,
                publishes: 0,
                redraws: 0,
            }
        );
        assert_eq!(
            traced.performance_counts(),
            PerformanceCounts {
                samples: 2,
                publishes: 1,
                redraws: 1,
            }
        );
    }

    #[test]
    fn trace_flag_is_explicit_and_false_values_stay_off() {
        assert!(!trace_enabled(None));
        assert!(!trace_enabled(Some(OsStr::new(""))));
        assert!(!trace_enabled(Some(OsStr::new("0"))));
        assert!(!trace_enabled(Some(OsStr::new("false"))));
        assert!(trace_enabled(Some(OsStr::new("1"))));
    }
}
