//! A compact, chunked disc format for the browser build.
//!
//! A raw PlayStation disc image spends most of its bytes on things a reader can
//! compute: the sync pattern, the sector header, the EDC and, on Mode 2 Form 1
//! sectors, 276 bytes of ECC. What is left, the user data, compresses by about
//! a third even when it is XA ADPCM. This format keeps only what cannot be
//! regenerated and deflates it in independent chunks, so a page can fetch the
//! few chunks a game touches with HTTP Range requests instead of the image.
//!
//! # Files
//!
//! * the **index** (`NAME.pxi`): a small header, then one deflate stream with
//!   the length and checksum of every chunk, the kind of every sector, the four
//!   subheader bytes of every sector and a short list of sectors kept
//!   verbatim. It is downloaded whole, once;
//! * the **payload** (`NAME.pxw`, or `NAME.pxw.001`, `.002`... when a host caps
//!   file size): every chunk's bytes back to back, chunk 0 first. The index
//!   says where each one starts.
//!
//! # Sectors
//!
//! A chunk holds [`CHUNK_SECTORS`] consecutive sectors (the last one may hold
//! fewer). Each sector is one of four kinds, which fix how many bytes it
//! contributes and how the rest is rebuilt:
//!
//! | kind | stored | rebuilt |
//! |---|---|---|
//! | `Form1` | 2048 user bytes | sync, header, subheader twice, EDC, P and Q parity |
//! | `Form2` | 2324 user bytes | sync, header, subheader twice, EDC |
//! | `Form2NoEdc` | 2324 user bytes | as `Form2` with the EDC field zero |
//! | `Verbatim` | all 2352 bytes | nothing |
//!
//! The header is the sector's absolute time (its index plus `lba_bias`
//! frames, in BCD) and mode 2. The packer rebuilds every sector and falls back
//! to `Verbatim` for any it cannot reproduce byte for byte, so a disc that
//! mastering tools made unusually still round-trips exactly.
//!
//! The chunk's stored bytes are one raw deflate stream; a chunk that deflate
//! cannot shrink is stored as is, which the reader recognises by length alone.
//! Its checksum is [`psx_iso::sector_edc`] of the inflated bytes.

use std::fmt;
use std::ops::Range;

mod pack;
pub use pack::{verify, Packer, PayloadFiles, Stats};

/// Bytes per raw sector.
pub const SECTOR: usize = psx_iso::SECTOR_BYTES;
/// Sectors per chunk.
pub const CHUNK_SECTORS: u32 = 32;

const MAGIC: &[u8; 4] = b"PXWD";
const VERSION: u8 = 1;
/// Magic, version, chunk sectors, sector count, bias, hash, part size, payload
/// size, boot chunks, body length.
const HEADER_BYTES: usize = 4 + 1 + 1 + 4 + 4 + 8 + 8 + 8 + 4 + 4;

/// A format or integrity failure, with a human-readable cause.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Error(pub String);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Error {}

fn err<T>(message: impl Into<String>) -> Result<T, Error> {
    Err(Error(message.into()))
}

/// How a sector is stored. See the module docs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
#[non_exhaustive]
pub enum Kind {
    /// Mode 2 Form 1: 2048 user bytes.
    Form1 = 0,
    /// Mode 2 Form 2 with a computed EDC: 2324 user bytes.
    Form2 = 1,
    /// Mode 2 Form 2 whose EDC field is zero: 2324 user bytes.
    Form2NoEdc = 2,
    /// Any other sector: all 2352 bytes.
    Verbatim = 3,
}

impl Kind {
    fn from_u8(value: u8) -> Result<Kind, Error> {
        Ok(match value {
            0 => Kind::Form1,
            1 => Kind::Form2,
            2 => Kind::Form2NoEdc,
            3 => Kind::Verbatim,
            other => return err(format!("unknown sector kind {other}")),
        })
    }

    /// Bytes this kind contributes to its chunk before compression.
    #[must_use]
    pub fn payload_len(self) -> usize {
        match self {
            Kind::Form1 => 2048,
            Kind::Form2 | Kind::Form2NoEdc => 2324,
            Kind::Verbatim => SECTOR,
        }
    }
}

/// Header bytes 12..16 of the sector at `index`: its absolute time in BCD,
/// then mode 2.
pub(crate) fn msf_header(index: u32, bias: u32) -> [u8; 4] {
    let (m, s, f) = psx_iso::frames_to_msf(index.saturating_add(bias));
    [
        psx_iso::bin_to_bcd(m),
        psx_iso::bin_to_bcd(s),
        psx_iso::bin_to_bcd(f),
        2,
    ]
}

/// Build the raw sector a `kind` describes from its stored bytes.
pub(crate) fn rebuild(
    kind: Kind,
    header: [u8; 4],
    subheader: [u8; 4],
    payload: &[u8],
) -> [u8; SECTOR] {
    let mut out = [0u8; SECTOR];
    if kind == Kind::Verbatim {
        out.copy_from_slice(payload);
        return out;
    }
    out[1..11].fill(0xff);
    out[12..16].copy_from_slice(&header);
    out[16..20].copy_from_slice(&subheader);
    out[20..24].copy_from_slice(&subheader);
    match kind {
        Kind::Form1 => {
            out[24..24 + 2048].copy_from_slice(payload);
            psx_iso::protect_mode2_form1(&mut out);
        }
        Kind::Form2 => {
            out[24..24 + 2324].copy_from_slice(payload);
            psx_iso::protect_mode2_form2(&mut out);
        }
        _ => out[24..24 + 2324].copy_from_slice(payload),
    }
    out
}

/// The parsed index of a compact disc.
#[derive(Clone, Debug)]
pub struct Index {
    sector_count: u32,
    lba_bias: u32,
    image_hash: u64,
    part_bytes: u64,
    payload_bytes: u64,
    boot_chunks: u32,
    chunk_offsets: Vec<u64>,
    chunk_crcs: Vec<u32>,
    kinds: Vec<Kind>,
    subheaders: Vec<[u8; 4]>,
    /// `(sector, header bytes 12..16)` of every verbatim sector, ascending.
    verbatim_headers: Vec<(u32, [u8; 4])>,
}

/// One inflated chunk: the stored bytes of its sectors, back to back.
pub struct Chunk {
    first_sector: u32,
    payload: Box<[u8]>,
}

impl Chunk {
    /// Index of the first sector in the chunk.
    #[must_use]
    pub fn first_sector(&self) -> u32 {
        self.first_sector
    }

    /// Bytes held in memory.
    #[must_use]
    pub fn len(&self) -> usize {
        self.payload.len()
    }

    /// Whether the chunk is empty (never, for a parsed index).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

impl Index {
    /// Number of sectors in the image.
    #[must_use]
    pub fn sector_count(&self) -> u32 {
        self.sector_count
    }

    /// Image length in bytes.
    #[must_use]
    pub fn image_bytes(&self) -> u64 {
        u64::from(self.sector_count) * SECTOR as u64
    }

    /// Frames added to a sector's index to get the time in its header.
    #[must_use]
    pub fn lba_bias(&self) -> u32 {
        self.lba_bias
    }

    /// FNV-1a 64 of the whole raw image, the key input tapes and saves use.
    #[must_use]
    pub fn image_hash(&self) -> u64 {
        self.image_hash
    }

    /// Bytes per payload file, 0 when the payload is one file.
    #[must_use]
    pub fn part_bytes(&self) -> u64 {
        self.part_bytes
    }

    /// Bytes in the payload, over all its files.
    #[must_use]
    pub fn payload_bytes(&self) -> u64 {
        self.payload_bytes
    }

    /// Chunks a boot reads before its first frame.
    #[must_use]
    pub fn boot_chunks(&self) -> u32 {
        self.boot_chunks
    }

    /// Number of chunks.
    #[must_use]
    pub fn chunk_count(&self) -> u32 {
        (self.chunk_offsets.len() - 1) as u32
    }

    /// The chunk that holds `sector`.
    #[must_use]
    pub fn chunk_of(&self, sector: u32) -> u32 {
        sector / CHUNK_SECTORS
    }

    /// Sectors a chunk holds.
    #[must_use]
    pub fn chunk_sectors(&self, chunk: u32) -> Range<u32> {
        let first = chunk * CHUNK_SECTORS;
        first..(first + CHUNK_SECTORS).min(self.sector_count)
    }

    /// Where a chunk's stored bytes sit in the payload.
    #[must_use]
    pub fn chunk_range(&self, chunk: u32) -> Range<u64> {
        self.chunk_offsets[chunk as usize]..self.chunk_offsets[chunk as usize + 1]
    }

    /// Bytes of the payload a boot reads up front (the first `boot_chunks`).
    #[must_use]
    pub fn boot_bytes(&self) -> u64 {
        self.chunk_offsets[self.boot_chunks.min(self.chunk_count()) as usize]
    }

    /// Bytes 12..20 of a sector (header, then the first subheader bytes):
    /// what the drive reports after a seek, available without its chunk.
    #[must_use]
    pub fn head8(&self, sector: u32) -> Option<[u8; 8]> {
        let kind = *self.kinds.get(sector as usize)?;
        let header = if kind == Kind::Verbatim {
            let at = self
                .verbatim_headers
                .binary_search_by_key(&sector, |&(s, _)| s)
                .ok()?;
            self.verbatim_headers[at].1
        } else {
            msf_header(sector, self.lba_bias)
        };
        let sub = self.subheaders[sector as usize];
        Some([
            header[0], header[1], header[2], header[3], sub[0], sub[1], sub[2], sub[3],
        ])
    }

    /// Stored bytes a chunk inflates to.
    fn chunk_payload_len(&self, chunk: u32) -> usize {
        self.chunk_sectors(chunk)
            .map(|s| self.kinds[s as usize].payload_len())
            .sum()
    }

    /// Inflate the stored bytes of `chunk` and check them against the index.
    ///
    /// # Errors
    ///
    /// When `chunk` is past the end of the disc or the stored bytes do not
    /// inflate to what the index expects.
    pub fn inflate(&self, chunk: u32, stored: &[u8]) -> Result<Chunk, Error> {
        if chunk >= self.chunk_count() {
            return err(format!("chunk {chunk} is past the end of the disc"));
        }
        let want = self.chunk_payload_len(chunk);
        let range = self.chunk_range(chunk);
        if stored.len() as u64 != range.end - range.start {
            return err(format!(
                "chunk {chunk}: {} bytes where the index says {}",
                stored.len(),
                range.end - range.start
            ));
        }
        let payload: Vec<u8> = if stored.len() == want {
            stored.to_vec()
        } else {
            miniz_oxide::inflate::decompress_to_vec_with_limit(stored, want)
                .map_err(|e| Error(format!("chunk {chunk}: inflate failed ({:?})", e.status)))?
        };
        if payload.len() != want {
            return err(format!(
                "chunk {chunk}: inflated to {} bytes, expected {want}",
                payload.len()
            ));
        }
        if psx_iso::sector_edc(&payload) != self.chunk_crcs[chunk as usize] {
            return err(format!("chunk {chunk}: checksum mismatch"));
        }
        Ok(Chunk {
            first_sector: chunk * CHUNK_SECTORS,
            payload: payload.into_boxed_slice(),
        })
    }

    /// Rebuild one raw sector from its inflated chunk.
    ///
    /// # Panics
    ///
    /// When `sector` is not in `chunk`.
    #[must_use]
    pub fn sector(&self, chunk: &Chunk, sector: u32) -> [u8; SECTOR] {
        let kind = self.kinds[sector as usize];
        let mut at = 0usize;
        for s in chunk.first_sector..sector {
            at += self.kinds[s as usize].payload_len();
        }
        let header = if kind == Kind::Verbatim {
            let found = self
                .verbatim_headers
                .binary_search_by_key(&sector, |&(s, _)| s)
                .expect("verbatim sector has a header record");
            self.verbatim_headers[found].1
        } else {
            msf_header(sector, self.lba_bias)
        };
        rebuild(
            kind,
            header,
            self.subheaders[sector as usize],
            &chunk.payload[at..at + kind.payload_len()],
        )
    }

    /// Parse an index file.
    ///
    /// # Errors
    ///
    /// When `bytes` is not a compact disc index of a version this reader knows,
    /// or the index is truncated or inconsistent.
    pub fn parse(bytes: &[u8]) -> Result<Index, Error> {
        if bytes.len() < HEADER_BYTES || &bytes[..4] != MAGIC {
            return err("not a compact disc index");
        }
        if bytes[4] != VERSION {
            return err(format!("compact disc index version {}", bytes[4]));
        }
        let mut r = Reader {
            bytes,
            at: 5,
            context: "index header",
        };
        let chunk_sectors = u32::from(r.u8()?);
        if chunk_sectors != CHUNK_SECTORS {
            return err(format!("{chunk_sectors} sectors per chunk"));
        }
        let sector_count = r.u32()?;
        let lba_bias = r.u32()?;
        let image_hash = r.u64()?;
        let part_bytes = r.u64()?;
        let payload_bytes = r.u64()?;
        let boot_chunks = r.u32()?;
        let body_len = r.u32()? as usize;
        let body =
            miniz_oxide::inflate::decompress_to_vec_with_limit(&bytes[HEADER_BYTES..], body_len)
                .map_err(|e| Error(format!("index body: inflate failed ({:?})", e.status)))?;
        if body.len() != body_len {
            return err("index body has the wrong length");
        }
        let chunks = sector_count.div_ceil(CHUNK_SECTORS) as usize;
        let mut b = Reader {
            bytes: &body,
            at: 0,
            context: "index body",
        };
        let mut chunk_offsets = Vec::with_capacity(chunks + 1);
        let mut offset = 0u64;
        chunk_offsets.push(0);
        for _ in 0..chunks {
            offset += u64::from(b.u32()?);
            chunk_offsets.push(offset);
        }
        if offset != payload_bytes {
            return err("chunk lengths do not add up to the payload size");
        }
        let chunk_crcs = (0..chunks)
            .map(|_| b.u32())
            .collect::<Result<Vec<_>, _>>()?;
        let kinds = b
            .take(sector_count as usize)?
            .iter()
            .map(|&k| Kind::from_u8(k))
            .collect::<Result<Vec<_>, _>>()?;
        let sub_columns = b.take(4 * sector_count as usize)?;
        let n = sector_count as usize;
        let subheaders = (0..n)
            .map(|i| {
                [
                    sub_columns[i],
                    sub_columns[n + i],
                    sub_columns[2 * n + i],
                    sub_columns[3 * n + i],
                ]
            })
            .collect();
        let verbatim_count = b.u32()? as usize;
        let mut verbatim_headers = Vec::with_capacity(verbatim_count);
        for _ in 0..verbatim_count {
            let sector = b.u32()?;
            let mut header = [0u8; 4];
            header.copy_from_slice(b.take(4)?);
            verbatim_headers.push((sector, header));
        }
        let expected = kinds.iter().filter(|&&k| k == Kind::Verbatim).count();
        let sorted = verbatim_headers.windows(2).all(|w| w[0].0 < w[1].0);
        if verbatim_headers.len() != expected
            || !sorted
            || verbatim_headers
                .iter()
                .any(|&(s, _)| kinds.get(s as usize) != Some(&Kind::Verbatim))
        {
            return err("verbatim sector records do not match the sector kinds");
        }
        if boot_chunks as usize > chunks {
            return err("more boot chunks than chunks");
        }
        Ok(Index {
            sector_count,
            lba_bias,
            image_hash,
            part_bytes,
            payload_bytes,
            boot_chunks,
            chunk_offsets,
            chunk_crcs,
            kinds,
            subheaders,
            verbatim_headers,
        })
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    context: &'static str,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let end = self.at.checked_add(n).filter(|&e| e <= self.bytes.len());
        let Some(end) = end else {
            return err(format!("{}: truncated", self.context));
        };
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(
            self.take(4)?.try_into().expect("4 bytes"),
        ))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(
            self.take(8)?.try_into().expect("8 bytes"),
        ))
    }
}

/// FNV-1a 64 of an image, fed in pieces. The same function
/// `emulator_core::GameImageHasher` computes (a test holds them equal).
#[derive(Clone, Copy, Debug)]
pub struct ImageHasher(u64);

impl Default for ImageHasher {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageHasher {
    /// A hasher over no bytes.
    #[must_use]
    pub fn new() -> Self {
        Self(0xcbf2_9ce4_8422_2325)
    }

    /// Continue the hash over `bytes`.
    pub fn update(&mut self, bytes: &[u8]) {
        let mut hash = self.0;
        for &byte in bytes {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
        self.0 = hash;
    }

    /// The hash so far.
    #[must_use]
    pub fn finish(&self) -> u64 {
        self.0
    }
}

/// Name of payload file `n` (1-based) when the payload is split.
#[must_use]
pub fn part_name(base: &str, n: u64) -> String {
    format!("{base}.{n:03}")
}

#[cfg(test)]
mod tests;
