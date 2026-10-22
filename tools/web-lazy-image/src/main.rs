//! Prototype: stage a raw disc image for the browser's lazy (HTTP Range) boot.
//!
//! The web build used to read a whole image before it could boot it, only to
//! learn two things about it: its input-tape hash and the 8 header bytes
//! (sector bytes 12..20) of every sector. This tool computes both once, at
//! press time, so the page can skip that read and fetch only the sectors the
//! game asks for.
//!
//!   web-lazy-image <image.bin> <outdir> [boot-sectors]
//!
//! writes `<outdir>/web-manifest.txt` and `<outdir>/lazy.hdr.gz` and prints
//! the manifest. The image itself is served as is, under the name given in
//! the manifest (`FILE`); link or copy it next to the manifest.
//!
//! Manifest line, consumed by `web_stream::parse_manifest`:
//!   `image FILE BYTES GAME_IMAGE_HASH HDR_FILE HDR_RAW_BYTES HDR_FNV32 BOOT_SECTORS`

use std::fs::{self, File};
use std::io::{BufReader, Read, Write};
use std::path::Path;

use flate2::write::GzEncoder;
use flate2::Compression;
use psx_iso::SECTOR_BYTES;

fn fnv1a32(data: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in data {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    h
}

fn main() -> Result<(), String> {
    let mut args = std::env::args().skip(1);
    let image = args
        .next()
        .ok_or("usage: web-lazy-image IMAGE OUTDIR [BOOT_SECTORS]")?;
    let out = args.next().ok_or("missing OUTDIR")?;
    let boot_sectors: u64 = args
        .next()
        .map_or(Ok(410), |v| v.parse().map_err(|_| "bad boot sectors"))?;
    let len = fs::metadata(&image)
        .map_err(|e| format!("{image}: {e}"))?
        .len();
    if len % SECTOR_BYTES as u64 != 0 {
        return Err(format!("{image}: not whole 2352-byte sectors"));
    }
    let mut reader =
        BufReader::with_capacity(1 << 20, File::open(&image).map_err(|e| e.to_string())?);
    let mut hasher = emulator_core::GameImageHasher::new();
    let mut headers = Vec::with_capacity((len / SECTOR_BYTES as u64) as usize * 8);
    let mut buf = vec![0u8; SECTOR_BYTES * 892];
    loop {
        let mut n = 0;
        while n < buf.len() {
            let got = reader.read(&mut buf[n..]).map_err(|e| e.to_string())?;
            if got == 0 {
                break;
            }
            n += got;
        }
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        for sector in buf[..n].chunks_exact(SECTOR_BYTES) {
            headers.extend_from_slice(&sector[12..20]);
        }
    }
    let mut gz = GzEncoder::new(Vec::new(), Compression::new(9));
    gz.write_all(&headers).map_err(|e| e.to_string())?;
    let gz = gz.finish().map_err(|e| e.to_string())?;
    let out = Path::new(&out);
    fs::create_dir_all(out).map_err(|e| e.to_string())?;
    fs::write(out.join("lazy.hdr.gz"), &gz).map_err(|e| e.to_string())?;
    let manifest = format!(
        "image lazy.bin {len} {} lazy.hdr.gz {} {} {boot_sectors}\n",
        hasher.finish(),
        headers.len(),
        fnv1a32(&headers)
    );
    fs::write(out.join("web-manifest.txt"), &manifest).map_err(|e| e.to_string())?;
    print!("{manifest}");
    eprintln!(
        "sidecar: {} bytes raw, {} bytes gzipped, {} sectors",
        headers.len(),
        gz.len(),
        headers.len() / 8
    );
    Ok(())
}
