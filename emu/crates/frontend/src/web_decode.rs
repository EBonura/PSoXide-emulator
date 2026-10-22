//! Time-budgeted FLAC decode for the streamed demo disc.
//!
//! The browser has one thread: whatever decodes a 30 MB track also delays the
//! frame, and a late frame starves the CD audio. So the decoder is resumable,
//! and each call is given a wall-clock budget instead of a block count: the
//! shell gives it a generous slice while nothing is running yet (the page is
//! waiting for the disc anyway) and a small one beside a running guest.
//!
//! The decoder also hashes what it produces as it goes. Checking a finished
//! 40 MB track in one go would be a visible hitch in the middle of play; a
//! running hash costs the same work spread over the decode.
//!
//! Compiled for the browser and for native tests; nothing here touches JS.

use std::io::Cursor;
use std::time::Duration;

use web_time::Instant;

const FNV_OFFSET: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

/// Continue an FNV-1a-32 hash over `data`.
pub fn fnv1a32_update(mut hash: u32, data: &[u8]) -> u32 {
    for &b in data {
        hash = (hash ^ b as u32).wrapping_mul(FNV_PRIME);
    }
    hash
}

/// FNV-1a-32, the project's checksum of habit. Matches `web-delivery.py`.
pub fn fnv1a32(data: &[u8]) -> u32 {
    fnv1a32_update(FNV_OFFSET, data)
}

/// A finished decode, before the caller checks it against the manifest.
pub struct Decoded {
    /// Interleaved 16-bit stereo, little-endian: a CD-DA track's raw sectors.
    pub pcm: Vec<u8>,
    /// FNV-1a-32 of `pcm`.
    pub fnv: u32,
    /// Time spent inside [`Decode::pump`], summed over all calls.
    pub busy: Duration,
    /// Time from the first call to the last, which the frame pacing stretches.
    pub wall: Duration,
}

/// A FLAC decode in progress, resumed a budget at a time.
pub struct Decode {
    reader: claxon::FlacReader<Cursor<Vec<u8>>>,
    /// Reused per-block sample buffer.
    buffer: Vec<i32>,
    out: Vec<u8>,
    hash: u32,
    busy: Duration,
    first_pump: Option<Instant>,
}

impl Decode {
    /// Open `flac` for decoding; `capacity` is the expected PCM size.
    pub fn new(flac: Vec<u8>, capacity: usize) -> Result<Self, claxon::Error> {
        Ok(Self {
            reader: claxon::FlacReader::new(Cursor::new(flac))?,
            buffer: Vec::new(),
            out: Vec::with_capacity(capacity),
            hash: FNV_OFFSET,
            busy: Duration::ZERO,
            first_pump: None,
        })
    }

    /// Decode whole blocks until `budget` has elapsed or the stream ends.
    /// Always decodes at least one block, so a zero budget still finishes.
    /// `Some` once the stream is over, whether it ended cleanly or on a
    /// decode error: the caller's size and hash check tells the two apart.
    ///
    /// Whole blocks only: claxon's readers resume cleanly between
    /// fully-consumed blocks, where a sample iterator dropped mid-block
    /// silently discards the block's tail.
    pub fn pump(&mut self, budget: Duration) -> Option<Decoded> {
        let start = Instant::now();
        let first = *self.first_pump.get_or_insert(start);
        let mut frames = self.reader.blocks();
        loop {
            let mark = self.out.len();
            match frames.read_next_or_eof(core::mem::take(&mut self.buffer)) {
                Ok(Some(block)) => {
                    for i in 0..block.duration() {
                        self.out
                            .extend_from_slice(&(block.sample(0, i) as i16).to_le_bytes());
                        self.out
                            .extend_from_slice(&(block.sample(1, i) as i16).to_le_bytes());
                    }
                    self.hash = fnv1a32_update(self.hash, &self.out[mark..]);
                    self.buffer = block.into_buffer();
                }
                Ok(None) | Err(_) => {
                    let busy = self.busy + start.elapsed();
                    return Some(Decoded {
                        pcm: core::mem::take(&mut self.out),
                        fnv: self.hash,
                        busy,
                        wall: first.elapsed(),
                    });
                }
            }
            if start.elapsed() >= budget {
                self.busy += start.elapsed();
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0.4 s of stereo 44.1 kHz (a sine left, a ramp right), encoded by
    /// `flac -8` at the default 4096-sample block size: five blocks, the
    /// last short.
    const SAMPLE_FLAC: &[u8] = include_bytes!("../tests/fixtures/web_decode_sample.flac");

    /// The same stream through claxon's one-shot sample iterator, as the
    /// reference the budgeted path must match byte for byte.
    fn reference() -> Vec<u8> {
        let mut reader = claxon::FlacReader::new(Cursor::new(SAMPLE_FLAC)).unwrap();
        let mut samples = reader.samples();
        let mut out = Vec::new();
        while let (Some(l), Some(r)) = (samples.next(), samples.next()) {
            out.extend_from_slice(&(l.unwrap() as i16).to_le_bytes());
            out.extend_from_slice(&(r.unwrap() as i16).to_le_bytes());
        }
        out
    }

    fn run_to_end(budget: Duration) -> (Decoded, usize) {
        let mut dec = Decode::new(SAMPLE_FLAC.to_vec(), 0).unwrap();
        let mut calls = 0;
        loop {
            calls += 1;
            if let Some(done) = dec.pump(budget) {
                return (done, calls);
            }
            assert!(calls < 100, "decode never finished");
        }
    }

    #[test]
    fn fnv_known_vectors_and_incremental_agree() {
        // Published FNV-1a-32 test vectors.
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
        assert_eq!(fnv1a32(b"foobar"), 0xbf9c_f968);
        let split = fnv1a32_update(fnv1a32(b"foo"), b"bar");
        assert_eq!(split, fnv1a32(b"foobar"));
    }

    #[test]
    fn a_generous_budget_finishes_in_one_call() {
        let (done, calls) = run_to_end(Duration::from_secs(60));
        assert_eq!(calls, 1);
        let want = reference();
        assert!(want.len() > 4096 * 4, "fixture spans several blocks");
        assert_eq!(done.pcm, want);
        assert_eq!(done.fnv, fnv1a32(&want));
        assert!(done.busy <= done.wall);
    }

    #[test]
    fn a_zero_budget_still_makes_progress_one_block_at_a_time() {
        let (done, calls) = run_to_end(Duration::ZERO);
        // Five blocks, then the call that sees the end of the stream.
        assert_eq!(calls, 6);
        let want = reference();
        assert_eq!(done.pcm, want);
        assert_eq!(done.fnv, fnv1a32(&want));
    }

    #[test]
    fn a_corrupt_stream_ends_short_and_fails_the_size_check() {
        let mut bad = SAMPLE_FLAC.to_vec();
        // Break the second frame: the decoder stops there, short of the
        // full length, instead of looping or panicking.
        let at = bad.len() * 2 / 3;
        bad[at] ^= 0xff;
        bad[at + 1] ^= 0xff;
        let mut dec = Decode::new(bad, 0).unwrap();
        let done = loop {
            if let Some(done) = dec.pump(Duration::from_secs(60)) {
                break done;
            }
        };
        assert!(done.pcm.len() < reference().len());
    }
}
