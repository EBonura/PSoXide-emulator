//! `--fetch-trace-log`: the order in which the guest retired its
//! instructions, compressed into sequential runs.
//!
//! The per-line and per-word logs say how often each instruction ran, which
//! is enough to know what is hot but not how the I-cache behaves: a
//! direct-mapped cache misses when two lines of one set are used alternately,
//! and only the order of the fetches says whether they were. With this log a
//! tool can replay the fetches against any other placement of the same code
//! and count the refills without running the emulator again.
//!
//! Format: the 8 bytes `PSXFTRC1`, then a stream of LEB128 varints. A run of
//! `n` sequential instruction words starting at word address `w` (the PC
//! over 4) is `zigzag(w - e) + 1` followed by `n`, where `e` is the word
//! address one past the previous run (0 at the start). A lone `0` marks the
//! start of a route tick. Addresses are the virtual PCs the CPU fetched, so a
//! reader decides which are cached (KUSEG and KSEG0) and canonicalises their
//! aliases itself.

use std::io::{BufWriter, Write};
use std::path::Path;

pub(super) const MAGIC: &[u8; 8] = b"PSXFTRC1";

pub(super) struct FetchTrace {
    writer: BufWriter<std::fs::File>,
    /// The run being built: its first word address and length in words.
    start: u32,
    words: u32,
    /// One past the last word address written.
    end: u32,
}

impl FetchTrace {
    pub(super) fn create(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("mkdir {}: {error}", parent.display()))?;
        }
        let file = std::fs::File::create(path)
            .map_err(|error| format!("create fetch trace {}: {error}", path.display()))?;
        let mut writer = BufWriter::with_capacity(1 << 20, file);
        writer
            .write_all(MAGIC)
            .map_err(|error| format!("write fetch trace {}: {error}", path.display()))?;
        Ok(Self {
            writer,
            start: 0,
            words: 0,
            end: 0,
        })
    }

    /// One retired instruction at `pc`.
    #[inline]
    pub(super) fn record(&mut self, pc: u32) -> std::io::Result<()> {
        let word = pc >> 2;
        if self.words != 0 && word == self.start.wrapping_add(self.words) {
            self.words += 1;
            return Ok(());
        }
        self.flush_run()?;
        self.start = word;
        self.words = 1;
        Ok(())
    }

    /// The start of a route tick.
    pub(super) fn tick(&mut self) -> std::io::Result<()> {
        self.flush_run()?;
        self.writer.write_all(&[0])
    }

    pub(super) fn finish(&mut self) -> std::io::Result<()> {
        self.flush_run()?;
        self.writer.flush()
    }

    fn flush_run(&mut self) -> std::io::Result<()> {
        if self.words == 0 {
            return Ok(());
        }
        let delta = i64::from(self.start) - i64::from(self.end);
        let zigzag = ((delta << 1) ^ (delta >> 63)) as u64;
        let mut bytes = [0u8; 20];
        let mut length = varint(zigzag + 1, &mut bytes);
        length += varint(u64::from(self.words), &mut bytes[length..]);
        self.writer.write_all(&bytes[..length])?;
        self.end = self.start.wrapping_add(self.words);
        self.words = 0;
        Ok(())
    }
}

fn varint(mut value: u64, out: &mut [u8]) -> usize {
    let mut index = 0;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out[index] = byte;
            return index + 1;
        }
        out[index] = byte | 0x80;
        index += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(bytes: &[u8]) -> Vec<Option<(u32, u32)>> {
        assert_eq!(&bytes[..8], MAGIC);
        let mut at = 8;
        let next = |at: &mut usize| {
            let mut value = 0u64;
            let mut shift = 0;
            loop {
                let byte = bytes[*at];
                *at += 1;
                value |= u64::from(byte & 0x7f) << shift;
                shift += 7;
                if byte & 0x80 == 0 {
                    return value;
                }
            }
        };
        let mut out = Vec::new();
        let mut end = 0i64;
        while at < bytes.len() {
            let head = next(&mut at);
            if head == 0 {
                out.push(None);
                continue;
            }
            let zigzag = head - 1;
            let delta = (zigzag >> 1) as i64 ^ -((zigzag & 1) as i64);
            let start = end + delta;
            let words = next(&mut at);
            out.push(Some(((start as u32) << 2, words as u32)));
            end = start + words as i64;
        }
        out
    }

    #[test]
    fn runs_and_ticks_round_trip() {
        let path = std::env::temp_dir().join(format!("fetch-trace-{}.bin", std::process::id()));
        let mut trace = FetchTrace::create(&path).unwrap();
        for pc in [
            0x8001_0000u32,
            0x8001_0004,
            0x8001_0008,
            0x8000_0080,
            0x8000_0084,
        ] {
            trace.record(pc).unwrap();
        }
        trace.tick().unwrap();
        trace.record(0x8000_0088).unwrap();
        trace.record(0xbfc0_0000).unwrap();
        trace.finish().unwrap();
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(
            read(&bytes),
            vec![
                Some((0x8001_0000, 3)),
                Some((0x8000_0080, 2)),
                None,
                Some((0x8000_0088, 1)),
                Some((0xbfc0_0000, 1)),
            ]
        );
    }
}
