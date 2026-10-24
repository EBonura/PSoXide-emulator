//! `webdisc`: write, check and describe compact web discs.
//!
//! ```text
//! webdisc pack IMAGE OUTDIR NAME [--part-bytes N] [--boot-sectors N]
//! webdisc verify DIR NAME IMAGE
//! webdisc info INDEX
//! ```
//!
//! `pack` writes `OUTDIR/NAME.pxi` and `OUTDIR/NAME.pxw` (or `NAME.pxw.001`...
//! with `--part-bytes`), reads every file back and compares each sector with
//! IMAGE, then prints `key value` lines for the caller. It exits non-zero on
//! any difference.

use std::fs::{self, File};
use std::io::{self, BufReader, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use psx_webdisc::{part_name, verify, Index, Packer, PayloadFiles, SECTOR};

/// The default number of sectors a boot reads before its first frame, with a
/// little to spare (the demo disc's launcher reads about 390).
const DEFAULT_BOOT_SECTORS: u32 = 410;

fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for &b in bytes {
        h = (h ^ u32::from(b)).wrapping_mul(0x0100_0193);
    }
    h
}

/// Writes a byte stream into consecutive files of `part` bytes (one file when
/// `part` is 0).
struct PartWriter<'a> {
    dir: &'a Path,
    base: String,
    part: u64,
    index: u64,
    room: u64,
    file: Option<File>,
}

impl Write for PartWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.room == 0 {
            self.index += 1;
            let name = if self.part == 0 {
                self.base.clone()
            } else {
                part_name(&self.base, self.index)
            };
            self.file = Some(File::create(self.dir.join(name))?);
            self.room = if self.part == 0 { u64::MAX } else { self.part };
        }
        let take = (bytes.len() as u64).min(self.room) as usize;
        self.file
            .as_mut()
            .expect("open file")
            .write_all(&bytes[..take])?;
        self.room -= take as u64;
        Ok(take)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

fn pack(args: &[String]) -> Result<(), String> {
    let [image, outdir, name, rest @ ..] = args else {
        return Err(
            "usage: webdisc pack IMAGE OUTDIR NAME [--part-bytes N] [--boot-sectors N]".into(),
        );
    };
    let mut part_bytes = 0u64;
    let mut boot_sectors = DEFAULT_BOOT_SECTORS;
    let mut it = rest.iter();
    while let Some(flag) = it.next() {
        let mut number = |what: &str| -> Result<u64, String> {
            it.next()
                .and_then(|v| v.parse().ok())
                .ok_or_else(|| format!("{what} needs a number"))
        };
        match flag.as_str() {
            "--part-bytes" => part_bytes = number("--part-bytes")?,
            "--boot-sectors" => boot_sectors = number("--boot-sectors")? as u32,
            other => return Err(format!("unknown option {other}")),
        }
    }
    let dir = Path::new(outdir);
    fs::create_dir_all(dir).map_err(|e| format!("{outdir}: {e}"))?;
    let len = fs::metadata(image)
        .map_err(|e| format!("{image}: {e}"))?
        .len();
    if len == 0 || len % SECTOR as u64 != 0 {
        return Err(format!(
            "{image}: {len} bytes is not a whole number of sectors"
        ));
    }
    let payload_name = format!("{name}.pxw");
    let writer = PartWriter {
        dir,
        base: payload_name.clone(),
        part: part_bytes,
        index: 0,
        room: 0,
        file: None,
    };
    let mut packer = Packer::new(writer);
    let mut source =
        BufReader::with_capacity(1 << 20, File::open(image).map_err(|e| e.to_string())?);
    let mut sector = [0u8; SECTOR];
    for _ in 0..len / SECTOR as u64 {
        source
            .read_exact(&mut sector)
            .map_err(|e| format!("{image}: {e}"))?;
        packer.push(&sector).map_err(|e| e.to_string())?;
    }
    let (index_bytes, stats) = packer
        .finish(part_bytes, boot_sectors)
        .map_err(|e| e.to_string())?;
    let index_name = format!("{name}.pxi");
    fs::write(dir.join(&index_name), &index_bytes).map_err(|e| e.to_string())?;

    // Read everything back from the files on disk.
    let index = Index::parse(&fs::read(dir.join(&index_name)).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let sectors = check(dir, &payload_name, &index, image)?;

    println!("index_file {index_name}");
    println!("index_bytes {}", index_bytes.len());
    println!("index_fnv32 {}", fnv1a32(&index_bytes));
    println!("payload_file {payload_name}");
    println!("payload_bytes {}", index.payload_bytes());
    println!("part_bytes {}", index.part_bytes());
    println!("image_bytes {len}");
    println!("image_hash {}", index.image_hash());
    println!("sectors {sectors}");
    println!("chunks {}", index.chunk_count());
    println!("boot_chunks {}", index.boot_chunks());
    println!("boot_bytes {}", index.boot_bytes());
    println!(
        "kinds form1={} form2={} form2_no_edc={} verbatim={}",
        stats.kinds[0], stats.kinds[1], stats.kinds[2], stats.kinds[3]
    );
    println!("stored_chunks {}", stats.stored_chunks);
    Ok(())
}

fn check(dir: &Path, payload_name: &str, index: &Index, image: &str) -> Result<u64, String> {
    let mut files = PayloadFiles::open(dir, payload_name, index).map_err(|e| e.to_string())?;
    let mut source =
        BufReader::with_capacity(1 << 20, File::open(image).map_err(|e| e.to_string())?);
    verify(index, &mut files, &mut source).map_err(|e| e.to_string())
}

fn verify_cmd(args: &[String]) -> Result<(), String> {
    let [dir, name, image] = args else {
        return Err("usage: webdisc verify DIR NAME IMAGE".into());
    };
    let dir = Path::new(dir);
    let index =
        Index::parse(&fs::read(dir.join(format!("{name}.pxi"))).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let sectors = check(dir, &format!("{name}.pxw"), &index, image)?;
    println!(
        "verified {sectors} sectors, image hash {}",
        index.image_hash()
    );
    Ok(())
}

fn info(args: &[String]) -> Result<(), String> {
    let [path] = args else {
        return Err("usage: webdisc info INDEX".into());
    };
    let index =
        Index::parse(&fs::read(path).map_err(|e| e.to_string())?).map_err(|e| e.to_string())?;
    println!("sectors {}", index.sector_count());
    println!("chunks {}", index.chunk_count());
    println!("payload_bytes {}", index.payload_bytes());
    println!("part_bytes {}", index.part_bytes());
    println!("image_hash {}", index.image_hash());
    println!("lba_bias {}", index.lba_bias());
    println!("boot_chunks {}", index.boot_chunks());
    Ok(())
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.split_first() {
        Some((cmd, rest)) if cmd == "pack" => pack(rest),
        Some((cmd, rest)) if cmd == "verify" => verify_cmd(rest),
        Some((cmd, rest)) if cmd == "info" => info(rest),
        _ => Err("usage: webdisc pack|verify|info ...".into()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("webdisc: {message}");
            ExitCode::FAILURE
        }
    }
}
