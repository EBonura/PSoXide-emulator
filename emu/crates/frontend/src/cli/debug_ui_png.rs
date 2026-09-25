//! Offscreen PNG snapshot of the debug sidebar's guest performance panel.
//!
//! `launch --debug-ui-png OUT` records [`GuestStats`] every route tick and,
//! when the run ends, lays the panel out at the sidebar width and renders it
//! through egui-wgpu into a texture that is read back and written as a PNG.
//! No window is created, so it is safe for agents and CI.

use std::path::Path;

use egui::{Pos2, RichText};
use psoxide_debug_ui::GuestStats;

use crate::theme;

/// Render the panel to `out`. `vram_rgba` is the 1024x512 VRAM image for the
/// texture-page heat map; `pointer` (panel pixels) places the mouse so one
/// hover tooltip and cursor show in the capture.
pub(super) fn render(
    stats: &mut GuestStats,
    vram_rgba: &[u8],
    width: u32,
    pointer: Option<(f32, f32)>,
    out: &Path,
) -> Result<(), String> {
    let ctx = egui::Context::default();
    theme::apply(&ctx);
    ctx.style_mut(|style| {
        style.interaction.tooltip_delay = 0.0;
        style.interaction.show_tooltips_only_when_still = false;
    });
    let vram = ctx.load_texture(
        "debug-ui-vram",
        egui::ColorImage::from_rgba_unmultiplied([1024, 512], vram_rgba),
        egui::TextureOptions::NEAREST,
    );
    let vram_id = vram.id();

    // Lay out once on a tall canvas to measure the panel, then capture at
    // exactly that height.
    let mut used = 0.0f32;
    let mut textures = egui::TexturesDelta::default();
    let measure = ctx.run(input(width, 8000, 0.0, Vec::new()), |ctx| {
        used = panel(ctx, stats, vram_id);
    });
    textures.append(measure.textures_delta);
    let height = (used.ceil() as u32 + 16).clamp(200, 8000);

    let mut last = None;
    for frame in 0..4u32 {
        let events = match pointer {
            Some((x, y)) if frame >= 1 => vec![egui::Event::PointerMoved(Pos2::new(x, y))],
            _ => Vec::new(),
        };
        let output = ctx.run(
            input(width, height, 1.0 + f64::from(frame) * 0.5, events),
            |ctx| {
                panel(ctx, stats, vram_id);
            },
        );
        textures.append(output.textures_delta.clone());
        last = Some(output);
    }
    let output = last.expect("ran at least one frame");
    let jobs = ctx.tessellate(output.shapes, output.pixels_per_point);
    let rgba = paint(width, height, output.pixels_per_point, &jobs, textures)?;
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
    }
    image::save_buffer(out, &rgba, width, height, image::ExtendedColorType::Rgba8)
        .map_err(|e| format!("write {}: {e}", out.display()))
}

/// The sidebar chrome around the panel; returns the content height.
fn panel(ctx: &egui::Context, stats: &mut GuestStats, vram: egui::TextureId) -> f32 {
    let mut used = 0.0;
    egui::CentralPanel::default()
        .frame(
            egui::Frame::NONE
                .fill(theme::PANEL_BG)
                .inner_margin(egui::Margin::same(8)),
        )
        .show(ctx, |ui| {
            ui.label(
                RichText::new("Debug")
                    .color(theme::ACCENT)
                    .size(theme::FONT_SIZE_HEADING),
            );
            ui.separator();
            let section = egui::CollapsingHeader::new(
                RichText::new("Guest performance (PS1)")
                    .color(theme::TEXT)
                    .strong(),
            )
            .default_open(true)
            .show(ui, |ui| {
                theme::viz_frame(ui, "", |ui| {
                    let _ = psoxide_debug_ui::draw(ui, stats, Some(vram));
                });
            });
            let mut bottom = section
                .body_response
                .map_or(section.header_response.rect.bottom(), |body| {
                    body.rect.bottom()
                });
            // The sidebar's other sections, collapsed as they start.
            for title in ["CPU Registers", "Memory", "VRAM"] {
                let header =
                    egui::CollapsingHeader::new(RichText::new(title).color(theme::TEXT).strong())
                        .default_open(false)
                        .show(ui, |_| {});
                bottom = header.header_response.rect.bottom();
            }
            used = bottom;
        });
    used
}

fn input(width: u32, height: u32, time: f64, events: Vec<egui::Event>) -> egui::RawInput {
    let rect = egui::Rect::from_min_size(Pos2::ZERO, egui::vec2(width as f32, height as f32));
    let mut input = egui::RawInput {
        screen_rect: Some(rect),
        time: Some(time),
        events,
        focused: true,
        ..Default::default()
    };
    if let Some(viewport) = input.viewports.get_mut(&egui::ViewportId::ROOT) {
        viewport.native_pixels_per_point = Some(1.0);
        viewport.inner_rect = Some(rect);
    }
    input
}

fn paint(
    width: u32,
    height: u32,
    pixels_per_point: f32,
    jobs: &[egui::ClippedPrimitive],
    textures: egui::TexturesDelta,
) -> Result<Vec<u8>, String> {
    let (device, queue) = super::headless_wgpu_device()?;
    let format = wgpu::TextureFormat::Rgba8UnormSrgb;
    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("psoxide-debug-ui-png"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = target.create_view(&wgpu::TextureViewDescriptor::default());
    let mut renderer = egui_wgpu::Renderer::new(&device, format, None, 1, false);
    let screen = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [width, height],
        pixels_per_point,
    };
    for (id, delta) in &textures.set {
        renderer.update_texture(&device, &queue, *id, delta);
    }
    let unpadded = width * 4;
    let padded =
        unpadded.div_ceil(wgpu::COPY_BYTES_PER_ROW_ALIGNMENT) * wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("psoxide-debug-ui-readback"),
        size: u64::from(padded * height),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("psoxide-debug-ui-encoder"),
    });
    renderer.update_buffers(&device, &queue, &mut encoder, jobs, &screen);
    {
        let mut pass = encoder
            .begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("psoxide-debug-ui-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::BLACK),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                occlusion_query_set: None,
                timestamp_writes: None,
            })
            .forget_lifetime();
        renderer.render(&mut pass, jobs, &screen);
    }
    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );
    queue.submit(Some(encoder.finish()));
    let slice = readback.slice(..);
    let (sender, receiver) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |result| {
        let _ = sender.send(result);
    });
    device.poll(wgpu::Maintain::Wait);
    receiver
        .recv()
        .map_err(|_| "debug UI readback callback dropped".to_string())?
        .map_err(|error| format!("debug UI readback: {error:?}"))?;
    let mapped = slice.get_mapped_range();
    let mut rgba = Vec::with_capacity((unpadded * height) as usize);
    for row in 0..height {
        let start = (row * padded) as usize;
        rgba.extend_from_slice(&mapped[start..start + unpadded as usize]);
    }
    Ok(rgba)
}
