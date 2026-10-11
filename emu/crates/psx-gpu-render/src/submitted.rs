//! Bounded opt-in evidence of actual submitted hardware candidates, never winners.
use crate::pipeline::HwVertex;
use crate::translator::TranslatedFrame;
use std::collections::VecDeque;

const LIMIT: usize = 4 * 1024 * 1024;
const REGION_LIMIT: usize = 512 * 1024;

pub(crate) fn intersects(vertices: &[HwVertex], roi: [u16; 4]) -> bool {
    if vertices.is_empty() {
        return false;
    }
    let min_x = vertices.iter().map(|v| v.pos[0]).min().unwrap() as i32;
    let max_x = vertices.iter().map(|v| v.pos[0]).max().unwrap() as i32;
    let min_y = vertices.iter().map(|v| v.pos[1]).min().unwrap() as i32;
    let max_y = vertices.iter().map(|v| v.pos[1]).max().unwrap() as i32;
    max_x >= roi[0] as i32
        && min_x <= roi[2] as i32
        && max_y >= roi[1] as i32
        && min_y <= roi[3] as i32
}

pub(crate) struct SubmittedTrace {
    pub roi: [u16; 4],
    pub tick: u64,
    pub epoch: u32,
    sequence: u64,
    frames: VecDeque<Vec<u8>>,
    bytes: usize,
    truncated: bool,
}
fn word(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}
fn vertex(out: &mut Vec<u8>, v: &HwVertex) {
    for n in v.pos {
        out.extend_from_slice(&n.to_le_bytes());
    }
    out.extend_from_slice(&v.color);
    for n in v.uv {
        out.extend_from_slice(&n.to_le_bytes());
    }
    word(out, v.flags);
    word(out, v.tex_window);
    for plane in v.planes {
        for n in plane {
            word(out, n);
        }
    }
}
impl SubmittedTrace {
    pub fn new(roi: [u16; 4]) -> Self {
        Self {
            roi,
            tick: 0,
            epoch: 0,
            sequence: 0,
            frames: VecDeque::new(),
            bytes: 0,
            truncated: false,
        }
    }
    fn retain(&mut self, bytes: Vec<u8>) {
        if bytes.is_empty() {
            return;
        }
        if bytes.len() + 4 > LIMIT {
            self.truncated = true;
            return;
        }
        while self.bytes + bytes.len() + 4 > LIMIT {
            self.bytes -= self.frames.pop_front().unwrap().len() + 4;
            self.truncated = true;
        }
        self.bytes += bytes.len() + 4;
        self.frames.push_back(bytes);
    }
    pub fn capture(&mut self, frame: &TranslatedFrame<'_>, scale: u32, texture: &[u16]) {
        self.truncated |= frame.spans_truncated;
        if frame.packet_spans.is_empty() {
            return;
        }
        self.sequence += 1;
        let mut out = Vec::new();
        out.extend_from_slice(&self.tick.to_le_bytes());
        word(&mut out, self.epoch);
        out.extend_from_slice(&self.sequence.to_le_bytes());
        word(&mut out, scale);
        for n in self.roi {
            out.extend_from_slice(&n.to_le_bytes());
        }
        word(&mut out, frame.packet_spans.len() as u32);
        let mut regions: Vec<[u16; 4]> = Vec::new();
        let mut region_bytes = 0usize;
        let mut region_truncated = false;
        for span in frame.packet_spans {
            word(&mut out, span.packet_index);
            word(&mut out, span.fifo.len() as u32);
            for n in &span.fifo {
                word(&mut out, *n);
            }
            for n in span.env {
                word(&mut out, n as u32);
            }
            word(&mut out, span.start as u32);
            word(&mut out, span.end as u32);
            for v in &frame.vertices[span.start..span.end] {
                vertex(&mut out, v);
                if v.flags & (1 << 22) != 0 {
                    let depth = (v.flags >> 5) & 3;
                    let width = match depth {
                        0 => 64,
                        1 => 128,
                        _ => 256,
                    };
                    let page = [
                        ((v.flags & 15) * 64) as u16,
                        (((v.flags >> 4) & 1) * 256) as u16,
                        width,
                        256,
                    ];
                    let clut = [
                        (((v.flags >> 7) & 63) * 16) as u16,
                        ((v.flags >> 13) & 511) as u16,
                        if depth == 0 { 16 } else { 256 },
                        1,
                    ];
                    for region in [Some(page), (depth < 2).then_some(clut)]
                        .into_iter()
                        .flatten()
                    {
                        if regions.contains(&region) {
                            continue;
                        }
                        let bytes = region[2] as usize * region[3] as usize * 2;
                        if regions.len() >= 16 || region_bytes + bytes > REGION_LIMIT {
                            region_truncated = true;
                            continue;
                        }
                        region_bytes += bytes;
                        regions.push(region);
                    }
                }
            }
            let runs: Vec<_> = frame
                .runs
                .iter()
                .filter(|r| {
                    (r.start as usize) < span.end
                        && (r.start as usize + r.count as usize) > span.start
                })
                .collect();
            word(&mut out, runs.len() as u32);
            for run in runs {
                word(&mut out, run.kind as u32);
                word(&mut out, run.start);
                word(&mut out, run.count);
                for n in run.clip {
                    out.extend_from_slice(&n.to_le_bytes());
                }
            }
        }
        word(&mut out, u32::from(region_truncated));
        word(&mut out, regions.len() as u32);
        for [x, y, w, h] in regions {
            for n in [x, y, w, h] {
                out.extend_from_slice(&n.to_le_bytes());
            }
            for row in 0..h {
                for col in 0..w {
                    let i = ((y as usize + row as usize) % 512) * 1024
                        + (x as usize + col as usize) % 1024;
                    out.extend_from_slice(&texture[i].to_le_bytes());
                }
            }
        }
        self.truncated |= region_truncated;
        self.retain(out);
    }
    pub fn bytes(&self) -> (Vec<u8>, bool) {
        let mut out = b"PXHWSV1\0".to_vec();
        word(&mut out, self.frames.len() as u32);
        for frame in &self.frames {
            word(&mut out, frame.len() as u32);
            out.extend_from_slice(frame);
        }
        (out, self.truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn recent_frames_are_byte_bounded_and_oversize_is_explicit() {
        let mut trace = SubmittedTrace::new([0, 0, 319, 239]);
        trace.retain(vec![1; LIMIT - 4]);
        trace.retain(vec![2; 32]);
        assert!(trace.truncated);
        assert_eq!(trace.frames.len(), 1);
        assert_eq!(trace.bytes, 36);
        trace.retain(vec![3; LIMIT + 1]);
        assert_eq!(trace.bytes, 36);
        for _ in 0..10000 {
            trace.retain(Vec::new());
        }
        assert_eq!(trace.frames.len(), 1);
    }
    #[test]
    fn vertex_encoding_preserves_exact_planes_and_packet_record_bytes() {
        let mut v = HwVertex::default();
        v.pos = [-17, 511];
        v.flags = 0xf1234567;
        v.planes[2] = [1, 2, 0xfffffffe, 4];
        let mut out = Vec::new();
        vertex(&mut out, &v);
        assert_eq!(&out[..2], &(-17i16).to_le_bytes());
        assert_eq!(&out[12..16], &v.flags.to_le_bytes());
        assert_eq!(&out[60..64], &0xfffffffeu32.to_le_bytes());
    }
    #[test]
    fn packet_epoch_binding_and_raw_words_survive_capture() {
        use crate::translator::{PacketSpan, TranslatedFrame};
        let vertices = [HwVertex::default(); 3];
        let spans = [PacketSpan {
            packet_index: 17,
            fifo: vec![0x20000001, 0x11223344],
            start: 0,
            end: 3,
            env: [0, 0, 319, 239, 0, 0],
        }];
        let frame = TranslatedFrame {
            vertices: &vertices,
            runs: &[],
            packet_spans: &spans,
            spans_truncated: false,
        };
        let mut trace = SubmittedTrace::new([0, 0, 319, 239]);
        trace.tick = 123;
        trace.epoch = 0;
        trace.capture(&frame, 3, &[]);
        trace.epoch = 1;
        trace.capture(&frame, 3, &[]);
        assert_eq!(&trace.frames[0][0..8], &123u64.to_le_bytes());
        assert_eq!(&trace.frames[0][8..12], &0u32.to_le_bytes());
        assert_eq!(&trace.frames[1][8..12], &1u32.to_le_bytes());
        // Fixed header is 36 bytes; then index, word count, exact GP0 words.
        assert_eq!(&trace.frames[0][36..40], &17u32.to_le_bytes());
        assert_eq!(&trace.frames[0][44..48], &0x20000001u32.to_le_bytes());
        assert_eq!(&trace.frames[0][48..52], &0x11223344u32.to_le_bytes());
        assert!(!trace.truncated);
    }

    #[test]
    fn textured_sampler_regions_are_actual_and_region_omissions_explicit() {
        use crate::translator::{PacketSpan, TranslatedFrame};
        let mut v = HwVertex::default();
        v.flags = (1 << 22) | 5 | (3 << 7) | (480 << 13);
        let vertices = [v; 3];
        let spans = [PacketSpan {
            packet_index: 17,
            fifo: vec![0x24000000],
            start: 0,
            end: 3,
            env: [0, 0, 319, 239, 0, 0],
        }];
        let frame = TranslatedFrame {
            vertices: &vertices,
            runs: &[],
            packet_spans: &spans,
            spans_truncated: false,
        };
        let mut texture = vec![0u16; 1024 * 512];
        texture[11 * 1024 + 355] = 0x1234;
        texture[480 * 1024 + 48] = 0xabcd;
        let mut trace = SubmittedTrace::new([0, 0, 319, 239]);
        trace.capture(&frame, 3, &texture);
        let bytes = &trace.frames[0];
        // Header + packet index/count/word + env + span bounds + 3 vertices + run count.
        let tail = 36 + 12 + 24 + 8 + 3 * 84 + 4;
        assert_eq!(
            u32::from_le_bytes(bytes[tail..tail + 4].try_into().unwrap()),
            0
        );
        assert_eq!(
            u32::from_le_bytes(bytes[tail + 4..tail + 8].try_into().unwrap()),
            2
        );
        let page = tail + 8;
        assert_eq!(
            &bytes[page..page + 8],
            &[320u16, 0, 64, 256].map(u16::to_le_bytes).concat()
        );
        let sample = page + 8 + (11 * 64 + 35) * 2;
        assert_eq!(&bytes[sample..sample + 2], &0x1234u16.to_le_bytes());
        let clut = page + 8 + 64 * 256 * 2;
        assert_eq!(
            &bytes[clut..clut + 8],
            &[48u16, 480, 16, 1].map(u16::to_le_bytes).concat()
        );
        assert_eq!(&bytes[clut + 8..clut + 10], &0xabcdu16.to_le_bytes());
        // Five distinct 15bpp pages exceed the 512 KiB sampler-region budget.
        let mut many = vec![HwVertex::default(); 15];
        for (i, triangle) in many.chunks_mut(3).enumerate() {
            for v in triangle {
                v.flags = (1 << 22) | (2 << 5) | i as u32;
            }
        }
        let spans = [PacketSpan {
            packet_index: 18,
            fifo: vec![0x24000000],
            start: 0,
            end: 15,
            env: [0, 0, 319, 239, 0, 0],
        }];
        let frame = TranslatedFrame {
            vertices: &many,
            runs: &[],
            packet_spans: &spans,
            spans_truncated: false,
        };
        trace.capture(&frame, 3, &texture);
        assert!(trace.truncated);
    }
    #[test]
    fn empty_capture_retains_no_entries_and_reports_omitted_spans() {
        let frame = TranslatedFrame {
            vertices: &[],
            runs: &[],
            packet_spans: &[],
            spans_truncated: false,
        };
        let mut trace = SubmittedTrace::new([0, 0, 319, 239]);
        for _ in 0..10000 {
            trace.capture(&frame, 3, &[]);
        }
        assert!(trace.frames.is_empty());
        assert_eq!(trace.bytes, 0);
        let omitted = TranslatedFrame {
            spans_truncated: true,
            ..frame
        };
        trace.capture(&omitted, 3, &[]);
        assert!(trace.truncated);
    }

    #[test]
    fn roi_uses_actual_submitted_geometry() {
        let mut v = HwVertex::default();
        v.pos = [120, 85];
        assert!(intersects(&[v], [118, 83, 122, 87]));
        assert!(!intersects(&[v], [118, 323, 122, 327]));
        assert!(!intersects(&[], [0, 0, 1023, 511]));
    }
}
