//! Host file access for development builds: the dev kit's "PC link".
//!
//! Games built for a 1990s development kit loaded their data from the
//! developer's PC instead of a disc. The guest reaches the host through
//! `BREAK` instructions with a function code in the instruction's code
//! field, then reads the result back from `v0`/`v1`. The convention below
//! was read off the call sites in NMS Software's 1996 builds (360X
//! `MAIN.CPE`): the library wrapper shifts its C arguments up one register
//! before the `BREAK`, so the host sees them in `a1`..`a3`, and the
//! wrapper returns `v1` when `v0` is zero and -1 otherwise.
//!
//! | Code  | Call   | a1        | a2           | a3     | v1 on success   |
//! |-------|--------|-----------|--------------|--------|-----------------|
//! | 0x101 | init   |           |              |        |                 |
//! | 0x102 | creat  | path ptr  | attributes   |        | handle          |
//! | 0x103 | open   | path ptr  | mode 0/1/2   |        | handle          |
//! | 0x104 | close  | handle    |              |        |                 |
//! | 0x105 | read   | handle    | length       | buffer | bytes read      |
//! | 0x106 | write  | handle    | length       | buffer | bytes written   |
//! | 0x107 | lseek  | handle    | offset       | whence | new position    |
//!
//! Off unless a launcher calls [`crate::bus::Bus::enable_pcdrv`]; without it
//! `BREAK` raises the exception as before. Paths resolve inside the host
//! root only: drive letters and `..` are dropped, separators and letter
//! case are matched loosely because the original paths are DOS paths.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// First `BREAK` code this device answers (init).
pub const FIRST_CODE: u32 = 0x101;
/// Last `BREAK` code this device answers (lseek).
pub const LAST_CODE: u32 = 0x107;

/// `pollhost()`: `BREAK 0x400` once the library has seen a host attached
/// (NMS's 360X calls it every frame). The dev kit's debug monitor checked
/// for debugger commands and resumed; with the PC link on it is a no-op.
pub const POLLHOST_CODE: u32 = 0x400;

/// Value the guest sees in `v0` when a call fails.
const FAIL: u32 = u32::MAX;

/// The PC link device: a host folder and the files the guest has open in it,
/// indexed by the handle the guest holds.
pub struct PcDrv {
    root: PathBuf,
    files: Vec<Option<File>>,
    /// Log every call to stderr (`PSOXIDE_PCDRV_TRACE=1`).
    trace: bool,
}

impl PcDrv {
    /// Serve files from `root`.
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            files: Vec::new(),
            trace: std::env::var_os("PSOXIDE_PCDRV_TRACE").is_some(),
        }
    }

    /// The host folder files are served from.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Run one call. `args` are `a1`, `a2`, `a3`; `ram` is main RAM.
    /// Returns `(v0, v1)`.
    pub fn call(&mut self, code: u32, args: [u32; 3], ram: &mut [u8]) -> (u32, u32) {
        let [a1, a2, a3] = args;
        let result = match code {
            0x101 => Some(0),
            0x102 => self.open(ram, a1, true),
            0x103 => self.open(ram, a1, false).or_else(|| {
                // Mode 1 and 2 may create the file, as the dev kit host did.
                if a2 & 3 != 0 {
                    self.open(ram, a1, true)
                } else {
                    None
                }
            }),
            0x104 => self.close(a1),
            0x105 => self.read(ram, a1, a2, a3),
            0x106 => self.write(ram, a1, a2, a3),
            0x107 => self.seek(a1, a2, a3),
            _ => None,
        };
        if self.trace {
            let name = match code {
                0x101 => "init".to_string(),
                0x102 | 0x103 => format!("open {:?}", read_c_string(ram, a1)),
                _ => format!("{code:#x}"),
            };
            eprintln!("[pcdrv] {name} a1={a1:#x} a2={a2:#x} a3={a3:#x} -> {result:?}");
        }
        match result {
            Some(v1) => (0, v1),
            None => (FAIL, 0),
        }
    }

    fn open(&mut self, ram: &[u8], name_ptr: u32, create: bool) -> Option<u32> {
        let name = read_c_string(ram, name_ptr)?;
        let path = if create {
            self.host_path_for_create(&name)?
        } else {
            self.find_host_path(&name)?
        };
        let file = if create {
            OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(true)
                .open(path)
                .ok()?
        } else {
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .or_else(|_| File::open(&path))
                .ok()?
        };
        let slot = match self.files.iter().position(Option::is_none) {
            Some(slot) => slot,
            None => {
                self.files.push(None);
                self.files.len() - 1
            }
        };
        self.files[slot] = Some(file);
        Some(slot as u32)
    }

    fn close(&mut self, handle: u32) -> Option<u32> {
        let slot = self.files.get_mut(handle as usize)?;
        slot.take().map(|_| 0)
    }

    fn read(&mut self, ram: &mut [u8], handle: u32, len: u32, buf: u32) -> Option<u32> {
        let file = self.files.get_mut(handle as usize)?.as_mut()?;
        let mut data = vec![0u8; len as usize];
        let mut got = 0;
        while got < data.len() {
            match file.read(&mut data[got..]) {
                Ok(0) => break,
                Ok(n) => got += n,
                Err(_) => return None,
            }
        }
        write_ram(ram, buf, &data[..got]);
        Some(got as u32)
    }

    fn write(&mut self, ram: &[u8], handle: u32, len: u32, buf: u32) -> Option<u32> {
        let file = self.files.get_mut(handle as usize)?.as_mut()?;
        let data = read_ram(ram, buf, len as usize);
        file.write_all(&data).ok()?;
        Some(len)
    }

    fn seek(&mut self, handle: u32, offset: u32, whence: u32) -> Option<u32> {
        let file = self.files.get_mut(handle as usize)?.as_mut()?;
        let from = match whence {
            0 => SeekFrom::Start(offset as u64),
            1 => SeekFrom::Current(offset as i32 as i64),
            2 => SeekFrom::End(offset as i32 as i64),
            _ => return None,
        };
        file.seek(from).ok().map(|pos| pos as u32)
    }

    /// The guest's DOS-style path split into its components, with any
    /// drive letter, root and `..` dropped.
    fn components(name: &str) -> Vec<String> {
        let name = name.trim();
        let name = match name.find(':') {
            Some(colon) => &name[colon + 1..],
            None => name,
        };
        name.split(['\\', '/'])
            .filter(|part| !part.is_empty() && *part != "." && *part != "..")
            .map(|part| part.trim_end_matches(";1").to_string())
            .collect()
    }

    /// Resolve an existing file, matching each component case-insensitively.
    fn find_host_path(&self, name: &str) -> Option<PathBuf> {
        let mut path = self.root.clone();
        for part in Self::components(name) {
            let exact = path.join(&part);
            if exact.exists() {
                path = exact;
                continue;
            }
            let wanted = part.to_ascii_lowercase();
            let entry = std::fs::read_dir(&path)
                .ok()?
                .flatten()
                .find(|entry| entry.file_name().to_string_lossy().to_ascii_lowercase() == wanted)?;
            path = entry.path();
        }
        path.is_file().then_some(path)
    }

    /// Where a new file goes: existing directories matched as above, the
    /// file name kept as the guest spelled it.
    fn host_path_for_create(&self, name: &str) -> Option<PathBuf> {
        let mut parts = Self::components(name);
        let file = parts.pop()?;
        let dir = if parts.is_empty() {
            self.root.clone()
        } else {
            let joined = parts.join("/");
            let mut path = self.root.clone();
            for part in joined.split('/') {
                let wanted = part.to_ascii_lowercase();
                path = std::fs::read_dir(&path)
                    .ok()?
                    .flatten()
                    .find(|entry| {
                        entry.file_name().to_string_lossy().to_ascii_lowercase() == wanted
                    })?
                    .path();
            }
            path
        };
        Some(dir.join(file))
    }
}

/// Main RAM is 2 MiB and mirrored; KUSEG/KSEG0/KSEG1 all map onto it.
fn ram_index(ram: &[u8], addr: u32) -> usize {
    (addr & 0x1fff_ffff) as usize % ram.len()
}

fn read_c_string(ram: &[u8], addr: u32) -> Option<String> {
    let mut out = Vec::new();
    for i in 0..256u32 {
        let byte = ram[ram_index(ram, addr.wrapping_add(i))];
        if byte == 0 {
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
        out.push(byte);
    }
    None
}

fn read_ram(ram: &[u8], addr: u32, len: usize) -> Vec<u8> {
    (0..len as u32)
        .map(|i| ram[ram_index(ram, addr.wrapping_add(i))])
        .collect()
}

fn write_ram(ram: &mut [u8], addr: u32, data: &[u8]) {
    for (i, byte) in data.iter().enumerate() {
        let at = ram_index(ram, addr.wrapping_add(i as u32));
        ram[at] = *byte;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put_str(ram: &mut [u8], addr: u32, text: &str) {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        write_ram(ram, addr, &bytes);
    }

    #[test]
    fn open_read_seek_close_round_trip() {
        let dir = std::env::temp_dir().join(format!("pcdrv-test-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("Levels")).unwrap();
        std::fs::write(dir.join("Levels").join("Lev1.Bin"), b"0123456789").unwrap();
        let mut ram = vec![0u8; 2 * 1024 * 1024];
        let mut drv = PcDrv::new(dir.clone());
        assert_eq!(drv.call(0x101, [0, 0, 0], &mut ram), (0, 0));

        put_str(&mut ram, 0x8001_0000, "C:\\LEVELS\\LEV1.BIN");
        let (v0, handle) = drv.call(0x103, [0x8001_0000, 0, 0], &mut ram);
        assert_eq!(v0, 0);
        assert_eq!(drv.call(0x107, [handle, 0, 2], &mut ram), (0, 10));
        assert_eq!(drv.call(0x107, [handle, 2, 0], &mut ram), (0, 2));
        assert_eq!(drv.call(0x105, [handle, 4, 0x8002_0000], &mut ram), (0, 4));
        assert_eq!(&ram[0x2_0000..0x2_0004], b"2345");
        assert_eq!(drv.call(0x104, [handle, 0, 0], &mut ram), (0, 0));
        assert_eq!(drv.call(0x105, [handle, 4, 0x8002_0000], &mut ram).0, FAIL);

        put_str(&mut ram, 0x8001_0000, "..\\missing.bin");
        assert_eq!(drv.call(0x103, [0x8001_0000, 0, 0], &mut ram).0, FAIL);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
