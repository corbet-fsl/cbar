use super::model::{
    Capability, GraphFrame, HISTORY_LEN, Metric, MetricSet, NetworkHistory, NetworkView, Provider,
    ROTATION_SECONDS, SAMPLE_INTERVAL_MS,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tracing::debug;

const NETWORK_FADE_FRAMES: u8 = 3;
// Targeted DRM-client reads follow the visible canvas cadence. Anything
// shorter than one frame is inherently below the graph's temporal resolution;
// jobs lasting at least one frame must leave a history sample.
const DRM_INTERVAL_NS: u64 = SAMPLE_INTERVAL_MS * 1_000_000;
// Filesystem topology changes are human-scale events. Keep them out of the
// 500ms history path while bounding hotplug discovery for a fitted cell.
const TOPOLOGY_DISCOVERY_NS: u64 = 5_000_000_000;
// Native open events trigger immediate client inventory. This periodic pass is
// only a safety net for a lost event or a replaced watch.
const DRM_LINK_REVALIDATE_NS: u64 = 30_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    pub proc: PathBuf,
    pub sys: PathBuf,
    pub dev: PathBuf,
}

impl Default for Roots {
    fn default() -> Self {
        Self {
            proc: PathBuf::from("/proc"),
            sys: PathBuf::from("/sys"),
            dev: PathBuf::from("/dev"),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ProbeCounts {
    pub hot_samples: usize,
    pub capability_passes: usize,
    pub drm_topology_scans: usize,
    pub disk_topology_scans: usize,
    pub npu_topology_scans: usize,
    pub network_topology_scans: usize,
    pub drm_discovery_scans: usize,
    pub drm_sample_scans: usize,
    pub drm_fd_link_reads: usize,
    pub drm_fdinfo_reads: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum EngineFamily {
    Gpu,
    Vpu,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum DiscoveryGroup {
    Disk,
    Drm,
    Npu,
    Network,
}

impl DiscoveryGroup {
    const ALL: [Self; 4] = [Self::Disk, Self::Drm, Self::Npu, Self::Network];

    const fn metrics(self) -> MetricSet {
        let bits = match self {
            Self::Disk => 1 << Metric::Io as u8,
            Self::Drm => (1 << Metric::Vpu as u8) | (1 << Metric::Gpu as u8),
            Self::Npu => 1 << Metric::Npu as u8,
            Self::Network => {
                (1 << Metric::Lan as u8)
                    | (1 << Metric::Wlan as u8)
                    | (1 << Metric::Wwan as u8)
                    | (1 << Metric::Vpn as u8)
            }
        };
        MetricSet::from_bits(bits)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct EngineKey {
    device: String,
    client: String,
    engine: String,
    family: EngineFamily,
}

#[derive(Debug, Clone, Copy)]
struct EngineCounter {
    busy: u64,
    total: Option<u64>,
}

#[derive(Debug, Default)]
struct DrmSnapshot {
    counters: HashMap<EngineKey, EngineCounter>,
    gpu: bool,
    vpu: bool,
    gpu_devices: HashSet<String>,
    vpu_devices: HashSet<String>,
}

#[derive(Debug)]
struct DrmProcessCache {
    clients: Vec<DrmClientPath>,
}

#[derive(Debug, Clone)]
struct DrmClientPath {
    fdinfo: PathBuf,
    device_hint: Option<String>,
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct DrmOpenMonitor {
    _watcher: notify::RecommendedWatcher,
    dirty: Arc<AtomicBool>,
}

#[cfg(target_os = "linux")]
impl DrmOpenMonitor {
    fn new(path: &Path) -> Option<Self> {
        use notify::{RecursiveMode, Watcher};

        let dirty = Arc::new(AtomicBool::new(true));
        let event_dirty = dirty.clone();
        let mut watcher = notify::recommended_watcher(move |_| {
            // OPEN catches a new DRM client even when it reuses the exact same
            // descriptor number. Create/remove/overflow events also request a
            // conservative rescan through the same callback.
            event_dirty.store(true, Ordering::Release);
        })
        .ok()?;
        watcher.watch(path, RecursiveMode::NonRecursive).ok()?;
        Some(Self {
            _watcher: watcher,
            dirty,
        })
    }
}

#[derive(Debug, Clone)]
struct TimedCounter {
    value: u64,
    at_ns: u64,
}

#[derive(Debug, Clone)]
struct NetworkInterface {
    name: String,
    metric: Metric,
    path: PathBuf,
    active: bool,
}

#[derive(Debug)]
pub struct Sampler {
    roots: Roots,
    history_len: usize,
    rotation_seconds: u64,
    frame: GraphFrame,
    drm_discovered_at_ns: Option<u64>,
    drm_gpu_capable: bool,
    drm_vpu_capable: bool,
    drm_gpu_devices: HashSet<String>,
    drm_vpu_devices: HashSet<String>,
    drm_topology: HashSet<String>,
    drm_gpu_instances: usize,
    drm_vpu_instances: usize,
    disk_devices: HashSet<String>,
    direct_gpu_paths: Vec<PathBuf>,
    npu_paths: Vec<PathBuf>,
    network_interfaces: Vec<NetworkInterface>,
    topology_discovered_at_ns: HashMap<DiscoveryGroup, u64>,
    cpu_previous: Option<(u64, u64)>,
    io_previous: HashMap<String, TimedCounter>,
    npu_previous: HashMap<PathBuf, TimedCounter>,
    drm_previous: HashMap<EngineKey, EngineCounter>,
    drm_previous_at_ns: Option<u64>,
    drm_sampled_at_ns: Option<u64>,
    drm_current_gpu: Option<f64>,
    drm_current_vpu: Option<f64>,
    drm_processes: HashMap<PathBuf, DrmProcessCache>,
    drm_inventory_at_ns: Option<u64>,
    drm_monitor_allowed: bool,
    #[cfg(target_os = "linux")]
    drm_monitor_attempted: bool,
    drm_rescan_signal: Option<Arc<AtomicBool>>,
    #[cfg(target_os = "linux")]
    drm_open_monitor: Option<DrmOpenMonitor>,
    network_previous_rx: HashMap<String, TimedCounter>,
    network_previous_tx: HashMap<String, TimedCounter>,
    network_histories: HashMap<String, NetworkHistory>,
    network_selected: HashMap<Metric, String>,
    network_previous_selected: HashMap<Metric, String>,
    network_fade: HashMap<Metric, u8>,
    capabilities: BTreeMap<(Metric, Provider), usize>,
    probes: ProbeCounts,
}

impl Default for Sampler {
    fn default() -> Self {
        Self::new(Roots::default(), HISTORY_LEN, ROTATION_SECONDS)
    }
}

impl Sampler {
    pub fn new(roots: Roots, history_len: usize, rotation_seconds: u64) -> Self {
        Self {
            roots,
            history_len: history_len.max(2),
            rotation_seconds: rotation_seconds.max(1),
            frame: GraphFrame::default(),
            drm_discovered_at_ns: None,
            drm_gpu_capable: false,
            drm_vpu_capable: false,
            drm_gpu_devices: HashSet::new(),
            drm_vpu_devices: HashSet::new(),
            drm_topology: HashSet::new(),
            drm_gpu_instances: 0,
            drm_vpu_instances: 0,
            disk_devices: HashSet::new(),
            direct_gpu_paths: Vec::new(),
            npu_paths: Vec::new(),
            network_interfaces: Vec::new(),
            topology_discovered_at_ns: HashMap::new(),
            cpu_previous: None,
            io_previous: HashMap::new(),
            npu_previous: HashMap::new(),
            drm_previous: HashMap::new(),
            drm_previous_at_ns: None,
            drm_sampled_at_ns: None,
            drm_current_gpu: None,
            drm_current_vpu: None,
            drm_processes: HashMap::new(),
            drm_inventory_at_ns: None,
            drm_monitor_allowed: false,
            #[cfg(target_os = "linux")]
            drm_monitor_attempted: false,
            drm_rescan_signal: None,
            #[cfg(target_os = "linux")]
            drm_open_monitor: None,
            network_previous_rx: HashMap::new(),
            network_previous_tx: HashMap::new(),
            network_histories: HashMap::new(),
            network_selected: HashMap::new(),
            network_previous_selected: HashMap::new(),
            network_fade: HashMap::new(),
            capabilities: BTreeMap::new(),
            probes: ProbeCounts::default(),
        }
    }

    pub fn production(roots: Roots, history_len: usize, rotation_seconds: u64) -> Self {
        let mut sampler = Self::new(roots, history_len, rotation_seconds);
        sampler.drm_monitor_allowed = true;
        sampler
    }

    pub fn idle_expensive_sources(&mut self) {
        self.drm_discovered_at_ns = None;
        self.drm_previous.clear();
        self.drm_previous_at_ns = None;
        self.drm_sampled_at_ns = None;
        self.drm_current_gpu = None;
        self.drm_current_vpu = None;
        self.drm_processes.clear();
        self.drm_inventory_at_ns = None;
        self.drm_rescan_signal = None;
        #[cfg(target_os = "linux")]
        {
            self.drm_monitor_attempted = false;
            self.drm_open_monitor = None;
        }
    }

    fn forget_drm_topology(&mut self) {
        self.idle_expensive_sources();
        self.drm_topology.clear();
        self.drm_gpu_devices.clear();
        self.drm_vpu_devices.clear();
        self.sync_drm_capabilities();
    }

    #[cfg(test)]
    fn synthetic_drm_open_events(&mut self) -> Arc<AtomicBool> {
        let dirty = Arc::new(AtomicBool::new(true));
        self.drm_rescan_signal = Some(dirty.clone());
        dirty
    }

    #[cfg(test)]
    pub const fn probe_counts(&self) -> ProbeCounts {
        self.probes
    }

    pub fn capabilities(&self) -> Vec<Capability> {
        self.capabilities
            .iter()
            .map(|(&(metric, provider), &instances)| Capability {
                metric,
                provider,
                instances,
            })
            .collect()
    }

    #[cfg(test)]
    pub fn sample(&mut self, now: Duration, demand: MetricSet) -> GraphFrame {
        self.sample_with_probe(now, demand, MetricSet::empty())
    }

    pub fn sample_with_probe(
        &mut self,
        now: Duration,
        demand: MetricSet,
        probe: MetricSet,
    ) -> GraphFrame {
        self.probes.hot_samples += 1;
        let now_ns = now.as_nanos().min(u128::from(u64::MAX)) as u64;
        let interest = MetricSet::from_bits(demand.bits() | probe.bits());
        self.refresh_topology(now_ns, interest);

        let cpu_available = self.roots.proc.join("stat").is_file();
        self.set_capability(Metric::Cpu, Provider::ProcStat, usize::from(cpu_available));
        let memory = self.read_memory();
        self.set_capability(
            Metric::Ram,
            Provider::ProcMeminfo,
            usize::from(memory.is_some()),
        );
        self.set_capability(
            Metric::Swap,
            Provider::ProcMeminfo,
            usize::from(memory.is_some_and(|memory| memory.swap_percent.is_some())),
        );

        let mut drm_interest_bits =
            (probe.bits() | demand.bits()) & ((1 << Metric::Vpu as u8) | (1 << Metric::Gpu as u8));
        if !self.direct_gpu_paths.is_empty() {
            drm_interest_bits &= !(1 << Metric::Gpu as u8);
        }
        let drm_discovery_due = self
            .drm_discovered_at_ns
            .is_none_or(|previous| now_ns.saturating_sub(previous) >= DRM_INTERVAL_NS);
        let mut missing_interesting_drm = MetricSet::empty();
        if drm_interest_bits & (1 << Metric::Vpu as u8) != 0 && !self.drm_vpu_capable {
            missing_interesting_drm.insert(Metric::Vpu);
        }
        if drm_interest_bits & (1 << Metric::Gpu as u8) != 0
            && !self.drm_gpu_capable
            && self.direct_gpu_paths.is_empty()
        {
            missing_interesting_drm.insert(Metric::Gpu);
        }
        let visible_drm_due = ((demand.contains(Metric::Vpu) && self.drm_vpu_capable)
            || (demand.contains(Metric::Gpu)
                && self.drm_gpu_capable
                && self.direct_gpu_paths.is_empty()))
            && self
                .drm_sampled_at_ns
                .is_none_or(|previous| now_ns.saturating_sub(previous) >= DRM_INTERVAL_NS);
        let has_drm_topology = !self.drm_topology.is_empty();
        let drm_inventory_due =
            has_drm_topology && drm_interest_bits != 0 && self.drm_inventory_due(now_ns);
        if has_drm_topology
            && drm_discovery_due
            && drm_inventory_due
            && missing_interesting_drm.bits() != 0
            && !visible_drm_due
        {
            self.discover_drm(now_ns, true);
            self.drm_discovered_at_ns = Some(now_ns);
        }

        if demand.contains(Metric::Cpu)
            && let Some(value) = self.sample_cpu()
        {
            self.frame.push_scalar(Metric::Cpu, value, self.history_len);
        }

        if let Some(memory) = memory {
            if demand.contains(Metric::Ram) {
                self.frame
                    .push_scalar(Metric::Ram, memory.ram_percent, self.history_len);
            }
            if demand.contains(Metric::Swap)
                && let Some(swap) = memory.swap_percent
            {
                self.frame.push_scalar(Metric::Swap, swap, self.history_len);
            }
        }

        if demand.contains(Metric::Io) {
            let diskstats = self.read_diskstats();
            if let Some(value) = self.sample_io(now_ns, diskstats.as_deref()) {
                self.frame.push_scalar(Metric::Io, value, self.history_len);
            }
        } else {
            self.io_previous.clear();
        }

        if demand.contains(Metric::Npu) {
            if let Some(value) = self.sample_npu(now_ns) {
                self.frame.push_scalar(Metric::Npu, value, self.history_len);
            }
        } else {
            self.npu_previous.clear();
        }

        let direct_gpu = demand
            .contains(Metric::Gpu)
            .then(|| self.sample_direct_gpu())
            .flatten();
        if let Some(value) = direct_gpu {
            self.frame.push_scalar(Metric::Gpu, value, self.history_len);
        }

        let need_drm_gpu = demand.contains(Metric::Gpu) && self.direct_gpu_paths.is_empty();
        let need_drm_vpu = demand.contains(Metric::Vpu);
        if need_drm_gpu || need_drm_vpu {
            let due = self
                .drm_sampled_at_ns
                .is_none_or(|previous| now_ns.saturating_sub(previous) >= DRM_INTERVAL_NS);
            if due {
                let (gpu, vpu) = self.sample_drm(now_ns, drm_inventory_due);
                self.drm_current_gpu = gpu;
                self.drm_current_vpu = vpu;
                self.drm_sampled_at_ns = Some(now_ns);
            }
            if need_drm_gpu && let Some(value) = self.drm_current_gpu {
                self.frame.push_scalar(Metric::Gpu, value, self.history_len);
            }
            if need_drm_vpu && let Some(value) = self.drm_current_vpu {
                self.frame.push_scalar(Metric::Vpu, value, self.history_len);
            }
        } else if drm_interest_bits == 0 {
            // A capability-only probe establishes the first counter snapshot
            // before GTK can expose the newly available cell. Preserve that
            // baseline across the draw/allocation hand-off; discard it only
            // when no mapped canvas is interested in either DRM metric.
            self.drm_previous.clear();
            self.drm_previous_at_ns = None;
            self.drm_sampled_at_ns = None;
            self.drm_current_gpu = None;
            self.drm_current_vpu = None;
            self.drm_processes.clear();
            self.drm_inventory_at_ns = None;
        }

        self.sample_network(now_ns, demand);
        self.sync_frame_sets(now_ns);
        self.seed_available_scalar_views();
        self.frame.capabilities = self.capabilities();
        self.frame.clone()
    }

    fn refresh_topology(&mut self, now_ns: u64, interest: MetricSet) {
        let mut discovered = false;
        for group in DiscoveryGroup::ALL {
            if interest.bits() & group.metrics().bits() == 0 || !self.topology_due(group, now_ns) {
                continue;
            }
            match group {
                DiscoveryGroup::Disk => self.discover_disk_topology(),
                DiscoveryGroup::Drm => self.discover_drm_topology(),
                DiscoveryGroup::Npu => self.discover_npu_topology(),
                DiscoveryGroup::Network => self.discover_network_topology(),
            }
            self.topology_discovered_at_ns.insert(group, now_ns);
            discovered = true;
        }
        self.probes.capability_passes += usize::from(discovered);
    }

    fn topology_due(&self, group: DiscoveryGroup, now_ns: u64) -> bool {
        self.topology_discovered_at_ns
            .get(&group)
            .is_none_or(|previous| now_ns.saturating_sub(*previous) >= TOPOLOGY_DISCOVERY_NS)
    }

    fn discover_disk_topology(&mut self) {
        self.probes.disk_topology_scans += 1;
        self.disk_devices = if self.roots.proc.join("diskstats").is_file() {
            fs::read_dir(self.roots.sys.join("block"))
                .into_iter()
                .flatten()
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect()
        } else {
            HashSet::new()
        };
        self.set_capability(Metric::Io, Provider::ProcDiskstats, self.disk_devices.len());
        if self.disk_devices.is_empty() {
            self.io_previous.clear();
        }
    }

    fn discover_drm_topology(&mut self) {
        self.probes.drm_topology_scans += 1;
        let topology = self.drm_topology_tokens();
        self.retain_drm_capabilities(&topology);
        self.drm_topology = topology;
        self.direct_gpu_paths = self.direct_gpu_paths();
        self.set_capability(
            Metric::Gpu,
            Provider::DrmBusySysfs,
            self.direct_gpu_paths.len(),
        );
        if self.drm_topology.is_empty() {
            self.forget_drm_topology();
        }
    }

    fn discover_npu_topology(&mut self) {
        self.probes.npu_topology_scans += 1;
        self.npu_paths = self.npu_paths();
        self.set_capability(Metric::Npu, Provider::NpuBusySysfs, self.npu_paths.len());
        if self.npu_paths.is_empty() {
            self.npu_previous.clear();
        }
    }

    fn discover_network_topology(&mut self) {
        self.probes.network_topology_scans += 1;
        self.network_interfaces = self.network_interfaces();
        for metric in Metric::NETWORK {
            let count = self
                .network_interfaces
                .iter()
                .filter(|interface| interface.metric == metric && interface.active)
                .count();
            self.set_capability(metric, Provider::NetworkSysfs, count);
        }
    }

    fn sync_frame_sets(&mut self, now_ns: u64) {
        self.frame.available = self
            .capabilities
            .keys()
            .map(|(metric, _)| *metric)
            .collect();

        let mut probeable = MetricSet::empty();
        for group in DiscoveryGroup::ALL {
            let drm_clients_need_probe =
                group == DiscoveryGroup::Drm && !self.drm_topology.is_empty();
            if !drm_clients_need_probe && !self.topology_due(group, now_ns) {
                continue;
            }
            for metric in group.metrics().iter() {
                if !self.frame.available.contains(metric) {
                    probeable.insert(metric);
                }
            }
        }
        self.frame.probeable = probeable;
    }

    fn set_capability(&mut self, metric: Metric, provider: Provider, instances: usize) {
        if instances == 0 {
            self.capabilities.remove(&(metric, provider));
        } else {
            self.capabilities.insert((metric, provider), instances);
        }
    }

    fn seed_available_scalar_views(&mut self) {
        let missing: Vec<_> = self
            .frame
            .available
            .iter()
            .filter(|metric| !metric.is_network() && !self.frame.scalar.contains_key(metric))
            .collect();
        for metric in missing {
            // Delta providers need one counter baseline before they can report
            // a rate. Availability must never reserve an entirely blank cell;
            // publish a deterministic zero view until the first delta arrives.
            self.frame.push_scalar(metric, 0.0, self.history_len);
        }
    }

    fn ensure_drm_open_monitor(&mut self) {
        if !self.drm_monitor_allowed || self.drm_rescan_signal.is_some() {
            return;
        }
        #[cfg(target_os = "linux")]
        {
            let path = self.roots.dev.join("dri");
            if !path.is_dir() || self.drm_monitor_attempted {
                return;
            }
            self.drm_monitor_attempted = true;
            if let Some(monitor) = DrmOpenMonitor::new(&path) {
                self.drm_rescan_signal = Some(monitor.dirty.clone());
                self.drm_open_monitor = Some(monitor);
            }
        }
    }

    fn drm_inventory_due(&mut self, now_ns: u64) -> bool {
        self.ensure_drm_open_monitor();
        self.drm_rescan_signal.as_ref().is_none_or(|dirty| {
            dirty.load(Ordering::Acquire)
                || self.drm_inventory_at_ns.is_none_or(|previous| {
                    now_ns.saturating_sub(previous) >= DRM_LINK_REVALIDATE_NS
                })
        })
    }

    fn discover_drm(&mut self, now_ns: u64, refresh_inventory: bool) {
        self.probes.drm_discovery_scans += 1;
        let snapshot = self.read_drm_snapshot(now_ns, refresh_inventory);
        self.observe_drm_capabilities(&snapshot);
        if self.drm_previous.is_empty() && !snapshot.counters.is_empty() {
            self.drm_previous = snapshot.counters;
            self.drm_previous_at_ns = Some(now_ns);
            self.drm_sampled_at_ns = Some(now_ns);
            self.drm_current_gpu = self.drm_gpu_capable.then_some(0.0);
            self.drm_current_vpu = self.drm_vpu_capable.then_some(0.0);
        }
        debug!(
            gpu = self.drm_gpu_capable || !self.direct_gpu_paths.is_empty(),
            vpu = self.drm_vpu_capable,
            "discovered native DRM graph sources"
        );
    }

    fn observe_drm_capabilities(&mut self, snapshot: &DrmSnapshot) {
        self.drm_gpu_devices
            .extend(snapshot.gpu_devices.iter().cloned());
        self.drm_vpu_devices
            .extend(snapshot.vpu_devices.iter().cloned());
        self.sync_drm_capabilities();
    }

    fn retain_drm_capabilities(&mut self, topology: &HashSet<String>) {
        self.drm_gpu_devices
            .retain(|device| device == "unknown" || topology.contains(device));
        self.drm_vpu_devices
            .retain(|device| device == "unknown" || topology.contains(device));
        if topology.is_empty() {
            self.drm_gpu_devices.clear();
            self.drm_vpu_devices.clear();
        }
        self.sync_drm_capabilities();
    }

    fn sync_drm_capabilities(&mut self) {
        self.drm_gpu_capable = !self.drm_gpu_devices.is_empty();
        self.drm_vpu_capable = !self.drm_vpu_devices.is_empty();
        self.drm_gpu_instances = self.drm_gpu_devices.len();
        self.drm_vpu_instances = self.drm_vpu_devices.len();
        self.set_capability(Metric::Gpu, Provider::DrmFdinfo, self.drm_gpu_instances);
        self.set_capability(Metric::Vpu, Provider::DrmFdinfo, self.drm_vpu_instances);
    }

    fn sample_cpu(&mut self) -> Option<f64> {
        let content = fs::read_to_string(self.roots.proc.join("stat")).ok()?;
        let line = content.lines().find(|line| line.starts_with("cpu "))?;
        let fields: Vec<u64> = line
            .split_whitespace()
            .skip(1)
            .filter_map(|value| value.parse().ok())
            .collect();
        if fields.len() < 4 {
            return None;
        }

        // guest and guest_nice (fields 9/10) are already included in user and
        // nice by the procfs ABI; summing them again inflates virtualized CPU
        // totals. Aggregate every CPU count, but only the first eight fields.
        let total = fields.iter().take(8).copied().sum::<u64>();
        let idle = fields[3].saturating_add(fields.get(4).copied().unwrap_or(0));
        let previous = self.cpu_previous.replace((total, idle));
        let (previous_total, previous_idle) = previous?;
        let elapsed = total.saturating_sub(previous_total);
        if elapsed == 0 {
            return Some(0.0);
        }
        let idle_elapsed = idle.saturating_sub(previous_idle).min(elapsed);
        Some((elapsed - idle_elapsed) as f64 * 100.0 / elapsed as f64)
    }

    fn read_memory(&self) -> Option<MemorySample> {
        let content = fs::read_to_string(self.roots.proc.join("meminfo")).ok()?;
        memory_from_str(&content)
    }

    fn read_diskstats(&self) -> Option<String> {
        fs::read_to_string(self.roots.proc.join("diskstats")).ok()
    }

    fn sample_io(&mut self, now_ns: u64, diskstats: Option<&str>) -> Option<f64> {
        let diskstats = diskstats?;
        let mut busiest = None::<f64>;
        let mut seen = HashSet::new();
        for line in diskstats.lines() {
            let fields: Vec<_> = line.split_whitespace().collect();
            if fields.len() < 13 {
                continue;
            }
            let name = fields[2];
            if !self.disk_devices.contains(name) {
                continue;
            }
            let Ok(ticks_ms) = fields[12].parse::<u64>() else {
                continue;
            };
            seen.insert(name.to_string());
            let current = TimedCounter {
                value: ticks_ms,
                at_ns: now_ns,
            };
            if let Some(previous) = self.io_previous.insert(name.to_string(), current)
                && let Some(value) = timed_percent(previous, ticks_ms, now_ns, 1_000_000)
            {
                busiest = Some(busiest.map_or(value, |best| best.max(value)));
            }
        }
        self.io_previous.retain(|name, _| seen.contains(name));
        busiest
    }

    fn direct_gpu_paths(&self) -> Vec<PathBuf> {
        let Ok(cards) = fs::read_dir(self.roots.sys.join("class/drm")) else {
            return Vec::new();
        };
        let mut files: Vec<_> = cards
            .filter_map(Result::ok)
            .filter(|entry| numeric_name(&entry.file_name().to_string_lossy(), "card"))
            .map(|entry| entry.path().join("device/gpu_busy_percent"))
            .filter(|path| read_u64(path).is_some())
            .collect();
        files.sort();
        files
    }

    fn drm_topology_tokens(&self) -> HashSet<String> {
        let mut tokens = HashSet::new();
        if let Ok(entries) = fs::read_dir(self.roots.dev.join("dri")) {
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if numeric_name(&name, "renderD") || numeric_name(&name, "card") {
                    tokens.insert(name);
                }
            }
        }
        if let Ok(entries) = fs::read_dir(self.roots.sys.join("class/drm")) {
            for entry in entries.filter_map(Result::ok) {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !numeric_name(&name, "renderD") && !numeric_name(&name, "card") {
                    continue;
                }
                tokens.insert(name);
                let device = entry.path().join("device");
                if let Ok(target) = fs::read_link(&device)
                    && let Some(name) = target.file_name()
                {
                    tokens.insert(name.to_string_lossy().into_owned());
                }
                if let Ok(target) = fs::canonicalize(&device)
                    && let Some(name) = target.file_name()
                    && name != "device"
                {
                    tokens.insert(name.to_string_lossy().into_owned());
                }
                if let Ok(uevent) = fs::read_to_string(device.join("uevent")) {
                    for token in uevent
                        .lines()
                        .filter_map(|line| line.strip_prefix("PCI_SLOT_NAME="))
                    {
                        tokens.insert(token.to_string());
                    }
                }
            }
        }
        tokens
    }

    fn sample_direct_gpu(&self) -> Option<f64> {
        self.direct_gpu_paths
            .iter()
            .filter_map(|path| read_u64(path).map(|value| value.min(100) as f64))
            .reduce(f64::max)
    }

    fn npu_paths(&self) -> Vec<PathBuf> {
        let Ok(accels) = fs::read_dir(self.roots.sys.join("class/accel")) else {
            return Vec::new();
        };
        let mut files: Vec<_> = accels
            .filter_map(Result::ok)
            .map(|entry| entry.path().join("device/npu_busy_time_us"))
            .filter(|path| read_u64(path).is_some())
            .collect();
        files.sort();
        files
    }

    fn sample_npu(&mut self, now_ns: u64) -> Option<f64> {
        let mut busiest = None::<f64>;
        let mut seen = HashSet::new();
        for path in &self.npu_paths {
            let Some(value_us) = read_u64(path) else {
                continue;
            };
            seen.insert(path.clone());
            let current = TimedCounter {
                value: value_us,
                at_ns: now_ns,
            };
            if let Some(previous) = self.npu_previous.insert(path.clone(), current)
                && let Some(value) = timed_percent(previous, value_us, now_ns, 1_000)
            {
                busiest = Some(busiest.map_or(value, |best| best.max(value)));
            }
        }
        self.npu_previous.retain(|path, _| seen.contains(path));
        busiest
    }

    fn sample_drm(&mut self, now_ns: u64, refresh_inventory: bool) -> (Option<f64>, Option<f64>) {
        self.probes.drm_sample_scans += 1;
        let snapshot = self.read_drm_snapshot(now_ns, refresh_inventory);
        self.observe_drm_capabilities(&snapshot);
        self.drm_discovered_at_ns = Some(now_ns);
        let mut groups: HashMap<(String, String, EngineFamily), (u64, u64)> = HashMap::new();
        let elapsed_ns = self
            .drm_previous_at_ns
            .map(|previous| now_ns.saturating_sub(previous))
            .unwrap_or(0);

        for (key, current) in &snapshot.counters {
            let Some(previous) = self.drm_previous.get(key) else {
                continue;
            };
            let busy = current.busy.saturating_sub(previous.busy);
            let denominator = match (current.total, previous.total) {
                (Some(current), Some(previous)) => current.saturating_sub(previous),
                _ => elapsed_ns,
            };
            if denominator == 0 {
                continue;
            }
            let group = groups
                .entry((key.device.clone(), key.engine.clone(), key.family))
                .or_default();
            group.0 = group.0.saturating_add(busy);
            group.1 = group.1.max(denominator);
        }

        let gpu_available = self.drm_gpu_capable;
        let vpu_available = self.drm_vpu_capable;
        self.drm_previous = snapshot.counters;
        self.drm_previous_at_ns = Some(now_ns);
        let mut gpu = None::<f64>;
        let mut vpu = None::<f64>;
        for ((_, _, family), (busy, total)) in groups {
            if total == 0 {
                continue;
            }
            let value = (busy as f64 * 100.0 / total as f64).clamp(0.0, 100.0);
            let target = match family {
                EngineFamily::Gpu => &mut gpu,
                EngineFamily::Vpu => &mut vpu,
            };
            *target = Some(target.map_or(value, |best| best.max(value)));
        }
        (
            gpu.or(gpu_available.then_some(0.0)),
            vpu.or(vpu_available.then_some(0.0)),
        )
    }

    fn read_drm_snapshot(&mut self, now_ns: u64, refresh_inventory: bool) -> DrmSnapshot {
        let mut snapshot = DrmSnapshot::default();
        if !refresh_inventory {
            for cached in self.drm_processes.values() {
                for client in &cached.clients {
                    self.probes.drm_fdinfo_reads += 1;
                    read_fdinfo(client, &mut snapshot);
                }
            }
            return snapshot;
        }

        if let Some(dirty) = &self.drm_rescan_signal {
            // Clear before enumeration so an OPEN racing the scan remains set
            // for the next graph frame.
            dirty.store(false, Ordering::Release);
        }
        self.drm_inventory_at_ns = Some(now_ns);
        let Ok(processes) = fs::read_dir(&self.roots.proc) else {
            self.drm_processes.clear();
            return snapshot;
        };

        let mut seen_processes = HashSet::new();

        for process in processes.filter_map(Result::ok) {
            if !process
                .file_name()
                .to_string_lossy()
                .bytes()
                .all(|byte| byte.is_ascii_digit())
            {
                continue;
            }
            let process_path = process.path();
            seen_processes.insert(process_path.clone());
            let mut clients = Vec::new();
            if let Ok(entries) = fs::read_dir(process_path.join("fd")) {
                for fd in entries.filter_map(Result::ok) {
                    self.probes.drm_fd_link_reads += 1;
                    // ENOENT is the normal race when a busy process closes an
                    // fd between readdir and readlink. Skipping it must not
                    // explode into a full fdinfo scan for that process.
                    let Ok(target) = fs::read_link(fd.path()) else {
                        continue;
                    };
                    if is_drm_device_target(&target) {
                        clients.push(DrmClientPath {
                            fdinfo: process_path.join("fdinfo").join(fd.file_name()),
                            device_hint: target
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned()),
                        });
                    }
                }
            } else {
                // Synthetic proc roots and some hidepid/security configurations
                // expose fdinfo without a resolvable fd directory. Retain that
                // correctness fallback only for the unavailable-root case.
                if let Ok(fdinfos) = fs::read_dir(process_path.join("fdinfo")) {
                    clients.extend(fdinfos.filter_map(Result::ok).map(|entry| DrmClientPath {
                        fdinfo: entry.path(),
                        device_hint: None,
                    }));
                }
            }

            for client in &clients {
                self.probes.drm_fdinfo_reads += 1;
                read_fdinfo(client, &mut snapshot);
            }
            self.drm_processes
                .insert(process_path, DrmProcessCache { clients });
        }
        self.drm_processes
            .retain(|process, _| seen_processes.contains(process));
        snapshot
    }

    fn network_interfaces(&self) -> Vec<NetworkInterface> {
        let Ok(entries) = fs::read_dir(self.roots.sys.join("class/net")) else {
            return Vec::new();
        };
        let primary_routes = default_route_interfaces(&self.roots.proc);
        let mut interfaces: Vec<_> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| classify_network(entry.path(), &primary_routes))
            .collect();
        interfaces.sort_by(|left, right| left.name.cmp(&right.name));
        interfaces
    }

    fn sample_network(&mut self, now_ns: u64, demand: MetricSet) {
        let mut names_seen = HashSet::new();
        for metric in Metric::NETWORK {
            let candidate_indices: Vec<_> = self
                .network_interfaces
                .iter()
                .enumerate()
                .filter(|(_, interface)| interface.metric == metric)
                .map(|(index, _)| index)
                .collect();

            for index in &candidate_indices {
                let interface = &self.network_interfaces[*index];
                names_seen.insert(interface.name.clone());
            }

            if demand.contains(metric) {
                for index in &candidate_indices {
                    let path = self.network_interfaces[*index].path.clone();
                    let interface_metric = self.network_interfaces[*index].metric;
                    let active = network_active(&path, interface_metric);
                    self.network_interfaces[*index].active = active;
                }
            }

            let candidates: Vec<_> = candidate_indices
                .iter()
                .map(|index| self.network_interfaces[*index].clone())
                .collect();
            let active_candidates: Vec<_> = candidates
                .iter()
                .filter(|interface| interface.active)
                .cloned()
                .collect();
            self.set_capability(metric, Provider::NetworkSysfs, active_candidates.len());

            if active_candidates.is_empty() {
                self.frame.network.remove(&metric);
                self.network_selected.remove(&metric);
                self.network_previous_selected.remove(&metric);
                self.network_fade.remove(&metric);
                for interface in &candidates {
                    self.network_previous_rx.remove(&interface.name);
                    self.network_previous_tx.remove(&interface.name);
                }
                continue;
            }

            let slot =
                (now_ns / 1_000_000_000 / self.rotation_seconds) as usize % active_candidates.len();
            let selected = &active_candidates[slot];

            if !demand.contains(metric) {
                for interface in &candidates {
                    self.network_previous_rx.remove(&interface.name);
                    self.network_previous_tx.remove(&interface.name);
                }

                // Preserve the one metadata view seeded when topology was
                // discovered. Hidden categories do not need link-state reads,
                // rotation bookkeeping or cloned frame updates every 500ms;
                // all of those refresh on the first demanded sample.
                if self.frame.network.get(&metric).is_some_and(|view| {
                    active_candidates
                        .iter()
                        .any(|interface| interface.name == view.current.name)
                }) {
                    continue;
                }

                // Availability and layout are produced before GTK can publish
                // demand for a newly discovered category. Keep a zero-rate
                // metadata view so that first frame paints the category label
                // instead of reserving a blank cell. No traffic counters are
                // read until the fitted canvas actually demands this metric.
                let history = self
                    .network_histories
                    .entry(selected.name.clone())
                    .or_insert_with(|| {
                        let mut history =
                            NetworkHistory::new(selected.name.clone(), self.history_len);
                        history.rx.push(0.0);
                        history.tx.push(0.0);
                        history
                    });
                history.index = slot + 1;
                history.total = active_candidates.len();
                history.active = true;
                self.network_selected.insert(metric, selected.name.clone());
                self.network_previous_selected.remove(&metric);
                self.network_fade.remove(&metric);
                self.frame.network.insert(
                    metric,
                    NetworkView {
                        current: history.clone(),
                        previous: None,
                        previous_alpha: 0.0,
                    },
                );
                continue;
            }

            for interface in candidates.iter().filter(|interface| !interface.active) {
                self.network_previous_rx.remove(&interface.name);
                self.network_previous_tx.remove(&interface.name);
            }
            for (index, interface) in active_candidates.iter().enumerate() {
                let counters = read_u64(&interface.path.join("statistics/rx_bytes"))
                    .zip(read_u64(&interface.path.join("statistics/tx_bytes")));
                let (rx_rate, tx_rate) = if let Some((rx, tx)) = counters {
                    (
                        network_rate(&mut self.network_previous_rx, &interface.name, rx, now_ns),
                        network_rate(&mut self.network_previous_tx, &interface.name, tx, now_ns),
                    )
                } else {
                    // An active link remains actionable even when a driver or
                    // container omits statistics. Do not retain counters across
                    // that gap, because a later reset is not traffic.
                    self.network_previous_rx.remove(&interface.name);
                    self.network_previous_tx.remove(&interface.name);
                    (0.0, 0.0)
                };
                let history = self
                    .network_histories
                    .entry(interface.name.clone())
                    .or_insert_with(|| {
                        NetworkHistory::new(interface.name.clone(), self.history_len)
                    });
                history.index = index + 1;
                history.total = active_candidates.len();
                history.active = true;
                history.rx.push(rx_rate);
                history.tx.push(tx_rate);
            }

            let selected = selected.name.clone();
            if self.network_selected.get(&metric) != Some(&selected)
                && let Some(previous) = self.network_selected.insert(metric, selected.clone())
            {
                self.network_previous_selected.insert(metric, previous);
                self.network_fade.insert(metric, NETWORK_FADE_FRAMES);
            }

            let Some(current) = self.network_histories.get(&selected).cloned() else {
                continue;
            };
            let fade = self.network_fade.get(&metric).copied().unwrap_or(0);
            let previous = self
                .network_previous_selected
                .get(&metric)
                .and_then(|name| self.network_histories.get(name))
                .cloned();
            self.frame.network.insert(
                metric,
                NetworkView {
                    current,
                    previous,
                    previous_alpha: 0.35 * f64::from(fade) / f64::from(NETWORK_FADE_FRAMES),
                },
            );
            if fade > 0 {
                self.network_fade.insert(metric, fade - 1);
            } else {
                self.network_previous_selected.remove(&metric);
            }
        }

        self.network_previous_rx
            .retain(|name, _| names_seen.contains(name));
        self.network_previous_tx
            .retain(|name, _| names_seen.contains(name));
        self.network_histories
            .retain(|name, _| names_seen.contains(name));
    }
}

#[derive(Debug, Clone, Copy)]
struct MemorySample {
    ram_percent: f64,
    swap_percent: Option<f64>,
}

fn memory_from_str(content: &str) -> Option<MemorySample> {
    let values: HashMap<_, _> = content
        .lines()
        .filter_map(|line| {
            let (name, raw) = line.split_once(':')?;
            let value = raw.split_whitespace().next()?.parse::<u64>().ok()?;
            Some((name, value))
        })
        .collect();
    let total = *values.get("MemTotal")?;
    let available = *values.get("MemAvailable")?;
    if total == 0 {
        return None;
    }
    let ram_percent = total.saturating_sub(available) as f64 * 100.0 / total as f64;
    let swap_total = values.get("SwapTotal").copied().unwrap_or(0);
    let swap_free = values.get("SwapFree").copied().unwrap_or(0);
    let swap_percent = (swap_total > 0)
        .then(|| swap_total.saturating_sub(swap_free) as f64 * 100.0 / swap_total as f64);
    Some(MemorySample {
        ram_percent,
        swap_percent,
    })
}

fn timed_percent(
    previous: TimedCounter,
    current: u64,
    now_ns: u64,
    counter_unit_ns: u64,
) -> Option<f64> {
    let elapsed = now_ns.saturating_sub(previous.at_ns);
    if elapsed == 0 || current < previous.value {
        return None;
    }
    Some((current - previous.value) as f64 * counter_unit_ns as f64 * 100.0 / elapsed as f64)
        .map(|value| value.clamp(0.0, 100.0))
}

fn read_u64(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn numeric_name(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn is_drm_device_target(path: &Path) -> bool {
    path.file_name()
        .map(|name| name.to_string_lossy())
        .is_some_and(|name| numeric_name(&name, "renderD") || numeric_name(&name, "card"))
}

fn read_fdinfo(client: &DrmClientPath, snapshot: &mut DrmSnapshot) {
    if let Ok(content) = fs::read_to_string(&client.fdinfo) {
        parse_fdinfo(
            &content,
            &client.fdinfo,
            client.device_hint.as_deref(),
            snapshot,
        );
    }
}

fn default_route_interfaces(proc_root: &Path) -> HashSet<String> {
    let mut interfaces = HashSet::new();
    if let Ok(routes) = fs::read_to_string(proc_root.join("net/route")) {
        for fields in routes
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
        {
            if fields.len() >= 8 && fields[1] == "00000000" && fields[7] == "00000000" {
                interfaces.insert(fields[0].to_string());
            }
        }
    }
    if let Ok(routes) = fs::read_to_string(proc_root.join("net/ipv6_route")) {
        for fields in routes
            .lines()
            .map(|line| line.split_whitespace().collect::<Vec<_>>())
        {
            if fields.len() >= 10
                && fields[0].bytes().all(|byte| byte == b'0')
                && fields[1] == "00"
                && let Some(interface) = fields.last()
            {
                interfaces.insert((*interface).to_string());
            }
        }
    }
    interfaces
}

fn virtual_lan_has_physical_backing(path: &Path) -> bool {
    let Some(net_root) = path.parent() else {
        return false;
    };
    let mut pending = vec![path.to_path_buf()];
    let mut visited = HashSet::new();
    while let Some(candidate) = pending.pop() {
        let Some(name) = candidate
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
        else {
            continue;
        };
        if !visited.insert(name) {
            continue;
        }
        if is_physical_wired_interface(&candidate) {
            return true;
        }
        pending.extend(
            linked_lower_interfaces(&candidate)
                .into_iter()
                .map(|lower| net_root.join(lower))
                .filter(|lower| lower.exists()),
        );
    }
    false
}

fn linked_lower_interfaces(path: &Path) -> HashSet<String> {
    let mut lowers = HashSet::new();
    if let Ok(entries) = fs::read_dir(path.join("brif")) {
        lowers.extend(
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().to_string_lossy().into_owned()),
        );
    }
    if let Ok(slaves) = fs::read_to_string(path.join("bonding/slaves")) {
        lowers.extend(slaves.split_whitespace().map(str::to_string));
    }
    if let Ok(entries) = fs::read_dir(path) {
        lowers.extend(entries.filter_map(Result::ok).filter_map(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .strip_prefix("lower_")
                .filter(|name| !name.is_empty())
                .map(str::to_string)
        }));
    }
    lowers
}

fn has_lower_interface_entry(path: &Path) -> bool {
    fs::read_dir(path).is_ok_and(|mut entries| {
        entries.any(|entry| {
            entry.is_ok_and(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .strip_prefix("lower_")
                    .is_some_and(|name| !name.is_empty())
            })
        })
    })
}

fn is_physical_wired_interface(path: &Path) -> bool {
    if !path.join("device").exists()
        || fs::read_to_string(path.join("type"))
            .ok()
            .is_none_or(|link_type| link_type.trim() != "1")
        || path.join("wireless").exists()
        || path.join("qmi/raw_ip").exists()
        || path.join("wwan").exists()
        || path.join("bridge").exists()
        || path.join("bonding").exists()
    {
        return false;
    }
    let uevent = fs::read_to_string(path.join("uevent")).unwrap_or_default();
    !uevent.lines().any(|line| {
        matches!(
            line.strip_prefix("DEVTYPE="),
            Some("wlan" | "wwan" | "bridge" | "bond" | "vlan")
        )
    })
}

fn classify_network(path: PathBuf, primary_routes: &HashSet<String>) -> Option<NetworkInterface> {
    let name = path.file_name()?.to_string_lossy().into_owned();
    if name == "lo" {
        return None;
    }
    let uevent = fs::read_to_string(path.join("uevent")).unwrap_or_default();
    let devtype = uevent
        .lines()
        .find_map(|line| line.strip_prefix("DEVTYPE="));
    let link_type = fs::read_to_string(path.join("type")).unwrap_or_default();
    let virtual_lan = matches!(devtype, Some("bridge" | "bond" | "vlan"))
        || path.join("bridge").exists()
        || path.join("bonding").exists()
        || has_lower_interface_entry(&path);
    let metric = match () {
        // Positive WWAN topology must precede ARPHRD_NONE: qmi_wwan raw-IP
        // links use that same link type as WireGuard/TUN.
        _ if devtype == Some("wwan")
            || path.join("qmi/raw_ip").exists()
            || path.join("wwan").exists() =>
        {
            Metric::Wwan
        }
        // Real wireless uevents often omit DEVTYPE. The kernel's wireless
        // topology directory is stable across arbitrary interface names.
        _ if devtype == Some("wlan") || path.join("wireless").exists() => Metric::Wlan,
        _ if matches!(devtype, Some("wireguard" | "tun")) || path.join("tun_flags").exists() => {
            Metric::Vpn
        }
        _ if virtual_lan
            && (primary_routes.contains(&name) || virtual_lan_has_physical_backing(&path)) =>
        {
            Metric::Lan
        }
        // Do not rotate container/libvirt-only bridges through the primary
        // wired graph merely because their kernel topology is bridge-shaped.
        _ if virtual_lan => return None,
        _ if link_type.trim() == "1"
            && (path.join("device").exists() || primary_routes.contains(&name)) =>
        {
            Metric::Lan
        }
        // WireGuard, TUN and NetBird commonly expose ARPHRD_NONE without a
        // DEVTYPE. The link type is a kernel topology signal and remains valid
        // for arbitrary interface names; no wg/wt/tun prefix heuristic is
        // needed.
        _ if link_type.trim() == "65534" => Metric::Vpn,
        _ => return None,
    };
    let active = network_active(&path, metric);
    Some(NetworkInterface {
        name,
        metric,
        path,
        active,
    })
}

fn network_active(path: &Path, metric: Metric) -> bool {
    let state = fs::read_to_string(path.join("operstate")).unwrap_or_default();
    let carrier = fs::read_to_string(path.join("carrier")).unwrap_or_default();
    state.trim() == "up"
        || carrier.trim() == "1"
        || (metric == Metric::Vpn && state.trim() == "unknown")
}

fn network_rate(
    previous: &mut HashMap<String, TimedCounter>,
    name: &str,
    current: u64,
    now_ns: u64,
) -> f64 {
    let old = previous.insert(
        name.to_string(),
        TimedCounter {
            value: current,
            at_ns: now_ns,
        },
    );
    old.and_then(|old| {
        let elapsed = now_ns.saturating_sub(old.at_ns);
        (elapsed > 0 && current >= old.value)
            .then(|| (current - old.value) as f64 * 1_000_000_000.0 / elapsed as f64)
    })
    .unwrap_or(0.0)
}

fn parse_fdinfo(content: &str, path: &Path, device_hint: Option<&str>, snapshot: &mut DrmSnapshot) {
    let fields: HashMap<_, _> = content
        .lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim(), value.trim()))
        .collect();
    let driver = fields.get("drm-driver").copied().unwrap_or("drm");
    let reported_device = fields
        .get("drm-pdev")
        .or_else(|| fields.get("drm-device"))
        .copied();
    let topology_device = device_hint.or(reported_device).unwrap_or("unknown");
    let counter_device = reported_device.or(device_hint).unwrap_or("unknown");
    let client = fields
        .get("drm-client-id")
        .copied()
        .map(str::to_string)
        .unwrap_or_else(|| path.display().to_string());

    for (key, raw) in &fields {
        let Some(engine) = key.strip_prefix("drm-cycles-") else {
            continue;
        };
        let Some(family) = engine_family(engine) else {
            continue;
        };
        let Some(busy) = parse_counter(raw) else {
            continue;
        };
        let total = fields
            .get(format!("drm-total-cycles-{engine}").as_str())
            .and_then(|value| parse_counter(value));
        insert_engine(
            snapshot,
            EngineKey {
                device: format!("{driver}:{counter_device}"),
                client: client.clone(),
                engine: engine.to_string(),
                family,
            },
            busy,
            total,
            topology_device,
        );
    }

    for (key, raw) in &fields {
        let Some(engine) = key.strip_prefix("drm-engine-") else {
            continue;
        };
        let Some(family) = engine_family(engine) else {
            continue;
        };
        let Some(busy) = parse_duration_ns(raw) else {
            continue;
        };
        insert_engine(
            snapshot,
            EngineKey {
                device: format!("{driver}:{counter_device}"),
                client: client.clone(),
                engine: engine.to_string(),
                family,
            },
            busy,
            None,
            topology_device,
        );
    }
}

fn insert_engine(
    snapshot: &mut DrmSnapshot,
    key: EngineKey,
    busy: u64,
    total: Option<u64>,
    topology_device: &str,
) {
    match key.family {
        EngineFamily::Gpu => {
            snapshot.gpu = true;
            snapshot.gpu_devices.insert(topology_device.to_string());
        }
        EngineFamily::Vpu => {
            snapshot.vpu = true;
            snapshot.vpu_devices.insert(topology_device.to_string());
        }
    }
    snapshot
        .counters
        .entry(key)
        .and_modify(|counter| {
            counter.busy = counter.busy.max(busy);
            counter.total = match (counter.total, total) {
                (Some(left), Some(right)) => Some(left.max(right)),
                (left, right) => left.or(right),
            };
        })
        .or_insert(EngineCounter { busy, total });
}

fn engine_family(engine: &str) -> Option<EngineFamily> {
    let engine = engine.to_ascii_lowercase();
    if ["vcs", "vecs", "video", "decode", "encode", "jpeg", "vcn"]
        .iter()
        .any(|needle| engine.contains(needle))
    {
        Some(EngineFamily::Vpu)
    } else if [
        "rcs", "ccs", "bcs", "render", "compute", "graphics", "copy", "gfx", "sdma",
    ]
    .iter()
    .any(|needle| engine.contains(needle))
    {
        Some(EngineFamily::Gpu)
    } else {
        None
    }
}

fn parse_counter(raw: &str) -> Option<u64> {
    raw.split_whitespace().next()?.parse().ok()
}

fn parse_duration_ns(raw: &str) -> Option<u64> {
    let mut fields = raw.split_whitespace();
    let value = fields.next()?.parse::<u64>().ok()?;
    match fields.next().unwrap_or("ns") {
        "ns" => Some(value),
        "us" => value.checked_mul(1_000),
        "ms" => value.checked_mul(1_000_000),
        "s" => value.checked_mul(1_000_000_000),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    static NEXT_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    struct Fixture {
        root: PathBuf,
        roots: Roots,
    }

    struct NetworkFixture<'a> {
        name: &'a str,
        devtype: &'a str,
        state: &'a str,
        carrier: &'a str,
        link_type: &'a str,
        rx: u64,
        tx: u64,
        physical: bool,
    }

    impl Fixture {
        fn new() -> Self {
            let root = env::temp_dir().join(format!(
                "cbar-system-graph-{}-{}",
                std::process::id(),
                NEXT_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(root.join("proc")).expect("fixture proc root should be created");
            fs::create_dir_all(root.join("sys")).expect("fixture sys root should be created");
            fs::create_dir_all(root.join("dev")).expect("fixture dev root should be created");
            let roots = Roots {
                proc: root.join("proc"),
                sys: root.join("sys"),
                dev: root.join("dev"),
            };
            Self { root, roots }
        }

        fn write(&self, relative: &str, content: &str) {
            let path = self.root.join(relative);
            fs::create_dir_all(path.parent().expect("fixture path should have a parent"))
                .expect("fixture parent should be created");
            fs::write(path, content).expect("fixture file should be written");
        }

        fn network(&self, spec: NetworkFixture<'_>) {
            let NetworkFixture {
                name,
                devtype,
                state,
                carrier,
                link_type,
                rx,
                tx,
                physical,
            } = spec;
            let prefix = format!("sys/class/net/{name}");
            let uevent = if devtype.is_empty() {
                format!("INTERFACE={name}\n")
            } else {
                format!("DEVTYPE={devtype}\nINTERFACE={name}\n")
            };
            self.write(&format!("{prefix}/uevent"), &uevent);
            self.write(&format!("{prefix}/operstate"), state);
            self.write(&format!("{prefix}/carrier"), carrier);
            self.write(&format!("{prefix}/type"), link_type);
            self.write(&format!("{prefix}/statistics/rx_bytes"), &rx.to_string());
            self.write(&format!("{prefix}/statistics/tx_bytes"), &tx.to_string());
            if physical {
                fs::create_dir_all(self.root.join(format!("{prefix}/device")))
                    .expect("physical network fixture should be created");
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn basic_proc(fixture: &Fixture) {
        fixture.write("proc/stat", "cpu  100 0 100 800 0 0 0 0\n");
        fixture.write(
            "proc/meminfo",
            "MemTotal: 1000 kB\nMemFree: 100 kB\nMemAvailable: 400 kB\nSwapTotal: 500 kB\nSwapFree: 400 kB\n",
        );
        fixture.write("proc/diskstats", "");
    }

    #[test]
    fn absent_proc_sys_and_dev_roots_are_a_valid_topology() {
        let fixture = Fixture::new();
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let frame = sampler.sample(Duration::from_secs(1), MetricSet::all());
        assert_eq!(frame.available, MetricSet::empty());
        assert!(sampler.capabilities().is_empty());
    }

    #[test]
    fn malformed_direct_accelerator_counters_do_not_reserve_cells() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write(
            "sys/class/drm/card0/device/gpu_busy_percent",
            "not-a-number\n",
        );
        fixture.write(
            "sys/class/accel/accel0/device/npu_busy_time_us",
            "not-a-number\n",
        );
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let frame = sampler.sample(Duration::from_secs(1), MetricSet::all());
        assert!(!frame.available.contains(Metric::Gpu));
        assert!(!frame.available.contains(Metric::Npu));
        assert!(!sampler.capabilities().iter().any(|capability| {
            matches!(
                capability.provider,
                Provider::DrmBusySysfs | Provider::NpuBusySysfs
            )
        }));
    }

    #[test]
    fn ram_uses_mem_available_headroom_and_swap_is_independent() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let demand: MetricSet = [Metric::Ram, Metric::Swap].into_iter().collect();
        let frame = sampler.sample(Duration::from_secs(1), demand);
        assert_eq!(frame.scalar[&Metric::Ram].current(), Some(60.0));
        assert_eq!(frame.scalar[&Metric::Swap].current(), Some(20.0));
    }

    #[test]
    fn available_delta_scalars_publish_zero_on_their_baseline_frame() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write("proc/diskstats", "8 0 sda 0 0 0 0 0 0 0 0 0 100 0 0\n");
        fs::create_dir_all(fixture.root.join("sys/block/sda"))
            .expect("whole block fixture should be created");
        fixture.write("sys/class/accel/accel7/device/npu_busy_time_us", "100\n");

        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let demand: MetricSet = [Metric::Cpu, Metric::Io, Metric::Npu].into_iter().collect();
        let frame = sampler.sample(Duration::from_secs(1), demand);
        for metric in [Metric::Cpu, Metric::Io, Metric::Npu] {
            assert!(frame.available.contains(metric));
            assert_eq!(frame.scalar[&metric].values().collect::<Vec<_>>(), [0.0]);
        }
    }

    #[test]
    fn cpu_and_io_use_counter_deltas() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write("proc/stat", "cpu  100 0 100 800 0 0 0 0 90 0\n");
        fixture.write("proc/diskstats", "8 0 sda 0 0 0 0 0 0 0 0 0 100 0 0\n");
        fs::create_dir_all(fixture.root.join("sys/block/sda"))
            .expect("whole block fixture should be created without a device symlink");
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let demand: MetricSet = [Metric::Cpu, Metric::Io].into_iter().collect();
        sampler.sample(Duration::from_secs(1), demand);
        fixture.write(
            "proc/stat",
            "cpu  150 0 150 900 0 0 0 0 140 0\ncpu0 10 0 10 40\ncpu1 20 0 20 80\ncpu127 1 0 1 8\n",
        );
        fixture.write("proc/diskstats", "8 0 sda 0 0 0 0 0 0 0 0 0 350 0 0\n");
        let frame = sampler.sample(Duration::from_secs(2), demand);
        assert!(
            (frame.scalar[&Metric::Cpu]
                .current()
                .expect("CPU sample should exist")
                - 50.0)
                .abs()
                < 0.01
        );
        assert!(
            (frame.scalar[&Metric::Io]
                .current()
                .expect("IO sample should exist")
                - 25.0)
                .abs()
                < 0.01
        );
    }

    #[test]
    fn drm_media_and_render_engines_are_native_and_deduplicated() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write("dev/dri/renderD128", "");
        let first = "drm-driver:\txe\ndrm-client-id:\t7\ndrm-pdev:\t0000:00:02.0\ndrm-cycles-rcs:\t100\ndrm-total-cycles-rcs:\t1000\ndrm-cycles-vcs:\t200\ndrm-total-cycles-vcs:\t1000\n";
        fixture.write("proc/42/fdinfo/3", first);
        fixture.write("proc/42/fdinfo/4", first);
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let demand: MetricSet = [Metric::Gpu, Metric::Vpu].into_iter().collect();
        sampler.sample(Duration::from_secs(1), demand);

        let second = "drm-driver:\txe\ndrm-client-id:\t7\ndrm-pdev:\t0000:00:02.0\ndrm-cycles-rcs:\t300\ndrm-total-cycles-rcs:\t2000\ndrm-cycles-vcs:\t700\ndrm-total-cycles-vcs:\t2000\n";
        fixture.write("proc/42/fdinfo/3", second);
        fixture.write("proc/42/fdinfo/4", second);
        let frame = sampler.sample(Duration::from_secs(3), demand);
        assert_eq!(frame.scalar[&Metric::Gpu].current(), Some(20.0));
        assert_eq!(frame.scalar[&Metric::Vpu].current(), Some(50.0));
        assert_eq!(sampler.probe_counts().drm_sample_scans, 1);
    }

    #[test]
    fn generic_drm_engine_time_tolerates_unknown_drivers_and_fields() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write("dev/dri/renderD129", "");
        fixture.write(
            "proc/9/fdinfo/5",
            "drm-driver:\tfuturegpu\ndrm-client-id:\t12\ndrm-pdev:\t0000:99:00.0\ndrm-engine-render:\t100 ns\ndrm-engine-video-decode:\t200 ns\ndrm-engine-mystery:\t999 ns\n",
        );
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let demand: MetricSet = [Metric::Gpu, Metric::Vpu].into_iter().collect();
        sampler.sample(Duration::from_secs(1), demand);
        fixture.write(
            "proc/9/fdinfo/5",
            "drm-driver:\tfuturegpu\ndrm-client-id:\t12\ndrm-pdev:\t0000:99:00.0\ndrm-engine-render:\t400000100 ns\ndrm-engine-video-decode:\t1000000200 ns\ndrm-engine-mystery:\t1999999999 ns\n",
        );
        let frame = sampler.sample(Duration::from_secs(3), demand);
        assert!(
            (frame.scalar[&Metric::Gpu]
                .current()
                .expect("GPU sample should exist")
                - 20.0)
                .abs()
                < 0.01
        );
        assert!(
            (frame.scalar[&Metric::Vpu]
                .current()
                .expect("VPU sample should exist")
                - 50.0)
                .abs()
                < 0.01
        );
    }

    #[test]
    fn multiple_devices_are_capabilities_not_host_special_cases() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write(
            "proc/diskstats",
            "8 0 sda 0 0 0 0 0 0 0 0 0 100 0 0\n8 16 sdb 0 0 0 0 0 0 0 0 0 200 0 0\n",
        );
        fs::create_dir_all(fixture.root.join("sys/block/sda/device"))
            .expect("first block fixture should be created");
        fs::create_dir_all(fixture.root.join("sys/block/sdb/device"))
            .expect("second block fixture should be created");
        fixture.write("sys/class/drm/card0/device/gpu_busy_percent", "5\n");
        fixture.write("sys/class/drm/card1/device/gpu_busy_percent", "65\n");
        fixture.write("sys/class/accel/accel0/device/npu_busy_time_us", "100\n");
        fixture.write("sys/class/accel/accel1/device/npu_busy_time_us", "200\n");

        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let demand: MetricSet = [Metric::Io, Metric::Gpu, Metric::Npu].into_iter().collect();
        sampler.sample(Duration::from_secs(1), demand);
        fixture.write("sys/class/accel/accel0/device/npu_busy_time_us", "500100\n");
        fixture.write("sys/class/accel/accel1/device/npu_busy_time_us", "250200\n");
        let frame = sampler.sample(Duration::from_secs(2), demand);
        assert_eq!(frame.scalar[&Metric::Gpu].current(), Some(65.0));
        assert_eq!(frame.scalar[&Metric::Npu].current(), Some(50.0));

        let capabilities = sampler.capabilities();
        assert!(capabilities.contains(&Capability {
            metric: Metric::Io,
            provider: Provider::ProcDiskstats,
            instances: 2,
        }));
        assert!(capabilities.contains(&Capability {
            metric: Metric::Gpu,
            provider: Provider::DrmBusySysfs,
            instances: 2,
        }));
        assert!(capabilities.contains(&Capability {
            metric: Metric::Npu,
            provider: Provider::NpuBusySysfs,
            instances: 2,
        }));
    }

    #[test]
    fn hidden_expensive_sources_are_not_polled() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write("dev/dri/renderD128", "");
        fixture.write(
            "proc/42/fdinfo/3",
            "drm-driver: xe\ndrm-client-id: 1\ndrm-cycles-vcs: 0\ndrm-total-cycles-vcs: 1\n",
        );
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let demand: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        sampler.sample(Duration::from_secs(1), demand);
        sampler.sample(Duration::from_secs(2), demand);
        sampler.sample(Duration::from_secs(60), demand);
        assert_eq!(sampler.probe_counts().drm_discovery_scans, 0);
        assert_eq!(sampler.probe_counts().drm_sample_scans, 0);
        assert_eq!(sampler.probe_counts().drm_fd_link_reads, 0);
        assert_eq!(sampler.probe_counts().drm_fdinfo_reads, 0);
    }

    #[test]
    fn wide_cold_canvas_discovers_every_fitted_optional_group_without_blank_cells() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write(
            "proc/meminfo",
            "MemTotal: 1000 kB\nMemFree: 100 kB\nMemAvailable: 400 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        );
        fixture.write("proc/diskstats", "8 0 sda 0 0 0 0 0 0 0 0 0 100 0 0\n");
        fs::create_dir_all(fixture.root.join("sys/block/sda"))
            .expect("whole block fixture should be created");
        fixture.write("sys/class/drm/card0/device/gpu_busy_percent", "10\n");
        fixture.write("dev/dri/renderD128", "");
        fixture.write(
            "proc/81/fdinfo/5",
            "drm-driver: portable\ndrm-client-id: 3\ndrm-device: renderD128\ndrm-engine-video-decode: 100 ns\n",
        );
        fixture.write("sys/class/accel/accel0/device/npu_busy_time_us", "100\n");
        for spec in [
            NetworkFixture {
                name: "wired",
                devtype: "",
                state: "up",
                carrier: "1",
                link_type: "1",
                rx: 0,
                tx: 0,
                physical: true,
            },
            NetworkFixture {
                name: "radio",
                devtype: "wlan",
                state: "up",
                carrier: "1",
                link_type: "1",
                rx: 0,
                tx: 0,
                physical: false,
            },
            NetworkFixture {
                name: "mobile",
                devtype: "wwan",
                state: "up",
                carrier: "1",
                link_type: "65534",
                rx: 0,
                tx: 0,
                physical: false,
            },
            NetworkFixture {
                name: "mesh",
                devtype: "wireguard",
                state: "unknown",
                carrier: "0",
                link_type: "65534",
                rx: 0,
                tx: 0,
                physical: false,
            },
        ] {
            fixture.network(spec);
        }

        let core: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        let optional: MetricSet = [
            Metric::Io,
            Metric::Vpu,
            Metric::Gpu,
            Metric::Npu,
            Metric::Lan,
            Metric::Wlan,
            Metric::Wwan,
            Metric::Vpn,
        ]
        .into_iter()
        .collect();
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let cold = sampler.sample(Duration::from_secs(1), core);
        assert_eq!(cold.available, core);
        assert_eq!(cold.probeable, optional);

        let (visible, probe) = super::super::fitted_graph_demand(&cold, 242, 1920);
        assert_eq!(visible.demand, core);
        assert_eq!(visible.cells.len(), 2);
        assert_eq!(probe, optional);

        let discovered = sampler.sample_with_probe(Duration::from_millis(1_500), core, probe);
        for metric in optional.iter() {
            assert!(
                discovered.available.contains(metric),
                "{} should be discovered from wide cold capacity",
                metric.label()
            );
        }
        let counts = sampler.probe_counts();
        assert_eq!(counts.capability_passes, 1);
        assert_eq!(counts.disk_topology_scans, 1);
        assert_eq!(counts.drm_topology_scans, 1);
        assert_eq!(counts.npu_topology_scans, 1);
        assert_eq!(counts.network_topology_scans, 1);
    }

    #[test]
    fn fitted_probe_discovers_only_its_due_topology_group() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write("proc/diskstats", "8 0 sda 0 0 0 0 0 0 0 0 0 100 0 0\n");
        fs::create_dir_all(fixture.root.join("sys/block/sda"))
            .expect("whole block fixture should be created");
        fixture.write("sys/class/accel/accel0/device/npu_busy_time_us", "100\n");
        fixture.network(NetworkFixture {
            name: "wired",
            devtype: "",
            state: "up",
            carrier: "1",
            link_type: "1",
            rx: 0,
            tx: 0,
            physical: true,
        });
        fixture.write("dev/dri/renderD128", "");

        let core: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        let io: MetricSet = [Metric::Io].into_iter().collect();
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let initial = sampler.sample(Duration::from_secs(1), core);
        assert!(initial.probeable.contains(Metric::Io));
        assert!(initial.probeable.contains(Metric::Npu));
        assert_eq!(sampler.probe_counts().capability_passes, 0);

        let discovered = sampler.sample_with_probe(Duration::from_secs(2), core, io);
        assert!(discovered.available.contains(Metric::Io));
        let counts = sampler.probe_counts();
        assert_eq!(counts.capability_passes, 1);
        assert_eq!(counts.disk_topology_scans, 1);
        assert_eq!(counts.drm_topology_scans, 0);
        assert_eq!(counts.npu_topology_scans, 0);
        assert_eq!(counts.network_topology_scans, 0);

        sampler.sample_with_probe(Duration::from_secs(4), core, io);
        assert_eq!(sampler.probe_counts().disk_topology_scans, 1);
        sampler.sample_with_probe(Duration::from_secs(7), core, io);
        assert_eq!(sampler.probe_counts().disk_topology_scans, 2);
    }

    #[test]
    fn absent_hotplug_is_reprobed_on_the_discovery_clock() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        let core: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        let npu: MetricSet = [Metric::Npu].into_iter().collect();
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);

        let absent = sampler.sample_with_probe(Duration::from_secs(1), core, npu);
        assert!(!absent.available.contains(Metric::Npu));
        assert!(!absent.probeable.contains(Metric::Npu));
        fixture.write("sys/class/accel/accel0/device/npu_busy_time_us", "100\n");

        let not_due = sampler.sample(Duration::from_secs(5), core);
        assert!(!not_due.probeable.contains(Metric::Npu));
        let due = sampler.sample(Duration::from_secs(6), core);
        assert!(due.probeable.contains(Metric::Npu));
        let discovered = sampler.sample_with_probe(Duration::from_secs(6), core, npu);
        assert!(discovered.available.contains(Metric::Npu));
        assert_eq!(sampler.probe_counts().npu_topology_scans, 2);
    }

    #[test]
    #[ignore = "manual deterministic server-only cost evidence"]
    fn manual_core_demand_cost_evidence() {
        const SAMPLES: usize = 500;

        let fixture = Fixture::new();
        basic_proc(&fixture);
        let mut diskstats = String::new();
        for index in 0..64 {
            let name = format!("vd{index}");
            fs::create_dir_all(fixture.root.join("sys/block").join(&name))
                .expect("synthetic block device should be created");
            diskstats.push_str(&format!("8 {index} {name} 0 0 0 0 0 0 0 0 0 {index} 0 0\n"));
        }
        fixture.write("proc/diskstats", &diskstats);
        for index in 0..32 {
            fixture.write(
                &format!("sys/class/drm/card{index}/device/gpu_busy_percent"),
                "0\n",
            );
            fixture.write(
                &format!("sys/class/accel/accel{index}/device/npu_busy_time_us"),
                "0\n",
            );
        }
        for index in 0..64 {
            fixture.network(NetworkFixture {
                name: &format!("wired-{index}"),
                devtype: "",
                state: "up",
                carrier: "1",
                link_type: "1",
                rx: 0,
                tx: 0,
                physical: true,
            });
        }

        let demand: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let started = Instant::now();
        for tick in 1..=SAMPLES {
            std::hint::black_box(sampler.sample(
                Duration::from_millis(tick as u64 * SAMPLE_INTERVAL_MS),
                demand,
            ));
        }
        let elapsed = started.elapsed();
        let counts = sampler.probe_counts();
        eprintln!(
            "SYSTEM_GRAPH_CORE_SAMPLES={SAMPLES} ELAPSED_US={} HOT_SAMPLES={} CAPABILITY_PASSES={} DRM_TOPOLOGY_SCANS={} DISK_TOPOLOGY_SCANS={} NPU_TOPOLOGY_SCANS={} NETWORK_TOPOLOGY_SCANS={}",
            elapsed.as_micros(),
            counts.hot_samples,
            counts.capability_passes,
            counts.drm_topology_scans,
            counts.disk_topology_scans,
            counts.npu_topology_scans,
            counts.network_topology_scans,
        );
    }

    #[cfg(unix)]
    #[test]
    fn drm_open_event_detects_same_number_target_replacement_and_caches_client() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write("dev/dri/renderD128", "");
        fixture.write("dev/null", "");
        fs::create_dir_all(fixture.root.join("proc/42/fd"))
            .expect("client fd root should be created");
        std::os::unix::fs::symlink(
            fixture.root.join("dev/null"),
            fixture.root.join("proc/42/fd/0"),
        )
        .expect("non-DRM client fd should be linked");

        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let drm_open = sampler.synthetic_drm_open_events();
        let probe: MetricSet = [Metric::Vpu].into_iter().collect();
        let first = sampler.sample_with_probe(Duration::from_secs(1), MetricSet::empty(), probe);
        assert!(!first.available.contains(Metric::Vpu));
        assert_eq!(sampler.probe_counts().drm_fd_link_reads, 1);

        fixture.write(
            "proc/42/fdinfo/0",
            "drm-driver: portable\ndrm-client-id: 2\ndrm-device: renderD128\ndrm-engine-video-encode: 100 ns\n",
        );
        fs::remove_file(fixture.root.join("proc/42/fd/0"))
            .expect("non-DRM fd should be replaceable at the same number");
        std::os::unix::fs::symlink(
            fixture.root.join("dev/dri/renderD128"),
            fixture.root.join("proc/42/fd/0"),
        )
        .expect("same-number DRM client fd should be linked");
        drm_open.store(true, Ordering::Release);
        let discovered =
            sampler.sample_with_probe(Duration::from_millis(1_500), MetricSet::empty(), probe);
        assert!(discovered.available.contains(Metric::Vpu));
        assert_eq!(sampler.probe_counts().drm_fd_link_reads, 2);
        assert_eq!(sampler.probe_counts().drm_fdinfo_reads, 1);

        fixture.write(
            "proc/42/fdinfo/0",
            "drm-driver: portable\ndrm-client-id: 2\ndrm-device: renderD128\ndrm-engine-video-encode: 250000100 ns\n",
        );
        let demand: MetricSet = [Metric::Vpu].into_iter().collect();
        let sampled = sampler.sample(Duration::from_secs(2), demand);
        assert_eq!(sampled.scalar[&Metric::Vpu].current(), Some(50.0));
        assert_eq!(sampler.probe_counts().drm_fd_link_reads, 2);
        assert_eq!(sampler.probe_counts().drm_fdinfo_reads, 2);
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "manual server-only cost evidence"]
    fn manual_targeted_drm_probe_cost_evidence() {
        let topology = Fixture::new();
        topology.write("dev/dri/renderD999", "");
        let mut real_proc = Sampler::new(
            Roots {
                proc: PathBuf::from("/proc"),
                sys: topology.roots.sys.clone(),
                dev: topology.roots.dev.clone(),
            },
            4,
            30,
        );
        let started = Instant::now();
        let first = real_proc.read_drm_snapshot(1_000_000_000, true);
        let first_elapsed = started.elapsed();
        let first_counts = real_proc.probe_counts();
        let started = Instant::now();
        let second = real_proc.read_drm_snapshot(1_500_000_000, false);
        let second_elapsed = started.elapsed();
        let second_counts = real_proc.probe_counts();
        eprintln!(
            "TARGETED_DRM_REAL_PROC_FIRST_US={} CACHED_US={} FIRST_LINKS={} CACHED_LINKS={} FIRST_FDINFO={} CACHED_FDINFO={} ENGINES_FIRST={} ENGINES_CACHED={}",
            first_elapsed.as_micros(),
            second_elapsed.as_micros(),
            first_counts.drm_fd_link_reads,
            second_counts
                .drm_fd_link_reads
                .saturating_sub(first_counts.drm_fd_link_reads),
            first_counts.drm_fdinfo_reads,
            second_counts
                .drm_fdinfo_reads
                .saturating_sub(first_counts.drm_fdinfo_reads),
            first.counters.len(),
            second.counters.len(),
        );

        let synthetic = Fixture::new();
        synthetic.write("dev/dri/renderD999", "");
        let null_target = synthetic.root.join("dev/null");
        for pid in 100_000..101_000 {
            let fd_root = synthetic.root.join(format!("proc/{pid}/fd"));
            fs::create_dir_all(&fd_root).expect("synthetic fd root should be created");
            for fd in 0..4 {
                std::os::unix::fs::symlink(&null_target, fd_root.join(fd.to_string()))
                    .expect("synthetic non-DRM fd should be linked");
            }
        }
        synthetic.write(
            "proc/100999/fdinfo/9",
            "drm-driver: portable\ndrm-client-id: 8\ndrm-device: renderD999\ndrm-engine-video-encode: 100 ns\n",
        );
        std::os::unix::fs::symlink(
            synthetic.root.join("dev/dri/renderD999"),
            synthetic.root.join("proc/100999/fd/9"),
        )
        .expect("synthetic DRM fd should be linked");
        let mut high_pid = Sampler::new(synthetic.roots.clone(), 4, 30);
        let started = Instant::now();
        let first = high_pid.read_drm_snapshot(1_000_000_000, true);
        let first_elapsed = started.elapsed();
        let first_counts = high_pid.probe_counts();
        let started = Instant::now();
        let second = high_pid.read_drm_snapshot(1_500_000_000, false);
        let second_elapsed = started.elapsed();
        let second_counts = high_pid.probe_counts();
        eprintln!(
            "TARGETED_DRM_1000_PID_4001_FD_FIRST_US={} CACHED_US={} FIRST_LINKS={} CACHED_LINKS={} FIRST_FDINFO={} CACHED_FDINFO={}",
            first_elapsed.as_micros(),
            second_elapsed.as_micros(),
            first_counts.drm_fd_link_reads,
            second_counts
                .drm_fd_link_reads
                .saturating_sub(first_counts.drm_fd_link_reads),
            first_counts.drm_fdinfo_reads,
            second_counts
                .drm_fdinfo_reads
                .saturating_sub(first_counts.drm_fdinfo_reads),
        );
        assert!(first.vpu && second.vpu);
        assert_eq!(first_counts.drm_fd_link_reads, 4_001);
        assert_eq!(second_counts.drm_fd_link_reads, 4_001);
        assert_eq!(second_counts.drm_fdinfo_reads, 2);
    }

    #[test]
    fn interested_canvas_discovers_short_drm_media_work_and_expires_removed_topology() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write("dev/dri/renderD128", "");
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let demand: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        let probe: MetricSet = [Metric::Vpu].into_iter().collect();

        sampler.sample_with_probe(Duration::from_secs(1), demand, probe);
        fixture.write(
            "proc/81/fdinfo/5",
            "drm-driver: portable\ndrm-client-id: 3\ndrm-device: renderD128\ndrm-engine-video-decode: 100 ns\n",
        );
        let active = sampler.sample_with_probe(Duration::from_secs(3), demand, probe);
        assert!(active.available.contains(Metric::Vpu));
        assert_eq!(sampler.probe_counts().drm_discovery_scans, 2);

        fs::remove_file(fixture.root.join("dev/dri/renderD128"))
            .expect("render-node fixture should be removable");
        fs::remove_file(fixture.root.join("proc/81/fdinfo/5"))
            .expect("fdinfo fixture should be removable");
        let removed = sampler.sample_with_probe(Duration::from_secs(7), demand, probe);
        assert!(!removed.available.contains(Metric::Vpu));
    }

    #[test]
    fn learned_media_capability_persists_idle_and_expires_with_its_device() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write("dev/dri/renderD128", "");
        fixture.write("dev/dri/renderD129", "");
        fixture.write(
            "proc/81/fdinfo/5",
            "drm-driver: portable\ndrm-client-id: 3\ndrm-device: renderD128\ndrm-engine-video-decode: 100 ns\n",
        );
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let core: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        let probe: MetricSet = [Metric::Vpu].into_iter().collect();
        let active = sampler.sample_with_probe(Duration::from_secs(1), core, probe);
        assert!(active.available.contains(Metric::Vpu));

        fs::remove_file(fixture.root.join("proc/81/fdinfo/5"))
            .expect("VPU client fixture should be removable");
        let visible: MetricSet = [Metric::Cpu, Metric::Ram, Metric::Vpu]
            .into_iter()
            .collect();
        sampler.sample_with_probe(Duration::from_secs(3), visible, MetricSet::empty());
        let idle = sampler.sample_with_probe(Duration::from_secs(5), visible, MetricSet::empty());
        assert!(fixture.root.join("dev/dri/renderD129").is_file());
        assert!(idle.available.contains(Metric::Vpu));
        assert_eq!(idle.scalar[&Metric::Vpu].current(), Some(0.0));

        fs::remove_file(fixture.root.join("dev/dri/renderD128"))
            .expect("learned VPU render-node fixture should be removable");
        let hot_unplugged =
            sampler.sample_with_probe(Duration::from_secs(6), visible, MetricSet::empty());
        assert!(fixture.root.join("dev/dri/renderD129").is_file());
        assert!(!hot_unplugged.available.contains(Metric::Vpu));
    }

    #[cfg(unix)]
    #[test]
    fn short_media_workload_after_long_idle_is_not_missed() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.write("dev/dri/renderD128", "");
        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let demand: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        let probe: MetricSet = [Metric::Vpu].into_iter().collect();

        for second in [1, 3, 5, 7, 9] {
            sampler.sample_with_probe(Duration::from_secs(second), demand, probe);
        }
        assert_eq!(sampler.probe_counts().drm_discovery_scans, 5);

        fixture.write(
            "proc/91/fdinfo/5",
            "drm-driver: portable\ndrm-client-id: 4\ndrm-device: renderD128\ndrm-engine-video-encode: 100 ns\n",
        );
        fs::create_dir_all(fixture.root.join("proc/91/fd"))
            .expect("DRM client fd root should be created");
        std::os::unix::fs::symlink(
            fixture.root.join("dev/dri/renderD128"),
            fixture.root.join("proc/91/fd/5"),
        )
        .expect("DRM client fd target should be linked");

        // The old ten-second negative backoff would skip this 2-9 second
        // workload window. A fitting capability probe follows the 500ms graph
        // cadence and inspects the newly opened DRM descriptor immediately.
        let active = sampler.sample_with_probe(Duration::from_secs(11), demand, probe);
        assert!(active.available.contains(Metric::Vpu));
        assert_eq!(sampler.probe_counts().drm_discovery_scans, 6);
    }

    #[test]
    fn active_network_link_without_statistics_remains_manageable() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.network(NetworkFixture {
            name: "radio-no-counters",
            devtype: "wlan",
            state: "up",
            carrier: "1",
            link_type: "1",
            rx: 0,
            tx: 0,
            physical: false,
        });
        fs::remove_file(
            fixture
                .root
                .join("sys/class/net/radio-no-counters/statistics/rx_bytes"),
        )
        .expect("RX fixture should be removable");
        fs::remove_file(
            fixture
                .root
                .join("sys/class/net/radio-no-counters/statistics/tx_bytes"),
        )
        .expect("TX fixture should be removable");

        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let demand: MetricSet = [Metric::Wlan].into_iter().collect();
        let frame = sampler.sample(Duration::from_secs(1), demand);
        assert!(frame.available.contains(Metric::Wlan));
        let network = &frame.network[&Metric::Wlan].current;
        assert_eq!(network.name, "radio-no-counters");
        assert!(network.active);
        assert_eq!(network.rx.current(), Some(0.0));
        assert_eq!(network.tx.current(), Some(0.0));
    }

    #[test]
    fn demanded_network_link_state_refreshes_between_topology_passes() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.network(NetworkFixture {
            name: "radio-state",
            devtype: "wlan",
            state: "up",
            carrier: "1",
            link_type: "1",
            rx: 100,
            tx: 200,
            physical: false,
        });

        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let demand: MetricSet = [Metric::Wlan].into_iter().collect();
        let up = sampler.sample(Duration::from_secs(1), demand);
        assert!(up.available.contains(Metric::Wlan));
        assert!(up.network[&Metric::Wlan].current.active);
        assert_eq!(sampler.probe_counts().network_topology_scans, 1);

        fixture.write("sys/class/net/radio-state/operstate", "down\n");
        fixture.write("sys/class/net/radio-state/carrier", "0\n");
        let down = sampler.sample(Duration::from_millis(1_500), demand);
        assert!(!down.available.contains(Metric::Wlan));
        assert!(!down.network.contains_key(&Metric::Wlan));
        assert_eq!(sampler.probe_counts().network_topology_scans, 1);

        fixture.write("sys/class/net/radio-state/operstate", "up\n");
        fixture.write("sys/class/net/radio-state/carrier", "1\n");
        fixture.write("sys/class/net/radio-state/statistics/rx_bytes", "600");
        let core: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        let before_probe = sampler.sample(Duration::from_secs(2), core);
        assert!(!before_probe.available.contains(Metric::Wlan));
        let recovered = sampler.sample_with_probe(Duration::from_secs(6), core, demand);
        assert!(recovered.available.contains(Metric::Wlan));
        assert!(recovered.network[&Metric::Wlan].current.active);
        assert_eq!(sampler.probe_counts().network_topology_scans, 2);
    }

    #[test]
    fn discovered_but_hidden_network_skips_link_state_refreshes() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.network(NetworkFixture {
            name: "hidden-wired",
            devtype: "",
            state: "up",
            carrier: "1",
            link_type: "1",
            rx: 100,
            tx: 200,
            physical: true,
        });

        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let core: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        let lan: MetricSet = [Metric::Lan].into_iter().collect();
        let discovered = sampler.sample_with_probe(Duration::from_secs(1), core, lan);
        assert!(discovered.network[&Metric::Lan].current.active);

        fs::remove_file(fixture.root.join("sys/class/net/hidden-wired/operstate"))
            .expect("hidden link state fixture should be removable");
        fs::remove_file(fixture.root.join("sys/class/net/hidden-wired/carrier"))
            .expect("hidden carrier fixture should be removable");
        let hidden = sampler.sample(Duration::from_millis(1_500), core);
        assert!(
            hidden.network[&Metric::Lan].current.active,
            "hidden metadata should remain untouched until fitted demand returns"
        );
        assert_eq!(sampler.probe_counts().network_topology_scans, 1);

        let visible = sampler.sample(Duration::from_secs(2), lan);
        assert!(!visible.available.contains(Metric::Lan));
        assert!(!visible.network.contains_key(&Metric::Lan));
        assert_eq!(sampler.probe_counts().network_topology_scans, 1);
    }

    #[test]
    fn inactive_lan_does_not_reserve_a_graph_cell() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.network(NetworkFixture {
            name: "arbitrary-wired",
            devtype: "",
            state: "down",
            carrier: "0",
            link_type: "1",
            rx: 100,
            tx: 200,
            physical: true,
        });

        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let core: MetricSet = [Metric::Cpu, Metric::Ram].into_iter().collect();
        let lan_probe: MetricSet = [Metric::Lan].into_iter().collect();
        let frame = sampler.sample_with_probe(Duration::from_secs(1), core, lan_probe);
        assert!(!frame.available.contains(Metric::Lan));
        assert!(!frame.network.contains_key(&Metric::Lan));
        assert!(sampler.network_previous_rx.is_empty());
        assert!(sampler.network_previous_tx.is_empty());

        fixture.write("sys/class/net/arbitrary-wired/operstate", "up\n");
        fixture.write("sys/class/net/arbitrary-wired/carrier", "1\n");
        let cached = sampler.sample_with_probe(Duration::from_secs(2), core, lan_probe);
        assert!(!cached.available.contains(Metric::Lan));
        let active = sampler.sample_with_probe(Duration::from_secs(6), core, lan_probe);
        assert!(active.available.contains(Metric::Lan));
        let lan = &active.network[&Metric::Lan].current;
        assert_eq!(lan.name, "arbitrary-wired");
        assert!(lan.active);
        assert_eq!(lan.rx.current(), Some(0.0));
        assert_eq!(lan.tx.current(), Some(0.0));
    }

    #[test]
    fn network_topology_classification_does_not_depend_on_names_or_devtype() {
        let fixture = Fixture::new();
        for (name, link_type, physical) in [
            ("alpha", "1", true),
            ("beta", "1", false),
            ("gamma", "1", false),
            ("delta", "65534", false),
            ("epsilon", "1", false),
            ("zeta", "65534", false),
            ("theta", "1", false),
            ("kappa", "1", true),
            ("iota", "1", false),
            ("lambda", "1", false),
            ("mu", "1", false),
        ] {
            fixture.network(NetworkFixture {
                name,
                devtype: "",
                state: "down",
                carrier: "0",
                link_type,
                rx: 0,
                tx: 0,
                physical,
            });
        }
        fixture.network(NetworkFixture {
            name: "eta",
            devtype: "vlan",
            state: "down",
            carrier: "0",
            link_type: "1",
            rx: 0,
            tx: 0,
            physical: false,
        });
        fs::create_dir_all(fixture.root.join("sys/class/net/alpha/wireless"))
            .expect("WLAN topology directory should be created");
        fs::create_dir_all(fixture.root.join("sys/class/net/beta/bridge"))
            .expect("bridge topology directory should be created");
        fs::create_dir_all(fixture.root.join("sys/class/net/beta/brif/gamma"))
            .expect("bridge-to-bond topology should be created");
        fs::create_dir_all(fixture.root.join("sys/class/net/gamma/bonding"))
            .expect("bond topology directory should be created");
        fixture.write("sys/class/net/gamma/bonding/slaves", "kappa\n");
        fs::create_dir_all(fixture.root.join("sys/class/net/eta/lower_gamma"))
            .expect("VLAN-to-bond lower-link topology should be created");
        fs::create_dir_all(fixture.root.join("sys/class/net/lambda/bridge"))
            .expect("internal bridge topology directory should be created");
        fs::create_dir_all(fixture.root.join("sys/class/net/lambda/brif/iota"))
            .expect("internal virtual bridge slave should be created");
        fs::create_dir_all(fixture.root.join("sys/class/net/iota/bridge"))
            .expect("cyclic internal bridge topology directory should be created");
        fs::create_dir_all(fixture.root.join("sys/class/net/iota/brif/lambda"))
            .expect("virtual topology cycle should be created");
        fs::create_dir_all(fixture.root.join("sys/class/net/mu/bridge"))
            .expect("default-route bridge topology should be created");
        fixture.write("sys/class/net/delta/qmi/raw_ip", "Y\n");
        fixture.write("sys/class/net/epsilon/tun_flags", "1001\n");
        fixture.write(
            "proc/net/route",
            "Iface Destination Gateway Flags RefCnt Use Metric Mask MTU Window IRTT\ntheta 00000000 00000000 0003 0 0 100 00000000 0 0 0\nmu 00000000 00000000 0003 0 0 200 00000000 0 0 0\n",
        );

        let sampler = Sampler::new(fixture.roots.clone(), 4, 30);
        let classes: BTreeMap<_, _> = sampler
            .network_interfaces()
            .into_iter()
            .map(|interface| (interface.name, interface.metric))
            .collect();
        assert_eq!(classes.get("alpha"), Some(&Metric::Wlan));
        assert_eq!(classes.get("beta"), Some(&Metric::Lan));
        assert_eq!(classes.get("gamma"), Some(&Metric::Lan));
        assert_eq!(classes.get("delta"), Some(&Metric::Wwan));
        assert_eq!(classes.get("epsilon"), Some(&Metric::Vpn));
        assert_eq!(classes.get("zeta"), Some(&Metric::Vpn));
        assert_eq!(classes.get("theta"), Some(&Metric::Lan));
        assert_eq!(classes.get("eta"), Some(&Metric::Lan));
        assert_eq!(classes.get("kappa"), Some(&Metric::Lan));
        assert_eq!(classes.get("mu"), Some(&Metric::Lan));
        assert!(!classes.contains_key("iota"));
        assert!(!classes.contains_key("lambda"));
    }

    #[test]
    fn network_classes_rotate_without_combining_histories() {
        let fixture = Fixture::new();
        basic_proc(&fixture);
        fixture.network(NetworkFixture {
            name: "br0",
            devtype: "bridge",
            state: "up",
            carrier: "1",
            link_type: "1",
            rx: 1_000,
            tx: 2_000,
            physical: false,
        });
        fixture.network(NetworkFixture {
            name: "enp1s0",
            devtype: "",
            state: "up",
            carrier: "1",
            link_type: "1",
            rx: 5_000,
            tx: 7_000,
            physical: true,
        });
        fs::create_dir_all(fixture.root.join("sys/class/net/br0/brif/enp1s0"))
            .expect("primary bridge should expose its physical slave topology");
        fixture.network(NetworkFixture {
            name: "wlan-é",
            devtype: "wlan",
            state: "down",
            carrier: "0",
            link_type: "1",
            rx: 10_000,
            tx: 20_000,
            physical: false,
        });
        fixture.network(NetworkFixture {
            name: "wwan0",
            devtype: "wwan",
            state: "up",
            carrier: "1",
            link_type: "65534",
            rx: 30_000,
            tx: 40_000,
            physical: false,
        });
        fixture.network(NetworkFixture {
            name: "private-mesh",
            devtype: "",
            state: "unknown",
            carrier: "1",
            link_type: "65534",
            rx: 50_000,
            tx: 60_000,
            physical: false,
        });
        fixture.network(NetworkFixture {
            name: "cilium_host",
            devtype: "",
            state: "up",
            carrier: "1",
            link_type: "1",
            rx: 1,
            tx: 1,
            physical: false,
        });
        assert!(
            !fs::read_to_string(fixture.root.join("sys/class/net/private-mesh/uevent"))
                .expect("VPN uevent fixture should be readable")
                .contains("DEVTYPE="),
            "arbitrary ARPHRD_NONE VPN fixture must exercise missing DEVTYPE"
        );

        let mut sampler = Sampler::new(fixture.roots.clone(), 4, 1);
        let demand: MetricSet = Metric::NETWORK.into_iter().collect();
        let first = sampler.sample(Duration::from_secs(2), demand);
        assert_eq!(first.network[&Metric::Lan].current.name, "br0");
        assert!(!first.available.contains(Metric::Wlan));
        assert!(!first.network.contains_key(&Metric::Wlan));
        assert_eq!(first.network[&Metric::Wwan].current.name, "wwan0");
        assert_eq!(first.network[&Metric::Vpn].current.name, "private-mesh");

        fixture.write("sys/class/net/br0/statistics/rx_bytes", "3000");
        fixture.write("sys/class/net/enp1s0/statistics/rx_bytes", "9000");
        let second = sampler.sample(Duration::from_secs(3), demand);
        assert_eq!(second.network[&Metric::Lan].current.name, "enp1s0");
        assert_eq!(
            second.network[&Metric::Lan].current.rx.current(),
            Some(4000.0)
        );
        let previous = second.network[&Metric::Lan]
            .previous
            .as_ref()
            .expect("rotated LAN graph should retain its previous interface");
        assert_eq!(previous.name, "br0");
        assert_eq!(previous.rx.current(), Some(2000.0));
    }
}
