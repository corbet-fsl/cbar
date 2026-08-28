use super::model::{Cell, GraphFrame, History, Layout, Metric, NetworkHistory};
use gtk::cairo::{Context, Error, FontSlant, FontWeight};

const DIVIDER_HEIGHT: f64 = 22.0;
const DIVIDER_WIDTH: f64 = 1.0;
const MIN_HISTORY_WIDTH: f64 = 10.0;

pub fn draw(
    context: &Context,
    frame: &GraphFrame,
    layout: &Layout,
    height: i32,
    font_family: &str,
    font_size: f64,
) -> Result<(), Error> {
    if !font_family.is_empty() {
        context.select_font_face(font_family, FontSlant::Normal, FontWeight::Normal);
    }
    context.set_font_size(font_size);

    for (index, cell) in layout.cells.iter().enumerate() {
        if index > 0 {
            let previous = layout.cells[index - 1];
            draw_divider(context, previous, *cell, f64::from(height))?;
        }

        if cell.metric.is_network() {
            if let Some(view) = frame.network.get(&cell.metric) {
                draw_network(
                    context,
                    *cell,
                    f64::from(height),
                    &view.current,
                    view.previous.as_ref(),
                    view.previous_alpha,
                )?;
            }
        } else if let Some(history) = frame.scalar.get(&cell.metric) {
            draw_scalar(context, *cell, f64::from(height), history)?;
        }
    }
    Ok(())
}

fn draw_divider(
    context: &Context,
    previous: Cell,
    current: Cell,
    height: f64,
) -> Result<(), Error> {
    let x = divider_x(previous, current);
    context.set_source_rgba(1.0, 1.0, 1.0, 0.22);
    context.rectangle(
        x,
        (height - DIVIDER_HEIGHT) / 2.0,
        DIVIDER_WIDTH,
        DIVIDER_HEIGHT,
    );
    context.fill()
}

fn divider_x(previous: Cell, current: Cell) -> f64 {
    let gap_start = previous.x + previous.width;
    let gap = current.x - gap_start;
    if matches!(
        (previous.metric, current.metric),
        (Metric::Cpu, Metric::Ram)
    ) {
        // The deployed Lua pair places a one-pixel rule after its 99.5px CPU
        // series and 10px pad. Account for the rule width itself instead of
        // centring its left edge: global x = 11 + 99.5 + 10 = 120.5px.
        gap_start + (gap - DIVIDER_WIDTH) / 2.0
    } else {
        // Optional cells retain their recorded 11/11 midpoint contract.
        gap_start + gap / 2.0
    }
}

fn draw_scalar(context: &Context, cell: Cell, height: f64, history: &History) -> Result<(), Error> {
    let current = history.current().unwrap_or(0.0);
    let (warning, hot) = if cell.metric == Metric::Ram {
        (85.0, 95.0)
    } else {
        (50.0, 80.0)
    };
    set_usage_colour(context, current, warning, hot);

    let mid = height / 2.0;
    let header_width = text_left_center(context, cell.x, mid, cell.metric.label())? + 6.0;
    let tail = format!("{current:.0}%");
    let candidate_tail_width = context.text_extents(&tail)?.width() + 6.0;
    let tail_width = if cell.width - header_width - candidate_tail_width >= MIN_HISTORY_WIDTH {
        text_right_center(context, cell.x + cell.width, mid, &tail)? + 6.0
    } else {
        // Under pressure, history is the semantic content. The current value
        // remains encoded by the newest bar; discard the redundant tail before
        // allowing a lower-priority label to crowd out a higher-priority graph.
        0.0
    };
    let graph_x = cell.x + header_width;
    let graph_width = cell.width - header_width - tail_width;
    draw_scalar_history(context, graph_x, graph_width, mid, history)
}

fn draw_scalar_history(
    context: &Context,
    x: f64,
    width: f64,
    mid: f64,
    history: &History,
) -> Result<(), Error> {
    if width <= 4.0 || history.len() < 2 {
        return Ok(());
    }
    let cap_height = context.text_extents("0")?.height();
    let baseline = mid + cap_height / 2.0;
    let span = (baseline - 1.0).min(cap_height * 1.6);
    let step = width / (history.capacity() - 1) as f64;
    let bar_width = (step - 1.0).max(1.0);
    let start = history.capacity() - history.len();
    for (index, value) in history.values().enumerate() {
        let bar_height = (value.clamp(0.0, 100.0) * span / 100.0).ceil().max(1.0);
        context.rectangle(
            x + (start + index) as f64 * step,
            baseline - bar_height,
            bar_width,
            bar_height,
        );
    }
    context.fill()
}

fn draw_network(
    context: &Context,
    cell: Cell,
    height: f64,
    current: &NetworkHistory,
    previous: Option<&NetworkHistory>,
    previous_alpha: f64,
) -> Result<(), Error> {
    let mid = height / 2.0;
    context.set_source_rgb(1.0, 1.0, 1.0);
    let header_width = text_left_center(context, cell.x, mid, cell.metric.label())? + 6.0;
    let tail_width = if cell.width >= 100.0 {
        let rx = current.rx.current().unwrap_or(0.0);
        let tx = current.tx.current().unwrap_or(0.0);
        let incoming = rx >= tx;
        let tail = format!(
            "{}{}",
            format_rate(if incoming { rx } else { tx }),
            if incoming { "↓" } else { "↑" }
        );
        text_right_center(context, cell.x + cell.width, mid, &tail)? + 6.0
    } else {
        0.0
    };
    let graph_x = cell.x + header_width;
    let graph_width = cell.width - header_width - tail_width;
    if graph_width <= 4.0 {
        return Ok(());
    }

    if let Some(previous) = previous
        && previous_alpha > 0.0
    {
        draw_network_history(
            context,
            graph_x,
            graph_width,
            height,
            previous,
            previous_alpha,
        )?;
    }
    draw_network_history(
        context,
        graph_x,
        graph_width,
        height,
        current,
        1.0 - previous_alpha,
    )
}

fn draw_network_history(
    context: &Context,
    x: f64,
    width: f64,
    height: f64,
    history: &NetworkHistory,
    alpha: f64,
) -> Result<(), Error> {
    let count = history.rx.len().min(history.tx.len());
    if width <= 4.0 || count < 2 || alpha <= 0.0 {
        return Ok(());
    }
    let peak = history
        .rx
        .values()
        .chain(history.tx.values())
        .fold(1.0_f64, f64::max);
    let denominator = peak.ln_1p();
    let mid = height / 2.0;
    let cap_height = context.text_extents("0")?.height();
    let span = (mid - 1.0).min(cap_height * 0.78);
    let step = width / (history.rx.capacity() - 1) as f64;
    let bar_width = (step - 1.0).max(1.0);
    let start = history.rx.capacity() - count;

    context.set_source_rgba(1.0, 1.0, 1.0, alpha);
    for (index, value) in history
        .rx
        .values()
        .skip(history.rx.len() - count)
        .enumerate()
    {
        if value <= 0.0 {
            continue;
        }
        let bar_height = (value.ln_1p() / denominator * span).ceil().max(1.0);
        context.rectangle(
            x + (start + index) as f64 * step,
            mid - bar_height,
            bar_width,
            bar_height,
        );
    }
    context.fill()?;

    context.set_source_rgba(1.0, 1.0, 1.0, alpha * 0.45);
    for (index, value) in history
        .tx
        .values()
        .skip(history.tx.len() - count)
        .enumerate()
    {
        if value <= 0.0 {
            continue;
        }
        let bar_height = (value.ln_1p() / denominator * span).ceil().max(1.0);
        context.rectangle(
            x + (start + index) as f64 * step,
            mid,
            bar_width,
            bar_height,
        );
    }
    context.fill()
}

fn set_usage_colour(context: &Context, percent: f64, warning: f64, hot: f64) {
    if percent > hot {
        context.set_source_rgb(1.0, 0.0, 0.0);
    } else if percent > warning {
        context.set_source_rgb(1.0, 1.0, 0.0);
    } else {
        context.set_source_rgb(1.0, 1.0, 1.0);
    }
}

fn text_left_center(context: &Context, x: f64, y: f64, text: &str) -> Result<f64, Error> {
    let extents = context.text_extents(text)?;
    context.move_to(x, y + extents.height() / 2.0);
    context.show_text(text)?;
    Ok(extents.width())
}

fn text_right_center(context: &Context, x: f64, y: f64, text: &str) -> Result<f64, Error> {
    let extents = context.text_extents(text)?;
    context.move_to(x - extents.width(), y + extents.height() / 2.0);
    context.show_text(text)?;
    Ok(extents.width())
}

fn format_rate(bytes: f64) -> String {
    if bytes >= 1_000_000_000.0 {
        format!("{:.1}G", bytes / 1_000_000_000.0)
    } else if bytes >= 1_000_000.0 {
        format!("{:.1}M", bytes / 1_000_000.0)
    } else if bytes >= 1_000.0 {
        format!("{:.0}K", bytes / 1_000.0)
    } else {
        format!("{bytes:.0}B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modules::system_graph::model::{
        GraphFrame, HISTORY_LEN, Layout, Metric, MetricSet, NetworkHistory, NetworkView,
    };
    use crate::modules::system_graph::sampler::{Roots, Sampler};
    use gtk::cairo::{Format, ImageSurface, TextExtents, UserFontFace};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    static NEXT_RENDER_FIXTURE: AtomicUsize = AtomicUsize::new(0);

    struct RenderFixture(PathBuf);

    impl RenderFixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "ironbar-system-graph-render-{}-{}",
                std::process::id(),
                NEXT_RENDER_FIXTURE.fetch_add(1, Ordering::Relaxed)
            ));
            for child in ["proc", "sys", "dev"] {
                fs::create_dir_all(root.join(child))
                    .expect("render fixture roots should be created");
            }
            Self(root)
        }

        fn roots(&self) -> Roots {
            Roots {
                proc: self.0.join("proc"),
                sys: self.0.join("sys"),
                dev: self.0.join("dev"),
            }
        }

        fn write(&self, relative: &str, content: &str) {
            let path = self.0.join(relative);
            fs::create_dir_all(path.parent().expect("fixture file should have a parent"))
                .expect("render fixture parent should be created");
            fs::write(path, content).expect("render fixture file should be written");
        }

        fn remove(&self, relative: &str) {
            fs::remove_file(self.0.join(relative)).expect("render fixture file should be removed");
        }
    }

    impl Drop for RenderFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn compact_network_rate_units_match_the_existing_canvas() {
        assert_eq!(format_rate(999.0), "999B");
        assert_eq!(format_rate(12_000.0), "12K");
        assert_eq!(format_rate(12_000_000.0), "12.0M");
        assert_eq!(format_rate(1_500_000_000.0), "1.5G");
    }

    #[test]
    fn divider_origins_match_the_recorded_legacy_canvas() {
        let layout = Layout::fit(MetricSet::all(), 1196);
        assert_eq!(divider_x(layout.cells[0], layout.cells[1]), 120.5);
        assert_eq!(divider_x(layout.cells[1], layout.cells[2]), 242.0);

        let centered = Layout::fit(MetricSet::all(), 1396);
        assert_eq!(divider_x(centered.cells[0], centered.cells[1]), 220.5);
    }

    fn reference_frame() -> GraphFrame {
        let mut frame = GraphFrame {
            available: MetricSet::all(),
            ..GraphFrame::default()
        };
        for (offset, metric) in [
            Metric::Cpu,
            Metric::Ram,
            Metric::Swap,
            Metric::Io,
            Metric::Vpu,
            Metric::Gpu,
            Metric::Npu,
        ]
        .into_iter()
        .enumerate()
        {
            for value in [0.0, 18.0, 42.0, 67.0, 91.0] {
                frame.push_scalar(metric, (value + offset as f64).min(100.0), HISTORY_LEN);
            }
        }
        for (offset, metric) in Metric::NETWORK.into_iter().enumerate() {
            let mut current = NetworkHistory::new(format!("interface-{offset}"), HISTORY_LEN);
            for value in [0.0, 1_000.0, 80_000.0, 2_000_000.0, 0.0] {
                current.rx.push(value * (offset + 1) as f64);
                current.tx.push(value * 0.35 * (offset + 1) as f64);
            }
            frame.network.insert(
                metric,
                NetworkView {
                    current,
                    previous: None,
                    previous_alpha: 0.0,
                },
            );
        }
        frame
    }

    fn deterministic_font() -> UserFontFace {
        let face = UserFontFace::create().expect("test user font should be created");
        face.set_init_func(|_, _, extents| {
            extents.set_ascent(0.8);
            extents.set_descent(0.2);
            extents.set_height(1.0);
            extents.set_max_x_advance(0.6);
            extents.set_max_y_advance(0.0);
            Ok(())
        });
        face.set_render_glyph_func(|_, _, _, extents| {
            // Fixed metrics with no glyph ink make the reference independent
            // of fontconfig, installed fonts, hinting and rasterizer versions.
            *extents = TextExtents::new(0.0, -0.75, 0.55, 0.75, 0.6, 0.0);
            Ok(())
        });
        face.set_unicode_to_glyph_func(|_, unicode| Ok(unicode));
        face
    }

    fn render_hash(frame: &GraphFrame, width: i32, font: &UserFontFace) -> u64 {
        let mut surface = ImageSurface::create(Format::ARgb32, width, 26)
            .expect("reference image surface should be valid");
        let context = Context::new(&surface).expect("reference Cairo context should be valid");
        context.set_font_face(font);
        let layout = Layout::fit(Layout::requested(frame.available), width);
        draw(&context, frame, &layout, 26, "", 12.0).expect("reference render should succeed");
        drop(context);
        surface.flush();

        surface
            .data()
            .expect("reference pixels should be readable")
            .iter()
            .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
                (hash ^ u64::from(*byte)).wrapping_mul(0x0000_0100_0000_01b3)
            })
    }

    fn synthetic_media_workload() -> (GraphFrame, usize, usize) {
        let fixture = RenderFixture::new();
        fixture.write(
            "proc/meminfo",
            "MemTotal: 1000 kB\nMemFree: 100 kB\nMemAvailable: 400 kB\nSwapTotal: 0 kB\nSwapFree: 0 kB\n",
        );
        fixture.write("proc/diskstats", "");
        fixture.write("dev/dri/renderD314", "");
        fixture.write("sys/class/accel/accel7/device/npu_busy_time_us", "100");

        let demand: MetricSet = [Metric::Ram, Metric::Vpu, Metric::Gpu, Metric::Npu]
            .into_iter()
            .collect();
        let initial_demand: MetricSet = [Metric::Ram, Metric::Npu].into_iter().collect();
        let probe: MetricSet = [Metric::Vpu, Metric::Gpu].into_iter().collect();
        let mut sampler = Sampler::new(fixture.roots(), 4, 30);
        sampler.sample_with_probe(Duration::from_secs(1), initial_demand, probe);

        fixture.write("sys/class/accel/accel7/device/npu_busy_time_us", "250100");
        fixture.write(
            "proc/42/fdinfo/9",
            "drm-driver: portable\ndrm-client-id: 7\ndrm-device: renderD314\ndrm-cycles-render: 100\ndrm-total-cycles-render: 1000\ndrm-cycles-video-decode: 200\ndrm-total-cycles-video-decode: 1000\n",
        );
        sampler.sample_with_probe(Duration::from_millis(1_500), initial_demand, probe);

        fixture.write("sys/class/accel/accel7/device/npu_busy_time_us", "500100");
        fixture.write(
            "proc/42/fdinfo/9",
            "drm-driver: portable\ndrm-client-id: 7\ndrm-device: renderD314\ndrm-cycles-render: 300\ndrm-total-cycles-render: 2000\ndrm-cycles-video-decode: 700\ndrm-total-cycles-video-decode: 2000\n",
        );
        sampler.sample(Duration::from_secs(2), demand);

        // The DRM client exits after one 500ms graph frame. The next frame must
        // retain the measured spike in history even though current load is 0.
        fixture.remove("proc/42/fdinfo/9");
        let frame = sampler.sample(Duration::from_millis(2_500), demand);
        let counts = sampler.probe_counts();
        (frame, counts.drm_discovery_scans, counts.drm_sample_scans)
    }

    #[test]
    fn cairo_references_and_synthetic_media_workload_are_hermetic() {
        let font = deterministic_font();
        let reference = reference_frame();
        let reference_hashes = [
            render_hash(&reference, 1196, &font),
            render_hash(&reference, 1100, &font),
            render_hash(&reference, 850, &font),
        ];
        #[cfg(target_endian = "little")]
        assert_eq!(
            reference_hashes,
            [
                14_419_014_071_079_099_776,
                4_915_036_840_708_691_194,
                8_217_295_459_609_970_648,
            ]
        );
        #[cfg(not(target_endian = "little"))]
        assert!(
            reference_hashes[0] != reference_hashes[1]
                && reference_hashes[1] != reference_hashes[2]
        );

        let (workload, discovery_scans, sample_scans) = synthetic_media_workload();
        assert_eq!(workload.scalar[&Metric::Ram].current(), Some(60.0));
        assert_eq!(
            workload.scalar[&Metric::Gpu].values().collect::<Vec<_>>(),
            [0.0, 20.0, 0.0]
        );
        assert_eq!(
            workload.scalar[&Metric::Vpu].values().collect::<Vec<_>>(),
            [0.0, 50.0, 0.0]
        );
        assert_eq!(
            workload.scalar[&Metric::Npu].values().collect::<Vec<_>>(),
            [0.0, 50.0, 50.0, 0.0]
        );
        assert_eq!((discovery_scans, sample_scans), (2, 2));
        let workload_hash = render_hash(&workload, 440, &font);
        #[cfg(target_endian = "little")]
        assert_eq!(workload_hash, 2_327_421_445_038_321_209);
        assert_eq!(workload_hash, render_hash(&workload, 440, &font));
        let labels_only = GraphFrame {
            available: workload.available,
            ..GraphFrame::default()
        };
        assert_ne!(workload_hash, render_hash(&labels_only, 440, &font));
    }
}
