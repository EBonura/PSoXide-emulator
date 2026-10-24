//! Writing a compact disc, and proving one against the image it came from.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{
    msf_header, part_name, rebuild, Error, ImageHasher, Index, Kind, CHUNK_SECTORS, HEADER_BYTES,
    MAGIC, SECTOR, VERSION,
};

/// Deflate level for chunks. Level 9 is within two percent of zstd 19 on XA
/// data (see the lane notes) and the pack runs once per release.
const LEVEL: u8 = 9;

/// Sector kind for `raw` at position `index`, plus the stored bytes, or the
/// verbatim fallback when no regeneration reproduces it exactly.
fn classify(index: u32, bias: u32, raw: &[u8; SECTOR]) -> (Kind, [u8; 4]) {
    let sub = [raw[16], raw[17], raw[18], raw[19]];
    let header = msf_header(index, bias);
    let plain = raw[0] == 0
        && raw[1..11].iter().all(|&b| b == 0xff)
        && raw[11] == 0
        && raw[12..16] == header
        && raw[16..20] == raw[20..24];
    if !plain {
        return (Kind::Verbatim, sub);
    }
    let form2 = sub[2] & 0x20 != 0;
    let candidates: &[(Kind, Range)] = if form2 {
        &[
            (Kind::Form2, 24..24 + 2324),
            (Kind::Form2NoEdc, 24..24 + 2324),
        ]
    } else {
        &[(Kind::Form1, 24..24 + 2048)]
    };
    for (kind, range) in candidates {
        if rebuild(*kind, header, sub, &raw[range.clone()]) == *raw {
            return (*kind, sub);
        }
    }
    (Kind::Verbatim, sub)
}

type Range = std::ops::Range<usize>;

/// Counts of what a pack stored.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Sectors by kind: form 1, form 2, form 2 without EDC, verbatim.
    pub kinds: [u64; 4],
    /// Chunks stored uncompressed because deflate did not shrink them.
    pub stored_chunks: u64,
    /// Bytes before compression (the stored sector bytes).
    pub raw_payload_bytes: u64,
}

/// Streams sectors in, writes chunk payloads out, and yields the index.
pub struct Packer<W: Write> {
    out: W,
    bias: Option<u32>,
    sectors: u32,
    kinds: Vec<Kind>,
    subs: Vec<[u8; 4]>,
    verbatim: Vec<(u32, [u8; 4])>,
    chunk_lens: Vec<u32>,
    chunk_crcs: Vec<u32>,
    pending: Vec<u8>,
    pending_sectors: u32,
    payload_bytes: u64,
    hasher: ImageHasher,
    stats: Stats,
}

impl<W: Write> Packer<W> {
    /// A packer whose chunk payloads go to `out`.
    pub fn new(out: W) -> Self {
        Packer {
            out,
            bias: None,
            sectors: 0,
            kinds: Vec::new(),
            subs: Vec::new(),
            verbatim: Vec::new(),
            chunk_lens: Vec::new(),
            chunk_crcs: Vec::new(),
            pending: Vec::new(),
            pending_sectors: 0,
            payload_bytes: 0,
            hasher: ImageHasher::new(),
            stats: Stats::default(),
        }
    }

    /// Add the next sector of the image.
    pub fn push(&mut self, raw: &[u8; SECTOR]) -> io::Result<()> {
        self.hasher.update(raw);
        let index = self.sectors;
        // The first sector's time fixes the bias: a data track that starts at
        // 00:02:00 has 150, one that starts at 00:00:00 has 0.
        let bias = *self.bias.get_or_insert_with(|| {
            let m = psx_iso::bcd_to_bin(raw[12]);
            let s = psx_iso::bcd_to_bin(raw[13]);
            let f = psx_iso::bcd_to_bin(raw[14]);
            if [m, s, f].contains(&0xff) {
                150
            } else {
                (u32::from(m) * 60 + u32::from(s)) * 75 + u32::from(f)
            }
        });
        let (kind, sub) = classify(index, bias, raw);
        self.stats.kinds[kind as usize] += 1;
        self.kinds.push(kind);
        self.subs.push(sub);
        match kind {
            Kind::Verbatim => {
                self.verbatim
                    .push((index, [raw[12], raw[13], raw[14], raw[15]]));
                self.pending.extend_from_slice(raw);
            }
            Kind::Form1 => self.pending.extend_from_slice(&raw[24..24 + 2048]),
            _ => self.pending.extend_from_slice(&raw[24..24 + 2324]),
        }
        self.sectors += 1;
        self.pending_sectors += 1;
        if self.pending_sectors == CHUNK_SECTORS {
            self.flush_chunk()?;
        }
        Ok(())
    }

    fn flush_chunk(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let raw = std::mem::take(&mut self.pending);
        let packed = miniz_oxide::deflate::compress_to_vec(&raw, LEVEL);
        let stored: &[u8] = if packed.len() < raw.len() {
            &packed
        } else {
            self.stats.stored_chunks += 1;
            &raw
        };
        self.out.write_all(stored)?;
        self.chunk_lens.push(stored.len() as u32);
        self.chunk_crcs.push(psx_iso::sector_edc(&raw));
        self.payload_bytes += stored.len() as u64;
        self.stats.raw_payload_bytes += raw.len() as u64;
        self.pending_sectors = 0;
        Ok(())
    }

    /// Flush the last chunk and build the index file. `part_bytes` and
    /// `boot_sectors` are recorded for the reader.
    pub fn finish(mut self, part_bytes: u64, boot_sectors: u32) -> io::Result<(Vec<u8>, Stats)> {
        self.flush_chunk()?;
        self.out.flush()?;
        let mut body = Vec::new();
        for len in &self.chunk_lens {
            body.extend_from_slice(&len.to_le_bytes());
        }
        for crc in &self.chunk_crcs {
            body.extend_from_slice(&crc.to_le_bytes());
        }
        body.extend(self.kinds.iter().map(|&k| k as u8));
        for column in 0..4 {
            body.extend(self.subs.iter().map(|s| s[column]));
        }
        body.extend_from_slice(&(self.verbatim.len() as u32).to_le_bytes());
        for (sector, header) in &self.verbatim {
            body.extend_from_slice(&sector.to_le_bytes());
            body.extend_from_slice(header);
        }
        let boot_chunks = boot_sectors
            .div_ceil(CHUNK_SECTORS)
            .min(self.chunk_lens.len() as u32);
        let mut file = Vec::with_capacity(HEADER_BYTES + body.len() / 4);
        file.extend_from_slice(MAGIC);
        file.push(VERSION);
        file.push(CHUNK_SECTORS as u8);
        file.extend_from_slice(&self.sectors.to_le_bytes());
        file.extend_from_slice(&self.bias.unwrap_or(150).to_le_bytes());
        file.extend_from_slice(&self.hasher.finish().to_le_bytes());
        file.extend_from_slice(&part_bytes.to_le_bytes());
        file.extend_from_slice(&self.payload_bytes.to_le_bytes());
        file.extend_from_slice(&boot_chunks.to_le_bytes());
        file.extend_from_slice(&(body.len() as u32).to_le_bytes());
        file.extend_from_slice(&miniz_oxide::deflate::compress_to_vec(&body, LEVEL));
        Ok((file, self.stats))
    }
}

/// The payload as the files on disk: one, or `base.001`, `base.002`... of
/// `part_bytes` each. Reads a byte range across them.
pub struct PayloadFiles {
    files: Vec<File>,
    part_bytes: u64,
}

impl PayloadFiles {
    /// Open the payload `base` in `dir`, as `index` says it is laid out.
    pub fn open(dir: &Path, base: &str, index: &Index) -> io::Result<PayloadFiles> {
        let paths: Vec<PathBuf> = if index.part_bytes() == 0 {
            vec![dir.join(base)]
        } else {
            (1..=index.payload_bytes().div_ceil(index.part_bytes()))
                .map(|n| dir.join(part_name(base, n)))
                .collect()
        };
        let files = paths
            .iter()
            .map(File::open)
            .collect::<io::Result<Vec<_>>>()?;
        Ok(PayloadFiles {
            files,
            part_bytes: index.part_bytes(),
        })
    }

    /// Read `len` bytes at `offset` of the payload.
    pub fn read(&mut self, mut offset: u64, len: u64) -> io::Result<Vec<u8>> {
        let mut out = Vec::with_capacity(len as usize);
        let mut left = len;
        while left > 0 {
            let (file, within, room) = match offset.checked_div(self.part_bytes) {
                None => (0usize, offset, u64::MAX),
                Some(n) => {
                    let within = offset - n * self.part_bytes;
                    (n as usize, within, self.part_bytes - within)
                }
            };
            let take = left.min(room);
            let f = self.files.get_mut(file).ok_or_else(|| {
                io::Error::new(io::ErrorKind::UnexpectedEof, "payload part missing")
            })?;
            f.seek(SeekFrom::Start(within))?;
            let before = out.len();
            f.take(take).read_to_end(&mut out)?;
            let got = (out.len() - before) as u64;
            if got != take {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "payload part is short",
                ));
            }
            offset += take;
            left -= take;
        }
        Ok(out)
    }
}

/// Decode every chunk of the compact disc and compare each sector with
/// `image`, byte for byte, then check the image hash the index records.
/// Returns the number of sectors verified.
pub fn verify(
    index: &Index,
    payload: &mut PayloadFiles,
    image: &mut impl Read,
) -> Result<u64, Error> {
    let io_err = |e: io::Error| Error(format!("verify: {e}"));
    let mut hasher = ImageHasher::new();
    let mut want = [0u8; SECTOR];
    let mut sectors = 0u64;
    for chunk in 0..index.chunk_count() {
        let range = index.chunk_range(chunk);
        let stored = payload
            .read(range.start, range.end - range.start)
            .map_err(io_err)?;
        let inflated = index.inflate(chunk, &stored)?;
        for sector in index.chunk_sectors(chunk) {
            image.read_exact(&mut want).map_err(io_err)?;
            hasher.update(&want);
            let got = index.sector(&inflated, sector);
            if got != want {
                let at = got.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(0);
                return Err(Error(format!(
                    "sector {sector} differs from the image at byte {at}"
                )));
            }
            if index.head8(sector) != Some(want[12..20].try_into().expect("8 bytes")) {
                return Err(Error(format!("sector {sector}: head8 differs")));
            }
            sectors += 1;
        }
    }
    let mut extra = [0u8; 1];
    if image.read(&mut extra).map_err(io_err)? != 0 {
        return Err(Error("the image has more sectors than the index".into()));
    }
    if hasher.finish() != index.image_hash() {
        return Err(Error("image hash differs from the index".into()));
    }
    Ok(sectors)
}
