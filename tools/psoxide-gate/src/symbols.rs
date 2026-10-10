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
}

impl Symbols {
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    pub fn insert(&mut self, name: &str, addr: u32) {
        self.by_name.insert(name.to_string(), addr);
    }

    /// Look a symbol up by exact name, then by `::`-path suffix (so
    /// `game::STATE` finds `nitroxide::game::STATE`). Ambiguous suffixes are
    /// an error, never a silent pick.
    pub fn resolve(&self, name: &str) -> Result<u32, String> {
        if let Some(&addr) = self.by_name.get(name) {
            return Ok(addr);
        }
        let suffix = format!("::{name}");
        let hits: Vec<(&String, &u32)> = self
            .by_name
            .iter()
            .filter(|(k, _)| k.ends_with(&suffix))
            .collect();
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
        let shentsize = u16at(0x2E)? as usize;
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
                let kind = info & 0xF;
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
                    let raw = toks[toks.len() - 1];
                    let name = demangle_legacy(raw).unwrap_or_else(|| raw.to_string());
                    self.by_name.entry(name).or_insert(addr);
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
        if last.len() == 17 && last.starts_with('h') && last[1..].chars().all(|c| c.is_ascii_hexdigit()) {
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
}
