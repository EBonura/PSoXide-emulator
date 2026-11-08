//! SN Systems `.CPE` executables: the format the 1990s PlayStation dev
//! kit linker wrote and its PC-side loader sent to the target board.
//!
//! A CPE file is the magic `CPE\x01` followed by tagged chunks:
//!
//! | Tag | Body                                 | Meaning                    |
//! |-----|--------------------------------------|----------------------------|
//! | 0   |                                      | end of file                |
//! | 1   | u32 address, u32 length, bytes       | load bytes at address      |
//! | 3   | u16 register, u32 value              | set a register (0x90 = PC) |
//! | 8   | u8 unit                              | select target unit         |
//!
//! All values are little-endian. The layout was read off NMS Software's
//! 1996 builds (360X `MAIN.CPE`, QD Up `POOL.CPE`), which use exactly these
//! four chunk kinds.

/// Register number a tag-3 chunk uses for the program counter.
pub const REG_PC: u16 = 0x90;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Cpe {
    /// `(load address, bytes)` in file order; later chunks win on overlap.
    pub segments: Vec<(u32, Vec<u8>)>,
    /// Register assignments in file order.
    pub registers: Vec<(u16, u32)>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CpeError {
    BadMagic,
    Truncated { offset: usize },
    UnknownChunk { tag: u8, offset: usize },
    NoEntryPoint,
}

impl Cpe {
    pub fn parse(bytes: &[u8]) -> Result<Self, CpeError> {
        if bytes.get(..4) != Some(b"CPE\x01") {
            return Err(CpeError::BadMagic);
        }
        let mut cpe = Cpe {
            segments: Vec::new(),
            registers: Vec::new(),
        };
        let mut at = 4;
        let take = |at: usize, len: usize| -> Result<&[u8], CpeError> {
            bytes
                .get(at..at + len)
                .ok_or(CpeError::Truncated { offset: at })
        };
        loop {
            let tag_at = at;
            let tag = *take(at, 1)?.first().unwrap_or(&0);
            at += 1;
            match tag {
                0 => break,
                1 => {
                    let head = take(at, 8)?;
                    let addr = u32::from_le_bytes(head[0..4].try_into().unwrap_or_default());
                    let len = u32::from_le_bytes(head[4..8].try_into().unwrap_or_default());
                    at += 8;
                    let body = take(at, len as usize)?;
                    cpe.segments.push((addr, body.to_vec()));
                    at += len as usize;
                }
                3 => {
                    let body = take(at, 6)?;
                    let reg = u16::from_le_bytes([body[0], body[1]]);
                    let value = u32::from_le_bytes(body[2..6].try_into().unwrap_or_default());
                    cpe.registers.push((reg, value));
                    at += 6;
                }
                8 => {
                    take(at, 1)?;
                    at += 1;
                }
                tag => {
                    return Err(CpeError::UnknownChunk {
                        tag,
                        offset: tag_at,
                    })
                }
            }
        }
        if cpe.entry_point().is_none() {
            return Err(CpeError::NoEntryPoint);
        }
        Ok(cpe)
    }

    /// The last value assigned to the PC.
    pub fn entry_point(&self) -> Option<u32> {
        self.registers
            .iter()
            .rev()
            .find(|(reg, _)| *reg == REG_PC)
            .map(|(_, value)| *value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_load_register_and_unit_chunks() {
        let mut file = b"CPE\x01".to_vec();
        file.extend([8, 0]);
        file.push(3);
        file.extend(REG_PC.to_le_bytes());
        file.extend(0x8001_0000u32.to_le_bytes());
        file.push(1);
        file.extend(0x8001_0000u32.to_le_bytes());
        file.extend(3u32.to_le_bytes());
        file.extend([1, 2, 3]);
        file.push(0);
        let cpe = Cpe::parse(&file).unwrap();
        assert_eq!(cpe.entry_point(), Some(0x8001_0000));
        assert_eq!(cpe.segments, vec![(0x8001_0000, vec![1, 2, 3])]);
    }

    #[test]
    fn rejects_bad_input() {
        assert_eq!(Cpe::parse(b"PS-X EXE"), Err(CpeError::BadMagic));
        assert_eq!(
            Cpe::parse(b"CPE\x01\x01\0\0"),
            Err(CpeError::Truncated { offset: 5 })
        );
        assert_eq!(
            Cpe::parse(b"CPE\x01\x09"),
            Err(CpeError::UnknownChunk { tag: 9, offset: 4 })
        );
        assert_eq!(Cpe::parse(b"CPE\x01\x00"), Err(CpeError::NoEntryPoint));
    }
}
