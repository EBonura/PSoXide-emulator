// SPDX-License-Identifier: GPL-2.0-or-later
//! The HLE kernel's Shift-JIS font (B(51h) Krom2RawAdd, B(53h) Krom2Offset).
//!
//! psx-spx "BIOS Character Sets" fixes the interface: charset 2 (16x15
//! pixel cells, 8140h..84BEh) sits at BFC66000h in the ROM, charset 3
//! (889Fh..9872h) at BFC69D68h, and Krom2RawAdd answers the address of a
//! character or -1. A cell is 16x15 one-bit pixels, 2 bytes a row, 30 bytes.
//!
//! The layout inside the banks is the JIS X 0208 chart. Charset 2 is its
//! rows 1 to 8 with only the assigned cells kept, in code order: 524 cells,
//! and 524 * 30 bytes is exactly the 3D68h between the two charsets. Charset
//! 3 is the level 1 kanji, rows 16 to 46 whole and row 47 up to cell 51:
//! 31 * 94 + 51 cells. A Shift-JIS code is turned into its JIS row and cell
//! by the standard pairing of two rows to a lead byte.
//!
//! A code that is not an assigned cell of either bank is answered with the
//! error value (-1 for Krom2RawAdd, FFFFh for Krom2Offset). psx-spx says
//! nothing about unassigned codes inside the ranges; there is no cell to
//! point at, so it is an error here too.
//!
//! Krom2Offset counts in cells from the start of the charset, and the
//! address is the charset base plus 30 bytes a cell.
//!
//! The glyphs are drawn for PSoXide (hle_font_glyphs.txt); an assigned cell
//! without a drawing shows an outlined box.

use std::collections::BTreeMap;
use std::sync::OnceLock;

/// Bytes of one character cell.
pub const CELL_BYTES: usize = 30;
/// ROM offset of charset 2 (psx-spx: BFC66000h).
pub const BANK1_ROM_OFFSET: usize = 0x6_6000;
/// Cells of charset 2: the assigned cells of JIS X 0208 rows 1 to 8.
pub const BANK1_CELLS: usize = 524;
/// ROM offset of charset 3 (psx-spx: BFC69D68h).
pub const BANK2_ROM_OFFSET: usize = BANK1_ROM_OFFSET + BANK1_CELLS * CELL_BYTES;
/// Cells of charset 3: the level 1 kanji, JIS X 0208 rows 16 to 47.
pub const BANK2_CELLS: usize = 31 * 94 + 51;
/// Where the BIOS ROM is mapped.
const ROM_BASE: u32 = 0xBFC0_0000;

/// The assigned cells of JIS X 0208 rows 1 to 8, as inclusive runs of cell
/// numbers (1 to 94) per row: symbols; more symbols with gaps; digits and
/// Latin letters; hiragana; katakana; Greek; Cyrillic; box drawing.
const BANK1_ASSIGNED: [&[(u8, u8)]; 8] = [
    &[(1, 94)],
    &[(1, 14), (26, 33), (42, 48), (60, 74), (82, 89), (94, 94)],
    &[(16, 25), (33, 58), (65, 90)],
    &[(1, 83)],
    &[(1, 86)],
    &[(1, 24), (33, 56)],
    &[(1, 33), (49, 81)],
    &[(1, 32)],
];

/// The Shift-JIS code of cell `cell` (1 to 94) of JIS row `row` (1 to 94):
/// two rows share a lead byte, odd rows take trail bytes 40h..7Eh and
/// 80h..9Eh, even rows 9Fh..FCh.
fn sjis(row: u8, cell: u8) -> u16 {
    let lead = u16::from(row.div_ceil(2)) + if row <= 62 { 0x80 } else { 0xC0 };
    let trail = if row % 2 == 1 {
        u16::from(cell) + if cell <= 63 { 0x3F } else { 0x40 }
    } else {
        u16::from(cell) + 0x9E
    };
    lead << 8 | trail
}

/// The JIS row and cell of a Shift-JIS code with a lead byte of 81h..9Fh,
/// or `None` for a code that is not a character position.
fn jis(code: u16) -> Option<(u8, u8)> {
    let (lead, trail) = ((code >> 8) as u8, code as u8);
    if !(0x81..=0x9F).contains(&lead) {
        return None;
    }
    let first_row = (lead - 0x81) * 2 + 1;
    match trail {
        0x40..=0x7E => Some((first_row, trail - 0x3F)),
        0x80..=0x9E => Some((first_row, trail - 0x40)),
        0x9F..=0xFC => Some((first_row + 1, trail - 0x9E)),
        _ => None,
    }
}

/// Index of the cell of `code` in charset 2: its place among the assigned
/// cells of rows 1 to 8, in code order.
fn bank1_assigned_cell(code: u16) -> Option<u16> {
    let (row, cell) = jis(code)?;
    let runs = BANK1_ASSIGNED.get(usize::from(row).checked_sub(1)?)?;
    let before: u16 = BANK1_ASSIGNED[..usize::from(row) - 1]
        .iter()
        .flat_map(|runs| runs.iter())
        .map(|&(first, last)| u16::from(last - first) + 1)
        .sum();
    let mut within = 0;
    for &(first, last) in *runs {
        if (first..=last).contains(&cell) {
            return Some(before + within + u16::from(cell - first));
        }
        within += u16::from(last - first) + 1;
    }
    None
}

/// Index of the cell of `code` in charset 3: rows 16 to 46 whole, then the
/// first 51 cells of row 47.
fn bank2_cell(code: u16) -> Option<u16> {
    let (row, cell) = jis(code)?;
    let limit = match row {
        16..=46 => 94,
        47 => 51,
        _ => return None,
    };
    (cell <= limit).then(|| u16::from(row - 16) * 94 + u16::from(cell - 1))
}

/// B(53h) Krom2Offset: the cell index of `code` within its charset, FFFFh
/// when the code has no cell.
pub fn krom2_offset(code: u32) -> u16 {
    let Ok(code) = u16::try_from(code) else {
        return u16::MAX;
    };
    bank1_assigned_cell(code)
        .or_else(|| bank2_cell(code))
        .unwrap_or(u16::MAX)
}

/// B(51h) Krom2RawAdd: the ROM address of the cell of `code`, or -1.
pub fn krom2_raw_add(code: u32) -> u32 {
    let Ok(code) = u16::try_from(code) else {
        return u32::MAX;
    };
    let (base, cell) = if let Some(cell) = bank1_assigned_cell(code) {
        (BANK1_ROM_OFFSET, cell)
    } else if let Some(cell) = bank2_cell(code) {
        (BANK2_ROM_OFFSET, cell)
    } else {
        return u32::MAX;
    };
    ROM_BASE + (base + usize::from(cell) * CELL_BYTES) as u32
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
    fn codes_map_to_jis_rows_and_back() {
        assert_eq!(sjis(1, 1), 0x8140);
        assert_eq!(sjis(2, 1), 0x819F);
        assert_eq!(sjis(1, 64), 0x8180);
        assert_eq!(sjis(3, 16), 0x824F);
        assert_eq!(sjis(16, 1), 0x889F);
        assert_eq!(sjis(47, 51), 0x9872);
        for row in 1..=47u8 {
            for cell in 1..=94u8 {
                assert_eq!(jis(sjis(row, cell)), Some((row, cell)), "{row}-{cell}");
            }
        }
        assert_eq!(jis(0x817F), None);
        assert_eq!(jis(0x0041), None);
    }

    #[test]
    fn bank1_holds_the_assigned_cells_of_rows_1_to_8() {
        // Space, full-width 0 and A, hiragana a, the last box-drawing cell.
        assert_eq!(krom2_offset(0x8140), 0);
        assert_eq!(krom2_offset(0x824F), 147);
        assert_eq!(krom2_offset(0x8260), 157);
        assert_eq!(krom2_offset(0x829F), 209);
        assert_eq!(krom2_offset(0x84BE), 523);
        let assigned: usize = BANK1_ASSIGNED
            .iter()
            .flat_map(|runs| runs.iter())
            .map(|&(first, last)| usize::from(last - first) + 1)
            .sum();
        assert_eq!(assigned, BANK1_CELLS);
        assert_eq!(BANK2_ROM_OFFSET, BANK1_ROM_OFFSET + 0x3D68);
        // Gaps in the chart have no cell: row 2 has cells 1 to 14, then 26 to 33.
        assert_eq!(krom2_offset(u32::from(sjis(2, 14))), 94 + 13);
        assert_eq!(krom2_offset(u32::from(sjis(2, 15))), u16::MAX);
        assert_eq!(krom2_offset(u32::from(sjis(2, 25))), u16::MAX);
        assert_eq!(krom2_offset(u32::from(sjis(2, 26))), 94 + 14);
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
    fn raw_add_answers_minus_one_for_codes_without_a_cell() {
        assert_eq!(krom2_raw_add(0x8260), 0xBFC6_6000 + 157 * 30);
        assert_eq!(krom2_raw_add(0x889F), 0xBFC6_6000 + 0x3D68);
        assert_eq!(krom2_raw_add(0x0041), u32::MAX);
        assert_eq!(krom2_raw_add(0x84BF), u32::MAX);
        assert_eq!(krom2_raw_add(0x9873), u32::MAX);
        assert_eq!(krom2_raw_add(0x1_8260), u32::MAX);
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
