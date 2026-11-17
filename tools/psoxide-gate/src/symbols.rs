//! Guest symbol lookup, so a journey can name `GAME_STATE` instead of an
//! address that moves with every build.
//!
//! Three sources, tried in the order a journey lists them: a 32-bit
//! little-endian ELF (its `.symtab`), an `lld` linker map, and a plain
//! `address name` list (what `nm` prints without the type column).

use std::collections::BTreeMap;
use std::path::Path;

#[derive(Default, Debug)]
pub struct Symbols {
    /// Display name (demangled where possible) to address.
    by_name: BTreeMap<String, u32>,
    /// Names that came from a map row with an ` (.N)` suffix, or that such a row collided with.
    from_suffixed_row: std::collections::BTreeSet<String>,
    /// Names two or more rows give different addresses, at least one of them by way of that suffix:
    /// never resolved, always listed.
    ambiguous: BTreeMap<String, Vec<u32>>,
}

impl Symbols {
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// The symbol with the highest address at or below `addr`, and `addr`'s offset into it,
    /// for naming a guest PC. Of several names at one address the shortest wins (a linker map
    /// also lists the input section, object path and all, at the function's address). Data
    /// symbols count too, so read the answer next to the map.
    pub fn nearest(&self, addr: u32) -> Option<(String, u32)> {
        self.by_name
            .iter()
            .filter(|(_, &a)| a <= addr)
            .max_by_key(|(name, &a)| (a, std::cmp::Reverse((name.len(), name.as_str()))))
            .map(|(name, &a)| (name.clone(), addr - a))
    }

    pub fn insert(&mut self, name: &str, addr: u32) {
        self.by_name.insert(name.to_string(), addr);
    }

    /// Look a symbol up by exact name, then by `::`-path suffix (so
    /// `game::STATE` finds `nitroxide::game::STATE`). Ambiguous suffixes are
    /// an error, never a silent pick.
    pub fn resolve(&self, name: &str) -> Result<u32, String> {
        if let Some(addrs) = self.ambiguous.get(name) {
            return Err(ambiguous_message(name, addrs));
        }
        if let Some(&addr) = self.by_name.get(name) {
            return Ok(addr);
        }
        let suffix = format!("::{name}");
        let mut hits: Vec<(&String, &u32)> = self
            .by_name
            .iter()
            .filter(|(k, _)| k.ends_with(&suffix))
            .collect();
        // A path-suffix match may land on a name two rows disagree about.
        for (full, addrs) in self.ambiguous.iter().filter(|(k, _)| k.ends_with(&suffix)) {
            if let Some(first) = addrs.first() {
                if !hits.iter().any(|(k, _)| *k == full) {
                    hits.push((full, first));
                }
            }
        }
        if let [(full, _)] = hits.as_slice() {
            if let Some(addrs) = self.ambiguous.get(*full) {
                return Err(ambiguous_message(full, addrs));
            }
        }
        match hits.as_slice() {
            [(_, &addr)] => Ok(addr),
            [] => Err(format!(
                "symbol `{name}` not found among {} loaded symbols",
                self.by_name.len()
            )),
            many => Err(format!(
                "symbol `{name}` is ambiguous: {}",
                many.iter()
                    .take(6)
                    .map(|(k, v)| format!("{k}@{v:#x}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }

    /// Load any of the three formats, chosen by content.
    pub fn load(&mut self, path: &Path) -> Result<usize, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        let before = self.by_name.len();
        if bytes.starts_with(b"\x7fELF") {
            self.load_elf(&bytes)?;
        } else {
            let text = String::from_utf8_lossy(&bytes);
            self.load_text(&text);
        }
        Ok(self.by_name.len() - before)
    }

    fn load_elf(&mut self, b: &[u8]) -> Result<(), String> {
        let u16at = |o: usize| -> Result<u16, String> {
            b.get(o..o + 2)
                .map(|s| u16::from_le_bytes([s[0], s[1]]))
                .ok_or_else(|| "truncated ELF".to_string())
        };
        let u32at = |o: usize| -> Result<u32, String> {
            b.get(o..o + 4)
                .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
                .ok_or_else(|| "truncated ELF".to_string())
        };
        if b.get(4) != Some(&1) || b.get(5) != Some(&1) {
            return Err("only 32-bit little-endian ELF is supported".into());
        }
        let shoff = u32at(0x20)? as usize;
        let shentsize = u16at(0x2e)? as usize;
        let shnum = u16at(0x30)? as usize;
        for i in 0..shnum {
            let sh = shoff + i * shentsize;
            if u32at(sh + 4)? != 2 {
                continue; // not SHT_SYMTAB
            }
            let off = u32at(sh + 0x10)? as usize;
            let size = u32at(sh + 0x14)? as usize;
            let link = u32at(sh + 0x18)? as usize;
            let entsize = (u32at(sh + 0x24)? as usize).max(16);
            let strsh = shoff + link * shentsize;
            let stroff = u32at(strsh + 0x10)? as usize;
            for s in 0..size / entsize {
                let e = off + s * entsize;
                let name_off = u32at(e)? as usize;
                let value = u32at(e + 4)?;
                let info = *b.get(e + 12).ok_or("truncated ELF")?;
                // STT_OBJECT (1), STT_FUNC (2), STT_NOTYPE (0) with a name.
                let kind = info & 0xf;
                if name_off == 0 || kind > 2 {
                    continue;
                }
                let start = stroff + name_off;
                let Some(rest) = b.get(start..) else { continue };
                let end = rest.iter().position(|&c| c == 0).unwrap_or(rest.len());
                let raw = String::from_utf8_lossy(&rest[..end]).into_owned();
                if raw.is_empty() {
                    continue;
                }
                let name = demangle_legacy(&raw).unwrap_or(raw);
                self.by_name.insert(name, value);
            }
        }
        Ok(())
    }

    /// Add one row of a text map. A name twice with the same address is one symbol. With
    /// different addresses the first row wins, as it always did, unless an ` (.N)` row is
    /// involved: then the name is ambiguous and `resolve` lists every address.
    fn add_text_row(&mut self, name: String, addr: u32, suffixed: bool) {
        if suffixed {
            self.from_suffixed_row.insert(name.clone());
        }
        match self.by_name.get(&name).copied() {
            None => {
                self.by_name.insert(name, addr);
            }
            Some(first) if first == addr => {}
            Some(first) if self.from_suffixed_row.contains(&name) => {
                let addrs = self.ambiguous.entry(name).or_insert_with(|| vec![first]);
                if !addrs.contains(&addr) {
                    addrs.push(addr);
                }
            }
            Some(_) => {}
        }
    }

    fn load_text(&mut self, text: &str) {
        for line in text.lines() {
            let toks: Vec<&str> = line.split_whitespace().collect();
            // lld map: `VMA LMA Size Align Out In Symbol` (symbol rows have no `:(`).
            if toks.len() >= 5
                && toks.iter().all(|t| !t.contains(":("))
                && toks[0].len() == 8
                && toks[1].len() == 8
                && u32::from_str_radix(toks[0], 16).is_ok()
                && u32::from_str_radix(toks[1], 16).is_ok()
                && toks[2].chars().all(|c| c.is_ascii_hexdigit())
            {
                if let Ok(addr) = u32::from_str_radix(toks[0], 16) {
                    // The symbol column starts after the four numeric ones.
                    let mut rest = line.trim_start();
                    for _ in 0..4 {
                        rest = rest.trim_start();
                        rest = &rest[rest.find(char::is_whitespace).unwrap_or(rest.len())..];
                    }
                    let (name, suffixed) = map_symbol(rest);
                    self.add_text_row(name, addr, suffixed);
                    continue;
                }
            }
            // `address name`, optionally `address type name`.
            if toks.len() >= 2 {
                let first = toks[0].trim_start_matches("0x");
                if let Ok(addr) = u32::from_str_radix(first, 16) {
                    let raw = toks[toks.len() - 1];
                    let name = demangle_legacy(raw).unwrap_or_else(|| raw.to_string());
                    self.by_name.entry(name).or_insert(addr);
                }
            }
        }
    }
}

fn ambiguous_message(name: &str, addrs: &[u32]) -> String {
    format!(
        "symbol `{name}` is ambiguous: the map lists it at {}",
        addrs
            .iter()
            .map(|a| format!("{a:#x}"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The name of an lld map row's symbol column and whether it carried an ` (.N)` suffix.
///
/// lld prints a Rust symbol demangled, and rustc gives a `static` with a private symbol an LLVM
/// uniquing suffix (`..._4SIMNOW.0`): the demangler prints that suffix after the path as ` (.0)`,
/// so the row reads `hl_psx::SIM_NOW (.0)`. The name a journey uses is the path before it.
/// Without a suffix the name is the row's last token, as before.
fn map_symbol(rest: &str) -> (String, bool) {
    let rest = rest.trim();
    if let Some(open) = rest.rfind(" (.") {
        let tail = &rest[open + 3..];
        if let Some(digits) = tail.strip_suffix(')') {
            if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
                let base = rest[..open].trim_end();
                return (
                    demangle_legacy(base).unwrap_or_else(|| base.to_string()),
                    true,
                );
            }
        }
    }
    let raw = rest.rsplit(char::is_whitespace).next().unwrap_or(rest);
    (
        demangle_legacy(raw).unwrap_or_else(|| raw.to_string()),
        false,
    )
}

/// Turn a legacy Rust mangled name (`_ZN8nitroxide4game5STATE17h0123456789abcdefE`)
/// into `nitroxide::game::STATE`. Returns `None` for anything else.
pub fn demangle_legacy(raw: &str) -> Option<String> {
    let body = raw.strip_prefix("_ZN")?.strip_suffix('E')?;
    let mut parts = Vec::new();
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let mut n = 0usize;
        let start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            n = n * 10 + usize::from(bytes[i] - b'0');
            i += 1;
        }
        if i == start || i + n > bytes.len() {
            return None;
        }
        parts.push(&body[i..i + n]);
        i += n;
    }
    // Drop the trailing `h<16 hex>` hash component.
    if let Some(last) = parts.last() {
        if last.len() == 17
            && last.starts_with('h')
            && last[1..].chars().all(|c| c.is_ascii_hexdigit())
        {
            parts.pop();
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("::"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demangles_legacy_names() {
        assert_eq!(
            demangle_legacy("_ZN9nitroxide4game5STATE17h0123456789abcdefE").as_deref(),
            Some("nitroxide::game::STATE")
        );
        assert_eq!(demangle_legacy("main"), None);
    }

    #[test]
    fn nearest_names_a_pc_by_the_shortest_name_below_it() {
        let mut s = Symbols::default();
        s.insert(
            "/build/game.o:(.text._RNvNtCs1_6psx_io2cd7ack_irq)",
            0x8001_0000,
        );
        s.insert("psx_io::cd::ack_irq", 0x8001_0000);
        s.insert("psx_io::cd::next", 0x8001_0040);
        assert_eq!(
            s.nearest(0x8001_0018),
            Some(("psx_io::cd::ack_irq".to_string(), 0x18))
        );
        assert_eq!(s.nearest(0x8001_0040).map(|(_, off)| off), Some(0));
        assert_eq!(s.nearest(0x8000_ffff), None);
    }

    #[test]
    fn resolves_by_suffix_and_flags_ambiguity() {
        let mut s = Symbols::default();
        s.insert("a::game::STATE", 0x8010_0000);
        s.insert("b::STATE", 0x8010_0100);
        assert_eq!(s.resolve("game::STATE"), Ok(0x8010_0000));
        assert!(s.resolve("STATE").unwrap_err().contains("ambiguous"));
        assert!(s.resolve("nope").is_err());
    }

    #[test]
    fn reads_nm_and_lld_map_rows() {
        let mut s = Symbols::default();
        s.load_text("80123456 D player_state\n  80200000 80200000       10     4         .bss      counter\n");
        assert_eq!(s.resolve("player_state"), Ok(0x8012_3456));
        assert_eq!(s.resolve("counter"), Ok(0x8020_0000));
    }

    const SUFFIXED_MAP: &str = "     VMA      LMA     Size Align Out     In      Symbol
801ea4f8 801ea4f8        2     4         /x/game.o:(.bss._RNvCs1_4game7SIM_NOW.0)
801ea4f8 801ea4f8        2     1                 game::SIM_NOW (.0)
801ee620 801ee620        2     1                 game::MOVE_TICK (.0)
801ee700 801ee700        4     1                 game::PLAIN
";

    #[test]
    fn a_name_with_the_dot_n_suffix_resolves_as_the_name() {
        let mut s = Symbols::default();
        s.load_text(SUFFIXED_MAP);
        assert_eq!(s.resolve("game::SIM_NOW"), Ok(0x801e_a4f8));
        assert_eq!(s.resolve("SIM_NOW"), Ok(0x801e_a4f8));
        assert_eq!(s.resolve("MOVE_TICK"), Ok(0x801e_e620));
        assert_eq!(s.resolve("PLAIN"), Ok(0x801e_e700));
        // The suffix is not a symbol of its own.
        assert!(s.resolve("(.0)").is_err());
    }

    #[test]
    fn two_suffixed_rows_with_one_name_and_two_addresses_are_ambiguous_not_picked() {
        let mut s = Symbols::default();
        s.load_text(
            "801ea4f8 801ea4f8        2     1                 game::T (.0)\n\
             801ea4fc 801ea4fc        2     1                 game::T (.1)\n",
        );
        let error = s.resolve("T").unwrap_err();
        assert!(error.contains("ambiguous"), "{error}");
        assert!(
            error.contains("0x801ea4f8") && error.contains("0x801ea4fc"),
            "{error}"
        );
        let error = s.resolve("game::T").unwrap_err();
        assert!(
            error.contains("0x801ea4f8") && error.contains("0x801ea4fc"),
            "{error}"
        );
    }

    #[test]
    fn suffixed_rows_at_one_address_are_one_symbol() {
        let mut s = Symbols::default();
        s.load_text(
            "801ea4f8 801ea4f8        2     1                 game::T (.0)\n\
             801ea4f8 801ea4f8        2     1                 game::T (.1)\n",
        );
        assert_eq!(s.resolve("T"), Ok(0x801e_a4f8));
    }

    #[test]
    fn a_plain_row_that_collides_with_a_suffixed_one_is_ambiguous() {
        let mut s = Symbols::default();
        s.load_text(
            "801ea4f8 801ea4f8        2     1                 game::T (.0)\n\
             801ea500 801ea500        2     1                 game::T\n",
        );
        assert!(s.resolve("game::T").unwrap_err().contains("ambiguous"));
    }
}
