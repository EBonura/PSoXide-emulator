// SPDX-License-Identifier: GPL-2.0-or-later
//! `SYSTEM.CNF` parsing and boot-executable discovery for disc boot.
//!
//! The BIOS reads `SYSTEM.CNF` from the ISO9660 root, loads the executable
//! named by `BOOT`, and applies `TCB`, `EVENT` and `STACK`. This module
//! reproduces the parts a fast boot needs, as a pure function over the disc
//! image, so both the HLE boot and the warm real-BIOS fast boot agree.
//!
//! Sources: psx-spx "CDROM File Playstation EXE and SYSTEM.CNF" (key names,
//! hexadecimal values, defaults, the 128-byte `BOOT` argument copied to
//! `0x180`, `PSX.EXE` when `SYSTEM.CNF` is absent) and black-box
//! measurements of the retail BIOS at EXE entry (the `STACK` prefix quirk
//! and the stack it falls back to). No BIOS code or data is involved.
//!
//! This belongs in `psx-iso` eventually; it lives here until the SDK crate
//! grows a file-by-path API and a `BOOT` parser that accepts an argument.

use psx_iso::{BootError, Disc, Exe, SECTOR_USER_DATA_BYTES};

/// Thread control blocks when `SYSTEM.CNF` has no `TCB` line (psx-spx).
pub const DEFAULT_TCB: u32 = 4;
/// Event control blocks when `SYSTEM.CNF` has no `EVENT` line (psx-spx:
/// "EVENT = 10", hexadecimal).
pub const DEFAULT_EVENT: u32 = 0x10;
/// Stack top when `SYSTEM.CNF` has no `STACK` line (psx-spx).
pub const DEFAULT_STACK: u32 = 0x801F_FF00;
/// Kernel RAM address that receives the optional `BOOT` argument.
pub const BOOT_ARG_ADDR: u32 = 0x0000_0180;
/// Size of the argument area at [`BOOT_ARG_ADDR`], terminator included.
pub const BOOT_ARG_BYTES: usize = 128;

/// `$sp` at EXE entry when `STACK` evaluates to 0. The BIOS then keeps the
/// caller's stack (psx-spx); this is the value measured at entry on every
/// census disc whose `STACK` has a `0x` prefix.
pub const CALLER_STACK_SP: u32 = 0x801F_FDD8;
/// `$fp` measured at EXE entry in the same case.
pub const CALLER_STACK_FP: u32 = 0x801F_FF00;

/// Parsed `SYSTEM.CNF` (or the defaults the BIOS uses without one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SystemCnf {
    /// Boot path, normalized to upper-case ISO9660 components joined with
    /// `\` and carrying a version (for example `SLUS_005.94;1`).
    pub boot_path: String,
    /// Optional argument that follows the path on the `BOOT` line.
    pub boot_arg: Option<String>,
    /// `TCB` value (hexadecimal in the file).
    pub tcb: u32,
    /// `EVENT` value (hexadecimal in the file).
    pub event: u32,
    /// `STACK` value as the BIOS evaluates it. A `0x` prefix makes this 0,
    /// which means "keep the caller's stack".
    pub stack: u32,
}

impl SystemCnf {
    /// Defaults used when the disc has no `SYSTEM.CNF`.
    pub fn psx_exe_defaults() -> Self {
        Self {
            boot_path: "PSX.EXE;1".to_string(),
            boot_arg: None,
            tcb: DEFAULT_TCB,
            event: DEFAULT_EVENT,
            stack: DEFAULT_STACK,
        }
    }

    /// Parse `SYSTEM.CNF` text. Returns `None` without a `BOOT` line.
    ///
    /// Keys are case-insensitive and may be padded with spaces or tabs.
    /// Numbers are read the way the retail BIOS reads them: leading
    /// hexadecimal digits up to the first other character. That is why
    /// `STACK = 0x801FFFF0` evaluates to 0 (measured on SCPH1001 with
    /// Metal Gear Solid and Resident Evil 2/3).
    pub fn parse(bytes: &[u8]) -> Option<Self> {
        let mut cnf = Self::psx_exe_defaults();
        let mut have_boot = false;
        for line in bytes.split(|&b| b == b'\n' || b == b'\r') {
            let Some(eq) = line.iter().position(|&b| b == b'=') else {
                continue;
            };
            let key = trim(&line[..eq]);
            let value = trim(&line[eq + 1..]);
            if key.eq_ignore_ascii_case(b"BOOT") {
                let text = String::from_utf8_lossy(value);
                let mut parts = text.splitn(2, [' ', '\t']);
                let path = parts.next().unwrap_or_default();
                cnf.boot_path = normalize_path(path).join("\\");
                cnf.boot_arg = parts
                    .next()
                    .map(|arg| arg.trim().to_string())
                    .filter(|arg| !arg.is_empty());
                have_boot = true;
            } else if key.eq_ignore_ascii_case(b"TCB") {
                cnf.tcb = parse_hex_prefix(value);
            } else if key.eq_ignore_ascii_case(b"EVENT") {
                cnf.event = parse_hex_prefix(value);
            } else if key.eq_ignore_ascii_case(b"STACK") {
                cnf.stack = parse_hex_prefix(value);
            }
        }
        have_boot.then_some(cnf)
    }

    /// `($sp, $fp)` at EXE entry. The executable header's stack fields are
    /// ignored on disc boot; `STACK` replaces them.
    pub fn entry_stack(&self) -> (u32, u32) {
        if self.stack == 0 {
            (CALLER_STACK_SP, CALLER_STACK_FP)
        } else {
            (self.stack, self.stack)
        }
    }

    /// Bytes to place at [`BOOT_ARG_ADDR`]: the argument, clipped to leave
    /// room for its terminator.
    pub fn boot_arg_bytes(&self) -> Option<Vec<u8>> {
        let arg = self.boot_arg.as_ref()?;
        let mut bytes: Vec<u8> = arg.bytes().take(BOOT_ARG_BYTES - 1).collect();
        bytes.push(0);
        Some(bytes)
    }
}

/// Everything a disc boot needs: the parsed configuration and executable.
#[derive(Debug)]
pub struct DiscBoot {
    /// Parsed `SYSTEM.CNF`, or the `PSX.EXE` defaults.
    pub cnf: SystemCnf,
    /// Boot executable.
    pub exe: Exe,
}

/// Read `SYSTEM.CNF` and the boot executable it names. A disc without
/// `SYSTEM.CNF` boots `PSX.EXE` with the default settings.
pub fn load_disc_boot(disc: &Disc) -> Result<DiscBoot, BootError> {
    let cnf = match read_file(disc, "SYSTEM.CNF;1") {
        Ok(bytes) => SystemCnf::parse(&bytes).ok_or(BootError::MissingBootPath)?,
        Err(BootError::FileNotFound(_)) => SystemCnf::psx_exe_defaults(),
        Err(e) => return Err(e),
    };
    let exe = Exe::parse(&read_file(disc, &cnf.boot_path)?)?;
    Ok(DiscBoot { cnf, exe })
}

/// Read a file from the disc's ISO9660 tree. `path` may carry a `cdrom:`
/// prefix, `\` or `/` separators and an optional `;1` version.
pub fn read_file(disc: &Disc, path: &str) -> Result<Vec<u8>, BootError> {
    let (extent_lba, size) = file_extent(disc, path)?;
    read_extent(disc, extent_lba, size)
}

/// Where a file lives on the disc: its first sector and its size in bytes.
pub fn file_extent(disc: &Disc, path: &str) -> Result<(u32, u32), BootError> {
    let components = normalize_path(path);
    let Some((last, parents)) = components.split_last() else {
        return Err(BootError::FileNotFound(path.to_string()));
    };
    let mut dir = root_directory(disc)?;
    for component in parents {
        dir = find_child(disc, &dir, component)?
            .ok_or_else(|| BootError::FileNotFound(path.to_string()))?;
        if !dir.is_dir() {
            return Err(BootError::NotDirectory(component.clone()));
        }
    }
    let file =
        find_child(disc, &dir, last)?.ok_or_else(|| BootError::FileNotFound(path.to_string()))?;
    Ok((file.extent_lba, file.size))
}

fn parse_hex_prefix(value: &[u8]) -> u32 {
    value
        .iter()
        .map_while(|&b| (b as char).to_digit(16))
        .fold(0u32, |acc, digit| (acc << 4) | digit)
}

fn trim(bytes: &[u8]) -> &[u8] {
    let is_space = |b: &u8| matches!(b, b'\0' | b'\t' | b' ' | 0x0B | 0x0C | b'"');
    let start = bytes
        .iter()
        .position(|b| !is_space(b))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !is_space(b))
        .map_or(start, |i| i + 1);
    &bytes[start..end.max(start)]
}

pub(crate) fn normalize_path(path: &str) -> Vec<String> {
    let upper = path.trim().trim_matches('\0').trim().to_ascii_uppercase();
    let rest = upper
        .strip_prefix("CDROM0:")
        .or_else(|| upper.strip_prefix("CDROM:"))
        .unwrap_or(&upper);
    rest.split(['\\', '/'])
        .filter(|component| !component.is_empty())
        .map(|component| {
            if component.contains(';') {
                component.to_string()
            } else {
                format!("{component};1")
            }
        })
        .collect()
}

const PVD_LBA: u32 = 16;
const ROOT_RECORD_OFFSET: usize = 156;

#[derive(Clone, Debug)]
struct DirEntry {
    extent_lba: u32,
    size: u32,
    flags: u8,
    identifier: String,
}

impl DirEntry {
    fn is_dir(&self) -> bool {
        self.flags & 0x02 != 0
    }
}

fn root_directory(disc: &Disc) -> Result<DirEntry, BootError> {
    let pvd = disc
        .read_sector_user(PVD_LBA)
        .ok_or(BootError::MissingPrimaryVolumeDescriptor)?;
    if pvd[0] != 1 || &pvd[1..6] != b"CD001" || pvd[6] != 1 {
        return Err(BootError::BadPrimaryVolumeDescriptor);
    }
    parse_dir_record(&pvd[ROOT_RECORD_OFFSET..]).ok_or(BootError::BadRootDirectoryRecord)
}

fn find_child(disc: &Disc, dir: &DirEntry, component: &str) -> Result<Option<DirEntry>, BootError> {
    let bytes = read_extent(disc, dir.extent_lba, dir.size)?;
    let wanted = component.split(';').next().unwrap_or(component);
    let mut offset = 0usize;
    while offset < bytes.len() {
        let len = bytes[offset] as usize;
        if len == 0 {
            // Records never span sectors; skip the zero padding.
            offset = (offset / SECTOR_USER_DATA_BYTES + 1) * SECTOR_USER_DATA_BYTES;
            continue;
        }
        let record = bytes
            .get(offset..offset + len)
            .ok_or(BootError::BadDirectoryRecord)?;
        let entry = parse_dir_record(record).ok_or(BootError::BadDirectoryRecord)?;
        let name = entry.identifier.to_ascii_uppercase();
        if name.split(';').next() == Some(wanted) {
            return Ok(Some(entry));
        }
        offset += len;
    }
    Ok(None)
}

fn parse_dir_record(record: &[u8]) -> Option<DirEntry> {
    let len = *record.first()? as usize;
    if len < 34 || record.len() < len {
        return None;
    }
    let ident_len = record[32] as usize;
    let identifier = record.get(33..33 + ident_len)?;
    Some(DirEntry {
        extent_lba: u32::from_le_bytes(record[2..6].try_into().ok()?),
        size: u32::from_le_bytes(record[10..14].try_into().ok()?),
        flags: record[25],
        identifier: String::from_utf8_lossy(identifier).into_owned(),
    })
}

fn read_extent(disc: &Disc, extent_lba: u32, size: u32) -> Result<Vec<u8>, BootError> {
    let mut out = Vec::with_capacity(size as usize);
    for sector in 0..size.div_ceil(SECTOR_USER_DATA_BYTES as u32) {
        let data = disc
            .read_sector_user(extent_lba.saturating_add(sector))
            .ok_or(BootError::DirectoryExtentUnreadable { extent_lba, size })?;
        out.extend_from_slice(data);
    }
    out.truncate(size as usize);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use psx_iso::{IsoBuilder, EXE_HEADER_BYTES};

    fn exe(pc: u32) -> Vec<u8> {
        let mut exe = vec![0u8; EXE_HEADER_BYTES];
        exe[..8].copy_from_slice(b"PS-X EXE");
        exe[0x10..0x14].copy_from_slice(&pc.to_le_bytes());
        exe[0x18..0x1C].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        exe[0x1C..0x20].copy_from_slice(&4u32.to_le_bytes());
        exe[0x30..0x34].copy_from_slice(&0x801F_FF00u32.to_le_bytes());
        exe.extend_from_slice(&[0; 4]);
        exe
    }

    #[test]
    fn parses_keys_as_leading_hex_digits() {
        let cnf = SystemCnf::parse(
            b"BOOT = cdrom:\\SLUS_005.94;1\r\nTCB = 4\r\nEVENT = 10\r\nSTACK = 801FFFF0\r\n",
        )
        .unwrap();
        assert_eq!(cnf.boot_path, "SLUS_005.94;1");
        assert_eq!(cnf.boot_arg, None);
        assert_eq!((cnf.tcb, cnf.event, cnf.stack), (4, 0x10, 0x801F_FFF0));
        assert_eq!(cnf.entry_stack(), (0x801F_FFF0, 0x801F_FFF0));
    }

    #[test]
    fn prefixed_stack_evaluates_to_zero_and_keeps_the_caller_stack() {
        for text in [&b"STACK = 0x801FFFF0"[..], b"STACK=0X801FFF00"] {
            let mut bytes = b"BOOT = cdrom:\\GAME.EXE;1\r\n".to_vec();
            bytes.extend_from_slice(text);
            let cnf = SystemCnf::parse(&bytes).unwrap();
            assert_eq!(cnf.stack, 0);
            assert_eq!(cnf.entry_stack(), (CALLER_STACK_SP, CALLER_STACK_FP));
        }
    }

    #[test]
    fn missing_keys_use_documented_defaults() {
        let cnf = SystemCnf::parse(b"boot=cdrom:\\dir\\foo.exe").unwrap();
        assert_eq!(cnf.boot_path, "DIR;1\\FOO.EXE;1");
        assert_eq!((cnf.tcb, cnf.event, cnf.stack), (4, 0x10, 0x801F_FF00));
        assert!(SystemCnf::parse(b"TCB = 4\r\n").is_none());
    }

    #[test]
    fn boot_argument_is_split_from_the_path_and_clipped() {
        let cnf = SystemCnf::parse(b"BOOT = cdrom:\\PSX.EXE;1 -level 3\r\n").unwrap();
        assert_eq!(cnf.boot_path, "PSX.EXE;1");
        assert_eq!(cnf.boot_arg.as_deref(), Some("-level 3"));
        assert_eq!(cnf.boot_arg_bytes().unwrap(), b"-level 3\0");

        let long = format!("BOOT = cdrom:\\PSX.EXE;1\t{}", "x".repeat(300));
        let bytes = SystemCnf::parse(long.as_bytes())
            .unwrap()
            .boot_arg_bytes()
            .unwrap();
        assert_eq!(bytes.len(), BOOT_ARG_BYTES);
        assert_eq!(bytes.last(), Some(&0));
    }

    #[test]
    fn loads_boot_exe_through_system_cnf_or_psx_exe() {
        let mut builder = IsoBuilder::new();
        builder.add_file(
            "SYSTEM.CNF",
            b"BOOT = cdrom:\\GAME.EXE;1 arg\r\nSTACK = 0x801FFFF0\r\n".to_vec(),
        );
        builder.add_file("GAME.EXE", exe(0x8001_2340));
        let boot = load_disc_boot(&Disc::from_bin(builder.build_bin())).unwrap();
        assert_eq!(boot.cnf.boot_path, "GAME.EXE;1");
        assert_eq!(boot.cnf.boot_arg.as_deref(), Some("arg"));
        assert_eq!(boot.cnf.stack, 0);
        assert_eq!(boot.exe.initial_pc, 0x8001_2340);

        let mut builder = IsoBuilder::new();
        builder.add_file("PSX.EXE", exe(0x8001_5000));
        let boot = load_disc_boot(&Disc::from_bin(builder.build_bin())).unwrap();
        assert_eq!(boot.cnf, SystemCnf::psx_exe_defaults());
        assert_eq!(boot.exe.initial_pc, 0x8001_5000);
    }
}
