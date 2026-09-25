//! In-memory decoder for ECM (Error Code Modeler) disc images.
//!
//! ECM is a public container format that shrinks raw CD images by dropping
//! the sync pattern, EDC and ECC fields a decoder can regenerate. The stream
//! is `"ECM\0"`, then records of `(type, count)` followed by their payload,
//! an end marker, and the EDC of the whole decoded output:
//!
//! - type 0: `count` literal bytes;
//! - type 1: `count` Mode 1 sectors, each stored as a 3-byte address plus
//!   2048 data bytes and expanded to 2352 bytes;
//! - type 2: `count` Mode 2 Form 1 sectors, stored as the 4-byte subheader
//!   plus 2048 data bytes and expanded to 2336 bytes (the sync and header
//!   of a Mode 2 sector are emitted as type-0 literals);
//! - type 3: `count` Mode 2 Form 2 sectors, stored as the subheader plus
//!   2324 data bytes and expanded to 2336 bytes.
//!
//! The EDC is the CD-ROM CRC-32 (polynomial 0xD8018001, reflected) and the
//! ECC is the ECMA-130 Reed-Solomon product code (P and Q parity), as the
//! Yellow Book defines them. Decoding happens in memory so a read-only
//! games folder is never written to.
//!
//! [`EcmImage`] decodes on demand: one pass over the record headers at open
//! builds a checkpoint every [`CHUNK_SECTORS`] output sectors, and a read
//! decodes only the chunks it touches. The whole-stream EDC is not checked
//! then (that would mean decoding everything); [`decode`] still does.

/// Header of every ECM stream.
const MAGIC: &[u8; 4] = b"ECM\0";

const SECTOR_BYTES: usize = 2352;
const MODE2_BYTES: usize = 2336;

struct Tables {
    ecc_f: [u8; 256],
    ecc_b: [u8; 256],
    edc: [u32; 256],
}

fn tables() -> &'static Tables {
    static TABLES: std::sync::OnceLock<Tables> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| {
        let mut t = Tables {
            ecc_f: [0; 256],
            ecc_b: [0; 256],
            edc: [0; 256],
        };
        for i in 0..256u32 {
            let j = (i << 1) ^ if i & 0x80 != 0 { 0x11D } else { 0 };
            t.ecc_f[i as usize] = j as u8;
            t.ecc_b[(i ^ j) as usize] = i as u8;
            let mut edc = i;
            for _ in 0..8 {
                edc = (edc >> 1) ^ if edc & 1 != 0 { 0xD801_8001 } else { 0 };
            }
            t.edc[i as usize] = edc;
        }
        t
    })
}

/// CD-ROM EDC (CRC-32, reflected polynomial 0xD8018001) continued from `edc`.
pub(crate) fn edc_update(mut edc: u32, bytes: &[u8]) -> u32 {
    let lut = &tables().edc;
    for &b in bytes {
        edc = (edc >> 8) ^ lut[((edc ^ u32::from(b)) & 0xFF) as usize];
    }
    edc
}

/// One ECC parity block (P: 86x24, Q: 52x43) over the sector from its
/// header at byte 0x0C.
fn ecc_block(
    sector: &mut [u8; SECTOR_BYTES],
    major_count: usize,
    minor_count: usize,
    major_mult: usize,
    minor_inc: usize,
    dest: usize,
) {
    let t = tables();
    let size = major_count * minor_count;
    for major in 0..major_count {
        let mut index = (major >> 1) * major_mult + (major & 1);
        let mut ecc_a = 0u8;
        let mut ecc_b = 0u8;
        for _ in 0..minor_count {
            let temp = sector[0x0C + index];
            index += minor_inc;
            if index >= size {
                index -= size;
            }
            ecc_a ^= temp;
            ecc_b ^= temp;
            ecc_a = t.ecc_f[ecc_a as usize];
        }
        ecc_a = t.ecc_b[(t.ecc_f[ecc_a as usize] ^ ecc_b) as usize];
        sector[dest + major] = ecc_a;
        sector[dest + major + major_count] = ecc_a ^ ecc_b;
    }
}

/// P then Q parity. Mode 2 computes them with the header address zeroed.
fn ecc_generate(sector: &mut [u8; SECTOR_BYTES], zero_address: bool) {
    let mut address = [0u8; 4];
    if zero_address {
        address.copy_from_slice(&sector[0x0C..0x10]);
        sector[0x0C..0x10].fill(0);
    }
    ecc_block(sector, 86, 24, 2, 86, 0x81C);
    ecc_block(sector, 52, 43, 86, 88, 0x8C8);
    if zero_address {
        sector[0x0C..0x10].copy_from_slice(&address);
    }
}

fn put_u32_le(sector: &mut [u8], at: usize, value: u32) {
    sector[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// Rebuild the EDC and ECC of a sector whose payload is in place.
fn regenerate(sector: &mut [u8; SECTOR_BYTES], kind: u8) {
    match kind {
        1 => {
            let edc = edc_update(0, &sector[..0x810]);
            put_u32_le(sector, 0x810, edc);
            sector[0x814..0x81C].fill(0);
            ecc_generate(sector, false);
        }
        2 => {
            sector.copy_within(0x14..0x18, 0x10);
            let edc = edc_update(0, &sector[0x10..0x818]);
            put_u32_le(sector, 0x818, edc);
            ecc_generate(sector, true);
        }
        _ => {
            sector.copy_within(0x14..0x18, 0x10);
            let edc = edc_update(0, &sector[0x10..0x92C]);
            put_u32_le(sector, 0x92C, edc);
        }
    }
}

#[cfg(test)]
struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

#[cfg(test)]
impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], String> {
        let end = self
            .at
            .checked_add(n)
            .filter(|&end| end <= self.bytes.len())
            .ok_or_else(|| format!("ECM stream truncated at byte {}", self.at))?;
        let out = &self.bytes[self.at..end];
        self.at = end;
        Ok(out)
    }

    fn byte(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
}

/// Decode a complete ECM stream into the raw image it was made from, and
/// check its EDC. The reference the on-demand [`EcmImage`] is tested against.
#[cfg(test)]
pub fn decode(ecm: &[u8]) -> Result<Vec<u8>, String> {
    if ecm.len() < MAGIC.len() || &ecm[..4] != MAGIC {
        return Err("not an ECM stream (missing ECM header)".into());
    }
    let mut r = Reader { bytes: ecm, at: 4 };
    let mut out = Vec::with_capacity(ecm.len().saturating_mul(2));
    let mut sector = [0u8; SECTOR_BYTES];
    loop {
        let mut c = r.byte()?;
        let kind = c & 3;
        let mut num = u64::from((c >> 2) & 0x1F);
        let mut bits = 5;
        while c & 0x80 != 0 {
            c = r.byte()?;
            if bits > 31 {
                return Err("ECM record count is too long".into());
            }
            num |= u64::from(c & 0x7F) << bits;
            bits += 7;
        }
        if num == 0xFFFF_FFFF {
            break;
        }
        if num >= 0x8000_0000 {
            return Err("ECM record count is out of range".into());
        }
        let count = num as usize + 1;
        match kind {
            0 => out.extend_from_slice(r.take(count)?),
            1 => {
                for _ in 0..count {
                    sector.fill(0);
                    sector[0x01..0x0B].fill(0xFF);
                    sector[0x0F] = 1;
                    sector[0x0C..0x0F].copy_from_slice(r.take(3)?);
                    sector[0x10..0x810].copy_from_slice(r.take(0x800)?);
                    regenerate(&mut sector, 1);
                    out.extend_from_slice(&sector);
                }
            }
            _ => {
                let payload = if kind == 2 { 0x804 } else { 0x918 };
                for _ in 0..count {
                    sector.fill(0);
                    sector[0x01..0x0B].fill(0xFF);
                    sector[0x0F] = 2;
                    sector[0x14..0x14 + payload].copy_from_slice(r.take(payload)?);
                    regenerate(&mut sector, kind);
                    out.extend_from_slice(&sector[0x10..0x10 + MODE2_BYTES]);
                }
            }
        }
    }
    let stored = u32::from_le_bytes(r.take(4)?.try_into().expect("four bytes"));
    let computed = edc_update(0, &out);
    if stored != computed {
        return Err(format!(
            "ECM checksum mismatch: stream says {stored:#010x}, decoded data gives {computed:#010x}"
        ));
    }
    Ok(out)
}

/// Output sectors per decoded chunk (and between index checkpoints).
pub const CHUNK_SECTORS: u64 = 16;
const CHUNK_BYTES: u64 = CHUNK_SECTORS * SECTOR_BYTES as u64;
/// Decoded chunks kept for reuse (sequential reads hit these).
const CACHED_CHUNKS: usize = 8;

/// `(input bytes, output bytes)` of one unit of a record type: a literal
/// byte, or one packed sector.
fn unit_sizes(kind: u8) -> (u64, u64) {
    match kind {
        0 => (1, 1),
        1 => (3 + 0x800, SECTOR_BYTES as u64),
        2 => (0x804, MODE2_BYTES as u64),
        _ => (0x918, MODE2_BYTES as u64),
    }
}

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

/// Parse a record header from `bytes`: `(kind, count, header length)`, with
/// `None` for the end marker.
fn parse_header(bytes: &[u8]) -> Result<(Option<(u8, u64)>, usize), String> {
    let mut at = 0usize;
    let mut next = || -> Result<u8, String> {
        let b = *bytes
            .get(at)
            .ok_or("ECM stream truncated in a record header")?;
        at += 1;
        Ok(b)
    };
    let mut c = next()?;
    let kind = c & 3;
    let mut num = u64::from((c >> 2) & 0x1F);
    let mut bits = 5;
    while c & 0x80 != 0 {
        c = next()?;
        if bits > 31 {
            return Err("ECM record count is too long".into());
        }
        num |= u64::from(c & 0x7F) << bits;
        bits += 7;
    }
    if num == 0xFFFF_FFFF {
        return Ok((None, at));
    }
    if num >= 0x8000_0000 {
        return Err("ECM record count is out of range".into());
    }
    Ok((Some((kind, num + 1)), at))
}

/// Decode one packed sector (`kind` 1..=3) from `input` into `sector`; the
/// returned range of `sector` is the unit's output.
fn decode_unit(kind: u8, input: &[u8], sector: &mut [u8; SECTOR_BYTES]) -> core::ops::Range<usize> {
    sector.fill(0);
    sector[0x01..0x0B].fill(0xFF);
    if kind == 1 {
        sector[0x0F] = 1;
        sector[0x0C..0x0F].copy_from_slice(&input[..3]);
        sector[0x10..0x810].copy_from_slice(&input[3..3 + 0x800]);
        regenerate(sector, 1);
        0..SECTOR_BYTES
    } else {
        let payload = if kind == 2 { 0x804 } else { 0x918 };
        sector[0x0F] = 2;
        sector[0x14..0x14 + payload].copy_from_slice(&input[..payload]);
        regenerate(sector, kind);
        0x10..0x10 + MODE2_BYTES
    }
}

/// An ECM-packed image decoded a chunk at a time on demand.
pub struct EcmImage {
    packed: crate::disc_image::SharedImage,
    len: u64,
    /// Stream position at the start of every chunk.
    checkpoints: Vec<Cursor>,
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
            cache: std::sync::Mutex::new(Vec::new()),
        })
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

    /// Encode `(type, count)` the way ECM does: 2 type bits, 5 count bits,
    /// then 7 bits per continuation byte.
    fn record_header(kind: u8, count: u32) -> Vec<u8> {
        let mut n = count - 1;
        let mut out = vec![kind | (((n & 0x1F) as u8) << 2)];
        n >>= 5;
        while n != 0 {
            *out.last_mut().unwrap() |= 0x80;
            out.push((n & 0x7F) as u8);
            n >>= 7;
        }
        out
    }

    fn finish(mut stream: Vec<u8>, decoded: &[u8]) -> Vec<u8> {
        stream.extend_from_slice(&[0xFC, 0xFF, 0xFF, 0xFF, 0x3F]);
        stream.extend_from_slice(&edc_update(0, decoded).to_le_bytes());
        stream
    }

    /// Bit-at-a-time CRC with the same reflected polynomial, independent of
    /// the table the decoder uses.
    fn edc_bitwise(bytes: &[u8]) -> u32 {
        let mut crc = 0u32;
        for &b in bytes {
            crc ^= u32::from(b);
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

    #[test]
    fn end_marker_encodes_as_the_documented_bytes() {
        // 0xFFFFFFFF + 1 wraps; the end marker is count field 0xFFFFFFFF.
        let mut n = 0xFFFF_FFFFu64;
        let mut out = vec![((n & 0x1F) as u8) << 2];
        n >>= 5;
        while n != 0 {
            *out.last_mut().unwrap() |= 0x80;
            out.push((n & 0x7F) as u8);
            n >>= 7;
        }
        assert_eq!(out, [0xFC, 0xFF, 0xFF, 0xFF, 0x3F]);
    }

    #[test]
    fn literal_records_round_trip_with_multibyte_counts() {
        let data: Vec<u8> = (0..300u32).map(|i| (i * 7) as u8).collect();
        let mut stream = MAGIC.to_vec();
        stream.extend(record_header(0, data.len() as u32));
        stream.extend(&data);
        let stream = finish(stream, &data);
        assert_eq!(decode(&stream).unwrap(), data);
    }

    #[test]
    fn mode1_sector_gets_sync_header_and_a_valid_edc() {
        let address = [0x00, 0x02, 0x16];
        let data: Vec<u8> = (0..0x800u32).map(|i| (i ^ (i >> 3)) as u8).collect();
        let mut stream = MAGIC.to_vec();
        stream.extend(record_header(1, 1));
        stream.extend(address);
        stream.extend(&data);

        let mut expected = [0u8; SECTOR_BYTES];
        expected[0x01..0x0B].fill(0xFF);
        expected[0x0C..0x0F].copy_from_slice(&address);
        expected[0x0F] = 1;
        expected[0x10..0x810].copy_from_slice(&data);
        regenerate(&mut expected, 1);
        let stream = finish(stream, &expected);

        let out = decode(&stream).unwrap();
        assert_eq!(out.len(), SECTOR_BYTES);
        assert_eq!(
            &out[..12],
            &[0, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0]
        );
        assert_eq!(&out[0x0C..0x10], &[0x00, 0x02, 0x16, 0x01]);
        let edc = u32::from_le_bytes(out[0x810..0x814].try_into().unwrap());
        assert_eq!(edc, edc_bitwise(&out[..0x810]));
        assert!(out[0x814..0x81C].iter().all(|&b| b == 0));
        // P parity of an all-zero payload is zero; this payload is not.
        assert!(out[0x81C..0x930].iter().any(|&b| b != 0));
    }

    #[test]
    fn mode2_form1_sector_duplicates_the_subheader_and_zeroes_nothing_else() {
        let sub = [0x01, 0x02, 0x08, 0x00];
        let data = vec![0x5Au8; 0x800];
        let mut stream = MAGIC.to_vec();
        stream.extend(record_header(2, 1));
        stream.extend(sub);
        stream.extend(&data);

        let mut sector = [0u8; SECTOR_BYTES];
        sector[0x14..0x18].copy_from_slice(&sub);
        sector[0x18..0x818].copy_from_slice(&data);
        regenerate(&mut sector, 2);
        let expected = sector[0x10..0x10 + MODE2_BYTES].to_vec();
        let stream = finish(stream, &expected);

        let out = decode(&stream).unwrap();
        assert_eq!(out.len(), MODE2_BYTES);
        assert_eq!(&out[..8], &[0x01, 0x02, 0x08, 0x00, 0x01, 0x02, 0x08, 0x00]);
        let edc = u32::from_le_bytes(out[0x808..0x80C].try_into().unwrap());
        assert_eq!(edc, edc_bitwise(&out[..0x808]));
    }

    #[test]
    fn mode2_form2_sector_has_edc_and_no_ecc() {
        let sub = [0x01, 0x02, 0x24, 0x00];
        let data = vec![0xA5u8; 0x914];
        let mut stream = MAGIC.to_vec();
        stream.extend(record_header(3, 1));
        stream.extend(sub);
        stream.extend(&data);
        let mut decoded = Vec::new();
        decoded.extend(sub);
        decoded.extend(sub);
        decoded.extend(&data);
        decoded.extend(edc_bitwise(&decoded).to_le_bytes());
        let stream = finish(stream, &decoded);
        assert_eq!(decode(&stream).unwrap(), decoded);
    }

    /// A stream mixing every record type: a long literal run, a Mode 1 run
    /// spanning several index checkpoints, and Mode 2 sectors split the way
    /// real encoders split them (16 literal bytes, then one packed unit).
    fn mixed_stream() -> (Vec<u8>, Vec<u8>) {
        let mut rng = 0x1234_5678u32;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 17;
            rng ^= rng << 5;
            rng as u8
        };
        let mut stream = MAGIC.to_vec();
        let mut decoded = Vec::new();
        let literal: Vec<u8> = (0..SECTOR_BYTES * 20 + 77).map(|_| next()).collect();
        stream.extend(record_header(0, literal.len() as u32));
        stream.extend(&literal);
        decoded.extend(&literal);
        stream.extend(record_header(1, 40));
        for i in 0..40u32 {
            let address = [0x00, 0x02 + (i / 75) as u8, (i % 75) as u8];
            let data: Vec<u8> = (0..0x800).map(|_| next()).collect();
            stream.extend(address);
            stream.extend(&data);
            let mut sector = [0u8; SECTOR_BYTES];
            decode_unit(1, &[&address[..], &data[..]].concat(), &mut sector);
            decoded.extend(sector);
        }
        for i in 0..50u32 {
            let head: Vec<u8> = (0..16).map(|_| next()).collect();
            stream.extend(record_header(0, 16));
            stream.extend(&head);
            decoded.extend(&head);
            let kind = if i % 3 == 0 { 3 } else { 2 };
            let payload = if kind == 2 { 0x804 } else { 0x918 };
            let input: Vec<u8> = (0..payload).map(|_| next()).collect();
            stream.extend(record_header(kind, 1));
            stream.extend(&input);
            let mut sector = [0u8; SECTOR_BYTES];
            let range = decode_unit(kind, &input, &mut sector);
            decoded.extend(&sector[range]);
        }
        let stream = finish(stream, &decoded);
        (stream, decoded)
    }

    #[test]
    fn on_demand_reads_match_the_whole_stream_decode() {
        let (stream, decoded) = mixed_stream();
        assert_eq!(decode(&stream).unwrap(), decoded);
        let image = EcmImage::open(std::sync::Arc::new(stream)).unwrap();
        use psx_iso::TrackSource;
        assert_eq!(image.len(), decoded.len() as u64);
        let mut whole = vec![0u8; decoded.len()];
        assert!(image.read_at(0, &mut whole));
        assert_eq!(whole, decoded);
        // Reads at awkward offsets and lengths, out of order, crossing chunk
        // and record boundaries.
        for (at, step) in (7u64..).zip(0..200u64) {
            let len = ((step * 7919) % (3 * SECTOR_BYTES as u64)) as usize + 1;
            let offset = (at * 104_729) % (decoded.len() as u64 - len as u64);
            let mut out = vec![0u8; len];
            assert!(image.read_at(offset, &mut out), "read {offset}+{len}");
            assert_eq!(out, decoded[offset as usize..offset as usize + len]);
        }
        let mut past = [0u8; 2];
        assert!(!image.read_at(decoded.len() as u64 - 1, &mut past));
    }

    #[test]
    fn index_rejects_bad_magic_and_truncation() {
        assert!(EcmImage::open(std::sync::Arc::new(b"NOPE".to_vec())).is_err());
        let mut stream = MAGIC.to_vec();
        stream.extend(record_header(0, 10));
        stream.extend([1, 2, 3]);
        assert!(EcmImage::open(std::sync::Arc::new(stream)).is_err());
    }

    #[test]
    fn rejects_bad_magic_truncation_and_checksum() {
        assert!(decode(b"NOPE").is_err());
        let mut stream = MAGIC.to_vec();
        stream.extend(record_header(0, 10));
        stream.extend([1, 2, 3]);
        assert!(decode(&stream).unwrap_err().contains("truncated"));

        let data = [9u8; 4];
        let mut stream = MAGIC.to_vec();
        stream.extend(record_header(0, 4));
        stream.extend(data);
        let mut stream = finish(stream, &data);
        let last = stream.len() - 1;
        stream[last] ^= 1;
        assert!(decode(&stream).unwrap_err().contains("checksum"));
    }
}
