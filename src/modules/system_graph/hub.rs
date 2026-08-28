use super::model::{
    GraphFrame, HISTORY_LEN, Metric, MetricSet, ROTATION_SECONDS, SAMPLE_INTERVAL_MS,
};
use super::sampler::{Roots, Sampler};
use std::sync::atomic::{AtomicBool, AtomicU16, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::watch;
use tracing::warn;

#[derive(Debug)]
pub struct GraphHub {
    tx: watch::Sender<Arc<GraphFrame>>,
    demands: Mutex<Vec<Weak<GraphDemand>>>,
    started: AtomicBool,
}

#[derive(Debug)]
pub struct GraphDemand {
    mapped: AtomicBool,
    visible: AtomicU16,
    probe: AtomicU16,
}

impl GraphDemand {
    pub fn core() -> Arc<Self> {
        Arc::new(Self {
            mapped: AtomicBool::new(false),
            visible: AtomicU16::new((1 << Metric::Cpu as u8) | (1 << Metric::Ram as u8)),
            probe: AtomicU16::new(0),
        })
    }

    pub fn set_mapped(&self, mapped: bool) {
        self.mapped.store(mapped, Ordering::Release);
    }

    pub fn store(&self, visible: MetricSet, probe: MetricSet) {
        self.visible.store(visible.bits(), Ordering::Release);
        self.probe.store(probe.bits(), Ordering::Release);
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SamplingDemand {
    visible: MetricSet,
    probe: MetricSet,
}

static HUB: OnceLock<Arc<GraphHub>> = OnceLock::new();

impl GraphHub {
    pub fn global() -> Arc<Self> {
        HUB.get_or_init(|| {
            let (tx, _) = watch::channel(Arc::new(GraphFrame::default()));
            Arc::new(Self {
                tx,
                demands: Mutex::new(Vec::new()),
                started: AtomicBool::new(false),
            })
        })
        .clone()
    }

    pub fn register(&self, demand: &Arc<GraphDemand>) {
        self.lock_demands().push(Arc::downgrade(demand));
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
            .name("ironbar-system-graph".to_string())
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
        let mut deadline = Instant::now();
        let mut was_active = false;
        loop {
            if let Some(demand) = self.aggregate_demand() {
                let frame =
                    sampler.sample_with_probe(started.elapsed(), demand.visible, demand.probe);
                let changed = self.tx.borrow().as_ref() != &frame;
                if changed || !was_active {
                    self.tx.send_replace(Arc::new(frame));
                }
                was_active = true;
            } else {
                if was_active {
                    sampler.idle_expensive_sources();
                }
                was_active = false;
            }

            deadline += Duration::from_millis(SAMPLE_INTERVAL_MS);
            if let Some(remaining) = deadline.checked_duration_since(Instant::now()) {
                thread::sleep(remaining);
            } else {
                deadline = Instant::now();
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn hub() -> GraphHub {
        let (tx, _) = watch::channel(Arc::new(GraphFrame::default()));
        GraphHub {
            tx,
            demands: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
        }
    }

    #[test]
    fn sampler_idles_without_graph_consumers() {
        let hub = hub();
        assert_eq!(hub.aggregate_demand(), None);

        let demand = Arc::new(GraphDemand {
            mapped: AtomicBool::new(true),
            visible: AtomicU16::new(1 << Metric::Gpu as u8),
            probe: AtomicU16::new(1 << Metric::Vpu as u8),
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
}
