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

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

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

/// Decode a complete ECM stream into the raw image it was made from.
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
