//! Exact visual evidence, independent of the gate's tolerance acceptance.
use crate::exec::Capture;
use crate::img::{Img, Rect};

pub fn exact_overlay(frame: &Img, reference: &Img) -> Img {
    assert_eq!((frame.w, frame.h), (reference.w, reference.h));
    let mut out = reference.clone();
    for ((dst, a), b) in out
        .rgba
        .chunks_exact_mut(4)
        .zip(frame.rgba.chunks_exact(4))
        .zip(reference.rgba.chunks_exact(4))
    {
        if a[..3] != b[..3] {
            dst[..3].copy_from_slice(&[255, 0, 255]);
        } else {
            for c in 0..3 {
                dst[c] = b[c] / 4;
            }
        }
    }
    out
}

pub fn crop(img: &Img, r: Rect) -> Result<Img, String> {
    if r.w == 0
        || r.h == 0
        || r.x.checked_add(r.w).is_none_or(|x| x > img.w)
        || r.y.checked_add(r.h).is_none_or(|y| y > img.h)
    {
        return Err(format!(
            "crop ({},{} {}x{}) outside {}x{}",
            r.x, r.y, r.w, r.h, img.w, img.h
        ));
    }
    let mut pixels = Vec::with_capacity((r.w * r.h * 4) as usize);
    for y in r.y..r.y + r.h {
        let start = ((y * img.w + r.x) * 4) as usize;
        pixels.extend_from_slice(&img.rgba[start..start + (r.w * 4) as usize]);
    }
    Ok(Img::from_rgba(r.w, r.h, pixels))
}

/// Every tile with any exact difference is retained, with no ranking or limit.
/// Non-overlapping tiles make coverage count auditable without double counting.
pub fn differing_regions(frame: &Img, reference: &Img) -> Vec<Rect> {
    assert_eq!((frame.w, frame.h), (reference.w, reference.h));
    let mut regions = Vec::new();
    for y in (0..frame.h).step_by(64) {
        for x in (0..frame.w).step_by(64) {
            let r = Rect {
                x,
                y,
                w: 64.min(frame.w - x),
                h: 64.min(frame.h - y),
            };
            if (y..y + r.h)
                .any(|yy| (x..x + r.w).any(|xx| frame.px(xx, yy) != reference.px(xx, yy)))
            {
                regions.push(r);
            }
        }
    }
    regions
}

fn glyph(c: char) -> [u8; 7] {
    match c.to_ascii_uppercase() {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [31, 4, 4, 4, 4, 4, 31],
        'J' => [7, 2, 2, 2, 2, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 25, 21, 19, 17, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [30, 1, 1, 14, 1, 1, 30],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 16, 30, 1, 1, 30],
        '6' => [14, 16, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 1, 14],
        '=' => [0, 0, 31, 0, 31, 0, 0],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '_' => [0, 0, 0, 0, 0, 0, 31],
        '/' => [1, 1, 2, 4, 8, 16, 16],
        ':' => [0, 4, 4, 0, 4, 4, 0],
        '(' => [2, 4, 8, 8, 8, 4, 2],
        ')' => [8, 4, 2, 2, 2, 4, 8],
        ',' => [0, 0, 0, 0, 0, 4, 8],
        ' ' => [0; 7],
        _ => [31, 17, 1, 2, 4, 0, 4],
    }
}

fn blank(w: u32, h: u32) -> Img {
    Img::from_rgba(w, h, vec![24; (w * h * 4) as usize])
}
fn paste(page: &mut Img, img: &Img, x: u32, y: u32) {
    for row in 0..img.h {
        let src = (row * img.w * 4) as usize;
        let dst = (((y + row) * page.w + x) * 4) as usize;
        page.rgba[dst..dst + (img.w * 4) as usize]
            .copy_from_slice(&img.rgba[src..src + (img.w * 4) as usize]);
    }
}

/// Labels are pixels inside the exported cell. Image pixels retain native size.
pub fn labeled(img: &Img, lines: &[String]) -> Img {
    labeled_with_font(img, lines, 1)
}

/// Adjustable embedded label size for visible before/after evidence.
pub fn labeled_with_font(img: &Img, lines: &[String], font: u32) -> Img {
    let w = img.w.max(192 * font);
    let columns = ((w - 8) / (6 * font)).max(1) as usize;
    let rows: Vec<Vec<char>> = lines
        .iter()
        .flat_map(|line| {
            let chars: Vec<_> = line.chars().collect();
            chars
                .chunks(columns)
                .map(|chunk| chunk.to_vec())
                .collect::<Vec<_>>()
        })
        .collect();
    let header = 8 + rows.len() as u32 * 10 * font;
    let mut out = blank(w, header + img.h);
    paste(&mut out, img, 0, header);
    for (row, chars) in rows.iter().enumerate() {
        for (col, c) in chars.iter().enumerate() {
            for (gy, bits) in glyph(*c).iter().enumerate() {
                for gx in 0..5 {
                    if bits & (1 << (4 - gx)) != 0 {
                        for dy in 0..font {
                            for dx in 0..font {
                                let y = 4 + row as u32 * 10 * font + gy as u32 * font + dy;
                                let x = 4 + col as u32 * 6 * font + gx * font + dx;
                                let i = ((y * w + x) * 4) as usize;
                                out.rgba[i..i + 4].copy_from_slice(&[230, 230, 230, 255]);
                            }
                        }
                    }
                }
            }
        }
    }
    out
}

pub fn label_lines(
    mode: &str,
    scale: u32,
    checkpoint: &str,
    tick: u64,
    frame: Option<u64>,
) -> Vec<String> {
    vec![
        format!("MODE={mode} SCALE={scale}"),
        format!("CHECKPOINT={checkpoint}"),
        format!(
            "FRAME={} TICK={tick}",
            frame.map_or_else(|| "UNRECORDED".into(), |f| f.to_string())
        ),
    ]
}

/// Paired cells never wrap apart, and the source image pixels remain at 1:1.
pub fn paired(
    left: &Img,
    left_labels: &[String],
    right: &Img,
    right_labels: &[String],
    font: u32,
) -> Img {
    let left = labeled_with_font(left, left_labels, font);
    let right = labeled_with_font(right, right_labels, font);
    let mut page = blank(left.w + right.w + 8, left.h.max(right.h));
    paste(&mut page, &left, 0, 0);
    paste(&mut page, &right, left.w + 8, 0);
    page
}

pub fn matrix(cap: &Capture, checkpoint: &str, tick: u64) -> Img {
    let mut top = vec![labeled(
        &cap.cpu,
        &label_lines("CPU1 REFERENCE", 1, checkpoint, tick, cap.frame),
    )];
    let mut diffs = Vec::new();
    for scale in [1, 3] {
        let mode = format!("HW{scale}");
        if let Some((_, hw)) = cap.hw.iter().find(|(s, _)| *s == scale) {
            top.push(labeled(
                hw,
                &label_lines(&mode, scale, checkpoint, tick, cap.frame),
            ));
            let reference = cap.cpu.enlarge(scale);
            if (hw.w, hw.h) == (reference.w, reference.h) {
                let count = hw.diff(&reference, 0, 1, &[], None).differing;
                let overlay = exact_overlay(hw, &reference);
                let display = if scale == 1 {
                    overlay.enlarge(3)
                } else {
                    overlay
                };
                let mut labels =
                    label_lines(&format!("{mode} VS CPU1"), 3, checkpoint, tick, cap.frame);
                labels.push(if count == 0 {
                    "IDENTICAL".into()
                } else {
                    format!("{count} DIFFERING PIXELS (EXACT)")
                });
                diffs.push(labeled(&display, &labels));
            } else {
                diffs.push(labeled(
                    &blank(cap.cpu.w * 3, 16),
                    &[format!(
                        "{mode} SIZE MISMATCH: {}X{} EXPECTED {}X{}",
                        hw.w, hw.h, reference.w, reference.h
                    )],
                ));
            }
        } else {
            let mut lines = label_lines(&mode, scale, checkpoint, tick, cap.frame);
            lines.push("MISSING CAPTURE".into());
            top.push(labeled(&blank(cap.cpu.w * scale, 16), &lines));
        }
    }
    let row_w =
        |row: &[Img]| row.iter().map(|i| i.w).sum::<u32>() + 8 * row.len().saturating_sub(1) as u32;
    let top_h = top.iter().map(|i| i.h).max().unwrap_or(0);
    let diff_h = diffs.iter().map(|i| i.h).max().unwrap_or(0);
    let mut page = blank(row_w(&top).max(row_w(&diffs)), top_h + 8 + diff_h);
    for (row, y) in [(&top, 0), (&diffs, top_h + 8)] {
        let mut x = 0;
        for cell in row {
            paste(&mut page, cell, x, y);
            x += cell.w + 8;
        }
    }
    page
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn matrix_preserves_native_cells_and_enlarges_only_hw1_diff() {
        let cpu = Img::from_rgba(320, 240, vec![100; 320 * 240 * 4]);
        let mut hw1 = cpu.clone();
        hw1.rgba[0] = 101;
        let mut hw3 = cpu.enlarge(3);
        hw3.rgba[0] = 102;
        let cap = Capture {
            frame: None,
            strict: Vec::new(),
            cpu,
            golden: None,
            golden_error: None,
            hw: vec![(1, hw1), (3, hw3)],
            hw_skip: None,
        };
        let page = matrix(&cap, "menu", 123);
        assert_eq!((page.w, page.h), (1928, 1534));
        assert_eq!(page.px(0, 38), [100; 3]);
        assert_eq!(page.px(328, 38), [101, 100, 100]);
        assert_eq!(page.px(656, 38), [102, 100, 100]);
        assert_eq!(page.px(0, 814), [255, 0, 255]);
        assert_eq!(page.px(2, 816), [255, 0, 255]);
        assert_eq!(page.px(3, 814), [25; 3]);
        assert_eq!(page.px(968, 814), [255, 0, 255]);
        assert_eq!(page.px(969, 814), [25; 3]);
        assert!(page.rgba.chunks_exact(4).all(|p| p[3] == 255));
    }
    #[test]
    fn exact_diff_keeps_small_and_displaced_differences() {
        let a = Img::from_rgba(2, 1, vec![10, 20, 30, 255, 30, 20, 10, 255]);
        let b = Img::from_rgba(2, 1, vec![11, 20, 30, 255, 30, 20, 10, 255]);
        let overlay = exact_overlay(&b, &a);
        assert_eq!(overlay.px(0, 0), [255, 0, 255]);
        assert_eq!(overlay.px(1, 0), [7, 5, 2]);
        assert_eq!(b.diff(&a, 24, 1, &[], None).differing, 0);
        assert_eq!(b.diff(&a, 0, 1, &[], None).differing, 1);
    }
    #[test]
    fn crop_inventory_covers_every_difference_including_edges() {
        let a = Img::from_rgba(130, 67, vec![0; 130 * 67 * 4]);
        let mut b = a.clone();
        for (x, y) in [(0, 0), (63, 63), (64, 64), (129, 66)] {
            b.rgba[((y * b.w + x) * 4) as usize] = 1;
        }
        let regions = differing_regions(&b, &a);
        assert_eq!(regions.len(), 3);
        let count: u64 = regions
            .iter()
            .map(|r| {
                crop(&b, *r)
                    .unwrap()
                    .diff(&crop(&a, *r).unwrap(), 0, 1, &[], None)
                    .differing
            })
            .sum();
        assert_eq!(count, b.diff(&a, 0, 1, &[], None).differing);
        assert_eq!(count, 4);
        assert!(crop(
            &a,
            Rect {
                x: 129,
                y: 66,
                w: 2,
                h: 1
            }
        )
        .is_err());
    }
}
