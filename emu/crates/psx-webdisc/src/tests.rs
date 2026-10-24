use super::*;

/// A deterministic byte stream.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> u8 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 33) as u8
    }

    fn fill(&mut self, out: &mut [u8], sparse: bool) {
        for b in out {
            *b = if sparse && self.next() & 3 != 0 {
                0
            } else {
                self.next()
            };
        }
    }
}

/// A mode 2 sector of `kind`, built the way a mastering tool would.
fn sector(index: u32, form: u8, noise: &mut Noise) -> [u8; SECTOR] {
    let mut s = [0u8; SECTOR];
    s[1..11].fill(0xff);
    s[12..16].copy_from_slice(&msf_header(index, 150));
    let submode = if form == 2 { 0x64 } else { 0x08 };
    s[16..20].copy_from_slice(&[1, (index % 3) as u8, submode, 0x01]);
    let sub: [u8; 4] = s[16..20].try_into().unwrap();
    s[20..24].copy_from_slice(&sub);
    let end = if form == 2 { 24 + 2324 } else { 24 + 2048 };
    noise.fill(&mut s[24..end], true);
    if form == 2 {
        psx_iso::protect_mode2_form2(&mut s);
    } else {
        psx_iso::protect_mode2_form1(&mut s);
    }
    s
}

/// A mixed image: both forms, plus every shape the packer must keep verbatim.
fn image(sectors: u32) -> Vec<[u8; SECTOR]> {
    let mut noise = Noise(7);
    (0..sectors)
        .map(|i| match i % 11 {
            0..=3 => sector(i, 1, &mut noise),
            4..=6 => sector(i, 2, &mut noise),
            7 => {
                // Form 2 whose EDC field was left zero.
                let mut s = sector(i, 2, &mut noise);
                s[0x92c..0x930].fill(0);
                s
            }
            8 => {
                // A Form 1 sector with a damaged parity byte.
                let mut s = sector(i, 1, &mut noise);
                s[2100] ^= 0x55;
                s
            }
            9 => {
                // Audio: no sync, no structure.
                let mut s = [0u8; SECTOR];
                noise.fill(&mut s, false);
                s
            }
            _ => {
                // A header that is not this sector's time.
                let mut s = sector(i, 1, &mut noise);
                s[12] = 0x59;
                s
            }
        })
        .collect()
}

fn pack(sectors: &[[u8; SECTOR]], part_bytes: u64) -> (Vec<u8>, Vec<u8>, Stats) {
    let mut payload = Vec::new();
    let mut packer = Packer::new(&mut payload);
    for s in sectors {
        packer.push(s).unwrap();
    }
    let (index, stats) = packer.finish(part_bytes, 40).unwrap();
    (index, payload, stats)
}

#[test]
fn every_sector_round_trips_and_the_odd_ones_stay_verbatim() {
    let sectors = image(200);
    let (index_bytes, payload, stats) = pack(&sectors, 0);
    let index = Index::parse(&index_bytes).unwrap();
    assert_eq!(index.sector_count(), 200);
    assert_eq!(index.payload_bytes(), payload.len() as u64);
    assert!(stats.kinds.iter().all(|&n| n > 0), "{stats:?}");
    for chunk in 0..index.chunk_count() {
        let r = index.chunk_range(chunk);
        let stored = &payload[r.start as usize..r.end as usize];
        let inflated = index.inflate(chunk, stored).unwrap();
        for s in index.chunk_sectors(chunk) {
            assert_eq!(
                index.sector(&inflated, s),
                sectors[s as usize],
                "sector {s}"
            );
            let head: [u8; 8] = sectors[s as usize][12..20].try_into().unwrap();
            assert_eq!(index.head8(s), Some(head), "head8 of {s}");
        }
    }
    assert_eq!(index.head8(200), None);
}

#[test]
fn the_index_records_the_image_hash_the_emulator_computes() {
    let sectors = image(70);
    let (index_bytes, _, _) = pack(&sectors, 0);
    let flat: Vec<u8> = sectors.iter().flatten().copied().collect();
    assert_eq!(
        Index::parse(&index_bytes).unwrap().image_hash(),
        emulator_core::game_image_hash(&flat)
    );
}

#[test]
fn a_partial_last_chunk_and_boot_chunks_are_recorded() {
    let sectors = image(70);
    let (index_bytes, _, _) = pack(&sectors, 12345);
    let index = Index::parse(&index_bytes).unwrap();
    assert_eq!(index.chunk_count(), 3);
    assert_eq!(index.chunk_sectors(2), 64..70);
    assert_eq!(index.boot_chunks(), 2);
    assert_eq!(index.part_bytes(), 12345);
    assert_eq!(index.boot_bytes(), index.chunk_range(1).end);
}

#[test]
fn incompressible_chunks_are_stored_and_still_read() {
    let mut noise = Noise(99);
    let sectors: Vec<[u8; SECTOR]> = (0..32)
        .map(|_| {
            let mut s = [0u8; SECTOR];
            noise.fill(&mut s, false);
            s
        })
        .collect();
    let (index_bytes, payload, stats) = pack(&sectors, 0);
    assert_eq!(stats.stored_chunks, 1);
    let index = Index::parse(&index_bytes).unwrap();
    assert_eq!(payload.len(), 32 * SECTOR);
    let chunk = index.inflate(0, &payload).unwrap();
    assert_eq!(index.sector(&chunk, 17), sectors[17]);
}

#[test]
fn a_damaged_chunk_is_refused() {
    let sectors = image(40);
    let (index_bytes, mut payload, _) = pack(&sectors, 0);
    let index = Index::parse(&index_bytes).unwrap();
    let r = index.chunk_range(0);
    payload[r.start as usize + 3] ^= 1;
    let stored = &payload[r.start as usize..r.end as usize];
    assert!(index.inflate(0, stored).is_err());
    assert!(index.inflate(0, &stored[..stored.len() - 1]).is_err());
    assert!(index.inflate(9, stored).is_err());
}

#[test]
fn a_damaged_index_is_refused() {
    let (index_bytes, _, _) = pack(&image(40), 0);
    assert!(Index::parse(&index_bytes[..index_bytes.len() - 5]).is_err());
    assert!(Index::parse(&index_bytes[..10]).is_err());
    let mut bad = index_bytes.clone();
    bad[0] = b'X';
    assert!(Index::parse(&bad).is_err());
    let mut bad = index_bytes;
    bad[4] = 9;
    assert!(Index::parse(&bad).is_err());
}

#[test]
fn files_verify_against_the_image_and_a_flipped_byte_fails() {
    let dir = std::env::temp_dir().join(format!("psx-webdisc-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let sectors = image(150);
    let part = 20_000u64;
    let (index_bytes, payload, _) = pack(&sectors, part);
    let index = Index::parse(&index_bytes).unwrap();
    for (n, piece) in payload.chunks(part as usize).enumerate() {
        std::fs::write(dir.join(part_name("t.pxw", n as u64 + 1)), piece).unwrap();
    }
    let flat: Vec<u8> = sectors.iter().flatten().copied().collect();
    let mut files = PayloadFiles::open(&dir, "t.pxw", &index).unwrap();
    assert_eq!(
        verify(&index, &mut files, &mut flat.as_slice()).unwrap(),
        150
    );

    let mut other = flat.clone();
    other[5000] ^= 1;
    let mut files = PayloadFiles::open(&dir, "t.pxw", &index).unwrap();
    assert!(verify(&index, &mut files, &mut other.as_slice()).is_err());
    let mut files = PayloadFiles::open(&dir, "t.pxw", &index).unwrap();
    assert!(verify(
        &index,
        &mut files,
        &mut flat[..flat.len() - SECTOR].as_ref()
    )
    .is_err());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_disc_that_starts_at_time_zero_gets_a_zero_bias() {
    let mut noise = Noise(3);
    let sectors: Vec<[u8; SECTOR]> = (0..10)
        .map(|i| {
            let mut s = sector(i, 1, &mut noise);
            s[12..16].copy_from_slice(&msf_header(i, 0));
            psx_iso::protect_mode2_form1(&mut s);
            s
        })
        .collect();
    let (index_bytes, payload, stats) = pack(&sectors, 0);
    let index = Index::parse(&index_bytes).unwrap();
    assert_eq!(index.lba_bias(), 0);
    assert_eq!(stats.kinds[Kind::Verbatim as usize], 0);
    let chunk = index.inflate(0, &payload).unwrap();
    assert_eq!(index.sector(&chunk, 9), sectors[9]);
}
