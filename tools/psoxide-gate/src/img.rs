//! Frame images and the comparisons the gate makes between them.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// An RGBA8 frame.
#[derive(Clone, Debug)]
pub struct Img {
    pub w: u32,
    pub h: u32,
    /// `w * h * 4` bytes, alpha forced to 0xFF.
    pub rgba: Vec<u8>,
}

/// A pixel rectangle in the frame's own coordinates (native 1x pixels for
/// journey files; the gate scales them for enlarged frames).
#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    fn scaled(self, s: u32) -> Rect {
        Rect {
            x: self.x * s,
            y: self.y * s,
            w: self.w * s,
            h: self.h * s,
        }
    }

    fn contains(&self, x: u32, y: u32) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// How different two frames may be and still count as equal.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Tolerance {
    /// A pixel differs when any channel differs by more than this.
    #[serde(default)]
    pub channel: u8,
    /// Fraction of compared pixels allowed to differ (0.0 to 1.0).
    #[serde(default)]
    pub fraction: f64,
}

impl Tolerance {
    pub const EXACT: Tolerance = Tolerance {
        channel: 0,
        fraction: 0.0,
    };
}

/// Result of comparing two same-sized frames.
#[derive(Clone, Debug)]
pub struct DiffStats {
    pub compared: u64,
    pub differing: u64,
    pub max_diff: u8,
    pub mean_diff: f64,
    /// Bounding box of the differing pixels, if any.
    pub bbox: Option<Rect>,
    /// Pixels per largest-channel difference value.
    pub hist: Vec<u64>,
}

impl DiffStats {
    pub fn fraction(&self) -> f64 {
        if self.compared == 0 {
            0.0
        } else {
            self.differing as f64 / self.compared as f64
        }
    }

    pub fn within(&self, tol: Tolerance) -> bool {
        self.compared > 0 && self.fraction() <= tol.fraction
    }

    /// Fraction of compared pixels whose largest channel difference exceeds `t`.
    pub fn over(&self, t: usize) -> f64 {
        if self.compared == 0 {
            return 0.0;
        }
        let n: u64 = self.hist.iter().skip(t + 1).sum();
        n as f64 / self.compared as f64
    }

    pub fn describe(&self) -> String {
        format!(
            "{} of {} px differ ({:.3}%), max channel diff {}, mean {:.2}; over 8: {:.2}%, 16: {:.2}%, 32: {:.2}%",
            self.differing,
            self.compared,
            self.fraction() * 100.0,
            self.max_diff,
            self.mean_diff,
            self.over(8) * 100.0,
            self.over(16) * 100.0,
            self.over(32) * 100.0,
        )
    }
}

impl Img {
    pub fn from_rgba(w: u32, h: u32, mut rgba: Vec<u8>) -> Img {
        for px in rgba.chunks_exact_mut(4) {
            px[3] = 0xFF;
        }
        Img { w, h, rgba }
    }

    pub fn px(&self, x: u32, y: u32) -> [u8; 3] {
        let i = ((y * self.w + x) * 4) as usize;
        [self.rgba[i], self.rgba[i + 1], self.rgba[i + 2]]
    }

    /// Nearest-neighbour enlargement by an integer factor.
    pub fn enlarge(&self, s: u32) -> Img {
        let (w, h) = (self.w * s, self.h * s);
        let mut out = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            let sy = y / s;
            for x in 0..w {
                let sx = x / s;
                let si = ((sy * self.w + sx) * 4) as usize;
                let di = ((y * w + x) * 4) as usize;
                out[di..di + 4].copy_from_slice(&self.rgba[si..si + 4]);
            }
        }
        Img { w, h, rgba: out }
    }

    /// Fraction of pixels that are the single most common colour.
    pub fn dominant_fraction(&self) -> f64 {
        use std::collections::HashMap;
        let mut counts: HashMap<u32, u32> = HashMap::new();
        for px in self.rgba.chunks_exact(4) {
            let key = u32::from(px[0]) << 16 | u32::from(px[1]) << 8 | u32::from(px[2]);
            *counts.entry(key).or_insert(0) += 1;
        }
        let total = (self.w * self.h).max(1);
        f64::from(counts.values().copied().max().unwrap_or(0)) / f64::from(total)
    }

    /// Fraction of pixels whose largest channel is below `level`.
    pub fn dark_fraction(&self, level: u8) -> f64 {
        let n = self
            .rgba
            .chunks_exact(4)
            .filter(|p| p[0].max(p[1]).max(p[2]) < level)
            .count();
        n as f64 / f64::from((self.w * self.h).max(1))
    }

    /// Compare against `other`, skipping every pixel inside a `mask` rect
    /// (rects are given at this image's resolution divided by `scale`).
    /// With `only` set, compare just those regions instead.
    pub fn diff(
        &self,
        other: &Img,
        channel: u8,
        scale: u32,
        mask: &[Rect],
        only: Option<&[Rect]>,
    ) -> DiffStats {
        self.diff_near(other, channel, 0, scale, mask, only)
    }

    /// Largest channel difference between this pixel and the closest-coloured
    /// pixel of `other` within `radius` (Chebyshev). Radius 0 is the plain
    /// per-pixel difference. A hardware frame drawn at a higher resolution
    /// puts edges a fraction of a native pixel away from where the CPU frame
    /// enlarged puts them; this forgives that and nothing else, since a
    /// wrong colour has no match nearby.
    fn pixel_diff(&self, other: &Img, x: u32, y: u32, radius: u32) -> u8 {
        let a = self.px(x, y);
        let dist = |b: [u8; 3]| (0..3).map(|c| a[c].abs_diff(b[c])).max().unwrap_or(0);
        let d0 = dist(other.px(x, y));
        if radius == 0 || d0 == 0 {
            return d0;
        }
        let mut best = d0;
        let (x0, x1) = (x.saturating_sub(radius), (x + radius).min(self.w - 1));
        let (y0, y1) = (y.saturating_sub(radius), (y + radius).min(self.h - 1));
        for ny in y0..=y1 {
            for nx in x0..=x1 {
                best = best.min(dist(other.px(nx, ny)));
                if best == 0 {
                    return 0;
                }
            }
        }
        best
    }

    pub fn diff_near(
        &self,
        other: &Img,
        channel: u8,
        radius: u32,
        scale: u32,
        mask: &[Rect],
        only: Option<&[Rect]>,
    ) -> DiffStats {
        assert_eq!(
            (self.w, self.h),
            (other.w, other.h),
            "diff of unequal sizes"
        );
        let mask: Vec<Rect> = mask.iter().map(|r| r.scaled(scale)).collect();
        let only: Option<Vec<Rect>> = only.map(|rs| rs.iter().map(|r| r.scaled(scale)).collect());
        let mut stats = DiffStats {
            compared: 0,
            differing: 0,
            max_diff: 0,
            mean_diff: 0.0,
            bbox: None,
            hist: vec![0; 256],
        };
        let mut sum = 0u64;
        let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for y in 0..self.h {
            for x in 0..self.w {
                if mask.iter().any(|r| r.contains(x, y)) {
                    continue;
                }
                if let Some(only) = &only {
                    if !only.iter().any(|r| r.contains(x, y)) {
                        continue;
                    }
                }
                stats.compared += 1;
                let d = self.pixel_diff(other, x, y, radius);
                sum += u64::from(d);
                stats.hist[usize::from(d)] += 1;
                if d > stats.max_diff {
                    stats.max_diff = d;
                }
                if d > channel {
                    stats.differing += 1;
                    x0 = x0.min(x);
                    y0 = y0.min(y);
                    x1 = x1.max(x);
                    y1 = y1.max(y);
                }
            }
        }
        if stats.compared > 0 {
            stats.mean_diff = sum as f64 / stats.compared as f64;
        }
        if stats.differing > 0 {
            stats.bbox = Some(Rect {
                x: x0,
                y: y0,
                w: x1 - x0 + 1,
                h: y1 - y0 + 1,
            });
        }
        stats
    }

    pub fn save_png(&self, path: &Path) -> Result<(), String> {
        use image::codecs::png::{CompressionType, FilterType, PngEncoder};
        use image::ImageEncoder;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }
        let file =
            std::fs::File::create(path).map_err(|e| format!("create {}: {e}", path.display()))?;
        let mut rgb = Vec::with_capacity((self.w * self.h * 3) as usize);
        for px in self.rgba.chunks_exact(4) {
            rgb.extend_from_slice(&px[..3]);
        }
        PngEncoder::new_with_quality(
            std::io::BufWriter::new(file),
            CompressionType::Best,
            FilterType::Adaptive,
        )
        .write_image(&rgb, self.w, self.h, image::ExtendedColorType::Rgb8)
        .map_err(|e| format!("write {}: {e}", path.display()))
    }

    pub fn load_png(path: &Path) -> Result<Img, String> {
        if path.extension().is_some_and(|ext| ext == "ppm") {
            let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
            return Self::load_ppm(&bytes);
        }
        let img = image::open(path)
            .map_err(|e| format!("read {}: {e}", path.display()))?
            .to_rgba8();
        let (w, h) = img.dimensions();
        Ok(Img::from_rgba(w, h, img.into_raw()))
    }

    /// Retained frontend P6 RGB capture, preserving every source pixel.
    fn load_ppm(bytes: &[u8]) -> Result<Img, String> {
        fn token<'a>(bytes: &'a [u8], at: &mut usize) -> Result<&'a str, String> {
            loop {
                while bytes.get(*at).is_some_and(u8::is_ascii_whitespace) {
                    *at += 1;
                }
                if bytes.get(*at) != Some(&b'#') {
                    break;
                }
                while bytes.get(*at).is_some_and(|b| *b != b'\n') {
                    *at += 1;
                }
            }
            let start = *at;
            while bytes.get(*at).is_some_and(|b| !b.is_ascii_whitespace()) {
                *at += 1;
            }
            std::str::from_utf8(&bytes[start..*at]).map_err(|e| e.to_string())
        }
        let mut at = 0;
        if token(bytes, &mut at)? != "P6" {
            return Err("capture must be binary P6 PPM".into());
        }
        let w: u32 = token(bytes, &mut at)?
            .parse()
            .map_err(|_| "invalid PPM width")?;
        let h: u32 = token(bytes, &mut at)?
            .parse()
            .map_err(|_| "invalid PPM height")?;
        if token(bytes, &mut at)? != "255" {
            return Err("capture PPM must use 8-bit RGB".into());
        }
        if !bytes.get(at).is_some_and(u8::is_ascii_whitespace) {
            return Err("missing PPM separator".into());
        }
        if bytes.get(at..at + 2) == Some(b"\r\n") {
            at += 2;
        } else {
            at += 1;
        }
        let count = w
            .checked_mul(h)
            .and_then(|n| n.checked_mul(3))
            .ok_or("PPM size overflow")? as usize;
        if w == 0 || h == 0 || bytes.len() - at != count {
            return Err("PPM pixel length mismatch".into());
        }
        let mut rgba = Vec::with_capacity(count / 3 * 4);
        for rgb in bytes[at..].chunks_exact(3) {
            rgba.extend_from_slice(rgb);
            rgba.push(255);
        }
        Ok(Self::from_rgba(w, h, rgba))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppm_parser_retains_whitespace_pixels_and_rejects_truncation() {
        let bytes = b"P6\n# retained capture\n2 1\n255\n\x20\x0a\x0d\xff\x00\x01";
        let image = Img::load_ppm(bytes).unwrap();
        assert_eq!((image.w, image.h), (2, 1));
        assert_eq!(image.px(0, 0), [32, 10, 13]);
        assert_eq!(image.px(1, 0), [255, 0, 1]);
        assert!(Img::load_ppm(&bytes[..bytes.len() - 1]).is_err());
        assert!(Img::load_ppm(b"P6\n0 1\n255\n").is_err());
    }

    fn solid(w: u32, h: u32, rgb: [u8; 3]) -> Img {
        let mut v = Vec::new();
        for _ in 0..w * h {
            v.extend_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
        }
        Img::from_rgba(w, h, v)
    }

    #[test]
    fn enlarge_repeats_each_pixel() {
        let mut a = solid(2, 1, [0, 0, 0]);
        a.rgba[4..7].copy_from_slice(&[9, 8, 7]);
        let b = a.enlarge(3);
        assert_eq!((b.w, b.h), (6, 3));
        assert_eq!(b.px(2, 2), [0, 0, 0]);
        assert_eq!(b.px(3, 0), [9, 8, 7]);
        assert_eq!(b.px(5, 2), [9, 8, 7]);
    }

    #[test]
    fn diff_counts_and_masks() {
        let a = solid(4, 4, [10, 10, 10]);
        let mut b = a.clone();
        b.rgba[0..3].copy_from_slice(&[30, 10, 10]);
        let s = a.diff(&b, 0, 1, &[], None);
        assert_eq!((s.compared, s.differing, s.max_diff), (16, 1, 20));
        let masked = a.diff(
            &b,
            0,
            1,
            &[Rect {
                x: 0,
                y: 0,
                w: 1,
                h: 1,
            }],
            None,
        );
        assert_eq!((masked.compared, masked.differing), (15, 0));
        assert_eq!(a.diff(&b, 20, 1, &[], None).differing, 0);
    }

    #[test]
    fn dominant_fraction_sees_flat_frames() {
        assert_eq!(solid(8, 8, [0, 0, 0]).dominant_fraction(), 1.0);
        let mut a = solid(8, 8, [0, 0, 0]);
        a.rgba[0..3].copy_from_slice(&[255, 0, 0]);
        assert!(a.dominant_fraction() < 1.0);
    }

    #[test]
    fn a_fully_masked_frame_cannot_pass() {
        let a = solid(2, 2, [0, 0, 0]);
        let d = a.diff(
            &a,
            0,
            1,
            &[Rect {
                x: 0,
                y: 0,
                w: 2,
                h: 2,
            }],
            None,
        );
        assert_eq!(d.compared, 0);
        assert!(!d.within(Tolerance::EXACT));
    }
}
