//! In-memory reader for ECM disc images.
//!
//! An ECM file is a raw CD image with the parts a reader can recompute left
//! out: sector sync patterns, EDC and ECC. The file is the four bytes
//! `"ECM\0"`, then records, an end marker, and a 4-byte little-endian EDC of
//! the whole decoded image. Each record header packs a type and a unit
//! count, and its units follow back to back:
//!
//! | Type | Stored per unit | Decoded per unit |
//! |---|---|---|
//! | 0 | 1 byte | that byte |
//! | 1 | 3-byte address + 2048 data bytes | a 2352-byte Mode 1 sector |
//! | 2 | 4-byte subheader + 2048 data bytes | bytes 10h..92Fh of a Mode 2 Form 1 sector |
//! | 3 | 4-byte subheader + 2324 data bytes | bytes 10h..92Fh of a Mode 2 Form 2 sector |
//!
//! The sync and header of Mode 2 sectors travel as type-0 bytes. Sectors are
//! rebuilt with psx-iso's ECMA-130 EDC/ECC, the same code the disc builder
//! uses. Decoding happens in memory, so a read-only games folder is never
//! written to.
//!
//! [`EcmImage`] decodes on demand: one pass over the record headers at open
//! builds a checkpoint every [`CHUNK_SECTORS`] output sectors, and a read
//! decodes only the chunks it touches. The trailing EDC needs the whole
//! image, so it is checked only by [`EcmImage::verify`] and [`decode`].

/// Header of every ECM stream.
const MAGIC: &[u8; 4] = b"ECM\0";

const SECTOR_BYTES: usize = 2352;
const MODE2_BYTES: usize = 2336;
/// Where a Mode 2 sector's decoded bytes start in the raw sector.
const MODE2_FROM: usize = 0x10;

/// (count - 1) of the end marker.
const END_MARKER: u32 = 0xFFFF_FFFF;

/// EDC of `bytes`, continued from `edc` (0 to start a stream).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn edc_update(edc: u32, bytes: &[u8]) -> u32 {
    psx_iso::sector_edc_continue(edc, bytes)
}

/// Stored and decoded bytes of one unit of record type `kind`.
fn unit_sizes(kind: u8) -> (u64, u64) {
    match kind {
        0 => (1, 1),
        1 => (3 + 2048, SECTOR_BYTES as u64),
        2 => (4 + 2048, MODE2_BYTES as u64),
        _ => (4 + 2324, MODE2_BYTES as u64),
    }
}

/// Read a record header from the start of `bytes` (at most five bytes
/// long): `(Some((type, count)), length)`, or `(None, length)` for the end
/// marker.
fn parse_header(bytes: &[u8]) -> Result<(Option<(u8, u64)>, usize), String> {
    let first = *bytes.first().ok_or("ECM record header missing")?;
    let kind = first & 3;
    let mut value = u64::from((first >> 2) & 0x1F);
    let mut shift = 5;
    let mut len = 1;
    let mut more = first & 0x80 != 0;
    while more {
        let byte = *bytes.get(len).ok_or("ECM record header truncated")?;
        len += 1;
        value |= u64::from(byte & 0x7F) << shift;
        shift += 7;
        more = byte & 0x80 != 0;
        if shift > 35 || value > u64::from(u32::MAX) {
            return Err("ECM record count does not fit in 32 bits".into());
        }
    }
    if value == u64::from(END_MARKER) {
        return Ok((None, len));
    }
    if value >= 0x8000_0000 {
        return Err(format!("ECM record count {value:#x} is corrupt"));
    }
    Ok((Some((kind, value + 1)), len))
}

/// Rebuild one packed sector of type 1..3 from its stored bytes into
/// `sector`, returning the part of it the image holds.
fn decode_unit(kind: u8, input: &[u8], sector: &mut [u8; SECTOR_BYTES]) -> core::ops::Range<usize> {
    sector[0] = 0;
    sector[1..11].fill(0xFF);
    sector[11] = 0;
    if kind == 1 {
        sector[0x0C..0x0F].copy_from_slice(&input[..3]);
        sector[0x0F] = 1;
        sector[0x10..0x810].copy_from_slice(&input[3..3 + 2048]);
        psx_iso::protect_mode1(sector);
        return 0..SECTOR_BYTES;
    }
    sector[0x0C..0x0F].fill(0);
    sector[0x0F] = 2;
    sector[0x10..0x14].copy_from_slice(&input[..4]);
    sector[0x14..0x18].copy_from_slice(&input[..4]);
    if kind == 2 {
        sector[0x18..0x818].copy_from_slice(&input[4..4 + 2048]);
        psx_iso::protect_mode2_form1(sector);
    } else {
        sector[0x18..0x92C].copy_from_slice(&input[4..4 + 2324]);
        psx_iso::protect_mode2_form2(sector);
    }
    MODE2_FROM..SECTOR_BYTES
}

/// Decode a whole ECM stream held in memory and check its trailing EDC.
/// Used by tests and the real-image check; reads go through [`EcmImage`].
#[cfg_attr(not(test), allow(dead_code))]
pub fn decode(ecm: &[u8]) -> Result<Vec<u8>, String> {
    if ecm.get(..4) != Some(MAGIC.as_slice()) {
        return Err("not an ECM stream (missing ECM header)".into());
    }
    let mut at = 4usize;
    let mut out = Vec::new();
    let mut sector = [0u8; SECTOR_BYTES];
    loop {
        let rest = &ecm[at.min(ecm.len())..];
        let (record, header_len) = parse_header(&rest[..rest.len().min(5)])?;
        at += header_len;
        let Some((kind, count)) = record else {
            break;
        };
        let (unit_in, _) = unit_sizes(kind);
        let need = count
            .checked_mul(unit_in)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|&n| at + n <= ecm.len())
            .ok_or_else(|| format!("ECM stream truncated at byte {at}"))?;
        let payload = &ecm[at..at + need];
        if kind == 0 {
            out.extend_from_slice(payload);
        } else {
            for unit in payload.chunks_exact(unit_in as usize) {
                let range = decode_unit(kind, unit, &mut sector);
                out.extend_from_slice(&sector[range]);
            }
        }
        at += need;
    }
    let stored = ecm
        .get(at..at + 4)
        .ok_or_else(|| format!("ECM checksum truncated at byte {at}"))?;
    let stored = u32::from_le_bytes([stored[0], stored[1], stored[2], stored[3]]);
    let actual = edc_update(0, &out);
    if stored != actual {
        return Err(format!(
            "ECM checksum mismatch: stored {stored:08x}, decoded {actual:08x}"
        ));
    }
    Ok(out)
}

pub const CHUNK_SECTORS: u64 = 16;
const CHUNK_BYTES: u64 = CHUNK_SECTORS * SECTOR_BYTES as u64;
/// Decoded chunks kept for reuse (sequential reads hit these).
const CACHED_CHUNKS: usize = 8;

/// Where the record stream stands at an output position: `remaining` units
/// of `kind` start at input `in_pos` and output `out_pos`. With nothing
/// remaining, the next record header is at `in_pos`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cursor {
    in_pos: u64,
    out_pos: u64,
    kind: u8,
    remaining: u64,
}

/// An ECM-packed image decoded a chunk at a time on demand.
pub struct EcmImage {
    packed: crate::disc_image::SharedImage,
    len: u64,
    /// Stream position at the start of every chunk.
    checkpoints: Vec<Cursor>,
    /// Where the trailing EDC of the decoded image is stored.
    #[cfg_attr(not(test), allow(dead_code))]
    checksum_at: u64,
    cache: std::sync::Mutex<Vec<(u64, Box<[u8]>)>>,
}

impl EcmImage {
    /// Index the ECM stream in `packed`: one sequential pass over its record
    /// headers, skipping the payloads.
    pub fn open(packed: crate::disc_image::SharedImage) -> Result<Self, String> {
        let total = packed.len();
        let mut magic = [0u8; 4];
        if !packed.read_at(0, &mut magic) || &magic != MAGIC {
            return Err("not an ECM stream (missing ECM header)".into());
        }
        let mut window = Window::new(&*packed, 1 << 20);
        let mut in_pos = 4u64;
        let mut out_pos = 0u64;
        let mut next_checkpoint = 0u64;
        let mut checkpoints = Vec::new();
        loop {
            let (record, header_len) = parse_header(window.at(in_pos, 5)?)?;
            let payload = in_pos + header_len as u64;
            let Some((kind, count)) = record else {
                if payload + 4 > total {
                    return Err(format!("ECM stream truncated at byte {payload}"));
                }
                in_pos = payload;
                break;
            };
            let (unit_in, unit_out) = unit_sizes(kind);
            // The payload must be in the file before the count is trusted
            // for anything: a corrupt header can claim billions of units.
            let next = payload + count * unit_in;
            if next > total {
                return Err(format!("ECM stream truncated at byte {total}"));
            }
            let record_out = count * unit_out;
            while next_checkpoint < out_pos + record_out {
                let unit = (next_checkpoint - out_pos) / unit_out;
                checkpoints.push(Cursor {
                    in_pos: payload + unit * unit_in,
                    out_pos: out_pos + unit * unit_out,
                    kind,
                    remaining: count - unit,
                });
                next_checkpoint += CHUNK_BYTES;
            }
            out_pos += record_out;
            in_pos = next;
        }
        Ok(Self {
            packed,
            len: out_pos,
            checkpoints,
            checksum_at: in_pos,
            cache: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Decode the whole image and compare its EDC with the one stored after
    /// the end marker.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn verify(&self) -> Result<(), String> {
        use psx_iso::TrackSource;
        let mut edc = 0;
        let mut buf = vec![0u8; CHUNK_BYTES as usize];
        let mut at = 0;
        while at < self.len {
            let n = (self.len - at).min(CHUNK_BYTES) as usize;
            if !self.read_at(at, &mut buf[..n]) {
                return Err(format!("ECM image undecodable at byte {at}"));
            }
            edc = edc_update(edc, &buf[..n]);
            at += n as u64;
        }
        let mut stored = [0u8; 4];
        if !self.packed.read_at(self.checksum_at, &mut stored) {
            return Err("ECM checksum unreadable".into());
        }
        let stored = u32::from_le_bytes(stored);
        if stored != edc {
            return Err(format!(
                "ECM checksum mismatch: stored {stored:08x}, decoded {edc:08x}"
            ));
        }
        Ok(())
    }

    /// Decode output chunk `index`.
    fn decode_chunk(&self, index: u64) -> Option<Box<[u8]>> {
        let start = index * CHUNK_BYTES;
        let end = (start + CHUNK_BYTES).min(self.len);
        let mut cursor = *self.checkpoints.get(index as usize)?;
        let mut out = vec![0u8; (end - start) as usize].into_boxed_slice();
        // A chunk's input is at most its output plus one straddling unit and
        // a few record headers.
        let mut window = Window::new(
            &*self.packed,
            (CHUNK_BYTES as usize) + 2 * SECTOR_BYTES + 1024,
        );
        let mut sector = [0u8; SECTOR_BYTES];
        let mut emit = |bytes: &[u8], at: u64| {
            let lo = at.max(start);
            let hi = (at + bytes.len() as u64).min(end);
            if lo < hi {
                out[(lo - start) as usize..(hi - start) as usize]
                    .copy_from_slice(&bytes[(lo - at) as usize..(hi - at) as usize]);
            }
        };
        while cursor.out_pos < end {
            if cursor.remaining == 0 {
                let (record, header_len) = parse_header(window.at(cursor.in_pos, 5).ok()?).ok()?;
                let (kind, count) = record?;
                cursor = Cursor {
                    in_pos: cursor.in_pos + header_len as u64,
                    out_pos: cursor.out_pos,
                    kind,
                    remaining: count,
                };
                continue;
            }
            let (unit_in, unit_out) = unit_sizes(cursor.kind);
            if cursor.kind == 0 {
                let n = cursor.remaining.min(end - cursor.out_pos);
                emit(window.at(cursor.in_pos, n as usize).ok()?, cursor.out_pos);
                cursor.in_pos += n;
                cursor.out_pos += n;
                cursor.remaining -= n;
            } else {
                let input = window.at(cursor.in_pos, unit_in as usize).ok()?;
                let range = decode_unit(cursor.kind, input, &mut sector);
                emit(&sector[range], cursor.out_pos);
                cursor.in_pos += unit_in;
                cursor.out_pos += unit_out;
                cursor.remaining -= 1;
            }
        }
        Some(out)
    }

    /// Copy the overlap of chunk `index` with `[offset, offset + out.len())`.
    fn copy_from_chunk(&self, index: u64, offset: u64, out: &mut [u8]) -> bool {
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        let hit = cache.iter().position(|(i, _)| *i == index);
        let slot = match hit {
            Some(i) => {
                let entry = cache.remove(i);
                cache.push(entry);
                cache.len() - 1
            }
            None => {
                let Some(chunk) = self.decode_chunk(index) else {
                    return false;
                };
                if cache.len() == CACHED_CHUNKS {
                    cache.remove(0);
                }
                cache.push((index, chunk));
                cache.len() - 1
            }
        };
        let chunk = &cache[slot].1;
        let chunk_start = index * CHUNK_BYTES;
        let lo = offset.max(chunk_start);
        let hi = (offset + out.len() as u64).min(chunk_start + chunk.len() as u64);
        out[(lo - offset) as usize..(hi - offset) as usize]
            .copy_from_slice(&chunk[(lo - chunk_start) as usize..(hi - chunk_start) as usize]);
        true
    }
}

impl psx_iso::TrackSource for EcmImage {
    fn len(&self) -> u64 {
        self.len
    }

    fn read_at(&self, offset: u64, out: &mut [u8]) -> bool {
        let Some(end) = offset
            .checked_add(out.len() as u64)
            .filter(|&e| e <= self.len)
        else {
            return false;
        };
        if out.is_empty() {
            return true;
        }
        (offset / CHUNK_BYTES..=(end - 1) / CHUNK_BYTES)
            .all(|index| self.copy_from_chunk(index, offset, out))
    }
}

/// A read-ahead window over a packed stream, refilled on demand.
struct Window<'a> {
    src: &'a dyn psx_iso::TrackSource,
    size: usize,
    start: u64,
    buf: Vec<u8>,
}

impl<'a> Window<'a> {
    fn new(src: &'a dyn psx_iso::TrackSource, size: usize) -> Self {
        Self {
            src,
            size,
            start: 0,
            buf: Vec::new(),
        }
    }

    /// Up to `n` bytes at `pos` (fewer only at the end of the stream).
    fn at(&mut self, pos: u64, n: usize) -> Result<&[u8], String> {
        let total = self.src.len();
        if pos >= total {
            return Err(format!("ECM stream truncated at byte {pos}"));
        }
        let want_end = (pos + n as u64).min(total);
        let have_end = self.start + self.buf.len() as u64;
        if pos < self.start || want_end > have_end {
            let len = (self.size.max(n) as u64).min(total - pos) as usize;
            self.buf.resize(len, 0);
            if !self.src.read_at(pos, &mut self.buf) {
                return Err(format!("ECM stream unreadable at byte {pos}"));
            }
            self.start = pos;
        }
        let from = (pos - self.start) as usize;
        let to = (want_end - self.start) as usize;
        if to - from < n && n > 5 {
            return Err(format!("ECM stream truncated at byte {pos}"));
        }
        Ok(&self.buf[from..to])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psx_iso::TrackSource;
    use std::sync::Arc;

    /// The EDC one bit at a time, straight from its definition: a CRC with
    /// the reflected generator D8018001h, starting at zero.
    fn edc_bitwise(bytes: &[u8]) -> u32 {
        let mut crc = 0u32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xD801_8001
                } else {
                    crc >> 1
                };
            }
        }
        crc
    }

    /// A record header for `count` units of `kind` (count 0 makes the end
    /// marker, whose stored count - 1 is FFFFFFFFh).
    fn header(kind: u8, count: u64) -> Vec<u8> {
        let mut rest = (count as u32).wrapping_sub(1);
        let mut out = vec![kind | (((rest & 0x1F) as u8) << 2)];
        rest >>= 5;
        while rest != 0 {
            *out.last_mut().unwrap() |= 0x80;
            out.push((rest & 0x7F) as u8);
            rest >>= 7;
        }
        out
    }

    /// Records `(kind, stored units)` packed into a stream whose trailer is
    /// the EDC of `decoded`.
    fn stream(records: &[(u8, Vec<u8>, u64)], decoded: &[u8]) -> Vec<u8> {
        let mut ecm = MAGIC.to_vec();
        for (kind, payload, count) in records {
            ecm.extend(header(*kind, *count));
            ecm.extend_from_slice(payload);
        }
        ecm.extend(header(0, 0));
        ecm.extend_from_slice(&edc_bitwise(decoded).to_le_bytes());
        ecm
    }

    fn pattern(len: usize, seed: u32) -> Vec<u8> {
        (0..len as u32)
            .map(|i| (i.wrapping_mul(31).wrapping_add(seed) >> 2) as u8)
            .collect()
    }

    /// Multiply by alpha in GF(2^8) with the ECMA-130 field polynomial.
    fn times_alpha(a: u8) -> u8 {
        (a << 1) ^ if a & 0x80 != 0 { 0x1D } else { 0 }
    }

    /// Check both ECMA-130 parity equations over every P column and Q
    /// diagonal of the 2340 bytes from the header on.
    fn parity_holds(area: &[u8]) -> bool {
        let word = |w: usize, plane: usize| area[2 * w + plane];
        let check = |symbols: &[u8]| {
            let sum = symbols.iter().fold(0, |a, &b| a ^ b);
            let weighted = symbols.iter().fold(0, |a, &b| times_alpha(a) ^ b);
            sum == 0 && weighted == 0
        };
        (0..2).all(|plane| {
            let p = (0..43).all(|col| {
                let s: Vec<u8> = (0..26).map(|row| word(row * 43 + col, plane)).collect();
                check(&s)
            });
            let q = (0..26).all(|diag| {
                let mut s: Vec<u8> = (0..43)
                    .map(|i| word((diag * 43 + i * 44) % (43 * 26), plane))
                    .collect();
                s.push(word(43 * 26 + diag, plane));
                s.push(word(43 * 26 + 26 + diag, plane));
                check(&s)
            });
            p && q
        })
    }

    #[test]
    fn end_marker_encodes_as_the_documented_bytes() {
        assert_eq!(header(0, 0), [0xFC, 0xFF, 0xFF, 0xFF, 0x3F]);
        assert_eq!(
            parse_header(&[0xFC, 0xFF, 0xFF, 0xFF, 0x3F]).unwrap(),
            (None, 5)
        );
        assert_eq!(parse_header(&[0x05]).unwrap(), (Some((1, 2)), 1));
    }

    #[test]
    fn literal_records_round_trip_with_multibyte_counts() {
        for len in [1usize, 32, 33, 4096, 300_000] {
            let bytes = pattern(len, len as u32);
            let ecm = stream(&[(0, bytes.clone(), len as u64)], &bytes);
            let (record, used) = parse_header(&ecm[4..9]).unwrap();
            assert_eq!(record, Some((0, len as u64)));
            assert_eq!(used, header(0, len as u64).len());
            assert_eq!(decode(&ecm).unwrap(), bytes);
        }
    }

    #[test]
    fn mode1_sector_gets_sync_header_and_a_valid_edc() {
        let mut unit = vec![0x01, 0x02, 0x03];
        unit.extend(pattern(2048, 7));
        let mut sector = [0u8; SECTOR_BYTES];
        assert_eq!(decode_unit(1, &unit, &mut sector), 0..SECTOR_BYTES);
        assert_eq!(
            sector[..12],
            [0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]
        );
        assert_eq!(sector[0x0C..0x10], [1, 2, 3, 1]);
        assert_eq!(sector[0x10..0x810], unit[3..]);
        assert_eq!(
            sector[0x810..0x814],
            edc_bitwise(&sector[..0x810]).to_le_bytes()
        );
        assert!(sector[0x814..0x81C].iter().all(|&b| b == 0));
        assert!(
            parity_holds(&sector[0x0C..]),
            "parity counts the real header"
        );
    }

    #[test]
    fn mode2_form1_sector_duplicates_the_subheader_and_zeroes_nothing_else() {
        let mut unit = vec![0x11, 0x22, 0x08, 0x44];
        unit.extend(pattern(2048, 9));
        let mut sector = [0xAAu8; SECTOR_BYTES];
        let range = decode_unit(2, &unit, &mut sector);
        assert_eq!(range, 0x10..SECTOR_BYTES);
        assert_eq!(sector[0x10..0x14], unit[..4]);
        assert_eq!(sector[0x14..0x18], unit[..4]);
        assert_eq!(sector[0x18..0x818], unit[4..]);
        assert_eq!(
            sector[0x818..0x81C],
            edc_bitwise(&sector[0x10..0x818]).to_le_bytes()
        );
        let mut zero_header = sector;
        zero_header[0x0C..0x10].fill(0);
        assert!(
            parity_holds(&zero_header[0x0C..]),
            "parity counts the header as zero"
        );
        // Compare with psx-iso run on the same sector with a real header.
        let mut reference = sector;
        reference[0x0C..0x10].copy_from_slice(&[0x00, 0x02, 0x16, 0x02]);
        reference[0x818..].fill(0);
        psx_iso::protect_mode2_form1(&mut reference);
        assert_eq!(reference[0x10..], sector[0x10..]);
    }

    #[test]
    fn mode2_form2_sector_has_edc_and_no_ecc() {
        let mut unit = vec![0x01, 0x00, 0x24, 0x00];
        unit.extend(pattern(2324, 3));
        let mut sector = [0u8; SECTOR_BYTES];
        decode_unit(2, &[0; 2052], &mut sector);
        let range = decode_unit(3, &unit, &mut sector);
        assert_eq!(range, 0x10..SECTOR_BYTES);
        assert_eq!(sector[0x18..0x92C], unit[4..]);
        assert_eq!(
            sector[0x92C..0x930],
            edc_bitwise(&sector[0x10..0x92C]).to_le_bytes()
        );
    }

    #[test]
    fn mode1_parity_matches_psx_iso_with_a_nonzero_header() {
        let mut unit = vec![0x12, 0x34, 0x56];
        unit.extend(pattern(2048, 11));
        let mut sector = [0u8; SECTOR_BYTES];
        decode_unit(1, &unit, &mut sector);
        let mut reference = sector;
        reference[0x810..].fill(0);
        psx_iso::protect_mode1(&mut reference);
        assert_eq!(reference, sector);
        assert!(parity_holds(&sector[0x0C..]));
    }

    /// A stream with every record type, and its decoded image.
    fn mixed() -> (Vec<u8>, Vec<u8>) {
        let mut decoded = Vec::new();
        let mut records = Vec::new();
        let lead = pattern(5000, 1);
        decoded.extend_from_slice(&lead);
        records.push((0u8, lead, 5000u64));
        let mut sector = [0u8; SECTOR_BYTES];
        for kind in 1..=3u8 {
            let (unit_in, _) = unit_sizes(kind);
            let count = 21u64;
            let mut payload = Vec::new();
            for i in 0..count {
                let unit = pattern(unit_in as usize, kind as u32 * 100 + i as u32);
                let range = decode_unit(kind, &unit, &mut sector);
                if kind != 1 {
                    let head = pattern(16, 9);
                    records.push((0, head.clone(), 16));
                    decoded.extend_from_slice(&head);
                    records.push((kind, unit.clone(), 1));
                } else {
                    payload.extend_from_slice(&unit);
                }
                decoded.extend_from_slice(&sector[range]);
            }
            if kind == 1 {
                records.push((1, payload, count));
            }
        }
        (stream(&records, &decoded), decoded)
    }

    #[test]
    fn on_demand_reads_match_the_whole_stream_decode() {
        let (ecm, decoded) = mixed();
        assert_eq!(decode(&ecm).unwrap(), decoded);
        let image = EcmImage::open(Arc::new(ecm)).unwrap();
        assert_eq!(image.len(), decoded.len() as u64);
        image.verify().unwrap();
        for (offset, len) in [
            (0usize, 100usize),
            (4990, 30),
            (60_000, 9000),
            (decoded.len() - 7, 7),
        ] {
            let mut out = vec![0u8; len];
            assert!(image.read_at(offset as u64, &mut out));
            assert_eq!(out, decoded[offset..offset + len], "at {offset}");
        }
        let mut out = vec![0u8; 8];
        assert!(!image.read_at(decoded.len() as u64 - 4, &mut out));
    }

    #[test]
    fn index_rejects_bad_magic_and_truncation() {
        let (ecm, _) = mixed();
        let open = |bytes: Vec<u8>| EcmImage::open(Arc::new(bytes)).map(|_| ());
        let mut bad = ecm.clone();
        bad[0] = b'X';
        assert!(open(bad).is_err());
        assert!(open(ecm[..ecm.len() - 2].to_vec()).is_err());
        assert!(open(ecm[..ecm.len() / 2].to_vec()).is_err());
        assert!(open(ecm).is_ok());
    }

    #[test]
    fn rejects_bad_magic_truncation_and_checksum() {
        let (ecm, _) = mixed();
        let mut bad = ecm.clone();
        bad[3] = 1;
        assert!(decode(&bad).is_err());
        assert!(decode(&ecm[..ecm.len() - 1]).is_err());
        assert!(decode(&ecm[..ecm.len() / 3]).is_err());
        let mut wrong = ecm.clone();
        let last = wrong.len() - 1;
        wrong[last] ^= 1;
        assert!(decode(&wrong).unwrap_err().contains("checksum"));
        let image = EcmImage::open(Arc::new(wrong)).unwrap();
        assert!(image.verify().is_err());
        // A count of 80000000h or more that is not the end marker.
        assert!(parse_header(&[0xFC, 0xFF, 0xFF, 0xFF, 0x2F]).is_err());
        // More than 32 bits of count.
        assert!(parse_header(&[0xFC, 0xFF, 0xFF, 0xFF, 0xFF]).is_err());
    }

    /// Full decode of a real image: `PSOXIDE_ECM_IMAGE=<file.img.ecm>`;
    /// with `PSOXIDE_ECM_OUT=<file>` the decoded image is written there.
    #[test]
    #[ignore]
    fn decodes_a_real_image_when_given_one() {
        let Some(path) = std::env::var_os("PSOXIDE_ECM_IMAGE") else {
            return;
        };
        let file = crate::disc_image::FileImage::open(std::path::Path::new(&path)).unwrap();
        let image = EcmImage::open(Arc::new(file)).unwrap();
        image.verify().unwrap();
        if let Some(out) = std::env::var_os("PSOXIDE_ECM_OUT") {
            use std::io::Write;
            let mut w = std::io::BufWriter::new(std::fs::File::create(out).unwrap());
            let mut buf = vec![0u8; CHUNK_BYTES as usize];
            let mut at = 0;
            while at < image.len() {
                let n = (image.len() - at).min(CHUNK_BYTES) as usize;
                assert!(image.read_at(at, &mut buf[..n]));
                w.write_all(&buf[..n]).unwrap();
                at += n as u64;
            }
        }
    }
}
