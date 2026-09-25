//! The guest performance panel.

use egui::{Align2, Color32, CornerRadius, Pos2, Rect, Response, RichText, Stroke, Ui};

use crate::plot::{self, TimeAxis};
use crate::{CpuClass, FrameSample, FrameTarget, GuestStats, CPU_CLASSES, VOICES};

/// Something the host should do on the panel's behalf.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PanelAction {
    /// Save this CSV (the newest `seconds` of history) somewhere useful.
    ExportCsv {
        /// File contents.
        csv: String,
        /// Seconds of history it covers.
        seconds: u32,
    },
}

const WINDOWS_S: [u64; 5] = [5, 10, 30, 60, 120];
const EXPORT_S: [u32; 4] = [10, 30, 60, 120];
const INTERVAL_BUCKETS: usize = 64;

/// Draw the panel. `vram` is the frontend's VRAM texture (1024x512), used
/// as the backdrop of the texture-page heat map when available.
pub fn draw(
    ui: &mut Ui,
    stats: &mut GuestStats,
    vram: Option<egui::TextureId>,
) -> Option<PanelAction> {
    let mut action = None;
    toolbar(ui, stats, &mut action);
    let (Some(newest), Some(_)) = (stats.newest_vblank(), stats.oldest_vblank()) else {
        ui.add_space(6.0);
        ui.label(
            RichText::new(
                "No guest telemetry yet. Run a game with this panel open: every emulated vblank \
                 is sampled from the PS1 hardware counters.",
            )
            .color(plot::TEXT_DIM),
        );
        return action;
    };
    let right = stats.view.paused_at.unwrap_or(newest).min(newest);
    let window = stats.view.window.max(30);
    let first = (right + 1).saturating_sub(window);
    let mut ctx = Ctx {
        first,
        last: first + window - 1,
        now: newest,
        refresh: stats.refresh_hz(),
        hovered: false,
    };
    let summary = Summary::compute(stats, &ctx);

    kpi_tiles(ui, stats, &ctx, &summary);
    ui.add_space(4.0);

    section(ui, "Frame rate", true, |ui| {
        fps_chart(ui, stats, &mut ctx, &summary);
        ui.add_space(14.0);
        frame_time_histogram(ui, &ctx, &summary);
    });
    section(ui, "Cycle budget per vblank", true, |ui| {
        budget_meters(ui, stats, &ctx);
    });
    section(ui, "Where the CPU time goes", true, |ui| {
        cpu_chart(ui, stats, &mut ctx);
    });
    section(ui, "What limits each frame", true, |ui| {
        limiter_chart(ui, stats, &mut ctx, &summary);
    });
    section(ui, "Hardware activity", true, |ui| {
        sparklines(ui, stats, &mut ctx);
    });
    section(ui, "SPU voices", true, |ui| {
        voice_meters(ui, stats);
    });
    section(ui, "Texture pages in use", true, |ui| {
        texpage_heat(ui, stats, vram);
    });

    if !ctx.hovered {
        stats.view.hover = None;
    }
    action
}

/// Per-draw context shared by the charts.
struct Ctx {
    first: u64,
    last: u64,
    now: u64,
    refresh: f64,
    hovered: bool,
}

impl Ctx {
    fn axis(&self, rect: Rect) -> TimeAxis {
        TimeAxis {
            rect,
            first: self.first,
            last: self.last,
        }
    }

    fn vblank_ms(&self) -> f64 {
        1000.0 / self.refresh
    }
}

/// Window aggregates.
struct Summary {
    intervals: [u32; INTERVAL_BUCKETS],
    frames: u32,
    interval_sum: u64,
    target: u16,
    late: u32,
    late_vblanks: u64,
    cycles: u64,
    cpu_work: u64,
    cpu_cycles: u64,
    gpu: u64,
}

impl Summary {
    fn compute(stats: &GuestStats, ctx: &Ctx) -> Self {
        let mut s = Summary {
            intervals: [0; INTERVAL_BUCKETS],
            frames: 0,
            interval_sum: 0,
            target: 1,
            late: 0,
            late_vblanks: 0,
            cycles: 0,
            cpu_work: 0,
            cpu_cycles: 0,
            gpu: 0,
        };
        for sample in stats.range(ctx.first, ctx.last) {
            s.cycles += u64::from(sample.cycles);
            s.gpu += u64::from(sample.gpu_busy);
            if sample.cpu_profiled {
                s.cpu_work += u64::from(sample.cpu_work());
                s.cpu_cycles += u64::from(sample.cycles);
            }
            if sample.presented && sample.present_interval > 0 {
                let bucket = usize::from(sample.present_interval).min(INTERVAL_BUCKETS - 1);
                s.intervals[bucket] += 1;
                s.frames += 1;
                s.interval_sum += u64::from(sample.present_interval);
            }
        }
        s.target = match stats.view.target {
            FrameTarget::Vblanks(n) => n.max(1),
            FrameTarget::Auto => {
                let mut best = 1;
                for (interval, &n) in s.intervals.iter().enumerate().skip(1) {
                    if n > s.intervals[best] {
                        best = interval;
                    }
                }
                best as u16
            }
        };
        for (interval, &n) in s.intervals.iter().enumerate() {
            if interval > usize::from(s.target) {
                s.late += n;
                s.late_vblanks += u64::from(n) * (interval as u64 - u64::from(s.target));
            }
        }
        s
    }

    fn fps(&self, refresh: f64) -> Option<f64> {
        (self.interval_sum > 0).then(|| f64::from(self.frames) * refresh / self.interval_sum as f64)
    }

    /// Frame interval (vblanks) at percentile `p`.
    fn percentile(&self, p: f64) -> Option<usize> {
        if self.frames == 0 {
            return None;
        }
        let rank = (f64::from(self.frames) * p).ceil().max(1.0) as u32;
        let mut seen = 0;
        for (interval, &n) in self.intervals.iter().enumerate() {
            seen += n;
            if seen >= rank {
                return Some(interval);
            }
        }
        None
    }
}

fn section(ui: &mut Ui, title: &str, open: bool, add: impl FnOnce(&mut Ui)) {
    egui::CollapsingHeader::new(RichText::new(title).color(plot::TEXT).strong())
        .id_salt(("guest-perf", title))
        .default_open(open)
        .show(ui, |ui| {
            ui.add_space(2.0);
            add(ui);
            ui.add_space(4.0);
        });
}

fn toolbar(ui: &mut Ui, stats: &mut GuestStats, action: &mut Option<PanelAction>) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let live = stats.view.paused_at.is_none();
        let label = if live { "Pause" } else { "Go live" };
        if ui
            .button(label)
            .on_hover_text("Freeze the charts (emulation keeps running). Drag a chart to scrub.")
            .clicked()
        {
            stats.view.paused_at = if live { stats.newest_vblank() } else { None };
        }
        let refresh_hz = stats.refresh_hz();
        let refresh = refresh_hz.round().max(1.0) as u64;
        for secs in WINDOWS_S {
            let span = secs * refresh;
            let selected = stats.view.window == span;
            if ui
                .selectable_label(selected, format!("{secs}s"))
                .on_hover_text("Visible time window")
                .clicked()
            {
                stats.view.window = span;
            }
        }
        egui::ComboBox::from_id_salt("guest-perf-target")
            .width(86.0)
            .selected_text(match stats.view.target {
                FrameTarget::Auto => "Target: auto".to_string(),
                FrameTarget::Vblanks(n) => format!("Target: {}", fps_label(stats.refresh_hz(), n)),
            })
            .show_ui(ui, |ui| {
                ui.selectable_value(
                    &mut stats.view.target,
                    FrameTarget::Auto,
                    "Auto (most common)",
                );
                for n in 1..=4u16 {
                    ui.selectable_value(
                        &mut stats.view.target,
                        FrameTarget::Vblanks(n),
                        format!(
                            "{} ({n} vblank{})",
                            fps_label(refresh_hz, n),
                            if n == 1 { "" } else { "s" }
                        ),
                    );
                }
            });
        ui.menu_button("Export CSV", |ui| {
            ui.label(RichText::new("One row per vblank, every counter.").color(plot::TEXT_DIM));
            for secs in EXPORT_S {
                if ui.button(format!("Last {secs} s")).clicked() {
                    stats.view.export_secs = secs;
                    *action = Some(PanelAction::ExportCsv {
                        csv: stats.csv(secs),
                        seconds: secs,
                    });
                    ui.close_menu();
                }
            }
        });
    });
    ui.label(
        RichText::new("Emulated PS1 only. Hover for values, drag to scrub, scroll to zoom, double-click for live.")
            .color(plot::TEXT_MUTED)
            .size(10.5),
    );
}

/// One row of legend keys above a chart: a line swatch, or a dot when
/// `dot` is set.
fn legend_row(ui: &mut Ui, items: &[(Color32, &str, bool)]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        for &(color, text, dot) in items {
            let (r, _) = ui.allocate_exact_size(egui::vec2(12.0, 10.0), egui::Sense::hover());
            if dot {
                ui.painter().circle_filled(r.center(), 3.5, color);
            } else {
                ui.painter().line_segment(
                    [
                        Pos2::new(r.left(), r.center().y),
                        Pos2::new(r.right(), r.center().y),
                    ],
                    Stroke::new(2.0, color),
                );
            }
            ui.label(RichText::new(text).color(plot::TEXT_DIM).size(10.5));
            ui.add_space(6.0);
        }
    });
}

fn fps_label(refresh: f64, interval: u16) -> String {
    format!("{:.0} fps", refresh / f64::from(interval.max(1)))
}

/// Shared chart interaction: hover sets the cursor, drag scrubs (and
/// pauses), scroll zooms, double-click returns to live.
fn interact(ui: &Ui, stats: &mut GuestStats, ctx: &mut Ctx, axis: &TimeAxis, response: &Response) {
    if let Some(pos) = response.hover_pos() {
        stats.view.hover = Some(axis.vblank_at(pos.x));
        ctx.hovered = true;
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll.abs() > 0.5 {
            let factor = if scroll > 0.0 { 0.85 } else { 1.0 / 0.85 };
            let max = crate::HISTORY_VBLANKS as u64;
            stats.view.window = ((stats.view.window as f32 * factor) as u64).clamp(60, max);
        }
    }
    if response.dragged() {
        let dx = response.drag_delta().x;
        let per_px = axis.span() as f32 / axis.rect.width().max(1.0);
        let newest = stats.newest_vblank().unwrap_or(0);
        let right = stats.view.paused_at.unwrap_or(newest) as f64 - f64::from(dx * per_px);
        let oldest = stats.oldest_vblank().unwrap_or(0) + stats.view.window.min(newest);
        stats.view.paused_at = Some((right.max(0.0) as u64).clamp(oldest.min(newest), newest));
    }
    if response.double_clicked() {
        stats.view.paused_at = None;
    }
}

fn focus_label(ctx: &Ctx, vblank: u64) -> String {
    let dt = (vblank as f64 - ctx.now as f64) / ctx.refresh;
    if dt.abs() < 0.5 / ctx.refresh {
        "now".to_string()
    } else {
        format!("{dt:.2} s")
    }
}

// ---------------------------------------------------------------- KPIs

fn kpi_tiles(ui: &mut Ui, stats: &GuestStats, ctx: &Ctx, s: &Summary) {
    let fps = s.fps(ctx.refresh);
    let p95 = s.percentile(0.95);
    let cpu = (s.cpu_cycles > 0).then(|| s.cpu_work as f64 / s.cpu_cycles as f64 * 100.0);
    let gpu = (s.cycles > 0).then(|| s.gpu as f64 / s.cycles as f64 * 100.0);
    let window_s = (ctx.last - ctx.first + 1) as f64 / ctx.refresh;
    let tiles: [(&str, String, String, Color32); 5] = [
        (
            "Game fps",
            fps.map_or("--".into(), |f| format!("{f:.1}")),
            format!("{} frames / {window_s:.0} s", s.frames),
            plot::TEXT,
        ),
        (
            "p95 frame",
            p95.map_or("--".into(), |i| {
                format!("{:.1} ms", i as f64 * ctx.vblank_ms())
            }),
            p95.map_or(String::new(), |i| {
                format!("{i} vblank{}", if i == 1 { "" } else { "s" })
            }),
            plot::TEXT,
        ),
        (
            "Late frames",
            format!("{}", s.late),
            format!(
                "{} missed vblanks, target {}",
                s.late_vblanks,
                fps_label(ctx.refresh, s.target)
            ),
            if s.late > 0 {
                plot::CRITICAL
            } else {
                plot::TEXT
            },
        ),
        (
            "CPU busy",
            cpu.map_or("--".into(), |c| format!("{c:.0}%")),
            "work, excl. wait loops".into(),
            plot::TEXT,
        ),
        (
            "GPU busy",
            gpu.map_or("--".into(), |g| format!("{g:.0}%")),
            "draw-time model".into(),
            plot::TEXT,
        ),
    ];
    let _ = stats;
    let spacing = 4.0;
    let avail = ui.available_width();
    let columns = if avail > 520.0 { 5 } else { 3 };
    let width = ((avail - spacing * (columns as f32 - 1.0)) / columns as f32).floor();
    for row in tiles.chunks(columns) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = spacing;
            for (title, value, note, color) in row {
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(width, 46.0), egui::Sense::hover());
                let painter = ui.painter();
                painter.rect_filled(rect, 3.0, plot::SURFACE);
                painter.text(
                    rect.left_top() + egui::vec2(6.0, 4.0),
                    Align2::LEFT_TOP,
                    *title,
                    plot::small(),
                    plot::TEXT_DIM,
                );
                painter.text(
                    rect.left_top() + egui::vec2(6.0, 16.0),
                    Align2::LEFT_TOP,
                    value,
                    plot::mono(16.0),
                    *color,
                );
                response.on_hover_text(note.as_str());
            }
        });
    }
}

// ---------------------------------------------------------------- frame rate

fn fps_chart(ui: &mut Ui, stats: &mut GuestStats, ctx: &mut Ctx, s: &Summary) {
    legend_row(
        ui,
        &[
            (plot::BLUE, "frame rate of each presented frame", false),
            (
                plot::shade(plot::NEUTRAL, 0.35),
                "frames in the last second",
                false,
            ),
            (plot::CRITICAL, "late frame", true),
        ],
    );
    let (rect, response) = plot::chart_area(ui, 132.0);
    let plot_rect = rect.shrink2(egui::vec2(4.0, 8.0));
    let axis = ctx.axis(plot_rect);
    interact(ui, stats, ctx, &axis, &response);
    let refresh = ctx.refresh;
    let y_max = refresh * 1.15;
    let y = |fps: f64| plot_rect.bottom() - (fps / y_max) as f32 * plot_rect.height();
    for (fps, label) in [(refresh, 1u16), (refresh / 2.0, 2), (refresh / 3.0, 3)] {
        plot::hline(
            ui,
            plot_rect,
            y(fps),
            &fps_label(refresh, label),
            true,
            plot::GRID,
        );
    }
    plot::time_ticks(ui, &axis, ctx.now, refresh);

    let painter = ui.painter_at(rect);
    let mut steps: Vec<Pos2> = Vec::new();
    let mut average: Vec<Pos2> = Vec::new();
    let mut late_marks: Vec<(f32, f32, u16)> = Vec::new();
    let mut recent: std::collections::VecDeque<u64> = std::collections::VecDeque::new();
    let dots = axis.span() <= (refresh * 12.0) as u64;
    let lookback = axis.first.saturating_sub(refresh as u64 + 1);
    for sample in stats.range(lookback, axis.last) {
        if !sample.presented {
            continue;
        }
        recent.push_back(sample.vblank);
        while recent
            .front()
            .is_some_and(|&v| v + (refresh as u64) <= sample.vblank)
        {
            recent.pop_front();
        }
        if sample.vblank < axis.first || sample.present_interval == 0 {
            continue;
        }
        let interval = sample.present_interval;
        let fps = refresh / f64::from(interval);
        let x0 = axis
            .x((sample.vblank - u64::from(interval)) as f64 + 0.5)
            .max(plot_rect.left());
        let x1 = axis.x(sample.vblank as f64 + 0.5);
        let yy = y(fps);
        if let Some(last) = steps.last().copied() {
            steps.push(Pos2::new(x0, last.y));
        } else {
            steps.push(Pos2::new(x0, yy));
        }
        steps.push(Pos2::new(x0, yy));
        steps.push(Pos2::new(x1, yy));
        let covered = recent.len() as f64;
        average.push(Pos2::new(x1, y(covered.min(y_max))));
        if interval > s.target {
            late_marks.push((x1, yy, interval));
        }
    }
    if steps.is_empty() {
        painter.text(
            plot_rect.center(),
            Align2::CENTER_CENTER,
            "No display flips in this window (static screen, load, or single-buffered)",
            plot::small(),
            plot::TEXT_DIM,
        );
    }
    if average.len() > 1 {
        painter.line(average, Stroke::new(1.5, plot::shade(plot::NEUTRAL, 0.35)));
    }
    if steps.len() > 1 {
        painter.line(steps.clone(), Stroke::new(2.0, plot::BLUE));
    }
    if dots {
        for p in steps.iter().skip(2).step_by(3) {
            painter.circle_filled(*p, 2.0, plot::BLUE);
        }
    }
    for (x, yy, _) in &late_marks {
        painter.circle(
            Pos2::new(*x, *yy),
            3.5,
            plot::CRITICAL,
            Stroke::new(1.5, plot::SURFACE),
        );
        painter.line_segment(
            [
                Pos2::new(*x, rect.bottom() - 5.0),
                Pos2::new(*x, rect.bottom()),
            ],
            Stroke::new(2.0, plot::CRITICAL),
        );
    }

    if let Some(hover) = stats.view.hover {
        plot::cursor(ui, &axis, hover);
        if response.hovered() {
            let frame = stats
                .range(hover, axis.last)
                .find(|sample| sample.presented && sample.present_interval > 0)
                .copied();
            let ms = ctx.vblank_ms();
            let when = focus_label(ctx, hover);
            let target = s.target;
            response.on_hover_ui_at_pointer(|ui| match frame {
                Some(f) => {
                    let interval = f.present_interval;
                    ui.label(
                        RichText::new(format!("{:.1} fps", refresh / f64::from(interval))).strong(),
                    );
                    ui.label(format!(
                        "{interval} vblank{} = {:.1} ms (presented {})",
                        if interval == 1 { "" } else { "s" },
                        f64::from(interval) * ms,
                        focus_label(ctx, f.vblank),
                    ));
                    if interval > target {
                        ui.label(
                            RichText::new(format!(
                                "Late: {} vblank{} over the {} target",
                                interval - target,
                                if interval - target == 1 { "" } else { "s" },
                                fps_label(refresh, target)
                            ))
                            .color(plot::CRITICAL),
                        );
                    }
                }
                None => {
                    ui.label(format!("{when}: no frame presented after this point yet"));
                }
            });
        }
    }
}

fn frame_time_histogram(ui: &mut Ui, ctx: &Ctx, s: &Summary) {
    ui.label(
        RichText::new("Frame-time distribution")
            .color(plot::TEXT_DIM)
            .size(11.0),
    );
    let rows = 6usize;
    let mut counts = [0u32; 6];
    for (interval, &n) in s.intervals.iter().enumerate().skip(1) {
        counts[(interval - 1).min(rows - 1)] += n;
    }
    let max = counts.iter().copied().max().unwrap_or(0).max(1);
    let width = ui.available_width();
    let label_w = 132.0;
    for (row, &n) in counts.iter().enumerate() {
        let interval = row + 1;
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, 15.0), egui::Sense::hover());
        let painter = ui.painter();
        let plus = if row == rows - 1 { "+" } else { "" };
        painter.text(
            Pos2::new(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            format!(
                "{interval}{plus} vb  {:>5.1} ms  {:>4.0} fps",
                interval as f64 * ctx.vblank_ms(),
                ctx.refresh / interval as f64
            ),
            plot::mono(10.5),
            plot::TEXT_DIM,
        );
        let bar_area = Rect::from_min_max(
            Pos2::new(rect.left() + label_w, rect.top() + 2.0),
            Pos2::new(rect.right() - 44.0, rect.bottom() - 2.0),
        );
        painter.rect_filled(bar_area, 2.0, plot::SURFACE);
        let late = interval > usize::from(s.target);
        let color = if late { plot::CRITICAL } else { plot::BLUE };
        let frac = n as f32 / max as f32;
        if n > 0 {
            let bar = Rect::from_min_size(
                bar_area.min,
                egui::vec2((bar_area.width() * frac).max(2.0), bar_area.height()),
            );
            painter.rect_filled(bar, CornerRadius::same(2), color);
        }
        let share = if s.frames > 0 {
            f64::from(n) / f64::from(s.frames) * 100.0
        } else {
            0.0
        };
        painter.text(
            Pos2::new(rect.right(), rect.center().y),
            Align2::RIGHT_CENTER,
            format!("{n}"),
            plot::mono(10.5),
            plot::TEXT,
        );
        response.on_hover_text(format!(
            "{n} frames took {interval}{plus} vblank{} ({share:.1}% of frames){}",
            if interval == 1 && plus.is_empty() {
                ""
            } else {
                "s"
            },
            if late { ", late for the target" } else { "" }
        ));
    }
}

// ---------------------------------------------------------------- budget

fn budget_meters(ui: &mut Ui, stats: &GuestStats, ctx: &Ctx) {
    // At the cursor: that vblank. Otherwise the last second of the window.
    let (samples, label): (Vec<FrameSample>, String) =
        match stats.view.hover.and_then(|v| stats.get(v)) {
            Some(sample) => (
                vec![*sample],
                format!("vblank at {}", focus_label(ctx, sample.vblank)),
            ),
            None => {
                let span = ctx.refresh.round() as u64;
                (
                    stats
                        .range(ctx.last.saturating_sub(span - 1), ctx.last)
                        .copied()
                        .collect(),
                    "average of the last second shown".to_string(),
                )
            }
        };
    if samples.is_empty() {
        return;
    }
    let n = samples.len() as f64;
    let avg =
        |f: &dyn Fn(&FrameSample) -> u32| samples.iter().map(|s| f64::from(f(s))).sum::<f64>() / n;
    let budget = avg(&|s| s.budget).max(1.0);
    let work = avg(&|s| s.cpu_work());
    let wait_io = avg(&|s| s.cpu[CpuClass::WaitHardware as usize]);
    let wait_ram = avg(&|s| s.cpu[CpuClass::WaitMemory as usize]);
    let gpu = avg(&|s| s.gpu_busy);
    let profiled = samples.iter().any(|s| s.cpu_profiled);
    ui.label(
        RichText::new(format!(
            "{} of {:.0}k cycles per vblank ({:.2} MHz)",
            label,
            budget / 1000.0,
            emulator_core::guest_stats::MASTER_CLOCK_HZ as f64 / 1e6
        ))
        .color(plot::TEXT_DIM)
        .size(11.0),
    );
    let meter = |ui: &mut Ui, name: &str, parts: &[(f64, Color32, &str)], note: String| {
        let width = ui.available_width();
        let (rect, response) =
            ui.allocate_exact_size(egui::vec2(width, 22.0), egui::Sense::hover());
        let painter = ui.painter();
        painter.text(
            Pos2::new(rect.left(), rect.center().y),
            Align2::LEFT_CENTER,
            name,
            plot::body(),
            plot::TEXT,
        );
        let bar = Rect::from_min_max(
            Pos2::new(rect.left() + 72.0, rect.top() + 4.0),
            Pos2::new(rect.right() - 92.0, rect.bottom() - 4.0),
        );
        painter.rect_filled(bar, 3.0, plot::SURFACE);
        let mut x = bar.left();
        for (value, color, _) in parts {
            let w = (value / budget) as f32 * bar.width();
            let w = w.min(bar.right() - x).max(0.0);
            if w > 0.0 {
                painter.rect_filled(
                    Rect::from_min_size(Pos2::new(x, bar.top()), egui::vec2(w, bar.height())),
                    0.0,
                    *color,
                );
                x += w;
            }
        }
        let total: f64 = parts.first().map_or(0.0, |p| p.0);
        painter.text(
            Pos2::new(rect.right(), rect.center().y),
            Align2::RIGHT_CENTER,
            format!("{:.0}k  {:>3.0}%", total / 1000.0, total / budget * 100.0),
            plot::mono(11.0),
            plot::TEXT,
        );
        let tip: String = parts
            .iter()
            .map(|(v, _, what)| format!("{what}: {:.0} cycles ({:.1}%)", v, v / budget * 100.0))
            .collect::<Vec<_>>()
            .join("\n");
        response.on_hover_text(format!("{tip}\n{note}"));
    };
    if profiled {
        meter(
            ui,
            "CPU",
            &[
                (work, plot::BLUE, "Work"),
                (wait_io, plot::VIOLET, "Wait loops polling I/O"),
                (
                    wait_ram,
                    plot::shade(plot::VIOLET, -0.5),
                    "Wait loops polling RAM",
                ),
            ],
            "Wait loops are tight store-free loops (a heuristic).".into(),
        );
    } else {
        ui.label(RichText::new("CPU attribution was off for this span.").color(plot::TEXT_DIM));
    }
    meter(
        ui,
        "GPU",
        &[(gpu, plot::ORANGE, "Drawing")],
        "GPU drawing time from the emulator's draw-time model (fitted to silicon benchmarks)."
            .into(),
    );
    let dma_gpu = avg(&|s| s.dma_busy[2]);
    let dma_other = avg(&|s| {
        s.dma_busy
            .iter()
            .enumerate()
            .filter(|(ch, _)| *ch != 2)
            .map(|(_, v)| *v)
            .sum()
    });
    meter(
        ui,
        "DMA",
        &[
            (dma_gpu, plot::MAGENTA, "GPU channel in flight"),
            (
                dma_other,
                plot::shade(plot::MAGENTA, -0.45),
                "Other channels in flight",
            ),
        ],
        "Time each channel had a transfer in flight; channels can overlap.".into(),
    );
}

// ---------------------------------------------------------------- CPU classes

fn class_color(class: CpuClass) -> Color32 {
    match class {
        CpuClass::Issue => plot::BLUE,
        CpuClass::RamLoad => plot::ORANGE,
        CpuClass::StackLoad => plot::shade(plot::ORANGE, 0.35),
        CpuClass::RamStore => plot::shade(plot::ORANGE, -0.35),
        CpuClass::Fetch => plot::AQUA,
        CpuClass::Gte => plot::YELLOW,
        CpuClass::MulDiv => plot::shade(plot::YELLOW, -0.35),
        CpuClass::Hardware => plot::MAGENTA,
        CpuClass::Other => plot::NEUTRAL,
        CpuClass::WaitHardware => plot::VIOLET,
        CpuClass::WaitMemory => plot::shade(plot::VIOLET, -0.5),
    }
}

fn class_hint(class: CpuClass) -> &'static str {
    match class {
        CpuClass::Issue => "One cycle per retired instruction: the code's own length.",
        CpuClass::RamLoad => "Main-RAM data loads stalling the pipeline.",
        CpuClass::StackLoad => "Main-RAM loads through $sp (spills and reloads).",
        CpuClass::RamStore => "Main-RAM stores stalling on the write path.",
        CpuClass::Fetch => "Instruction-cache refills and uncached fetches.",
        CpuClass::Gte => "Waiting for a busy GTE before the next command.",
        CpuClass::MulDiv => "MFHI/MFLO waiting for a multiply or divide.",
        CpuClass::Hardware => "I/O register and DMA accesses, including GPU FIFO back-pressure.",
        CpuClass::Other => "Charged cycles no class above claims.",
        CpuClass::WaitHardware => {
            "Tight store-free loops that read I/O (GPUSTAT, DMA, CD): waiting on hardware."
        }
        CpuClass::WaitMemory => {
            "Tight store-free loops that read RAM (vsync counters, flags), plus HLE kernel waits."
        }
    }
}

fn cpu_chart(ui: &mut Ui, stats: &mut GuestStats, ctx: &mut Ctx) {
    let any_profiled = stats.range(ctx.first, ctx.last).any(|s| s.cpu_profiled);
    if !any_profiled {
        ui.label(
            RichText::new("CPU cycle attribution starts when this panel opens.")
                .color(plot::TEXT_DIM),
        );
        return;
    }
    let (rect, response) = plot::chart_area(ui, 140.0);
    let plot_rect = rect.shrink2(egui::vec2(4.0, 6.0));
    let axis = ctx.axis(plot_rect);
    interact(ui, stats, ctx, &axis, &response);
    let hidden = stats.view.hidden_cpu;
    let bin = axis.bin();
    // Scale: 0..max(100%, tallest column), in % of the vblank budget.
    let mut columns: Vec<(u64, [f64; CPU_CLASSES])> = Vec::new();
    let mut v = axis.first;
    let mut peak: f64 = 100.0;
    while v <= axis.last {
        let mut sums = [0.0; CPU_CLASSES];
        let mut n = 0.0;
        for sample in stats.range(v, v + bin - 1) {
            if !sample.cpu_profiled || sample.budget == 0 {
                continue;
            }
            n += 1.0;
            for (i, sum) in sums.iter_mut().enumerate() {
                *sum += f64::from(sample.cpu[i]) / f64::from(sample.budget) * 100.0;
            }
        }
        if n > 0.0 {
            for sum in &mut sums {
                *sum /= n;
            }
            let total: f64 = sums
                .iter()
                .enumerate()
                .filter(|(i, _)| hidden & (1 << i) == 0)
                .map(|(_, v)| v)
                .sum();
            peak = peak.max(total);
            columns.push((v, sums));
        }
        v += bin;
    }
    let y_max = (peak / 25.0).ceil() * 25.0;
    let y = |pct: f64| plot_rect.bottom() - (pct / y_max) as f32 * plot_rect.height();
    for pct in [25.0, 50.0, 75.0] {
        if pct < y_max {
            plot::hline(
                ui,
                plot_rect,
                y(pct),
                &format!("{pct:.0}%"),
                false,
                plot::GRID,
            );
        }
    }
    let painter = ui.painter_at(rect);
    for (start, sums) in &columns {
        let x0 = axis.x(*start as f64);
        let x1 = axis.x((*start + bin) as f64).min(plot_rect.right());
        let w = (x1 - x0).max(1.0);
        let gap = if w >= 4.0 { 1.0 } else { 0.0 };
        let mut top = plot_rect.bottom();
        for class in CpuClass::ALL {
            let i = class as usize;
            if hidden & (1 << i) != 0 || sums[i] <= 0.0 {
                continue;
            }
            let h = (sums[i] / y_max) as f32 * plot_rect.height();
            painter.rect_filled(
                Rect::from_min_max(Pos2::new(x0, top - h), Pos2::new(x0 + w - gap, top)),
                0.0,
                class_color(class),
            );
            top -= h;
        }
    }
    plot::hline(
        ui,
        plot_rect,
        y(100.0),
        "vblank budget",
        true,
        plot::shade(plot::TEXT_DIM, -0.2),
    );
    plot::time_ticks(ui, &axis, ctx.now, ctx.refresh);

    // Focus: hovered vblank, else window average.
    let focus = stats
        .view
        .hover
        .and_then(|v| stats.get(v))
        .filter(|s| s.cpu_profiled)
        .copied();
    let (values, focus_text) = match focus {
        Some(sample) => {
            plot::cursor(ui, &axis, sample.vblank);
            let budget = f64::from(sample.budget.max(1));
            (
                std::array::from_fn(|i| f64::from(sample.cpu[i]) / budget * 100.0),
                format!("vblank at {}", focus_label(ctx, sample.vblank)),
            )
        }
        None => {
            let mut sums = [0.0f64; CPU_CLASSES];
            let mut budget = 0.0;
            for sample in stats.range(ctx.first, ctx.last).filter(|s| s.cpu_profiled) {
                budget += f64::from(sample.budget);
                for (i, sum) in sums.iter_mut().enumerate() {
                    *sum += f64::from(sample.cpu[i]);
                }
            }
            let budget = budget.max(1.0);
            (
                sums.map(|v| v / budget * 100.0),
                "window average".to_string(),
            )
        }
    };
    if response.hovered() {
        if let Some(sample) = focus {
            response.on_hover_ui_at_pointer(|ui| {
                ui.label(
                    RichText::new(format!("vblank at {}", focus_label(ctx, sample.vblank)))
                        .strong(),
                );
                for class in CpuClass::ALL {
                    let v = sample.cpu[class as usize];
                    if v > 0 {
                        ui.label(format!("{}: {} cycles", class.label(), v));
                    }
                }
                ui.label(format!(
                    "{} instructions, {} I-cache refills",
                    sample.instructions, sample.icache_refills
                ));
            });
        }
    }

    // One stacked bar for the focus, then a clickable legend.
    ui.add_space(14.0);
    let width = ui.available_width();
    let (bar_rect, _) = ui.allocate_exact_size(egui::vec2(width, 16.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(bar_rect, 3.0, plot::SURFACE);
    let total: f64 = values.iter().sum::<f64>().max(100.0);
    let mut x = bar_rect.left();
    for class in CpuClass::ALL {
        let v = values[class as usize];
        if v <= 0.0 || hidden & (1 << class as usize) != 0 {
            continue;
        }
        let w = (v / total) as f32 * bar_rect.width();
        painter.rect_filled(
            Rect::from_min_size(
                Pos2::new(x, bar_rect.top()),
                egui::vec2((w - 1.0).max(0.5), bar_rect.height()),
            ),
            0.0,
            class_color(class),
        );
        if w > 34.0 {
            painter.text(
                Pos2::new(x + w / 2.0, bar_rect.center().y),
                Align2::CENTER_CENTER,
                format!("{v:.0}%"),
                plot::small(),
                Color32::WHITE,
            );
        }
        x += w;
    }
    ui.label(
        RichText::new(format!(
            "{focus_text}, % of the vblank budget. Click a class to hide it."
        ))
        .color(plot::TEXT_MUTED)
        .size(10.5),
    );
    let columns_n = if width > 460.0 { 3 } else { 2 };
    let chip_w = ((width - 4.0 * (columns_n as f32 - 1.0)) / columns_n as f32).floor();
    for row in CpuClass::ALL.chunks(columns_n) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for &class in row {
                let i = class as usize;
                let hidden_now = stats.view.hidden_cpu & (1 << i) != 0;
                let (rect, response) =
                    ui.allocate_exact_size(egui::vec2(chip_w, 17.0), egui::Sense::click());
                let painter = ui.painter();
                if response.hovered() {
                    painter.rect_filled(rect, 3.0, plot::SURFACE);
                }
                let swatch = Rect::from_center_size(
                    Pos2::new(rect.left() + 7.0, rect.center().y),
                    egui::vec2(9.0, 9.0),
                );
                if hidden_now {
                    painter.rect_stroke(
                        swatch,
                        2.0,
                        Stroke::new(1.0, class_color(class)),
                        egui::StrokeKind::Inside,
                    );
                } else {
                    painter.rect_filled(swatch, 2.0, class_color(class));
                }
                let text_color = if hidden_now {
                    plot::TEXT_MUTED
                } else {
                    plot::TEXT_DIM
                };
                painter.text(
                    Pos2::new(rect.left() + 15.0, rect.center().y),
                    Align2::LEFT_CENTER,
                    class.label(),
                    plot::small(),
                    text_color,
                );
                painter.text(
                    Pos2::new(rect.right() - 2.0, rect.center().y),
                    Align2::RIGHT_CENTER,
                    format!("{:.1}%", values[i]),
                    plot::mono(10.0),
                    if hidden_now {
                        plot::TEXT_MUTED
                    } else {
                        plot::TEXT
                    },
                );
                if response.clicked() {
                    stats.view.hidden_cpu ^= 1 << i;
                }
                response.on_hover_text(class_hint(class));
            }
        });
    }
}

// ---------------------------------------------------------------- limiter

#[derive(Clone, Copy, PartialEq, Eq)]
enum Limit {
    Cpu,
    Gpu,
    WaitIo,
    WaitVblank,
}

impl Limit {
    fn color(self) -> Color32 {
        match self {
            Limit::Cpu => plot::BLUE,
            Limit::Gpu => plot::ORANGE,
            Limit::WaitIo => plot::VIOLET,
            Limit::WaitVblank => plot::shade(plot::VIOLET, -0.5),
        }
    }

    fn label(self) -> &'static str {
        match self {
            Limit::Cpu => "CPU-bound",
            Limit::Gpu => "GPU-bound",
            Limit::WaitIo => "Waiting on I/O",
            Limit::WaitVblank => "Paced (waiting on vblank/flags)",
        }
    }
}

/// Classify one presented frame from the vblanks it spanned.
fn classify(stats: &GuestStats, present: &FrameSample) -> Option<(Limit, f64, f64)> {
    let start = present.vblank + 1 - u64::from(present.present_interval.max(1));
    let (mut cycles, mut work, mut gpu, mut wait_io, mut wait_ram, mut profiled) =
        (0u64, 0u64, 0u64, 0u64, 0u64, false);
    for s in stats.range(start, present.vblank) {
        cycles += u64::from(s.cycles);
        work += u64::from(s.cpu_work());
        gpu += u64::from(s.gpu_busy);
        wait_io += u64::from(s.cpu[CpuClass::WaitHardware as usize]);
        wait_ram += u64::from(s.cpu[CpuClass::WaitMemory as usize]);
        profiled |= s.cpu_profiled;
    }
    if cycles == 0 || !profiled {
        return None;
    }
    let cpu_pct = work as f64 / cycles as f64 * 100.0;
    let gpu_pct = gpu as f64 / cycles as f64 * 100.0;
    let limit = if cpu_pct >= 90.0 {
        Limit::Cpu
    } else if gpu_pct >= 90.0 {
        Limit::Gpu
    } else if wait_io > wait_ram {
        Limit::WaitIo
    } else {
        Limit::WaitVblank
    };
    Some((limit, cpu_pct, gpu_pct))
}

fn limiter_chart(ui: &mut Ui, stats: &mut GuestStats, ctx: &mut Ctx, _s: &Summary) {
    legend_row(
        ui,
        &[
            (plot::BLUE, "CPU work, % of cycles", false),
            (plot::ORANGE, "GPU drawing, % of cycles", false),
        ],
    );
    let (rect, response) = plot::chart_area(ui, 104.0);
    let plot_rect = Rect::from_min_max(
        rect.min + egui::vec2(4.0, 6.0),
        Pos2::new(rect.right() - 4.0, rect.bottom() - 18.0),
    );
    let strip = Rect::from_min_max(
        Pos2::new(plot_rect.left(), rect.bottom() - 13.0),
        Pos2::new(plot_rect.right(), rect.bottom() - 4.0),
    );
    let axis = ctx.axis(plot_rect);
    interact(ui, stats, ctx, &axis, &response);
    let bin = axis.bin();
    let mut cpu_line = Vec::new();
    let mut gpu_line = Vec::new();
    let mut peak: f64 = 100.0;
    let mut v = axis.first;
    let mut points = Vec::new();
    while v <= axis.last {
        let (mut cycles, mut work, mut gpu, mut prof) = (0u64, 0u64, 0u64, false);
        for s in stats.range(v, v + bin - 1) {
            cycles += u64::from(s.cycles);
            work += u64::from(s.cpu_work());
            gpu += u64::from(s.gpu_busy);
            prof |= s.cpu_profiled;
        }
        if cycles > 0 {
            let cpu = work as f64 / cycles as f64 * 100.0;
            let gpu = gpu as f64 / cycles as f64 * 100.0;
            peak = peak.max(cpu).max(gpu);
            points.push((v, prof.then_some(cpu), gpu));
        }
        v += bin;
    }
    let y_max = (peak / 25.0).ceil() * 25.0;
    let y = |pct: f64| plot_rect.bottom() - (pct / y_max) as f32 * plot_rect.height();
    plot::hline(ui, plot_rect, y(50.0), "50%", false, plot::GRID);
    plot::hline(
        ui,
        plot_rect,
        y(100.0),
        "100%",
        true,
        plot::shade(plot::TEXT_DIM, -0.2),
    );
    for (v, cpu, gpu) in points {
        let x = axis.x(v as f64 + bin as f64 / 2.0);
        if let Some(cpu) = cpu {
            cpu_line.push(Pos2::new(x, y(cpu)));
        }
        gpu_line.push(Pos2::new(x, y(gpu)));
    }
    let painter = ui.painter_at(rect);
    if gpu_line.len() > 1 {
        painter.line(gpu_line, Stroke::new(1.5, plot::ORANGE));
    }
    if cpu_line.len() > 1 {
        painter.line(cpu_line, Stroke::new(1.5, plot::BLUE));
    }
    // Per-frame verdict strip.
    let mut counts = [0u32; 4];
    for present in stats
        .range(axis.first, axis.last)
        .filter(|s| s.presented && s.present_interval > 0)
    {
        let Some((limit, _, _)) = classify(stats, present) else {
            continue;
        };
        counts[limit as usize] += 1;
        let x0 = axis
            .x((present.vblank + 1 - u64::from(present.present_interval)) as f64)
            .max(strip.left());
        let x1 = axis.x(present.vblank as f64 + 1.0).min(strip.right());
        painter.rect_filled(
            Rect::from_min_max(
                Pos2::new(x0, strip.top()),
                Pos2::new((x1 - 1.0).max(x0 + 0.5), strip.bottom()),
            ),
            1.0,
            limit.color(),
        );
    }
    if let Some(hover) = stats.view.hover {
        plot::cursor(ui, &axis, hover);
        if response.hovered() {
            let frame = stats
                .range(hover, axis.last)
                .find(|s| s.presented && s.present_interval > 0)
                .copied();
            let verdict = frame.and_then(|f| classify(stats, &f).map(|c| (f, c)));
            let when = focus_label(ctx, hover);
            response.on_hover_ui_at_pointer(|ui| match verdict {
                Some((f, (limit, cpu, gpu))) => {
                    ui.label(RichText::new(limit.label()).strong());
                    ui.label(format!(
                        "Frame presented {} over {} vblank{}: CPU work {cpu:.0}%, GPU drawing {gpu:.0}% of its cycles",
                        focus_label(ctx, f.vblank),
                        f.present_interval,
                        if f.present_interval == 1 { "" } else { "s" }
                    ));
                }
                None => {
                    ui.label(format!("{when}: no classified frame here"));
                }
            });
        }
    }
    ui.add_space(2.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 10.0;
        for limit in [Limit::Cpu, Limit::Gpu, Limit::WaitIo, Limit::WaitVblank] {
            let (r, _) = ui.allocate_exact_size(egui::vec2(9.0, 9.0), egui::Sense::hover());
            ui.painter().rect_filled(r, 2.0, limit.color());
            ui.label(
                RichText::new(format!("{} {}", limit.label(), counts[limit as usize]))
                    .color(plot::TEXT_DIM)
                    .size(10.5),
            );
        }
    })
    .response
    .on_hover_text("Each presented frame: CPU-bound if CPU work filled at least 90% of its vblanks, GPU-bound if drawing did, otherwise what the waiting CPU polled.");
}

// ---------------------------------------------------------------- sparklines

struct Series {
    group: &'static str,
    name: &'static str,
    color: Color32,
    /// Value per vblank; `percent` series are relative to the budget.
    value: fn(&FrameSample) -> f64,
    format: fn(f64, f64) -> String,
}

fn pct_of_budget(s: &FrameSample, v: u32) -> f64 {
    if s.budget == 0 {
        0.0
    } else {
        f64::from(v) / f64::from(s.budget) * 100.0
    }
}

fn fmt_count(v: f64, _refresh: f64) -> String {
    plot::count(v)
}

fn fmt_pct(v: f64, _refresh: f64) -> String {
    format!("{v:.0}%")
}

fn fmt_mips(v: f64, refresh: f64) -> String {
    format!("{:.1} MIPS", v * refresh / 1e6)
}

fn fmt_rate(v: f64, refresh: f64) -> String {
    format!("{}/s", plot::count(v * refresh))
}

const SERIES: &[Series] = &[
    Series {
        group: "GPU",
        name: "Polygons",
        color: plot::ORANGE,
        value: |s| f64::from(s.polygons),
        format: fmt_count,
    },
    Series {
        group: "GPU",
        name: "Sprites/rects",
        color: plot::ORANGE,
        value: |s| f64::from(s.rects),
        format: fmt_count,
    },
    Series {
        group: "GPU",
        name: "Pixels drawn",
        color: plot::ORANGE,
        value: |s| f64::from(s.pixels),
        format: fmt_count,
    },
    Series {
        group: "GPU",
        name: "Textured pixels",
        color: plot::ORANGE,
        value: |s| f64::from(s.texture_pixels),
        format: fmt_count,
    },
    Series {
        group: "GPU",
        name: "VRAM upload pixels",
        color: plot::ORANGE,
        value: |s| f64::from(s.vram_upload_pixels),
        format: fmt_count,
    },
    Series {
        group: "GPU",
        name: "Fill time",
        color: plot::ORANGE,
        value: |s| pct_of_budget(s, s.gpu_fill_cycles),
        format: fmt_pct,
    },
    Series {
        group: "CD-ROM",
        name: "Data sectors",
        color: plot::AQUA,
        value: |s| f64::from(s.cd_data_sectors),
        format: fmt_rate,
    },
    Series {
        group: "CD-ROM",
        name: "XA audio sectors",
        color: plot::AQUA,
        value: |s| f64::from(s.cd_xa_sectors),
        format: fmt_rate,
    },
    Series {
        group: "CD-ROM",
        name: "CD-DA sectors",
        color: plot::AQUA,
        value: |s| f64::from(s.cd_cdda_sectors),
        format: fmt_rate,
    },
    Series {
        group: "CD-ROM",
        name: "Seeks",
        color: plot::AQUA,
        value: |s| f64::from(s.cd_seeks),
        format: fmt_count,
    },
    Series {
        group: "SPU",
        name: "Active voices",
        color: plot::MAGENTA,
        value: |s| f64::from(s.spu_active_voices),
        format: fmt_count,
    },
    Series {
        group: "SPU",
        name: "Key-ons",
        color: plot::MAGENTA,
        value: |s| f64::from(s.spu_key_ons),
        format: fmt_count,
    },
    Series {
        group: "DMA",
        name: "GPU channel busy",
        color: plot::VIOLET,
        value: |s| pct_of_budget(s, s.dma_busy[2]),
        format: fmt_pct,
    },
    Series {
        group: "DMA",
        name: "OT clear busy",
        color: plot::VIOLET,
        value: |s| pct_of_budget(s, s.dma_busy[6]),
        format: fmt_pct,
    },
    Series {
        group: "DMA",
        name: "CD channel busy",
        color: plot::VIOLET,
        value: |s| pct_of_budget(s, s.dma_busy[3]),
        format: fmt_pct,
    },
    Series {
        group: "DMA",
        name: "SPU channel busy",
        color: plot::VIOLET,
        value: |s| pct_of_budget(s, s.dma_busy[4]),
        format: fmt_pct,
    },
    Series {
        group: "MDEC",
        name: "MDEC channels busy",
        color: plot::YELLOW,
        value: |s| pct_of_budget(s, s.dma_busy[0].max(s.dma_busy[1])),
        format: fmt_pct,
    },
    Series {
        group: "MDEC",
        name: "Macroblocks",
        color: plot::YELLOW,
        value: |s| f64::from(s.mdec_macroblocks),
        format: fmt_count,
    },
    Series {
        group: "System",
        name: "Instructions",
        color: plot::BLUE,
        value: |s| f64::from(s.instructions),
        format: fmt_mips,
    },
    Series {
        group: "System",
        name: "RAM data accesses",
        color: plot::BLUE,
        value: |s| f64::from(s.ram_loads) + f64::from(s.ram_stores),
        format: fmt_count,
    },
    Series {
        group: "System",
        name: "I-cache refills",
        color: plot::BLUE,
        value: |s| f64::from(s.icache_refills),
        format: fmt_count,
    },
    Series {
        group: "System",
        name: "Interrupts",
        color: plot::BLUE,
        value: |s| s.irqs.iter().map(|&n| f64::from(n)).sum(),
        format: fmt_rate,
    },
    Series {
        group: "System",
        name: "Timer interrupts",
        color: plot::BLUE,
        value: |s| f64::from(s.irqs[4]) + f64::from(s.irqs[5]) + f64::from(s.irqs[6]),
        format: fmt_rate,
    },
    Series {
        group: "System",
        name: "Pad polls",
        color: plot::BLUE,
        value: |s| f64::from(s.pad_polls),
        format: fmt_rate,
    },
];

fn sparklines(ui: &mut Ui, stats: &mut GuestStats, ctx: &mut Ctx) {
    ui.horizontal(|ui| {
        ui.label(
            RichText::new("Per vblank. Numbers are 1 s means at the cursor (else now).")
                .color(plot::TEXT_MUTED)
                .size(10.5),
        );
        ui.menu_button("Series", |ui| {
            for (i, series) in SERIES.iter().enumerate() {
                let mut shown = stats.view.hidden_series & (1 << i) == 0;
                if ui
                    .checkbox(&mut shown, format!("{}: {}", series.group, series.name))
                    .changed()
                {
                    stats.view.hidden_series ^= 1 << i;
                }
            }
        });
    });
    let width = ui.available_width();
    let columns = if width > 520.0 { 3 } else { 2 };
    let tile_w = ((width - 6.0 * (columns as f32 - 1.0)) / columns as f32).floor();
    let visible: Vec<usize> = (0..SERIES.len())
        .filter(|i| stats.view.hidden_series & (1 << i) == 0)
        .collect();
    let focus = stats.view.hover.unwrap_or(ctx.last);
    let mut group = "";
    let mut index = 0;
    while index < visible.len() {
        let g = SERIES[visible[index]].group;
        if g != group {
            group = g;
            ui.add_space(2.0);
            ui.label(
                RichText::new(group)
                    .color(plot::TEXT_DIM)
                    .size(11.0)
                    .strong(),
            );
        }
        let row: Vec<usize> = visible[index..]
            .iter()
            .copied()
            .take_while(|&i| SERIES[i].group == group)
            .take(columns)
            .collect();
        index += row.len();
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            for i in row {
                sparkline_tile(ui, stats, ctx, &SERIES[i], tile_w, focus);
            }
        });
    }
}

fn sparkline_tile(
    ui: &mut Ui,
    stats: &mut GuestStats,
    ctx: &mut Ctx,
    series: &Series,
    width: f32,
    focus: u64,
) {
    let (rect, response) =
        ui.allocate_exact_size(egui::vec2(width, 44.0), egui::Sense::click_and_drag());
    ui.painter().rect_filled(rect, 3.0, plot::SURFACE);
    let plot_rect = Rect::from_min_max(
        rect.min + egui::vec2(4.0, 17.0),
        rect.max - egui::vec2(4.0, 3.0),
    );
    let axis = ctx.axis(plot_rect);
    interact(ui, stats, ctx, &axis, &response);
    let bin = axis.bin();
    let mut values = Vec::new();
    let mut peak: f64 = 0.0;
    let mut v = axis.first;
    while v <= axis.last {
        let mut sum = 0.0;
        let mut n = 0.0;
        for s in stats.range(v, v + bin - 1) {
            sum += (series.value)(s);
            n += 1.0;
        }
        if n > 0.0 {
            let mean = sum / n;
            peak = peak.max(mean);
            values.push((v, mean));
        }
        v += bin;
    }
    let painter = ui.painter_at(rect);
    // One-second mean ending at the focus: per-vblank counts are too
    // spiky to read as a single number.
    let focus = focus.min(stats.newest_vblank().unwrap_or(0));
    let span = ctx.refresh.round().max(1.0) as u64;
    let (sum, n) = stats
        .range(focus.saturating_sub(span - 1), focus)
        .fold((0.0, 0.0), |(sum, n), s| (sum + (series.value)(s), n + 1.0));
    let current = (n > 0.0).then(|| sum / n);
    painter.text(
        rect.left_top() + egui::vec2(5.0, 3.0),
        Align2::LEFT_TOP,
        series.name,
        plot::small(),
        plot::TEXT_DIM,
    );
    painter.text(
        Pos2::new(rect.right() - 5.0, rect.top() + 2.0),
        Align2::RIGHT_TOP,
        current.map_or("--".into(), |c| (series.format)(c, ctx.refresh)),
        plot::mono(11.0),
        plot::TEXT,
    );
    if peak > 0.0 && values.len() > 1 {
        let y = |val: f64| plot_rect.bottom() - (val / peak) as f32 * plot_rect.height();
        let mut line = Vec::with_capacity(values.len());
        for &(v, val) in &values {
            let x = axis.x(v as f64 + bin as f64 / 2.0);
            line.push(Pos2::new(x, y(val)));
        }
        // Filled area as thin columns (the outline is not convex).
        for pair in line.windows(2) {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    Pos2::new(pair[0].x, plot_rect.bottom()),
                    pair[0],
                    pair[1],
                    Pos2::new(pair[1].x, plot_rect.bottom()),
                ],
                series.color.gamma_multiply(0.22),
                Stroke::NONE,
            ));
        }
        painter.line(line, Stroke::new(1.25, series.color));
    }
    if let Some(hover) = stats.view.hover {
        plot::cursor(ui, &axis, hover);
    }
    let peak_text = (series.format)(peak, ctx.refresh);
    response.on_hover_text(format!(
        "{} ({}): peak {} per vblank in view",
        series.name, series.group, peak_text
    ));
}

// ---------------------------------------------------------------- SPU

fn voice_meters(ui: &mut Ui, stats: &GuestStats) {
    let width = ui.available_width();
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, 58.0), egui::Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 3.0, plot::SURFACE);
    let inner = rect.shrink2(egui::vec2(4.0, 4.0));
    let meter_area = Rect::from_min_max(inner.min, Pos2::new(inner.right(), inner.bottom() - 11.0));
    let slot = meter_area.width() / VOICES as f32;
    let levels = stats.voice_levels();
    let peaks = stats.voice_peaks();
    for voice in 0..VOICES {
        let x0 = meter_area.left() + slot * voice as f32 + 1.0;
        let bar = Rect::from_min_max(
            Pos2::new(x0, meter_area.top()),
            Pos2::new(x0 + slot - 2.0, meter_area.bottom()),
        );
        painter.rect_filled(bar, 1.0, Color32::from_rgb(30, 30, 36));
        let level = f32::from(levels[voice]) / 32767.0;
        if level > 0.0 {
            let h = bar.height() * level;
            painter.rect_filled(
                Rect::from_min_max(Pos2::new(bar.left(), bar.bottom() - h), bar.max),
                1.0,
                plot::MAGENTA,
            );
        }
        let peak = peaks[voice];
        if peak > 0.02 {
            let y = bar.bottom() - bar.height() * peak.min(1.0);
            painter.line_segment(
                [Pos2::new(bar.left(), y), Pos2::new(bar.right(), y)],
                Stroke::new(1.5, plot::shade(plot::MAGENTA, 0.5)),
            );
        }
        if voice % 4 == 0 {
            painter.text(
                Pos2::new(bar.left(), inner.bottom()),
                Align2::LEFT_BOTTOM,
                format!("{voice}"),
                plot::small(),
                plot::TEXT_MUTED,
            );
        }
    }
    let active = levels.iter().filter(|&&l| l > 0).count();
    response.on_hover_text(format!(
        "Envelope level per voice, live. The bright tick is a falling peak that jumps to the top on key-on. {active} of {VOICES} voices sounding."
    ));
}

// ---------------------------------------------------------------- VRAM

fn texpage_heat(ui: &mut Ui, stats: &GuestStats, vram: Option<egui::TextureId>) {
    let heat = stats.texpage_heat();
    let total: f32 = heat.iter().sum();
    let max = heat.iter().copied().fold(0.0f32, f32::max);
    let width = ui.available_width().min(640.0);
    let height = width / 2.0;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(width, height), egui::Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 2.0, plot::SURFACE);
    if let Some(tex) = vram {
        egui::Image::new((tex, rect.size()))
            .uv(Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)))
            .tint(Color32::from_gray(150))
            .paint_at(ui, rect);
    }
    let cell = egui::vec2(rect.width() / 16.0, rect.height() / 2.0);
    let mut ranked: Vec<(usize, f32)> = heat.iter().copied().enumerate().collect();
    ranked.sort_by(|a, b| b.1.total_cmp(&a.1));
    for (page, &value) in heat.iter().enumerate() {
        let min = rect.min + egui::vec2(cell.x * (page % 16) as f32, cell.y * (page / 16) as f32);
        let r = Rect::from_min_size(min, cell);
        let t = if max > 0.0 { (value / max).sqrt() } else { 0.0 };
        if t > 0.01 {
            painter.rect_filled(
                r.shrink(0.5),
                0.0,
                plot::ORANGE.gamma_multiply(0.15 + 0.6 * t),
            );
        }
        painter.rect_stroke(
            r,
            0.0,
            Stroke::new(0.5, Color32::from_rgba_unmultiplied(255, 255, 255, 28)),
            egui::StrokeKind::Inside,
        );
    }
    for &(page, value) in ranked.iter().take(4) {
        if value <= 0.0 || total <= 0.0 {
            continue;
        }
        let min = rect.min + egui::vec2(cell.x * (page % 16) as f32, cell.y * (page / 16) as f32);
        let r = Rect::from_min_size(min, cell);
        painter.text(
            r.center(),
            Align2::CENTER_CENTER,
            format!("{:.0}%", value / total * 100.0),
            plot::small(),
            Color32::WHITE,
        );
    }
    let hovered_page = response.hover_pos().map(|p| {
        let col = (((p.x - rect.left()) / cell.x) as usize).min(15);
        let row = (((p.y - rect.top()) / cell.y) as usize).min(1);
        row * 16 + col
    });
    let tip = match hovered_page {
        Some(page) => format!(
            "Texture page {page} (VRAM x {}, y {}): {:.1}% of recent textured pixels",
            (page % 16) * 64,
            (page / 16) * 256,
            if total > 0.0 {
                heat[page] / total * 100.0
            } else {
                0.0
            }
        ),
        None => String::new(),
    };
    response.on_hover_text(tip);
    ui.label(
        RichText::new("Share of textured pixels sampled from each 64x256 texture page, decaying over about half a second.")
            .color(plot::TEXT_MUTED)
            .size(10.5),
    );
}
