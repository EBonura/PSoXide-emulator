//! DMA controller -- 7 channels plus global control registers.
//!
//! Channel layout (each is 16 bytes at `0x1F80_1080 + 0x10 * ch`):
//! - `+0x0` MADR, the RAM address the channel transfers to or from
//! - `+0x4` BCR, block count and size (or word count in sync mode 0)
//! - `+0x8` CHCR, direction, step, sync mode and the start/busy bits
//!
//! Global:
//! - `0x1F80_10F0` DPCR, per-channel enable and priority
//! - `0x1F80_10F4` DICR, interrupt enables and flags
//!
//! This module holds the register file and the DICR interrupt logic. The
//! bus owns the transfers themselves, except OTC (channel 6), which only
//! needs RAM and is run from here.
//!
//! | ch | consumer     |
//! |----|--------------|
//! | 0  | MDEC-in      |
//! | 1  | MDEC-out     |
//! | 2  | GPU          |
//! | 3  | CD-ROM       |
//! | 4  | SPU          |
//! | 5  | PIO          |
//! | 6  | OTC          |
//!
//! ## Provenance
//!
//! Written from the nocash PSX-SPX "DMA Channels" chapter: the channel
//! register layout, DPCR enable bits, the DICR bit map with its flag and
//! master-flag rules, and the OTC (reverse clear ordering table) behaviour.
//! Completion latency is not modelled here; the bus schedules it. See
//! `LICENSE` and `docs/PROVENANCE.md`.

/// Number of DMA channels.
pub const NUM_CHANNELS: usize = 7;

/// OTC CHCR exposes only the start/busy and manual-trigger bits; direction
/// and step are hardwired (PSX-SPX: D6_CHCR reads back 11000002h at most),
/// and all other software writes read back zero.
const OTC_CHCR_WRITABLE: u32 = (1 << 24) | (1 << 28) | (1 << 30);
const OTC_CHCR_FIXED: u32 = 1 << 1;

/// CHCR bit 24: start / busy.
const CHCR_START: u32 = 1 << 24;
/// CHCR bit 28: manual start trigger.
const CHCR_TRIGGER: u32 = 1 << 28;

/// DICR bits that software can write directly: the unused-but-stored low
/// bits 0..=5, the bus-error flag at 15 is excluded, and the per-channel
/// enables plus master enable at 16..=23.
const DICR_WRITABLE: u32 = 0x003F | 0x00FF_0000;
/// DICR bit 15, the bus error flag. Forces the master flag.
const DICR_BUS_ERROR: u32 = 1 << 15;
/// DICR bit 23, the master enable.
const DICR_MASTER_ENABLE: u32 = 1 << 23;
/// DICR bits 24..=30, the per-channel flags (write 1 to clear).
const DICR_FLAGS: u32 = 0x7F00_0000;
/// DICR bit 31, the master flag (read only, computed).
const DICR_MASTER_FLAG: u32 = 1 << 31;

/// Per-channel register state.
#[derive(Default, Clone, Copy, serde::Serialize, serde::Deserialize)]
pub struct DmaChannel {
    /// `MADR`: base address. The low 24 bits are the RAM offset; upper bits
    /// read back as written but are not used during transfers.
    pub base: u32,
    /// `BCR`: block control. In block mode the low 16 bits are the block
    /// size and the high 16 bits the block count.
    pub block_control: u32,
    /// `CHCR`: channel control (direction, step, sync mode, start).
    pub channel_control: u32,
}

/// Global controller state and the 7 channels.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Dma {
    /// Per-channel register blocks.
    pub channels: [DmaChannel; NUM_CHANNELS],
    /// `DPCR`: per-channel enable (bit `4 * n + 3`) and priority. The
    /// BIOS writes it early, so the reset value does not matter here.
    pub dpcr: u32,
    /// `DICR` as software wrote it: enables, the unused low bits and the
    /// per-channel flags. Bit 31 is not stored; see [`Dma::read32`].
    pub dicr: u32,
    /// Per-channel count of CHCR writes with the start bit set.
    /// Diagnostic only, excluded from save states.
    #[serde(skip)]
    pub start_trigger_counts: [u64; NUM_CHANNELS],
    /// Per-channel count of CHCR writes of any kind. Diagnostic only,
    /// excluded from save states.
    #[serde(skip)]
    pub chcr_write_count: [u64; NUM_CHANNELS],
}

impl Dma {
    /// Low edge of the controller's MMIO range.
    pub const BASE: u32 = 0x1F80_1080;
    /// High edge (exclusive): DPCR is at `BASE + 0x70` and DICR at
    /// `BASE + 0x74`; the range is rounded up so the dispatch check is a
    /// single comparison.
    pub const END: u32 = 0x1F80_10F8;
    /// Channel stride within the DMA window.
    pub const STRIDE: u32 = 0x10;
    /// Offset of DPCR from [`Dma::BASE`].
    pub const DPCR_OFFSET: u32 = 0x70;
    /// Offset of DICR from [`Dma::BASE`].
    pub const DICR_OFFSET: u32 = 0x74;

    /// All channels and both global registers cleared.
    pub fn new() -> Self {
        Self {
            channels: [DmaChannel::default(); NUM_CHANNELS],
            dpcr: 0,
            dicr: 0,
            start_trigger_counts: [0; NUM_CHANNELS],
            chcr_write_count: [0; NUM_CHANNELS],
        }
    }

    /// `true` when `phys` falls inside `BASE..END`.
    pub fn contains(phys: u32) -> bool {
        (Self::BASE..Self::END).contains(&phys)
    }

    /// `true` when DPCR has channel `ch` enabled (PSX-SPX: bit `4 * ch + 3`).
    /// A CHCR start write to a disabled channel must not start a transfer.
    pub fn is_channel_enabled(&self, ch: usize) -> bool {
        self.dpcr & (1 << (ch * 4 + 3)) != 0
    }

    /// DICR as software reads it: the stored bits plus the computed master
    /// flag, `bit15 | (bit23 & any flag)` (PSX-SPX).
    fn dicr_read(&self) -> u32 {
        self.dicr & !DICR_MASTER_FLAG
            | if self.master_flag() {
                DICR_MASTER_FLAG
            } else {
                0
            }
    }

    fn master_flag(&self) -> bool {
        self.dicr & DICR_BUS_ERROR != 0
            || (self.dicr & DICR_MASTER_ENABLE != 0 && self.dicr & DICR_FLAGS != 0)
    }

    /// Read a 32-bit word. `phys` must be inside [`Dma::BASE`]..[`Dma::END`].
    pub fn read32(&self, phys: u32) -> u32 {
        let rel = phys - Self::BASE;
        match rel {
            Self::DPCR_OFFSET => self.dpcr,
            Self::DICR_OFFSET => self.dicr_read(),
            _ => {
                let (ch, field) = decode(phys);
                match (self.channels.get(ch), field) {
                    (Some(c), 0x0) => c.base,
                    (Some(c), 0x4) => c.block_control,
                    (Some(c), 0x8) => c.channel_control,
                    _ => 0,
                }
            }
        }
    }

    /// Read a byte; partial reads see the live register bytes. Some titles
    /// toggle channel DICR enables with `sb` at `DICR+2` while streaming FMV.
    pub fn read8(&self, phys: u32) -> u8 {
        let word = self.read32(phys & !3);
        (word >> ((phys & 3) * 8)) as u8
    }

    /// Read a little-endian halfword from the DMA register window (built from
    /// two byte reads, so an odd address straddles two registers).
    pub fn read16(&self, phys: u32) -> u16 {
        u16::from(self.read8(phys)) | u16::from(self.read8(phys + 1)) << 8
    }

    /// Write a 32-bit word. `phys` must be inside [`Dma::BASE`]..[`Dma::END`].
    ///
    /// Returns `true` when a DICR write takes the master interrupt flag from
    /// clear to set (PSX-SPX: the 0-to-1 edge raises IRQ3). CHCR start bits
    /// are counted here; running the transfer belongs to the bus.
    pub fn write32(&mut self, phys: u32, value: u32) -> bool {
        let rel = phys - Self::BASE;
        if rel == Self::DICR_OFFSET {
            return self.write_dicr(value);
        }
        self.write_raw32(phys, value);
        if rel < Self::DPCR_OFFSET && rel % Self::STRIDE == 8 && value & CHCR_START != 0 {
            let ch = (rel / Self::STRIDE) as usize;
            self.chcr_write_count[ch] += 1;
            self.start_trigger_counts[ch] += 1;
        }
        false
    }

    /// Write a byte. A partial write patches the stored register bytes only:
    /// it does not run DICR's write-one-to-clear and does not count or start
    /// a channel.
    pub fn write8(&mut self, phys: u32, value: u8) -> bool {
        let shift = (phys & 3) * 8;
        let word = self.read_stored(phys & !3);
        let patched = word & !(0xFF << shift) | (value as u32) << shift;
        self.write_raw32(phys & !3, patched);
        false
    }

    /// Write a little-endian halfword as two byte writes; see [`Dma::write8`].
    pub fn write16(&mut self, phys: u32, value: u16) -> bool {
        self.write8(phys, value as u8);
        self.write8(phys + 1, (value >> 8) as u8)
    }

    /// A register's stored value, without the computed DICR master flag.
    fn read_stored(&self, phys: u32) -> u32 {
        if phys - Self::BASE == Self::DICR_OFFSET {
            self.dicr
        } else {
            self.read32(phys)
        }
    }

    fn write_raw32(&mut self, phys: u32, value: u32) {
        let rel = phys - Self::BASE;
        match rel {
            Self::DPCR_OFFSET => self.dpcr = value,
            Self::DICR_OFFSET => self.dicr = value & !DICR_MASTER_FLAG,
            _ => {
                let (ch, field) = decode(phys);
                let Some(c) = self.channels.get_mut(ch) else {
                    return;
                };
                match field {
                    0x0 => c.base = value & 0x00FF_FFFF,
                    0x4 => c.block_control = value,
                    0x8 if ch == 6 => {
                        c.channel_control = (value & OTC_CHCR_WRITABLE) | OTC_CHCR_FIXED;
                    }
                    0x8 => c.channel_control = value,
                    _ => {}
                }
            }
        }
    }
}

impl Default for Dma {
    fn default() -> Self {
        Self::new()
    }
}

// --- DICR semantics (PSX-SPX "DMA Channels", DICR) ---
//
//   bits  0..5  : stored, no documented effect
//   bit   15    : bus error flag
//   bits 16..22 : per-channel interrupt enable, DMA0..DMA6
//   bit   23    : master enable
//   bits 24..30 : per-channel interrupt flag (read, write 1 to clear)
//   bit   31    : master flag, computed on every write:
//                 b15 | (b23 & any of b24..b30)
// The IRQ3 request is the 0-to-1 edge of the master flag.
impl Dma {
    fn write_dicr(&mut self, value: u32) -> bool {
        let before = self.master_flag();
        // Writing 1 to a flag clears it; the writable bits are replaced.
        let flags = self.dicr & DICR_FLAGS & !(value & DICR_FLAGS);
        self.dicr = self.dicr & DICR_BUS_ERROR | (value & DICR_WRITABLE) | flags;
        !before && self.master_flag()
    }

    /// Channel `ch` has finished a transfer. Sets its DICR flag (bit
    /// `24 + ch`) when its enable bit (`16 + ch`) is set, and returns `true`
    /// when that takes the master flag from clear to set: the edge the
    /// interrupt controller treats as a DMA request.
    pub fn notify_channel_done(&mut self, ch: usize) -> bool {
        // PSX-SPX: the flag is set only when both the channel's enable and
        // the master enable are on.
        let enabled = self.dicr & (1 << (16 + ch)) != 0 && self.dicr & DICR_MASTER_ENABLE != 0;
        if !enabled {
            return false;
        }
        let before = self.master_flag();
        self.dicr |= 1 << (24 + ch);
        !before && self.master_flag()
    }
}

// --- Transfer execution ---
impl Dma {
    /// Run the OTC (channel 6) transfer if its start and manual-trigger bits
    /// are both set; otherwise RAM is left untouched.
    ///
    /// OTC builds an ordering-table linked list: MADR is the head (highest
    /// address), each word holds the 24-bit address of the next word
    /// 4 bytes lower, and the word at the lowest address is the terminator
    /// `0x00FF_FFFF` (PSX-SPX: "reverse clear OT"). BCR is the word count.
    ///
    /// CHCR start/busy is not cleared here: the caller schedules the
    /// completion, so polling CHCR keeps seeing busy until then.
    ///
    /// Returns the word count transferred (0 if not started). Called by
    /// [`crate::Bus`] after every CHCR write.
    pub fn run_otc(&mut self, ram: &mut [u8]) -> u32 {
        let otc = self.channels[6];
        if otc.channel_control & (CHCR_START | CHCR_TRIGGER) != (CHCR_START | CHCR_TRIGGER) {
            return 0;
        }
        let count = match otc.block_control & 0xFFFF {
            0 => 0x1_0000,
            n => n,
        };
        let mask = ram.len() as u32 - 1;
        let mut addr = otc.base & 0x00FF_FFFC;
        for i in 0..count {
            let next = if i + 1 == count {
                0x00FF_FFFF
            } else {
                addr.wrapping_sub(4) & 0x00FF_FFFF
            };
            let at = (addr & mask) as usize;
            ram[at..at + 4].copy_from_slice(&next.to_le_bytes());
            addr = addr.wrapping_sub(4) & 0x00FF_FFFC;
        }
        count
    }
}

fn decode(phys: u32) -> (usize, u32) {
    let rel = phys - Dma::BASE;
    let ch = (rel / Dma::STRIDE) as usize;
    let field = rel % Dma::STRIDE;
    (ch, field)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contains_matches_mmio_range() {
        assert!(Dma::contains(0x1F80_1080));
        assert!(Dma::contains(0x1F80_10F4));
        assert!(!Dma::contains(0x1F80_107F));
        assert!(!Dma::contains(0x1F80_10F8));
    }

    #[test]
    fn channel_roundtrips_all_three_regs() {
        let mut dma = Dma::new();
        dma.write32(0x1F80_1080, 0x0012_3456); // ch0 base
        dma.write32(0x1F80_1084, 0xCAFEBABE); // ch0 bcr
        dma.write32(0x1F80_1088, 0xDEADBEEF); // ch0 chcr
        assert_eq!(dma.read32(0x1F80_1080), 0x0012_3456);
        assert_eq!(dma.read32(0x1F80_1084), 0xCAFEBABE);
        assert_eq!(dma.read32(0x1F80_1088), 0xDEADBEEF);
    }

    #[test]
    fn base_address_is_masked_to_24_bits() {
        let mut dma = Dma::new();
        dma.write32(0x1F80_1080, 0xDEAD_BEEF);
        assert_eq!(dma.read32(0x1F80_1080), 0x00AD_BEEF);
    }

    #[test]
    fn dpcr_roundtrips_verbatim() {
        let mut dma = Dma::new();
        dma.write32(0x1F80_10F0, 0x0765_4321);
        assert_eq!(dma.read32(0x1F80_10F0), 0x0765_4321);
    }

    #[test]
    fn partial_reads_see_live_register_bytes() {
        let mut dma = Dma::new();
        dma.write32(0x1F80_10F0, 0x0765_4321);
        assert_eq!(dma.read8(0x1F80_10F0), 0x21);
        assert_eq!(dma.read8(0x1F80_10F2), 0x65);
        assert_eq!(dma.read16(0x1F80_10F1), 0x6543);
    }

    #[test]
    fn dicr_byte_write_toggles_channel_irq_enable() {
        let mut disabled = Dma::new();
        disabled.write8(0x1F80_10F6, 0x80); // master enable, channel 3 IRQ disabled
        assert_eq!(disabled.read8(0x1F80_10F6) & 0x08, 0);
        assert!(!disabled.notify_channel_done(3));

        let mut enabled = Dma::new();
        enabled.write8(0x1F80_10F6, 0x88); // master enable + channel 3 IRQ enable
        assert_ne!(enabled.read8(0x1F80_10F6) & 0x08, 0);
        assert!(enabled.notify_channel_done(3));
    }

    #[test]
    fn dicr_rw_bits_roundtrip_w1c_bits_dont() {
        // Configure all enables + master, leave flags clear. R/W bits
        // round-trip; W1C bits (15, 24..30) ignore writes-of-zero.
        let mut dma = Dma::new();
        dma.write32(0x1F80_10F4, 0x00FF_0000); // enables 16..23
                                               // Read-back includes the computed master flag in bit 31 -- clear
                                               // here because no per-channel flag is set.
        assert_eq!(dma.read32(0x1F80_10F4), 0x00FF_0000);
    }

    #[test]
    fn dicr_channel_done_sets_pending_when_enabled() {
        let mut dma = Dma::new();
        // Enable channel 6 + master.
        dma.write32(0x1F80_10F4, (1 << (16 + 6)) | (1 << 23));
        let edge = dma.notify_channel_done(6);
        assert!(edge, "0->1 master flag transition expected");
        let dicr = dma.read32(0x1F80_10F4);
        assert!(dicr & (1 << (24 + 6)) != 0, "channel-6 flag set");
        assert!(dicr & (1 << 31) != 0, "master flag set");
    }

    #[test]
    fn dicr_channel_done_ignored_when_disabled() {
        let mut dma = Dma::new();
        // Master enable on but channel 2 enable off.
        dma.write32(0x1F80_10F4, 1 << 23);
        let edge = dma.notify_channel_done(2);
        assert!(!edge);
        assert_eq!(dma.read32(0x1F80_10F4) & (1 << (24 + 2)), 0);
    }

    #[test]
    fn dicr_w1c_clears_pending_flag() {
        let mut dma = Dma::new();
        dma.write32(0x1F80_10F4, (1 << 16) | (1 << 23));
        dma.notify_channel_done(0);
        assert!(dma.read32(0x1F80_10F4) & (1 << 24) != 0);
        // BIOS acks by writing 1 to the flag bit (along with re-asserting
        // the R/W enables, which the BIOS has been managing all along).
        dma.write32(0x1F80_10F4, (1 << 16) | (1 << 23) | (1 << 24));
        let dicr = dma.read32(0x1F80_10F4);
        assert_eq!(dicr & (1 << 24), 0, "flag cleared by W1C");
        assert_eq!(dicr & (1 << 31), 0, "master flag follows");
    }

    #[test]
    fn dicr_write_can_raise_master_irq_edge() {
        let mut dma = Dma::new();
        // Simulate a pending GPU-DMA flag with the channel enable set,
        // but the master flag still clear. Writing DICR with the master
        // enable on in this state is a 0 to 1 edge on the master flag.
        dma.dicr = (1 << (16 + 2)) | (1 << (24 + 2));
        let edge = dma.write32(0x1F80_10F4, (1 << (16 + 2)) | (1 << 23));
        assert!(edge, "DICR write should create a master IRQ edge");
        assert!(dma.read32(0x1F80_10F4) & (1 << 31) != 0);
    }

    #[test]
    fn all_seven_channels_are_addressable() {
        let mut dma = Dma::new();
        for ch in 0..NUM_CHANNELS as u32 {
            let addr = 0x1F80_1080 + ch * 0x10;
            dma.write32(addr, ch * 0x1000);
            assert_eq!(dma.read32(addr), ch * 0x1000);
        }
    }

    #[test]
    fn otc_chcr_masks_unused_bits_and_hardwires_decrement() {
        let mut dma = Dma::new();
        dma.write32(0x1F80_10E8, 0x7077_0703);
        assert_eq!(dma.read32(0x1F80_10E8), 0x5000_0002);
        dma.write32(0x1F80_10E8, 0);
        assert_eq!(dma.read32(0x1F80_10E8), 0x0000_0002);
        dma.write32(0x1F80_10E8, 0x8E88_F8FC);
        assert_eq!(dma.read32(0x1F80_10E8), 0x0000_0002);
    }

    fn set_otc(dma: &mut Dma, base: u32, count: u32) {
        // CH6: base, block count, start (bit 24) + busy (bit 28)
        dma.write32(0x1F80_10E0, base);
        dma.write32(0x1F80_10E4, count);
        dma.write32(0x1F80_10E8, (1 << 24) | (1 << 28));
    }

    #[test]
    fn otc_madr_is_head_terminator_at_tail() {
        let mut dma = Dma::new();
        let mut ram = vec![0u8; 2 * 1024 * 1024];
        // Sentinel just below the OT range -- must remain untouched
        // (validates that the loop's last chain-write to 0x3F4 is
        // overwritten by the terminator, not extended into 0x3F0).
        write_u32(&mut ram, 0x3F0, 0xDEAD_BEEF);

        // 4-entry OT with MADR (head) at 0x400; terminator lands at
        // 0x3F4 (= 0x400 - (4-1)*4).
        set_otc(&mut dma, 0x0000_0400, 4);
        assert_eq!(dma.run_otc(&mut ram), 4);

        // Head: MADR points to next-step-down.
        assert_eq!(read_u32(&ram, 0x400), 0x0000_03FC);
        assert_eq!(read_u32(&ram, 0x3FC), 0x0000_03F8);
        assert_eq!(read_u32(&ram, 0x3F8), 0x0000_03F4);
        // Tail: lowest address holds the terminator.
        assert_eq!(read_u32(&ram, 0x3F4), 0x00FF_FFFF);
        // Sentinel below the tail is untouched.
        assert_eq!(read_u32(&ram, 0x3F0), 0xDEAD_BEEF);
    }

    #[test]
    fn otc_does_not_clear_start_and_busy_bits_synchronously() {
        // Bus is responsible for clearing the busy bits at the
        // scheduled completion cycle; the
        // DMA module itself just transfers data. Start AND busy bits
        // both stay set during the "virtual transfer window" so BIOS
        // polling of CHCR sees the busy state until then.
        //
        // Preventing duplicate runs is done at the bus level via the
        // CHCR-write-only trigger -- only a CHCR write with bit 24 set
        // enters `maybe_run_dma`. See `bus.rs:write32` handling of the
        // DMA region.
        let mut dma = Dma::new();
        let mut ram = vec![0u8; 2 * 1024 * 1024];
        set_otc(&mut dma, 0x0000_0100, 1);
        assert_eq!(dma.run_otc(&mut ram), 1);

        let chcr = dma.channels[6].channel_control;
        assert_ne!(chcr & (1 << 24), 0, "start bit must remain set");
        assert_ne!(chcr & (1 << 28), 0, "busy bit must remain set");
    }

    #[test]
    fn otc_is_noop_when_start_bit_not_set() {
        let mut dma = Dma::new();
        let mut ram = vec![0u8; 2 * 1024 * 1024];
        // Write base/count but not start bit.
        dma.write32(0x1F80_10E0, 0x0000_0100);
        dma.write32(0x1F80_10E4, 1);
        assert_eq!(dma.run_otc(&mut ram), 0);
        assert_eq!(read_u32(&ram, 0x100), 0);
    }

    #[test]
    fn otc_is_noop_without_manual_trigger_bit() {
        let mut dma = Dma::new();
        let mut ram = vec![0u8; 2 * 1024 * 1024];
        dma.write32(0x1F80_10E0, 0x0000_0100);
        dma.write32(0x1F80_10E4, 1);
        dma.write32(0x1F80_10E8, 1 << 24);
        assert_eq!(dma.run_otc(&mut ram), 0);
        assert_eq!(read_u32(&ram, 0x100), 0);
    }

    fn read_u32(ram: &[u8], offset: u32) -> u32 {
        let o = offset as usize;
        u32::from_le_bytes([ram[o], ram[o + 1], ram[o + 2], ram[o + 3]])
    }

    fn write_u32(ram: &mut [u8], offset: u32, value: u32) {
        let o = offset as usize;
        ram[o..o + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn dicr_flag_needs_both_the_channel_enable_and_the_master_enable() {
        let mut dma = Dma::new();
        dma.write32(0x1F80_10F4, 1 << (16 + 3)); // channel 3 enable only
        assert!(!dma.notify_channel_done(3));
        assert_eq!(dma.read32(0x1F80_10F4) & (1 << 27), 0);
    }

    #[test]
    fn dicr_master_flag_is_computed_and_ignores_the_written_bit_31() {
        let mut dma = Dma::new();
        // Writing bit 31 with nothing pending does not set it.
        dma.write32(0x1F80_10F4, 0x8000_0000);
        assert_eq!(dma.read32(0x1F80_10F4), 0);
        // A flag raises it only while the master enable is on.
        dma.write32(0x1F80_10F4, (1 << 17) | (1 << 23));
        assert!(dma.notify_channel_done(1));
        assert_ne!(dma.read32(0x1F80_10F4) & (1 << 31), 0);
        dma.write32(0x1F80_10F4, 1 << 17); // master enable off, flag kept
        assert_eq!(dma.read32(0x1F80_10F4) & (1 << 31), 0);
        // Turning the master enable back on is a 0 to 1 edge.
        assert!(dma.write32(0x1F80_10F4, (1 << 17) | (1 << 23)));
    }

    #[test]
    fn dicr_partial_writes_do_not_acknowledge_flags() {
        let mut dma = Dma::new();
        dma.write32(0x1F80_10F4, (1 << 17) | (1 << 23));
        dma.notify_channel_done(1);
        // A byte write of 1 to the flag does not clear it, unlike a word write.
        dma.write8(0x1F80_10F7, 0x02);
        assert_ne!(dma.read32(0x1F80_10F4) & (1 << 25), 0);
    }
}
