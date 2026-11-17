//! Conformance check: guest actions that commercial software never takes and
//! that at least one other emulator or hardware revision does not accept.
//!
//! The retail model (`real`) accepts all of them, as the console does, so a
//! program can rely on one and still pass every gate here while it fails
//! elsewhere. This module counts them instead, per run, with the guest PC of
//! the first one, so a gate can refuse them (`psoxide-gate run --conformance`).
//! Recording changes no behaviour.
//!
//! The rules, with their evidence, are listed in `docs/conformance.md`. Keep
//! the list short and every rule precise: a commercial disc that trips a rule
//! means the rule is wrong.
//!
//! The PC of a hit is the instruction whose store caused it. The bus only
//! sees the access, so it asks for the PC (`Bus::conformance_wants_pc`) and
//! the CPU answers after the instruction (`Bus::attribute_conformance`).
//! That answer is compiled in with the `conformance` feature (the gate turns
//! it on; the player build does not pay for it); without it the counts are
//! still kept and the PCs are unknown.

/// One conformance rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum Rule {
    /// A CD-ROM interrupt acknowledge (`1F801803h` index 1) that clears only
    /// some of the pending response bits.
    CdAckPartial,
    /// A CD-ROM interrupt acknowledge combined with the parameter FIFO reset
    /// (bit 6) in the same write.
    CdAckWithParamReset,
    /// A GP0(1Fh) command (GPU interrupt request, GPUSTAT bit 24).
    GpuIrqCommand,
}

impl Rule {
    /// Every rule, in report order.
    pub const ALL: [Rule; 3] = [
        Rule::CdAckPartial,
        Rule::CdAckWithParamReset,
        Rule::GpuIrqCommand,
    ];

    /// Stable id, as printed and as named in `docs/conformance.md`.
    #[must_use]
    pub const fn id(self) -> &'static str {
        match self {
            Rule::CdAckPartial => "cd-ack-partial",
            Rule::CdAckWithParamReset => "cd-ack-param-reset",
            Rule::GpuIrqCommand => "gpu-irq-command",
        }
    }

    /// One line: what it is and why it is listed.
    #[must_use]
    pub const fn summary(self) -> &'static str {
        match self {
            Rule::CdAckPartial => {
                "CD acknowledge of a pending response with a value other than 07h or 1Fh; \
                 PCSX-Redux stops on it"
            }
            Rule::CdAckWithParamReset => {
                "CD acknowledge and parameter reset (bit 6) in one write; \
                 PCSX-Redux resets the parameters and does not acknowledge"
            }
            Rule::GpuIrqCommand => {
                "GP0(1Fh) GPU interrupt request; PCSX-ReARMed never raises GPUSTAT bit 24 \
                 and the MiSTer core on the SuperStation One mostly does not"
            }
        }
    }

    /// Whether this rule is about the CD-ROM interrupt acknowledge.
    #[must_use]
    pub const fn is_cd_ack(self) -> bool {
        matches!(self, Rule::CdAckPartial | Rule::CdAckWithParamReset)
    }

    const fn index(self) -> usize {
        match self {
            Rule::CdAckPartial => 0,
            Rule::CdAckWithParamReset => 1,
            Rule::GpuIrqCommand => 2,
        }
    }
}

/// Which rule, if any, a write of `value` to the CD-ROM interrupt flag
/// register (`1F801803h` index 1) breaks while `flags` (`HINTSTS` bits 0-4)
/// are set.
///
/// Commercial code (Sony's library, the BIOS, PSn00bSDK) acknowledges with
/// 07h or 1Fh and resets the parameter FIFO with a separate 40h. Writes that
/// touch no response bit (00h, 08h, 10h, 18h, 40h alone) are fine.
#[must_use]
pub const fn cd_ack_rule(value: u8, flags: u8) -> Option<Rule> {
    let ack = value & 0x1f;
    if value & 0x40 != 0 && ack != 0 {
        return Some(Rule::CdAckWithParamReset);
    }
    if flags & 0x07 != 0 && value & 0x07 != 0 && ack != 0x07 && ack != 0x1f {
        return Some(Rule::CdAckPartial);
    }
    None
}

/// Hits of one or more rules in one device, with the PC of each rule's first
/// hit once the CPU has given it.
#[derive(Clone, Debug, Default)]
pub struct Tally {
    hits: [u64; 3],
    first_pc: [Option<u32>; 3],
    /// A first hit still waiting for its PC.
    unattributed: Option<Rule>,
    /// Writes checked, hit or not (context for the counts).
    checked: u64,
}

impl Tally {
    /// Count a write the rules looked at.
    pub fn checked(&mut self) {
        self.checked += 1;
    }

    /// Writes the rules looked at so far.
    #[must_use]
    pub fn checked_count(&self) -> u64 {
        self.checked
    }

    /// Count a hit of `rule`.
    pub fn hit(&mut self, rule: Rule) {
        let i = rule.index();
        self.hits[i] += 1;
        if self.hits[i] == 1 {
            self.unattributed = Some(rule);
        }
    }

    /// Whether a first hit waits for the PC of the instruction that caused it.
    #[inline(always)]
    #[must_use]
    pub fn wants_pc(&self) -> bool {
        self.unattributed.is_some()
    }

    /// Give the waiting first hit its PC.
    pub fn attribute(&mut self, pc: u32) {
        if let Some(rule) = self.unattributed.take() {
            self.first_pc[rule.index()] = Some(pc);
        }
    }

    /// Hits of `rule` so far.
    #[must_use]
    pub fn hits(&self, rule: Rule) -> u64 {
        self.hits[rule.index()]
    }

    /// PC of the first hit of `rule`, when known.
    #[must_use]
    pub fn first_pc(&self, rule: Rule) -> Option<u32> {
        self.first_pc[rule.index()]
    }
}

/// Where the guest fed the GPU from, for the PC of the first GP0(1Fh).
///
/// A GP0(1Fh) usually arrives through DMA channel 2, long after the store
/// that started the transfer. The site is the last CPU store to GP0 or to
/// DMA channel 2's control register before the first GP0(1Fh) ran: the store
/// that wrote it, or the one that started the list holding it.
#[derive(Clone, Debug, Default)]
pub struct GpuSite {
    pc: Option<u32>,
    wanted: bool,
    frozen: bool,
}

impl GpuSite {
    /// A CPU store to GP0 or DMA channel 2's control register, with the
    /// GP0(1Fh) count before it.
    pub fn store(&mut self, irq_commands: u64) {
        if irq_commands != 0 {
            self.frozen = true;
        }
        if !self.frozen {
            self.wanted = true;
        }
    }

    /// Whether a store waits for its PC.
    #[inline(always)]
    #[must_use]
    pub fn wants_pc(&self) -> bool {
        self.wanted
    }

    /// The PC of the store that asked, with the GP0(1Fh) count after it.
    pub fn attribute(&mut self, pc: u32, irq_commands: u64) {
        if core::mem::take(&mut self.wanted) {
            self.pc = Some(pc);
            if irq_commands != 0 {
                self.frozen = true;
            }
        }
    }

    /// The site of the first GP0(1Fh), once there was one.
    #[must_use]
    pub fn pc(&self) -> Option<u32> {
        self.pc
    }
}

/// One rule's result for a run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Finding {
    /// The rule.
    pub rule: Rule,
    /// How many times the guest broke it.
    pub hits: u64,
    /// PC of the first time, when known.
    pub first_pc: Option<u32>,
}

/// Every rule's result for a run, in [`Rule::ALL`] order.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct Report {
    /// One entry per rule, hit or not.
    pub findings: Vec<Finding>,
    /// CD-ROM interrupt flag writes that acknowledged something (bits 0-4 not zero), broken
    /// rule or not: shows a clean run did acknowledge.
    pub cd_acknowledges: u64,
}

impl Finding {
    /// A rule's result.
    #[must_use]
    pub const fn new(rule: Rule, hits: u64, first_pc: Option<u32>) -> Self {
        Self {
            rule,
            hits,
            first_pc,
        }
    }
}

impl Report {
    /// A run's result: `findings` in [`Rule::ALL`] order, and the CD acknowledges written.
    #[must_use]
    pub fn new(findings: Vec<Finding>, cd_acknowledges: u64) -> Self {
        Self {
            findings,
            cd_acknowledges,
        }
    }

    /// The rules that were hit.
    pub fn hits(&self) -> impl Iterator<Item = &Finding> {
        self.findings.iter().filter(|f| f.hits != 0)
    }

    /// Whether no rule was hit.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.hits().next().is_none()
    }

    /// Hits of `rule`.
    #[must_use]
    pub fn count(&self, rule: Rule) -> u64 {
        self.findings
            .iter()
            .find(|f| f.rule == rule)
            .map_or(0, |f| f.hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn commercial_acknowledges_pass() {
        for flags in [0x01, 0x02, 0x03, 0x05, 0x13, 0x00] {
            for value in [0x07, 0x1f, 0x40, 0x18, 0x08, 0x10, 0x00] {
                assert_eq!(cd_ack_rule(value, flags), None, "{value:#x} on {flags:#x}");
            }
        }
    }

    #[test]
    fn writing_the_flag_back_is_partial() {
        // The SDK acknowledged with the interrupt number it read.
        for flags in [0x01, 0x02, 0x03, 0x04, 0x05] {
            assert_eq!(cd_ack_rule(flags, flags), Some(Rule::CdAckPartial));
        }
        // 07h plus one of the buffer bits is not what libcd writes either.
        assert_eq!(cd_ack_rule(0x0f, 0x03), Some(Rule::CdAckPartial));
    }

    #[test]
    fn partial_needs_a_pending_response() {
        assert_eq!(cd_ack_rule(0x03, 0x00), None);
        assert_eq!(cd_ack_rule(0x03, 0x18), None);
    }

    #[test]
    fn acknowledge_with_parameter_reset() {
        assert_eq!(cd_ack_rule(0x5f, 0x03), Some(Rule::CdAckWithParamReset));
        assert_eq!(cd_ack_rule(0x5f, 0x00), Some(Rule::CdAckWithParamReset));
        assert_eq!(cd_ack_rule(0x47, 0x00), Some(Rule::CdAckWithParamReset));
        assert_eq!(cd_ack_rule(0x40, 0x03), None);
    }

    #[test]
    fn tally_keeps_the_first_pc() {
        let mut t = Tally::default();
        assert!(!t.wants_pc());
        t.hit(Rule::CdAckPartial);
        assert!(t.wants_pc());
        t.attribute(0x8001_0000);
        t.hit(Rule::CdAckPartial);
        assert!(!t.wants_pc());
        t.attribute(0x8002_0000);
        assert_eq!(t.hits(Rule::CdAckPartial), 2);
        assert_eq!(t.first_pc(Rule::CdAckPartial), Some(0x8001_0000));
        assert_eq!(t.first_pc(Rule::CdAckWithParamReset), None);
    }

    #[test]
    fn gpu_site_is_the_store_before_the_first_command() {
        // A DMA kick, then the 1Fh runs inside the list later.
        let mut s = GpuSite::default();
        s.store(0);
        s.attribute(0x8000_1000, 0);
        s.store(0);
        s.attribute(0x8000_2000, 0);
        // The command ran; the next store asks for nothing.
        s.store(1);
        assert!(!s.wants_pc());
        s.attribute(0x8000_3000, 1);
        assert_eq!(s.pc(), Some(0x8000_2000));

        // A CPU GP0 write of the 1Fh itself.
        let mut s = GpuSite::default();
        s.store(0);
        s.attribute(0x8000_4000, 1);
        s.store(1);
        assert_eq!(s.pc(), Some(0x8000_4000));
    }
}
