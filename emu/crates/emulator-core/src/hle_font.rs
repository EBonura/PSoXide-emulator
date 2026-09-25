//! The character font behind B(51h) Krom2RawAdd and B(53h) Krom2Offset.
//!
//! Games ask the kernel where the bitmap of a Shift-JIS character is and
//! copy it from there (Chrono Cross draws its name-entry grid this way). A
//! retail kernel points into the font in Sony's ROM, which PSoXide does not
//! ship. This module serves an original font instead, drawn for PSoXide in
//! `hle_font_glyphs.txt`, from the same ROM area and in the same cell format,
//! so the games' own drawing code works unchanged:
//!
//! - a cell is 16x15 pixels at one bit per pixel, two bytes a row (first
//!   byte the left half, bit 7 the leftmost pixel): 30 bytes;
//! - bank 1, at ROM offset [`BANK1_ROM_OFFSET`], holds the non-kanji rows
//!   1-8 of JIS X 0208, only the cells the standard assigns, in JIS order
//!   (524 cells: symbols, full-width digits and Latin letters, kana, Greek,
//!   Cyrillic, box drawing);
//! - bank 2 follows it and holds the level-1 kanji, rows 16-47, 94 cells a
//!   row (row 47 has 51).
//!
//! Both layouts follow from the JIS X 0208 code chart, and psx-spx documents
//! the cell format, the two code ranges and the -1 answer outside them. A
//! code inside a range that the standard leaves unassigned resolves through
//! the run of consecutive codes before it, as a range-table lookup does.
//! Assigned cells with no glyph drawn yet show an outlined box, so a missing
//! character is visible rather than silently blank.

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Bytes per 16x15 cell.
pub const CELL_BYTES: usize = 30;
/// ROM offset of bank 1 (the address games receive is `0xBFC0_0000` plus
/// this plus the cell's offset).
pub const BANK1_ROM_OFFSET: usize = 0x6_6000;
/// Cells in bank 1.
pub const BANK1_CELLS: usize = 524;
/// ROM offset of bank 2, straight after bank 1.
pub const BANK2_ROM_OFFSET: usize = BANK1_ROM_OFFSET + BANK1_CELLS * CELL_BYTES;
/// Cells in bank 2: rows 16-46 full, row 47 to cell 51.
pub const BANK2_CELLS: usize = 31 * 94 + 51;

const ROM_BASE: u32 = 0xBFC0_0000;
const BANK1_CODES: std::ops::RangeInclusive<u16> = 0x8140..=0x84BE;
const BANK2_CODES: std::ops::RangeInclusive<u16> = 0x889F..=0x9872;

/// JIS X 0208 rows 1-8: the assigned cells of each row, as inclusive ranges.
const BANK1_ASSIGNED: [&[(u8, u8)]; 8] = [
    &[(1, 94)],                                                   // symbols
    &[(1, 14), (26, 33), (42, 48), (60, 74), (82, 89), (94, 94)], // symbols
    &[(16, 25), (33, 58), (65, 90)],                              // digits, Latin
    &[(1, 83)],                                                   // hiragana
    &[(1, 86)],                                                   // katakana
    &[(1, 24), (33, 56)],                                         // Greek
    &[(1, 33), (49, 81)],                                         // Cyrillic
    &[(1, 32)],                                                   // box drawing
];

/// Shift-JIS code of JIS row `row`, cell `cell` (both from 1).
fn sjis(row: u8, cell: u8) -> u16 {
    let lead = u16::from(row.div_ceil(2)) + 0x80;
    let trail = if row % 2 == 1 {
        u16::from(cell) + 0x3F + u16::from(cell >= 64)
    } else {
        u16::from(cell) + 0x9E
    };
    lead << 8 | trail
}

/// Bank 1 as runs of consecutive codes: (first code, its cell index).
fn bank1_runs() -> &'static [(u16, u16)] {
    static RUNS: OnceLock<Vec<(u16, u16)>> = OnceLock::new();
    RUNS.get_or_init(|| {
        let mut runs = Vec::new();
        let mut previous = None;
        let mut index = 0u16;
        for (row, ranges) in (1u8..).zip(BANK1_ASSIGNED) {
            for &(first, last) in ranges {
                for cell in first..=last {
                    let code = sjis(row, cell);
                    if previous.is_none_or(|p: u16| code != p + 1) {
                        runs.push((code, index));
                    }
                    previous = Some(code);
                    index += 1;
                }
            }
        }
        runs
    })
}

/// Cell of an assigned bank-1 code, if the standard assigns it.
fn bank1_assigned_cell(code: u16) -> Option<u16> {
    let mut index = 0u16;
    for (row, ranges) in (1u8..).zip(BANK1_ASSIGNED) {
        for &(first, last) in ranges {
            for cell in first..=last {
                if sjis(row, cell) == code {
                    return Some(index);
                }
                index += 1;
            }
        }
    }
    None
}

/// Bank 2 cell of a level-1 kanji code (94 cells a JIS row).
fn bank2_cell(code: u16) -> Option<u16> {
    if !BANK2_CODES.contains(&code) {
        return None;
    }
    let (lead, trail) = (code >> 8, code & 0xFF);
    let odd_row_cell = match trail {
        0x40..=0x7E => Some(trail - 0x3F),
        0x80..=0x9E => Some(trail - 0x40),
        _ => None,
    };
    let (row, cell) = match (odd_row_cell, trail) {
        (Some(cell), _) => ((lead - 0x80) * 2 - 1, cell),
        (None, 0x9F..=0xFC) => ((lead - 0x80) * 2, trail - 0x9E),
        _ => return None,
    };
    (16..=47).contains(&row).then(|| (row - 16) * 94 + cell - 1)
}

/// B(53h) Krom2Offset: the cell index of `code` within its bank.
pub fn krom2_offset(code: u32) -> u16 {
    let code = code as u16;
    if BANK1_CODES.contains(&code) {
        let runs = bank1_runs();
        let run = runs.partition_point(|&(first, _)| first <= code) - 1;
        let (first, index) = runs[run];
        index + (code - first)
    } else {
        bank2_cell(code).unwrap_or(0)
    }
}

/// B(51h) Krom2RawAdd: the address of `code`'s cell, or -1 outside both
/// banks.
pub fn krom2_raw_add(code: u32) -> u32 {
    let code16 = code as u16;
    let base = if BANK1_CODES.contains(&code16) {
        BANK1_ROM_OFFSET
    } else if bank2_cell(code16).is_some() {
        BANK2_ROM_OFFSET
    } else {
        return u32::MAX;
    };
    ROM_BASE + (base + usize::from(krom2_offset(code)) * CELL_BYTES) as u32
}

/// Parse the glyph file: a `XXXX name` line (Shift-JIS code in hex) followed
/// by 15 rows of 16 `#`/`.` characters; `;` starts a comment line.
fn parse_glyphs(text: &str) -> BTreeMap<u16, [u8; CELL_BYTES]> {
    let mut glyphs = BTreeMap::new();
    let mut lines = text
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty() && !line.starts_with(';'));
    while let Some(header) = lines.next() {
        let code = u16::from_str_radix(&header[..4], 16)
            .unwrap_or_else(|_| panic!("hle_font_glyphs.txt: bad header {header:?}"));
        let mut cell = [0u8; CELL_BYTES];
        for row in 0..15 {
            let bits = lines
                .next()
                .unwrap_or_else(|| panic!("hle_font_glyphs.txt: {code:04X} is short"));
            assert_eq!(bits.len(), 16, "hle_font_glyphs.txt: {code:04X} row {row}");
            let word = bits.bytes().fold(0u16, |word, b| match b {
                b'#' => word << 1 | 1,
                b'.' => word << 1,
                _ => panic!("hle_font_glyphs.txt: {code:04X} row {row} has {b:?}"),
            });
            cell[row * 2..row * 2 + 2].copy_from_slice(&word.to_be_bytes());
        }
        assert!(
            glyphs.insert(code, cell).is_none(),
            "hle_font_glyphs.txt: {code:04X} twice"
        );
    }
    glyphs
}

/// The drawn glyphs, by Shift-JIS code.
pub fn glyphs() -> &'static BTreeMap<u16, [u8; CELL_BYTES]> {
    static GLYPHS: OnceLock<BTreeMap<u16, [u8; CELL_BYTES]>> = OnceLock::new();
    GLYPHS.get_or_init(|| parse_glyphs(include_str!("hle_font_glyphs.txt")))
}

/// The placeholder for an assigned cell with no glyph drawn: an outlined
/// box, so missing characters show.
fn missing_glyph() -> [u8; CELL_BYTES] {
    let mut cell = [0u8; CELL_BYTES];
    for row in 1..14 {
        let word: u16 = if row == 1 || row == 13 {
            0x7FFE
        } else {
            0x4002
        };
        cell[row * 2..row * 2 + 2].copy_from_slice(&word.to_be_bytes());
    }
    cell
}

/// Write both banks into a 512 KiB ROM image.
pub fn install(rom: &mut [u8]) {
    let glyphs = glyphs();
    let missing = missing_glyph();
    let mut put = |offset: usize, cell: &[u8; CELL_BYTES]| {
        rom[offset..offset + CELL_BYTES].copy_from_slice(cell);
    };
    for (row, ranges) in (1u8..).zip(BANK1_ASSIGNED) {
        for &(first, last) in ranges {
            for cell in first..=last {
                let code = sjis(row, cell);
                let index = bank1_assigned_cell(code).expect("assigned") as usize;
                put(
                    BANK1_ROM_OFFSET + index * CELL_BYTES,
                    glyphs.get(&code).unwrap_or(&missing),
                );
            }
        }
    }
    for row in 16u8..=47 {
        let cells = if row == 47 { 51 } else { 94 };
        for cell in 1..=cells {
            let code = sjis(row, cell);
            let index = bank2_cell(code).expect("level-1 kanji") as usize;
            put(
                BANK2_ROM_OFFSET + index * CELL_BYTES,
                glyphs.get(&code).unwrap_or(&missing),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bank1_layout_follows_the_jis_chart() {
        // Space, full-width 0 and A, hiragana a, the last box-drawing cell.
        assert_eq!(krom2_offset(0x8140), 0);
        assert_eq!(krom2_offset(0x824F), 147);
        assert_eq!(krom2_offset(0x8260), 157);
        assert_eq!(krom2_offset(0x829F), 209);
        assert_eq!(krom2_offset(0x84BE), 523);
        assert_eq!(bank1_runs().len(), 19);
        assert_eq!(BANK2_ROM_OFFSET, BANK1_ROM_OFFSET + 0x3D68);
    }

    #[test]
    fn bank2_holds_level_1_kanji_row_by_row() {
        assert_eq!(bank2_cell(0x889F), Some(0));
        assert_eq!(bank2_cell(0x8940), Some(94));
        assert_eq!(bank2_cell(0x8980), Some(94 + 63));
        assert_eq!(bank2_cell(0x899F), Some(188));
        assert_eq!(bank2_cell(0x9872), Some(BANK2_CELLS as u16 - 1));
        assert_eq!(bank2_cell(0x9873), None);
        const { assert!(BANK2_ROM_OFFSET + BANK2_CELLS * CELL_BYTES <= 0x8_0000) };
    }

    #[test]
    fn raw_add_answers_minus_one_outside_the_banks() {
        assert_eq!(krom2_raw_add(0x8260), 0xBFC6_6000 + 157 * 30);
        assert_eq!(krom2_raw_add(0x889F), 0xBFC6_6000 + 0x3D68);
        assert_eq!(krom2_raw_add(0x0041), u32::MAX);
        assert_eq!(krom2_raw_add(0x84BF), u32::MAX);
        assert_eq!(krom2_raw_add(0x9873), u32::MAX);
    }

    #[test]
    fn installed_rom_serves_drawn_glyphs_and_boxes_for_the_rest() {
        let mut rom = vec![0u8; 0x8_0000];
        install(&mut rom);
        let at = |code: u32| {
            let offset = (krom2_raw_add(code) - 0xBFC0_0000) as usize;
            rom[offset..offset + CELL_BYTES].to_vec()
        };
        for (&code, cell) in glyphs() {
            assert_eq!(at(u32::from(code)), cell.to_vec(), "{code:04X}");
        }
        // An undrawn kanji shows the box.
        assert_eq!(at(0x9872), missing_glyph().to_vec());
    }
}
