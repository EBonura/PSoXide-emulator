//! Read-only card transport evidence and persisted card receipts.

use emulator_core::{Bus, Cpu};
use sha2::{Digest, Sha256};

pub fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn load_fixture(
    root: &std::path::Path,
    fixture: &crate::journey::CardFixture,
) -> Result<Vec<u8>, String> {
    let path = root.join(&fixture.path);
    let bytes =
        std::fs::read(&path).map_err(|e| format!("read card fixture {}: {e}", path.display()))?;
    if bytes.len() != emulator_core::pad::MEMCARD_SIZE
        || !hash(&bytes).eq_ignore_ascii_case(&fixture.sha256)
    {
        return Err(format!(
            "card fixture size or SHA256 mismatch: {}",
            path.display()
        ));
    }
    Ok(bytes)
}

pub fn bytes(bus: &Bus) -> Result<Vec<u8>, String> {
    bus.sio0()
        .port1()
        .memcard()
        .map(|c| c.as_bytes().to_vec())
        .ok_or_else(|| "no memory card in port 1".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pulse(target: Option<u64>) -> Observer {
        Observer {
            irq: Some((90, 0x8005_2590)),
            pulses: vec![(100, 144, 0x52, 194, 82, 0x1003)],
            target_ack: target,
            ..Observer::default()
        }
    }

    #[test]
    fn selected_full_pulse_survives_evidence_limit() {
        let mut observer = pulse(Some(100));
        observer.overlaps = vec!["earlier overlap".into(); 32];
        observer.discard_truncated(143, Some(144));
        observer.finish_irq(180);
        assert!(observer.target_observed);
        assert_eq!(observer.overlap_count, 1);
        assert_eq!(observer.overlaps.len(), 32);
        assert_eq!(observer.evidence().len(), 33);
        assert!(observer
            .target_overlap
            .as_ref()
            .unwrap()
            .contains("ACK 100..144"));
    }

    #[test]
    fn truncated_wrong_or_absent_target_cannot_satisfy_fault() {
        let mut truncated = pulse(Some(100));
        truncated.discard_truncated(143, None);
        truncated.finish_irq(180);
        assert!(!truncated.target_observed);
        assert_eq!(truncated.overlap_count, 0);
        for target in [None, Some(101)] {
            let mut observer = pulse(target);
            observer.finish_irq(180);
            assert_eq!(observer.overlap_count, 1);
            assert!(!observer.target_observed);
            assert!(observer.target_overlap.is_none());
        }
        let mut short_handler = pulse(Some(100));
        short_handler.finish_irq(143);
        assert!(!short_handler.target_observed);
        assert_eq!(short_handler.overlap_count, 0);
    }

    #[test]
    fn fixture_binding_fails_closed_without_modifying_source() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fixture.mcd");
        let bytes = vec![0x5a; emulator_core::pad::MEMCARD_SIZE];
        std::fs::write(&path, &bytes).unwrap();
        let fixture = crate::journey::CardFixture {
            path: "fixture.mcd".into(),
            sha256: hash(&bytes),
        };
        assert_eq!(load_fixture(dir.path(), &fixture).unwrap(), bytes);
        let mut changed = bytes.clone();
        changed[12345] ^= 1;
        std::fs::write(&path, &changed).unwrap();
        assert!(load_fixture(dir.path(), &fixture).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), changed);
        std::fs::write(&path, &bytes[..128]).unwrap();
        assert!(load_fixture(dir.path(), &fixture).is_err());
    }
}

pub fn progress(bus: &Bus) -> (u8, u16, usize) {
    bus.sio0()
        .port1()
        .memcard()
        .map(|c| c.debug_progress())
        .unwrap_or_default()
}

pub fn active(bus: &Bus) -> bool {
    bus.sio0().port1().selected_is_memcard()
}

pub struct CardImage {
    pub name: String,
    pub bytes: Vec<u8>,
    pub detail: String,
}

#[derive(Default)]
pub struct Observer {
    irq: Option<(u64, u32)>,
    ack_start: Option<u64>,
    pulses: Vec<(u64, u64, u8, u16, usize, u16)>,
    pub overlaps: Vec<String>,
    pub overlap_count: u64,
    pub target_ack: Option<u64>,
    pub target_observed: bool,
    pub target_overlap: Option<String>,
}

impl Observer {
    pub fn evidence(&self) -> Vec<String> {
        let mut spans = self.overlaps.clone();
        if let Some(target) = &self.target_overlap {
            if !spans.contains(target) {
                spans.push(target.clone());
            }
        }
        spans
    }
    fn discard_truncated(&mut self, now: u64, actual_end: Option<u64>) {
        self.pulses
            .retain(|p| now >= p.1 || actual_end == Some(p.1));
    }
    fn finish_irq(&mut self, now: u64) {
        if let Some((begin, epc)) = self.irq.take() {
            for &(start, end, cmd, frame, index, ctrl) in &self.pulses {
                if cmd != 0 && begin <= start && now >= end {
                    let span = format!("IRQ vector span {begin}..{now}, EPC {epc:#010x}; ACK {start}..{end}, command {cmd:#04x}, frame {frame}, accepted bytes {index}, CTRL {ctrl:#06x}");
                    if self.target_ack == Some(start) {
                        self.target_observed = true;
                        self.target_overlap = Some(span.clone());
                    }
                    self.overlap_count += 1;
                    if self.overlaps.len() < 32 {
                        self.overlaps.push(span);
                    }
                }
            }
        }
        self.pulses.clear();
    }
    pub fn in_irq(&self) -> bool {
        self.irq.is_some()
    }
    pub fn sample(&mut self, cpu: &Cpu, bus: &Bus) {
        let now = bus.cycles();
        if matches!(cpu.pc(), 0x8000_0080 | 0xbfc0_0180) && cpu.cop0()[13] & 0x7c == 0 {
            self.irq.get_or_insert((now, cpu.cop0()[14]));
        }
        let sio = bus.sio0();
        // A deselect or early next byte can truncate an ACK. Such a pulse
        // cannot provide evidence that the entire scheduled width elapsed.
        self.discard_truncated(now, sio.debug_ack_end_deadline());
        if active(bus) {
            if let Some(start) = sio.debug_ack_deadline() {
                self.ack_start = Some(start);
            }
            if let (Some(start), Some(end)) = (self.ack_start, sio.debug_ack_end_deadline()) {
                if self.pulses.last().is_none_or(|p| p.0 != start) {
                    let (cmd, frame, index) = progress(bus);
                    self.pulses
                        .push((start, end, cmd, frame, index, sio.debug_ctrl()));
                }
                self.ack_start = None;
            }
        }
        if self.irq.is_some() {
            // RFE restores the interrupted current interrupt-enable bit.
            if cpu.cop0()[12] & 1 != 0 {
                self.finish_irq(now);
            }
        } else {
            self.pulses.clear();
        }
    }
}
