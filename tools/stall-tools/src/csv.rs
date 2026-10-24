//! Minimal CSV reading for the emulator's profile logs (no quoting, no deps).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::Path;

pub struct Table {
    index: HashMap<String, usize>,
    pub rows: Vec<Vec<String>>,
}

impl Table {
    pub fn parse(text: &str) -> Table {
        let mut lines = text.lines();
        let header = lines.next().unwrap_or("");
        let index = header
            .split(',')
            .enumerate()
            .map(|(i, h)| (h.to_string(), i))
            .collect();
        let rows = lines
            .filter(|l| !l.is_empty())
            .map(|l| l.split(',').map(str::to_string).collect())
            .collect();
        Table { index, rows }
    }

    pub fn read(path: &Path) -> Result<Table, String> {
        let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Ok(Self::parse(&text))
    }

    pub fn col(&self, name: &str) -> Result<usize, String> {
        self.index
            .get(name)
            .copied()
            .ok_or_else(|| format!("missing column {name}"))
    }

    pub fn num(&self, row: &[String], name: &str) -> u64 {
        self.index
            .get(name)
            .and_then(|&i| row.get(i))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0)
    }

    /// Sum of a column over the rows whose `route_tick` is at least `from`.
    pub fn sum_from(&self, name: &str, from: u64) -> u64 {
        self.rows
            .iter()
            .filter(|r| self.num(r, "route_tick") >= from)
            .map(|r| self.num(r, name))
            .sum()
    }
}

/// `pc,<value column>,...` logs keyed by a hex address.
pub fn read_by_pc(path: &Path, value: &str) -> BTreeMap<u32, u64> {
    let Ok(table) = Table::read(path) else {
        return BTreeMap::new();
    };
    let Ok(pc_col) = table.col("pc") else {
        return BTreeMap::new();
    };
    table
        .rows
        .iter()
        .filter_map(|r| {
            let addr = u32::from_str_radix(r.get(pc_col)?.trim_start_matches("0x"), 16).ok()?;
            Some((addr, table.num(r, value)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sums_rows_from_a_tick() {
        let t = Table::parse("route_tick,issue_cycles\n1,10\n2,20\n3,30\n");
        assert_eq!(t.sum_from("issue_cycles", 2), 50);
        assert_eq!(t.sum_from("missing", 1), 0);
        assert!(t.col("route_tick").is_ok());
    }
}
