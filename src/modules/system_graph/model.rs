use std::collections::{BTreeMap, VecDeque};

pub const GRAPH_HEIGHT: i32 = 26;
pub const HISTORY_LEN: usize = 40;
pub const SAMPLE_INTERVAL_MS: u64 = 500;
pub const ROTATION_SECONDS: u64 = 30;

const OUTER_INSET: f64 = 11.0;
const PAIR_GAP: f64 = 21.0;
// Neighbouring legacy Cairo modules each contributed 11px of padding. Their
// 1px divider was painted over the shared edge and consumed no layout width.
const CELL_GAP: f64 = 22.0;
// The deployed Lua pair divides a 220px canvas around a 1px rule and two 10px
// internal pads: (220 - 1 - 20) / 2 = 99.5px per series.
const CPU_WIDTH: f64 = 99.5;
const RAM_WIDTH: f64 = 99.5;
const OPTIONAL_WIDTH: f64 = 84.0;
const CORE_MIN_WIDTH: f64 = 60.0;
const OPTIONAL_MIN_WIDTH: f64 = 56.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum Metric {
    Cpu = 0,
    Ram = 1,
    Swap = 2,
    Io = 3,
    Vpu = 4,
    Gpu = 5,
    Npu = 6,
    Lan = 7,
    Wlan = 8,
    Wwan = 9,
    Vpn = 10,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Provider {
    ProcStat,
    ProcMeminfo,
    ProcDiskstats,
    DrmBusySysfs,
    DrmFdinfo,
    NpuBusySysfs,
    NetworkSysfs,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capability {
    pub metric: Metric,
    pub provider: Provider,
    pub instances: usize,
}

impl Metric {
    pub const PRIORITY: [Self; 11] = [
        Self::Cpu,
        Self::Ram,
        Self::Swap,
        Self::Io,
        Self::Vpu,
        Self::Gpu,
        Self::Npu,
        Self::Lan,
        Self::Wlan,
        Self::Wwan,
        Self::Vpn,
    ];

    pub const NETWORK: [Self; 4] = [Self::Lan, Self::Wlan, Self::Wwan, Self::Vpn];

    pub const fn label(self) -> &'static str {
        match self {
            Self::Cpu => "CPU",
            Self::Ram => "RAM",
            Self::Swap => "SWAP",
            Self::Io => "IO",
            Self::Vpu => "VPU",
            Self::Gpu => "GPU",
            Self::Npu => "NPU",
            Self::Lan => "LAN",
            Self::Wlan => "WLAN",
            Self::Wwan => "WWAN",
            Self::Vpn => "VPN",
        }
    }

    pub const fn is_network(self) -> bool {
        matches!(self, Self::Lan | Self::Wlan | Self::Wwan | Self::Vpn)
    }

    const fn preferred_width(self) -> f64 {
        match self {
            Self::Cpu => CPU_WIDTH,
            Self::Ram => RAM_WIDTH,
            _ => OPTIONAL_WIDTH,
        }
    }

    const fn minimum_width(self) -> f64 {
        match self {
            Self::Cpu | Self::Ram => CORE_MIN_WIDTH,
            _ => OPTIONAL_MIN_WIDTH,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct MetricSet(u16);

impl MetricSet {
    pub const fn empty() -> Self {
        Self(0)
    }

    pub const fn all() -> Self {
        Self((1 << Metric::PRIORITY.len()) - 1)
    }

    pub const fn from_bits(bits: u16) -> Self {
        Self(bits & Self::all().0)
    }

    pub const fn bits(self) -> u16 {
        self.0
    }

    pub const fn contains(self, metric: Metric) -> bool {
        self.0 & (1 << metric as u8) != 0
    }

    pub fn insert(&mut self, metric: Metric) {
        self.0 |= 1 << metric as u8;
    }

    pub fn iter(self) -> impl Iterator<Item = Metric> {
        Metric::PRIORITY
            .into_iter()
            .filter(move |metric| self.contains(*metric))
    }
}

impl FromIterator<Metric> for MetricSet {
    fn from_iter<T: IntoIterator<Item = Metric>>(iter: T) -> Self {
        let mut set = Self::empty();
        for metric in iter {
            set.insert(metric);
        }
        set
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct History {
    capacity: usize,
    values: VecDeque<f64>,
}

impl History {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(2),
            values: VecDeque::with_capacity(capacity.max(2)),
        }
    }

    pub fn push(&mut self, value: f64) {
        if self.values.len() == self.capacity {
            self.values.pop_front();
        }
        self.values.push_back(value);
    }

    pub fn values(&self) -> impl ExactSizeIterator<Item = f64> + '_ {
        self.values.iter().copied()
    }

    pub fn current(&self) -> Option<f64> {
        self.values.back().copied()
    }

    pub fn len(&self) -> usize {
        self.values.len()
    }

    pub const fn capacity(&self) -> usize {
        self.capacity
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NetworkHistory {
    pub name: String,
    pub active: bool,
    pub rx: History,
    pub tx: History,
    pub index: usize,
    pub total: usize,
}

impl NetworkHistory {
    pub fn new(name: String, history_len: usize) -> Self {
        Self {
            name,
            active: true,
            rx: History::new(history_len),
            tx: History::new(history_len),
            index: 1,
            total: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NetworkView {
    pub current: NetworkHistory,
    pub previous: Option<NetworkHistory>,
    /// Previous-series opacity. It reaches zero after a few sampler frames.
    pub previous_alpha: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GraphFrame {
    pub available: MetricSet,
    /// Expensive providers which are topologically plausible but not yet
    /// proven. The canvas uses this only to request a gated capability probe.
    pub probeable: MetricSet,
    pub capabilities: Vec<Capability>,
    pub scalar: BTreeMap<Metric, History>,
    pub network: BTreeMap<Metric, NetworkView>,
}

impl Default for GraphFrame {
    fn default() -> Self {
        Self {
            available: MetricSet::empty(),
            probeable: MetricSet::empty(),
            capabilities: Vec::new(),
            scalar: BTreeMap::new(),
            network: BTreeMap::new(),
        }
    }
}

impl GraphFrame {
    pub fn push_scalar(&mut self, metric: Metric, value: f64, history_len: usize) {
        self.scalar
            .entry(metric)
            .or_insert_with(|| History::new(history_len))
            .push(value.clamp(0.0, 100.0));
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Cell {
    pub metric: Metric,
    pub x: f64,
    pub width: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Layout {
    pub cells: Vec<Cell>,
    pub demand: MetricSet,
    pub preferred_width: i32,
    pub compressed: bool,
}

impl Layout {
    /// The product policy is capability based, never host based. GTK's actual
    /// allocation determines compression and reverse-priority shedding; output
    /// model names and private host declarations are deliberately irrelevant.
    pub const fn requested(available: MetricSet) -> MetricSet {
        available
    }

    pub fn preferred_width(requested: MetricSet) -> i32 {
        Self::width_for(requested.iter(), Metric::preferred_width).ceil() as i32
    }

    /// Smallest GTK allocation that preserves the CPU/RAM core pair when both
    /// exist, or otherwise the single highest-priority available source. This
    /// deliberately stays below the natural width so GTK can compress and
    /// reverse-shed optional cells instead of forcing the bar off-screen.
    pub fn allocation_floor_width(requested: MetricSet) -> i32 {
        let retained: MetricSet =
            if requested.contains(Metric::Cpu) && requested.contains(Metric::Ram) {
                [Metric::Cpu, Metric::Ram].into_iter().collect()
            } else {
                requested.iter().take(1).collect()
            };
        if retained.bits() == 0 {
            1
        } else {
            Self::width_for(retained.iter(), Metric::minimum_width).ceil() as i32
        }
    }

    /// Fits a single canvas into its actual GTK allocation. History widths are
    /// reduced proportionally first. If even all minimum widths do not fit,
    /// cells are shed from VPN back toward CPU.
    pub fn fit(requested: MetricSet, allocated_width: i32) -> Self {
        let allocated = f64::from(allocated_width.max(1));
        let mut metrics: Vec<_> = requested.iter().collect();
        let preferred_width = Self::preferred_width(requested);

        while metrics.len() > 1
            && Self::width_for(metrics.iter().copied(), Metric::minimum_width) > allocated
        {
            metrics.pop();
        }

        if metrics.is_empty() {
            return Self {
                cells: Vec::new(),
                demand: MetricSet::empty(),
                preferred_width,
                compressed: false,
            };
        }

        let fixed = Self::fixed_width(&metrics);
        let available_for_cells = (allocated - fixed).max(metrics.len() as f64);
        let minimum_sum: f64 = metrics.iter().map(|metric| metric.minimum_width()).sum();
        let preferred_sum: f64 = metrics.iter().map(|metric| metric.preferred_width()).sum();

        let cell_budget = available_for_cells.min(preferred_sum);
        let ratio = if preferred_sum > minimum_sum {
            ((cell_budget - minimum_sum) / (preferred_sum - minimum_sum)).clamp(0.0, 1.0)
        } else {
            1.0
        };

        let provisional_widths: Vec<_> = metrics
            .iter()
            .map(|metric| {
                let minimum = metric.minimum_width();
                minimum + (metric.preferred_width() - minimum) * ratio
            })
            .collect();
        let strip_width = fixed + provisional_widths.iter().sum::<f64>();
        let centered_surplus = ((allocated - strip_width) / 2.0).max(0.0);
        let mut x = OUTER_INSET.min(allocated / 2.0) + centered_surplus;
        let mut cells = Vec::with_capacity(metrics.len());
        for (index, (metric, provisional_width)) in
            metrics.iter().copied().zip(provisional_widths).enumerate()
        {
            if index > 0 {
                x += Self::gap(metrics[index - 1], metric);
            }

            let width = provisional_width.max(1.0).min((allocated - x).max(1.0));
            cells.push(Cell { metric, x, width });
            x += width;
        }

        let demand = metrics.into_iter().collect();
        Self {
            cells,
            demand,
            preferred_width,
            compressed: allocated_width < preferred_width,
        }
    }

    fn width_for(metrics: impl IntoIterator<Item = Metric>, width: impl Fn(Metric) -> f64) -> f64 {
        let metrics: Vec<_> = metrics.into_iter().collect();
        Self::fixed_width(&metrics) + metrics.into_iter().map(width).sum::<f64>()
    }

    fn fixed_width(metrics: &[Metric]) -> f64 {
        let gaps: f64 = metrics
            .windows(2)
            .map(|pair| Self::gap(pair[0], pair[1]))
            .sum();
        OUTER_INSET * 2.0 + gaps
    }

    const fn gap(left: Metric, right: Metric) -> f64 {
        if matches!((left, right), (Metric::Cpu, Metric::Ram)) {
            PAIR_GAP
        } else {
            CELL_GAP
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_and_geometry_match_the_existing_graph_suite() {
        let all = MetricSet::all();
        assert_eq!(GRAPH_HEIGHT, 26);
        assert_eq!(Layout::preferred_width(all), 1196);
        assert_eq!(
            Layout::preferred_width([Metric::Cpu, Metric::Ram].into_iter().collect()),
            242
        );
        assert_eq!(Layout::allocation_floor_width(all), 163);
        assert_eq!(
            Layout::allocation_floor_width([Metric::Gpu].into_iter().collect()),
            78
        );
        assert_eq!(
            Layout::requested(all).iter().collect::<Vec<_>>(),
            Metric::PRIORITY
        );
    }

    #[test]
    fn layout_compresses_then_sheds_in_reverse_priority() {
        let requested = MetricSet::all();
        let ideal = Layout::fit(requested, Layout::preferred_width(requested));
        assert!(!ideal.compressed);
        assert_eq!(ideal.demand, requested);

        let compressed = Layout::fit(requested, 1100);
        assert!(compressed.compressed);
        assert_eq!(compressed.demand, requested);
        assert!(compressed.cells[0].width < CPU_WIDTH);

        let shed = Layout::fit(requested, 850);
        assert!(!shed.demand.contains(Metric::Vpn));
        assert!(shed.demand.contains(Metric::Wwan));
        assert_eq!(shed.demand.iter().last(), Some(Metric::Wwan));
    }

    #[test]
    fn unsupported_sources_never_consume_layout_or_demand() {
        let available: MetricSet = [Metric::Cpu, Metric::Ram, Metric::Gpu]
            .into_iter()
            .collect();
        let requested = Layout::requested(available);
        let layout = Layout::fit(requested, 1000);
        assert_eq!(
            layout.demand.iter().collect::<Vec<_>>(),
            vec![Metric::Cpu, Metric::Ram, Metric::Gpu,]
        );
        assert!(!layout.demand.contains(Metric::Vpu));
    }

    #[test]
    fn history_is_a_bounded_ring() {
        let mut history = History::new(3);
        for value in 0..5 {
            history.push(f64::from(value));
        }
        assert_eq!(history.values().collect::<Vec<_>>(), vec![2.0, 3.0, 4.0]);
    }

    fn geometry_snapshot(layout: &Layout) -> String {
        layout
            .cells
            .iter()
            .map(|cell| format!("{}@{:.2}+{:.2}", cell.metric.label(), cell.x, cell.width))
            .collect::<Vec<_>>()
            .join("|")
    }

    #[test]
    fn wide_geometry_matches_the_recorded_legacy_cell_contract() {
        let provenance = include_str!("../../../test-configs/system-graph-legacy-contract.md");
        assert!(
            provenance.contains("4910626be2eb4d27f13450c6bcba860da8f6d6a3c766d3ffe520ba22cd58f715")
        );
        assert!(provenance.contains("120.5 px"));
        let layout = Layout::fit(MetricSet::all(), 1196);
        assert_eq!(
            geometry_snapshot(&layout),
            "CPU@11.00+99.50|RAM@131.50+99.50|SWAP@253.00+84.00|IO@359.00+84.00|VPU@465.00+84.00|GPU@571.00+84.00|NPU@677.00+84.00|LAN@783.00+84.00|WLAN@889.00+84.00|WWAN@995.00+84.00|VPN@1101.00+84.00"
        );
    }

    #[test]
    fn surplus_allocation_centers_the_unchanged_graph_strip() {
        let layout = Layout::fit(MetricSet::all(), 1396);
        assert!(!layout.compressed);
        assert_eq!(
            geometry_snapshot(&layout),
            "CPU@111.00+99.50|RAM@231.50+99.50|SWAP@353.00+84.00|IO@459.00+84.00|VPU@565.00+84.00|GPU@671.00+84.00|NPU@777.00+84.00|LAN@883.00+84.00|WLAN@989.00+84.00|WWAN@1095.00+84.00|VPN@1201.00+84.00"
        );
        let left_margin = layout.cells[0].x;
        let right_margin = 1396.0
            - layout
                .cells
                .last()
                .map(|cell| cell.x + cell.width)
                .expect("wide layout should contain cells");
        assert!((left_margin - right_margin).abs() < f64::EPSILON);
    }

    #[test]
    fn narrow_geometry_records_compression_and_reverse_shedding_only() {
        let compressed = Layout::fit(MetricSet::all(), 1100);
        assert_eq!(
            geometry_snapshot(&compressed),
            "CPU@11.00+88.04|RAM@120.04+88.04|SWAP@230.09+75.88|IO@327.97+75.88|VPU@425.85+75.88|GPU@523.73+75.88|NPU@621.60+75.88|LAN@719.48+75.88|WLAN@817.36+75.88|WWAN@915.24+75.88|VPN@1013.12+75.88"
        );

        let shed = Layout::fit(MetricSet::all(), 850);
        assert_eq!(shed.demand.iter().last(), Some(Metric::Wwan));
        assert_eq!(
            geometry_snapshot(&shed),
            "CPU@11.00+68.21|RAM@100.21+68.21|SWAP@190.43+61.82|IO@274.25+61.82|VPU@358.07+61.82|GPU@441.89+61.82|NPU@525.71+61.82|LAN@609.53+61.82|WLAN@693.36+61.82|WWAN@777.18+61.82"
        );
    }
}
