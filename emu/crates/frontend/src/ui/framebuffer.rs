//! Framebuffer view for the central panel.
//!
//! The central panel presents the selected visible display texture.
//! Normal 15-bit gameplay samples the host-GPU renderer's VRAM-shaped
//! target; 24bpp scanout samples the packed display texture until the
//! HW path grows RGB888 display decoding.

/// CRT display aspect ratio for NTSC. The visible area is 4:3
/// regardless of which horizontal-resolution mode the game picks
/// (256/320/368/384/512/640) -- a 512×240 frame is supposed to
/// squash horizontally on a real CRT, not stretch into 16:9.
const CRT_ASPECT: f32 = 4.0 / 3.0;

/// How much of the window's width a picture may give up to land on a whole multiple of the
/// display's own pixels: up to 8%.
const INTEGER_LOSS: f32 = 0.08;

/// Where the 4:3 picture goes in `avail` (points), at `pixels_per_point`.
///
/// Its corners always sit on whole device pixels, so a texel never straddles two of them (the
/// half-pixel offsets of a centred, fractional rectangle soften every edge). With `integer` set
/// and a display `native_h` rows tall, the picture also snaps down to the nearest whole multiple
/// of those rows when that costs less than [`INTEGER_LOSS`] of its size: every game pixel is then
/// the same number of device pixels, and the picture stays sharp and even.
pub fn fit_rect(
    avail: egui::Rect,
    pixels_per_point: f32,
    integer: bool,
    native_h: u32,
) -> egui::Rect {
    let ppp = pixels_per_point.max(1.0);
    let h = avail.height().min(avail.width() / CRT_ASPECT);
    let mut h_px = (h * ppp).floor().max(1.0);
    if integer && native_h > 0 {
        let k = (h_px / native_h as f32).floor();
        if k >= 1.0 && k * native_h as f32 >= h_px * (1.0 - INTEGER_LOSS) {
            h_px = k * native_h as f32;
        }
    }
    // Four thirds on whole pixels, both sizes even so the picture centres without a half pixel,
    // and never past the room it was given.
    let h_px = (h_px / 2.0).floor().max(1.0) * 2.0;
    let w_px = ((h_px * CRT_ASPECT) / 2.0).floor().max(1.0) * 2.0;
    let min_x = (avail.center().x * ppp - w_px / 2.0).round();
    let min_y = (avail.center().y * ppp - h_px / 2.0).round();
    egui::Rect::from_min_size(
        egui::pos2(min_x / ppp, min_y / ppp),
        egui::vec2(w_px / ppp, h_px / ppp),
    )
}

pub fn draw(
    ui: &mut egui::Ui,
    display_tex: egui::TextureId,
    display_uv: egui::Rect,
    present_size_px: &mut (u32, u32),
    game_rect: &mut [f32; 4],
    integer_scale: bool,
    native_h: u32,
) {
    // Painted edge-to-edge: no frame, no margin -- the 4:3 fit against
    // the raw panel rect is the only letterboxing.
    let avail = ui.available_rect_before_wrap();
    if avail.width() <= 0.0 || avail.height() <= 0.0 {
        return;
    }
    let pixels_per_point = ui.ctx().pixels_per_point().max(1.0);
    let rect = fit_rect(avail, pixels_per_point, integer_scale, native_h);
    *game_rect = [rect.left(), rect.top(), rect.right(), rect.bottom()];
    *present_size_px = (
        (rect.width() * pixels_per_point).round().max(1.0) as u32,
        (rect.height() * pixels_per_point).round().max(1.0) as u32,
    );

    ui.allocate_rect(avail, egui::Sense::hover());
    egui::Image::new((display_tex, rect.size()))
        .uv(display_uv)
        .paint_at(ui, rect);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn avail(w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h))
    }

    #[test]
    fn the_picture_sits_on_whole_device_pixels_at_every_ratio() {
        for ppp in [1.0, 1.5, 2.0, 2.625, 3.0] {
            for (w, h) in [
                (393.0, 295.0),
                (852.0, 393.0),
                (360.0, 270.0),
                (1280.0, 766.0),
            ] {
                for integer in [false, true] {
                    let r = fit_rect(avail(w, h), ppp, integer, 240);
                    for v in [r.left(), r.top(), r.right(), r.bottom()] {
                        let px = v * ppp;
                        assert!((px - px.round()).abs() < 1e-3, "{w}x{h}@{ppp}: {v} -> {px}");
                    }
                    assert!(
                        r.width() <= w + 1e-3 && r.height() <= h + 1e-3,
                        "{w}x{h}@{ppp} int={integer}: {r:?}"
                    );
                    assert!((r.width() / r.height() - 4.0 / 3.0).abs() < 0.01, "{r:?}");
                }
            }
        }
    }

    #[test]
    fn a_close_fit_snaps_to_a_whole_multiple_and_a_poor_one_does_not() {
        // 1280x960 device pixels: exactly 4x of a 240-row display.
        let r = fit_rect(avail(1280.0, 960.0), 1.0, true, 240);
        assert_eq!((r.width(), r.height()), (1280.0, 960.0));
        // 1170 rows of room: 4x (960) costs 18%, so it stays a plain fit.
        let r = fit_rect(avail(1560.0, 1170.0), 1.0, true, 240);
        assert!(r.height() > 1100.0, "{r:?}");
        // 990 rows: 4x is 960, within 8%.
        let r = fit_rect(avail(1320.0, 990.0), 1.0, true, 240);
        assert_eq!(r.height(), 960.0);
        // Off, the picture fills the room.
        let r = fit_rect(avail(1320.0, 990.0), 1.0, false, 240);
        assert_eq!(r.height(), 990.0);
        // A 480-row (interlaced) display counts its own rows.
        let r = fit_rect(avail(1280.0, 960.0), 1.0, true, 480);
        assert_eq!(r.height(), 960.0);
    }
}
