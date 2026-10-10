//! SIO0 -- controller / memory-card serial port.
//!
//! Register map at `0x1F80_1040`:
//!   +0  `SIO0_DATA`  TX/RX FIFO (8-bit; reads may be accessed as wider)
//!   +4  `SIO0_STAT`  status (32-bit, read-only)
//!   +8  `SIO0_MODE`  framing config (16-bit)
//!   +A  `SIO0_CTRL`  control (16-bit)
//!   +E  `SIO0_BAUD`  baud divisor (16-bit)
//!
//! The core default has a digital controller and memory card on port 1,
//! plus a memory card on port 2. Frontend launches replace the card
//! backing with the per-game save file, while parity probes use the
//! same fresh (formatted, empty) card default.
//!
//! ## Provenance
//!
//! The register map, the STAT/CTRL bit meanings, the BAUD relation and
//! the pad and memory card byte protocols are written from nocash PSX-SPX
//! ("Serial Interfaces (SIO)" and "Controllers and Memory Cards"). The
//! exchange timing is a model, and each part says what pins it:
//!
//! - **A byte takes ten bit times**: the received byte reaches DATA, and
//!   `STAT.RX_NOT_EMPTY` rises, when the transfer ends; `/ACK` follows it by
//!   the measured delay of the device and its pulse is as wide as measured
//!   (hardware tests v2.1). Writing DATA while a byte is on the wire queues
//!   the next one.
//! - **A missing device** returns 0xFF and produces no ACK and no IRQ
//!   (PSX-SPX: IRQ7 follows the /ACK edge). A timeout IRQ there gave a
//!   commercial title that polls an empty port 2 extra IRQ7 passes inside
//!   one folded ISR (gate-pinned).
//! - **No second IRQ is scheduled while STAT.IRQ is still latched**; the
//!   bit stays set until CTRL.ACK clears it (PSX-SPX "SIO0_STAT"), and the
//!   compat hashes expect no new IRQ in between (gate-pinned).
//!
//! See `LICENSE` and `docs/PROVENANCE.md`.

mod stat_bit {
    // Layout facts come from the shared hardware-model crate, the same
    // source psx-pad and psx-mc consume, so the third hand-copied set of
    // these bits is gone. Local names kept for the emulator's call sites.
    use psx_hw::sio::sio0;
    pub const TX_READY_1: u32 = sio0::stat::TX_READY;
    pub const RX_NOT_EMPTY: u32 = sio0::stat::RX_NOT_EMPTY;
    pub const TX_READY_2: u32 = sio0::stat::TX_IDLE;
    /// ACK/DSR input level from the selected device.
    pub const ACK_INPUT: u32 = sio0::stat::DSR_LEVEL;
    /// SIO0 interrupt latched in the controller's own STAT register.
    pub const IRQ: u32 = sio0::stat::IRQ;
}

mod ctrl_bit {
    use psx_hw::sio::sio0;
    /// bit 1 -- drive `/JOYN` output. Transitioning high-to-low is how
    /// the CPU "selects" the device at the start of a transfer; going
    /// low-to-high deselects and resets the device state machine.
    pub const JOYN_OUTPUT: u16 = sio0::ctrl::DTR;
    /// SIO0-only one-shot receive override. Normal selected-port traffic keeps
    /// this clear; hardware automatically clears it after one received byte.
    pub const FORCE_RX_ONCE: u16 = sio0::ctrl::FORCE_RX_ONCE;
    /// Write-1 to acknowledge pending IRQ bits in STAT.
    pub const ACK: u16 = sio0::ctrl::ACK;
    /// Write-1 to soft-reset the port.
    pub const RESET: u16 = sio0::ctrl::RESET;
    /// bit 12 -- enable IRQ7 generation from controller ACK pulses.
    pub const ACK_IRQ_ENABLE: u16 = sio0::ctrl::ACK_IRQ_EN;
    /// bit 13 -- port / slot select (0 = JOY1, 1 = JOY2).
    pub const SLOT: u16 = sio0::ctrl::SLOT_PORT2;
}

mod offset {
    pub const DATA: u32 = 0x0;
    pub const STAT: u32 = 0x4;
    /// High half of the 32-bit STAT register; 16-bit reads land here
    /// when software does two half-reads. Returns zero on hardware.
    pub const STAT_HI: u32 = 0x6;
    pub const MODE: u32 = 0x8;
    pub const CTRL: u32 = 0xA;
    pub const BAUD: u32 = 0xE;
}

/// SIO0 MODE exposes only its framing/baud configuration bits. Reserved bits
/// read back zero; this is visible even when software uses byte stores because
/// the R3000A still drives the complete source register on the peripheral bus.
const MODE_WRITE_MASK: u16 = 0x013F;

/// Bit times one byte takes on the wire (hwtest v2.1 on a console: the byte
/// is in RX 1326 to 1438 clocks after the write with BAUD 0x88, a 136-clock
/// bit, so ten bit times and not the eight of the data bits alone).
const BYTE_BIT_TIMES: u64 = 10;
/// Serial-transfer time for one byte when BAUD is zero: the BIOS's common
/// BAUD of 0x88.
const DEFAULT_TRANSFER_TICKS: u64 = 0x88 * BYTE_BIT_TIMES;
// `/ACK` timing, from hwtest v2.2 on a console (an SCPH-110 with its pad,
// id 0x73, and cards in both slots). The rise is measured from the DATA write
// and the width between the edges, both through a STAT polling loop; the
// figures below are those less the ten bit times (1360) and the few clocks
// the probe's own read adds on the emulator, so the probe reads the console's
// figure back. The device answers each byte of a transaction in its own time,
// so the delay depends on the byte's place in it.
//
// Pad, bytes 0 to 7 (the ninth byte of a poll draws no `/ACK`); the console's
// rise was 1665, 1678, 1627, 1629, 1468, 1695, 1695, 1692 clocks, width 92.
const PAD_ACK_DELAYS: [u64; 8] = [290, 311, 260, 262, 101, 320, 320, 325];
/// Clocks from the end of a pad byte to its `/ACK` rising, for a byte past
/// the table.
const PAD_ACK_DELAY_TICKS: u64 = 328;
// Memory card, slot 1 and 2, bytes 0 to 3 (select, command, ID bytes) as
// measured: rise 1717, 1682, 1523, 1516 and 1691, 1552, 1465, 1497, width 44
// and 68, 68, 80, 80. The bytes after those were not timed one by one; the
// delay for them is fitted so a whole frame read, 140 bytes, takes what the
// console's did, 124 HBlanks in slot 1 and 138 to 139 in slot 2 (records
// 0x641 and 0x649, 0x693).
const MEMCARD_ACK_DELAYS: [[u64; 4]; 2] = [[352, 309, 146, 146], [316, 177, 90, 122]];
/// Fitted delay for the card bytes after the fourth, slot 1 and 2.
const MEMCARD_DATA_ACK_DELAY_TICKS: [u64; 2] = [308, 512];
/// `/ACK` is a pulse, not a sticky level: its width for a pad and for a card
/// in slot 1 and 2 (bytes 0 to 3, then the rest).
const PAD_ACK_PULSE_TICKS: u64 = 92;
const PAD_ACK_PULSES: [u64; 8] = [100, 92, 92, 92, 92, 92, 92, 92];
const MEMCARD_ACK_PULSES: [[u64; 4]; 2] = [[38, 44, 48, 50], [74, 74, 86, 86]];
const MEMCARD_DATA_ACK_PULSE_TICKS: [u64; 2] = [44, 86];

/// `/ACK` delay and pulse width of byte `index` of a transaction.
fn ack_timing(is_card: bool, slot: usize, index: u32) -> (u64, u64) {
    let index = index as usize;
    if is_card {
        match MEMCARD_ACK_DELAYS[slot].get(index) {
            Some(&delay) => (delay, MEMCARD_ACK_PULSES[slot][index]),
            None => (
                MEMCARD_DATA_ACK_DELAY_TICKS[slot],
                MEMCARD_DATA_ACK_PULSE_TICKS[slot],
            ),
        }
    } else {
        match PAD_ACK_DELAYS.get(index) {
            Some(&delay) => (delay, PAD_ACK_PULSES[index]),
            None => (PAD_ACK_DELAY_TICKS, PAD_ACK_PULSE_TICKS),
        }
    }
}

/// SIO0 state. Register-level accuracy for the "nothing plugged in"
/// path; no shift-clock simulation, but every byte-write pulses an
/// IRQ7 so the BIOS's pad-poll handler advances its descriptors.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct Sio0 {
    mode: u16,
    ctrl: u16,
    baud: u16,
    /// One-slot RX buffer. The real chip has a small FIFO; the BIOS
    /// only reads one response per TX so a single slot is enough.
    /// `None` means RX empty (`STAT.RX_NOT_EMPTY` clears).
    rx: Option<u8>,
    /// Set by `write_data`, consumed by [`Sio0::take_pending_irq`].
    /// The Bus raises `IrqSource::Controller` when true. Mirrors how
    /// real hardware pulses IRQ7 when `/DSR` ACKs a transfer (or
    /// DSR-timeout fires if nothing answers).
    pending_irq: bool,
    /// Sticky ACK input level exposed in `STAT` bit 7. Cleared when a
    /// new transfer starts, on deselect, or on reset.
    ack_input: bool,
    /// Sticky SIO IRQ bit exposed in `STAT` bit 9. Writing CTRL.ACK
    /// clears it; the bus-level interrupt controller is separate.
    irq_latched: bool,
    /// Byte the selected device answered with; it reaches DATA when the
    /// transfer on the wire ends.
    pending_rx: u8,
    /// Whether that byte is received at all (a port that is not selected
    /// receives nothing).
    #[serde(default)]
    pending_rx_valid: bool,
    /// One-byte TX holding register: a byte written while another is on the
    /// wire waits here and starts when it ends.
    queued_tx: Option<u8>,
    /// Whether the current byte will be followed by a delayed SIO
    /// event.
    pending_ack: bool,
    /// Whether the current byte should raise an IRQ without a visible
    /// ACK pulse. Kept for the legacy delayed-byte path; missing devices
    /// leave this false.
    pending_dsr_timeout: bool,
    /// A byte is on the wire.
    transfer_busy: bool,
    /// The byte has gone and its `/ACK` is yet to come.
    awaiting_ack: bool,
    /// Absolute cycle at which the current byte transfer completes.
    transfer_deadline: Option<u64>,
    /// Absolute cycle at which the pending ACK pulse should fire.
    ack_deadline: Option<u64>,
    /// Absolute cycle at which the ACK pulse ends.
    ack_end_deadline: Option<u64>,
    /// Delay for the currently selected device kind.
    ack_delay_ticks: u64,
    /// Width of the `/ACK` pulse of the byte on the wire.
    #[serde(default)]
    ack_pulse_ticks: u64,
    /// Bytes clocked since the port was selected: the devices answer each
    /// byte of a transaction in their own time.
    #[serde(default)]
    byte_index: u32,
    /// Device on port 1 (controller slot 1 + memory card 1).
    port1: crate::pad::PortDevice,
    /// Device on port 2 (controller slot 2 + memory card 2).
    port2: crate::pad::PortDevice,
    /// Last observed JOYN-output level. We use high-to-low
    /// transitions (deselect → select) to reset device state
    /// machines, matching hardware.
    last_joyn: bool,
    /// Opt-in: model a slow original controller (e.g. SCPH-1200) that only
    /// becomes ready for the next byte once it has pulsed `/ACK`. A host that
    /// clocks the next byte without waiting for that pulse desyncs the packet.
    /// Off by default; the idealized fast pad above is what normal runs use.
    slow_pad: bool,
    /// While `slow_pad`: the cycle the device is ready for the next byte (the
    /// current byte's `/ACK` deadline). `None` between transactions.
    slow_ready_cycle: Option<u64>,
    /// While `slow_pad`: set once a byte was clocked before the device was
    /// ready; the rest of the packet then returns `0xFF` until deselect.
    slow_desynced: bool,
}

impl Sio0 {
    /// Physical base address of SIO0.
    pub const BASE: u32 = 0x1F80_1040;
    /// Size of the register window (`DATA..=BAUD` plus padding).
    pub const SIZE: u32 = 0x10;

    /// All registers zero, port 1 pre-populated with a digital pad and
    /// a fresh memory card, port 2 with a fresh memory card. The
    /// frontend replaces port 1's card backing on game launch; keeping
    /// a default card here lets BIOS/game card probes observe an inserted
    /// card.
    pub fn new() -> Self {
        Self {
            mode: 0,
            ctrl: 0,
            baud: 0,
            rx: None,
            pending_irq: false,
            ack_input: false,
            irq_latched: false,
            pending_rx: 0xFF,
            pending_rx_valid: false,
            queued_tx: None,
            pending_ack: false,
            pending_dsr_timeout: false,
            transfer_busy: false,
            awaiting_ack: false,
            transfer_deadline: None,
            ack_deadline: None,
            ack_end_deadline: None,
            ack_delay_ticks: PAD_ACK_DELAY_TICKS,
            ack_pulse_ticks: PAD_ACK_PULSE_TICKS,
            byte_index: 0,
            port1: crate::pad::PortDevice::empty()
                .with_pad(crate::pad::DigitalPad::new())
                .with_memcard(crate::pad::MemoryCard::new()),
            port2: crate::pad::PortDevice::empty().with_memcard(crate::pad::MemoryCard::new()),
            last_joyn: false,
            slow_pad: false,
            slow_ready_cycle: None,
            slow_desynced: false,
        }
    }

    /// Enable or disable the slow original-controller timing model. When on, a
    /// poll that does not wait for each byte's `/ACK` pulse desyncs, reproducing
    /// an SCPH-1200 against a host that clocks bytes back-to-back. Used by the
    /// regression tests below.
    pub fn set_slow_pad(&mut self, slow: bool) {
        self.slow_pad = slow;
        if !slow {
            self.slow_ready_cycle = None;
            self.slow_desynced = false;
        }
    }

    /// Immutable access to port 1. Used by the frontend to read
    /// pad state (rumble motor) without having to mutate.
    pub fn port1(&self) -> &crate::pad::PortDevice {
        &self.port1
    }

    /// Mutable access to port 1 -- lets higher layers swap a
    /// memory card in while keeping the pad attached.
    pub fn port1_mut(&mut self) -> &mut crate::pad::PortDevice {
        &mut self.port1
    }

    /// Immutable access to port 2.
    pub fn port2(&self) -> &crate::pad::PortDevice {
        &self.port2
    }

    /// Mutable access to port 2.
    pub fn port2_mut(&mut self) -> &mut crate::pad::PortDevice {
        &mut self.port2
    }

    /// Plug a device into port 1 (typical for "player 1" games).
    pub fn attach_port1(&mut self, device: crate::pad::PortDevice) {
        self.port1 = device;
    }

    /// Plug a device into port 2.
    pub fn attach_port2(&mut self, device: crate::pad::PortDevice) {
        self.port2 = device;
    }

    /// Update the button state held on port 1.
    pub fn set_port1_buttons(&mut self, buttons: crate::pad::ButtonState) {
        self.port1.set_buttons(buttons);
    }

    /// Update the button state held on port 2.
    pub fn set_port2_buttons(&mut self, buttons: crate::pad::ButtonState) {
        self.port2.set_buttons(buttons);
    }

    /// Returns true and clears the flag when a DATA write has armed
    /// IRQ7. The Bus calls this after dispatching a write.
    pub fn take_pending_irq(&mut self) -> bool {
        let p = self.pending_irq;
        self.pending_irq = false;
        p
    }

    /// Snapshot the software-visible STAT register without consuming
    /// any FIFO state. Diagnostic-only.
    pub fn debug_stat(&self) -> u32 {
        self.stat()
    }

    /// Diagnostic-only copy of the raw CTRL register.
    pub fn debug_ctrl(&self) -> u16 {
        self.ctrl
    }

    /// Whether `service_sio0` would currently raise a controller IRQ
    /// edge to the bus.
    pub fn debug_pending_irq(&self) -> bool {
        self.pending_irq
    }

    /// Sticky IRQ bit exposed in `STAT`.
    pub fn debug_irq_latched(&self) -> bool {
        self.irq_latched
    }

    /// Whether a TX byte is currently shifting.
    pub fn debug_transfer_busy(&self) -> bool {
        self.transfer_busy
    }

    /// Whether the transfer phase finished and the ACK delay is in
    /// flight.
    pub fn debug_awaiting_ack(&self) -> bool {
        self.awaiting_ack
    }

    /// Absolute cycle where the current byte transfer completes.
    pub fn debug_transfer_deadline(&self) -> Option<u64> {
        self.transfer_deadline
    }

    /// Absolute cycle where the next ACK pulse should begin.
    pub fn debug_ack_deadline(&self) -> Option<u64> {
        self.ack_deadline
    }

    /// Absolute cycle where the current ACK pulse ends.
    pub fn debug_ack_end_deadline(&self) -> Option<u64> {
        self.ack_end_deadline
    }

    /// Earliest pending state-machine deadline across the three
    /// timers, or `None` if SIO0 is idle. The bus uses this to
    /// (re)schedule [`EventSlot::Sio0`] so the per-instruction
    /// `Bus::tick` poll can be retired in favour of one event-driven
    /// wake-up.
    pub fn next_deadline(&self) -> Option<u64> {
        let mut next: Option<u64> = None;
        let consider = |cur: &mut Option<u64>, candidate: Option<u64>| {
            if let Some(c) = candidate {
                *cur = Some(match cur {
                    Some(prev) => (*prev).min(c),
                    None => c,
                });
            }
        };
        consider(&mut next, self.transfer_deadline);
        consider(&mut next, self.ack_deadline);
        consider(&mut next, self.ack_end_deadline);
        next
    }

    /// RX slot contents, if a byte is waiting to be read.
    pub fn debug_rx(&self) -> Option<u8> {
        self.rx
    }

    /// Next TX byte queued behind the active transfer.
    pub fn debug_queued_tx(&self) -> Option<u8> {
        self.queued_tx
    }

    /// `true` when `phys` falls within the SIO0 register window.
    #[inline]
    pub fn contains(phys: u32) -> bool {
        (Self::BASE..Self::BASE + Self::SIZE).contains(&phys)
    }

    /// `SIO0_STAT`. Byte exchange completes synchronously; the baud-clocked
    /// delay is for the later SIO IRQ event, not DATA visibility.
    fn stat(&self) -> u32 {
        let mut s = 0;
        if self.queued_tx.is_none() {
            s |= stat_bit::TX_READY_1;
        }
        if !self.transfer_busy {
            s |= stat_bit::TX_READY_2;
        }
        if self.rx.is_some() {
            s |= stat_bit::RX_NOT_EMPTY;
        }
        if self.ack_input {
            s |= stat_bit::ACK_INPUT;
        }
        if self.irq_latched {
            s |= stat_bit::IRQ;
        }
        s
    }

    /// Pop the RX slot, returning 0xFF when empty. Used by all three
    /// widths of DATA read -- the real chip zero-extends on the bus.
    fn pop_rx(&mut self) -> u8 {
        self.rx.take().unwrap_or(0xFF)
    }

    /// Transfer time for one byte: [`BYTE_BIT_TIMES`] bit times of `BAUD`
    /// clocks; when the BIOS hasn't set BAUD yet we still give software a
    /// realistic default delay instead of "instant byte".
    fn transfer_ticks(&self) -> u64 {
        let baud = self.baud as u64;
        if baud != 0 {
            baud.saturating_mul(BYTE_BIT_TIMES)
        } else {
            DEFAULT_TRANSFER_TICKS
        }
    }

    /// Advance any pending transfer / ACK timers to `now`.
    pub fn tick(&mut self, now: u64) {
        loop {
            if let Some(deadline) = self.transfer_deadline {
                if deadline <= now {
                    self.transfer_deadline = None;
                    if self.pending_rx_valid {
                        self.rx = Some(self.pending_rx);
                    }
                    self.transfer_busy = false;
                    if self.pending_ack {
                        self.awaiting_ack = true;
                        self.ack_deadline = Some(deadline.saturating_add(self.ack_delay_ticks));
                    }
                    if let Some(value) = self.queued_tx.take() {
                        self.start_transfer_at(value, deadline, false);
                    } else if !self.awaiting_ack {
                        self.transfer_busy = false;
                    }
                    continue;
                }
            }

            if let Some(deadline) = self.ack_deadline {
                if deadline <= now {
                    self.ack_deadline = None;
                    self.awaiting_ack = false;
                    let ack_pulse = self.pending_ack;
                    let dsr_timeout = self.pending_dsr_timeout;
                    self.pending_ack = false;
                    self.pending_dsr_timeout = false;
                    if ack_pulse {
                        // Keep IRQ and visible ACK/DSR in phase. Crash's
                        // BIOS pad handler samples the ACK level while it
                        // services the interrupt; raising IRQ at RX-ready
                        // time makes digital polls stop before the high
                        // button byte.
                        self.ack_input = true;
                        self.ack_end_deadline = Some(deadline.saturating_add(self.ack_pulse_ticks));
                    }
                    if (ack_pulse || dsr_timeout)
                        && self.ctrl & ctrl_bit::ACK_IRQ_ENABLE != 0
                        && !self.irq_latched
                    {
                        self.irq_latched = true;
                        self.pending_irq = true;
                    }
                    continue;
                }
            }

            if let Some(deadline) = self.ack_end_deadline {
                if deadline <= now {
                    self.ack_end_deadline = None;
                    self.ack_input = false;
                    continue;
                }
            }

            break;
        }
    }

    /// 32-bit read dispatch. `Some(value)` for every offset in the
    /// window; unrecognised offsets read as zero to match the general
    /// MMIO echo-buffer fallback. Reading `DATA` consumes the RX slot.
    pub fn read32(&mut self, phys: u32) -> Option<u32> {
        match phys - Self::BASE {
            offset::DATA => Some(self.pop_rx() as u32),
            offset::STAT => Some(self.stat()),
            offset::MODE => Some(self.mode as u32),
            offset::CTRL => Some(self.ctrl as u32),
            offset::BAUD => Some(self.baud as u32),
            _ => Some(0),
        }
    }

    /// 16-bit read dispatch.
    pub fn read16(&mut self, phys: u32) -> Option<u16> {
        match phys - Self::BASE {
            offset::DATA => Some(self.pop_rx() as u16),
            offset::STAT => Some(self.stat() as u16),
            offset::STAT_HI => Some(0),
            offset::MODE => Some(self.mode),
            offset::CTRL => Some(self.ctrl),
            offset::BAUD => Some(self.baud),
            _ => Some(0),
        }
    }

    /// 8-bit read dispatch.
    pub fn read8(&mut self, phys: u32) -> Option<u8> {
        match phys - Self::BASE {
            offset::DATA => Some(self.pop_rx()),
            offset::STAT => Some(self.stat() as u8),
            offset::MODE => Some(self.mode as u8),
            offset::CTRL => Some(self.ctrl as u8),
            offset::BAUD => Some(self.baud as u8),
            _ => Some(0),
        }
    }

    /// TX clocks one byte across the active port's serial link. The
    /// selected device (if any) returns its RX byte and whether it
    /// wants another round (pulls `/DSR` low → IRQ7 armed).
    fn write_data_at(&mut self, value: u8, now: u64) {
        if self.transfer_busy {
            // The shifter is full: the byte waits in the TX register (a
            // second one overwrites it, as the chip drops a byte written
            // into a full holding register).
            self.queued_tx = Some(value);
            return;
        }
        self.start_transfer_at(value, now, true);
    }

    fn start_transfer_at(&mut self, value: u8, now: u64, clear_ack_pulse: bool) {
        // A fresh byte clocks a new phase of the transfer, so the
        // previous ACK pulse is no longer visible.
        if clear_ack_pulse {
            self.ack_input = false;
            self.ack_end_deadline = None;
        }
        let selected = self.ctrl & ctrl_bit::JOYN_OUTPUT != 0;
        let force_rx_once = self.ctrl & ctrl_bit::FORCE_RX_ONCE != 0;
        let receive_enabled = selected || force_rx_once;
        let byte_index = self.byte_index;
        self.byte_index = self.byte_index.saturating_add(1);
        let (rx, ack, _device_present, ack_delay_ticks, ack_pulse_ticks) = if selected {
            // Slow original-controller timing: a byte clocked before the
            // previous byte's `/ACK` deadline arrives while the device is still
            // busy, so it misses the clock and the rest of the packet desyncs.
            if self.slow_pad {
                if self.slow_ready_cycle.is_some_and(|ready| now < ready) {
                    self.slow_desynced = true;
                }
                self.slow_ready_cycle = Some(now.saturating_add(self.transfer_ticks()));
            }
            if self.slow_pad && self.slow_desynced {
                // The device missed the clock; it returns idle and does not
                // advance its state machine.
                (0xFF, false, true, PAD_ACK_DELAY_TICKS, PAD_ACK_PULSE_TICKS)
            } else {
                let slot = usize::from(self.ctrl & ctrl_bit::SLOT != 0);
                let port = self.active_port();
                let result = port.exchange_detailed_at(value, now);
                let (ack_delay_ticks, ack_pulse_ticks) =
                    ack_timing(port.selected_is_memcard(), slot, byte_index);
                (
                    result.rx,
                    result.ack,
                    result.device_present,
                    ack_delay_ticks,
                    ack_pulse_ticks,
                )
            }
        } else {
            (0xFF, false, true, PAD_ACK_DELAY_TICKS, PAD_ACK_PULSE_TICKS)
        };
        // A missing device returns 0xFF with no ACK/DSR IRQ for that byte.
        // One commercial title polls port 2 during the BIOS pad handler;
        // raising a timeout IRQ there invents extra IRQ7 passes inside the
        // same folded ISR.
        let dsr_timeout = false;
        self.pending_rx = rx;
        self.pending_rx_valid = receive_enabled;
        self.pending_ack = ack;
        self.pending_dsr_timeout = dsr_timeout;
        self.ack_delay_ticks = ack_delay_ticks;
        self.ack_pulse_ticks = ack_pulse_ticks;
        if receive_enabled {
            // SIO0 CTRL.2 is a force-receive strobe, not a persistent RX
            // enable. Retail hardware clears it after exactly one byte.
            self.ctrl &= !ctrl_bit::FORCE_RX_ONCE;
        }
        // The byte is on the wire for ten bit times; DATA gets the answer,
        // and `/ACK` follows, when they have passed (see `tick`).
        self.transfer_busy = true;
        self.awaiting_ack = false;
        self.ack_deadline = None;
        self.transfer_deadline = Some(now.saturating_add(self.transfer_ticks()));
    }

    /// Device selected by the current `CTRL.SLOT` bit.
    fn active_port(&mut self) -> &mut crate::pad::PortDevice {
        if self.ctrl & ctrl_bit::SLOT == 0 {
            &mut self.port1
        } else {
            &mut self.port2
        }
    }

    fn write_ctrl(&mut self, value: u16) {
        if value & ctrl_bit::RESET != 0 {
            self.mode = 0;
            self.ctrl = 0;
            self.baud = 0;
            self.pending_irq = false;
            self.ack_input = false;
            self.irq_latched = false;
            self.pending_rx = 0xFF;
            self.pending_rx_valid = false;
            self.byte_index = 0;
            self.queued_tx = None;
            self.pending_ack = false;
            self.pending_dsr_timeout = false;
            self.transfer_busy = false;
            self.awaiting_ack = false;
            self.transfer_deadline = None;
            self.ack_deadline = None;
            self.ack_end_deadline = None;
            self.rx = None;
            self.port1.deselect();
            self.port2.deselect();
            self.last_joyn = false;
            // Keep the slow-pad opt-in, drop its per-transaction state.
            self.slow_ready_cycle = None;
            self.slow_desynced = false;
            return;
        }
        if value & ctrl_bit::ACK != 0 {
            self.irq_latched = false;
            self.pending_irq = false;
        }
        // ACK bit is write-1-to-clear of STAT.IRQ9; don't keep it in
        // the stored register value.
        let new_ctrl = value & !ctrl_bit::ACK;
        // Edge-detect JOYN: the high-to-low transition starts a new
        // transfer; the low-to-high transition deselects and the
        // device state machine resets.
        let old_joyn = self.last_joyn;
        let new_joyn = new_ctrl & ctrl_bit::JOYN_OUTPUT != 0;
        if old_joyn && !new_joyn {
            self.byte_index = 0;
            self.ack_input = false;
            self.queued_tx = None;
            self.pending_ack = false;
            self.pending_dsr_timeout = false;
            self.transfer_busy = false;
            self.awaiting_ack = false;
            self.transfer_deadline = None;
            self.ack_deadline = None;
            self.ack_end_deadline = None;
            self.port1.deselect();
            self.port2.deselect();
            // A fresh select starts a clean packet for the slow-pad model.
            self.slow_ready_cycle = None;
            self.slow_desynced = false;
        }
        self.last_joyn = new_joyn;
        self.ctrl = new_ctrl;
    }

    /// 32-bit write dispatch. STAT is 32-bit on hardware but the
    /// writable registers all live in the 16-bit half, so we delegate.
    pub fn write32(&mut self, phys: u32, value: u32) -> bool {
        self.write16_at(phys, value as u16, 0)
    }

    /// 16-bit write dispatch. Returns `true` when the address fell in
    /// a recognised slot (even if the write was semantically ignored).
    pub fn write16(&mut self, phys: u32, value: u16) -> bool {
        self.write16_at(phys, value, 0)
    }

    /// Cycle-aware 32-bit write dispatch.
    pub fn write32_at(&mut self, phys: u32, value: u32, now: u64) -> bool {
        self.write16_at(phys, value as u16, now)
    }

    /// Cycle-aware 16-bit write dispatch.
    pub fn write16_at(&mut self, phys: u32, value: u16, now: u64) -> bool {
        match phys - Self::BASE {
            offset::DATA => self.write_data_at(value as u8, now),
            offset::STAT => {}
            offset::MODE => self.mode = value & MODE_WRITE_MASK,
            offset::CTRL => self.write_ctrl(value),
            offset::BAUD => self.baud = value,
            _ => return false,
        }
        true
    }

    /// 8-bit write dispatch. Only `DATA` accepts a byte write; every
    /// other register is 16-bit on the real chip.
    pub fn write8(&mut self, phys: u32, value: u8) -> bool {
        self.write8_at(phys, value, 0)
    }

    /// Cycle-aware 8-bit write dispatch.
    pub fn write8_at(&mut self, phys: u32, value: u8, now: u64) -> bool {
        match phys - Self::BASE {
            offset::DATA => self.write_data_at(value, now),
            _ => return false,
        }
        true
    }
}

impl Default for Sio0 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stat_reports_tx_ready_bits() {
        let mut sio = Sio0::new();
        let s = sio.read32(Sio0::BASE + 0x4).unwrap();
        assert_eq!(s & 0x1, 0x1, "TX_READY_1 must be set");
        assert_eq!(s & 0x4, 0x4, "TX_READY_2 must be set");
        assert_eq!(s & 0x2, 0, "RX_NOT_EMPTY must be clear");
    }

    #[test]
    fn data_read_returns_ff_no_device() {
        // Unseat the default pad so we can verify the no-device path
        // stays correct (TX ignored, DATA reads 0xFF).
        let mut sio = Sio0::new();
        sio.attach_port1(crate::pad::PortDevice::empty());
        assert_eq!(sio.read8(Sio0::BASE).unwrap(), 0xFF);
        assert_eq!(sio.read16(Sio0::BASE).unwrap(), 0x00FF);
    }

    #[test]
    fn stat_hi_half_reads_zero() {
        let mut sio = Sio0::new();
        assert_eq!(sio.read16(Sio0::BASE + 0x6).unwrap(), 0);
    }

    #[test]
    fn tx_fills_rx_with_ff_and_sets_stat_bit() {
        // With /CS high, CTRL.2 force-arms exactly one received byte.
        let mut sio = Sio0::new();
        sio.attach_port1(crate::pad::PortDevice::empty());
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::FORCE_RX_ONCE);
        sio.write8(Sio0::BASE, 0x01);
        sio.tick(DEFAULT_TRANSFER_TICKS);
        let s = sio.read32(Sio0::BASE + 0x4).unwrap();
        assert_eq!(s & 0x2, 0x2, "RX_NOT_EMPTY must be set after TX");
        assert_eq!(sio.read8(Sio0::BASE).unwrap(), 0xFF);
        // Reading DATA consumes the RX slot.
        let s = sio.read32(Sio0::BASE + 0x4).unwrap();
        assert_eq!(s & 0x2, 0, "RX_NOT_EMPTY must be clear after DATA read");
    }

    #[test]
    fn force_rx_is_one_shot_and_selected_receive_does_not_need_it() {
        let mut sio = Sio0::new();
        sio.attach_port1(crate::pad::PortDevice::empty());
        let t = DEFAULT_TRANSFER_TICKS;

        sio.write16(Sio0::BASE + 0xA, ctrl_bit::FORCE_RX_ONCE);
        sio.write8_at(Sio0::BASE, 0x00, 0);
        sio.tick(t);
        assert_eq!(sio.read8(Sio0::BASE).unwrap(), 0xFF);
        assert_eq!(
            sio.debug_ctrl() & ctrl_bit::FORCE_RX_ONCE,
            0,
            "hardware clears CTRL.2 after one received byte"
        );

        sio.write8_at(Sio0::BASE, 0x00, t + 1);
        sio.tick(2 * t + 1);
        assert_eq!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::RX_NOT_EMPTY,
            0,
            "a second byte is not received while /CS stays high"
        );

        sio.write16(Sio0::BASE + 0xA, ctrl_bit::JOYN_OUTPUT);
        sio.write8_at(Sio0::BASE, 0x00, 2 * t + 2);
        sio.tick(3 * t + 2);
        assert_ne!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::RX_NOT_EMPTY,
            0,
            "selected-port receiving works with CTRL.2 clear"
        );
    }

    #[test]
    fn write_then_read_roundtrips_mode_baud() {
        let mut sio = Sio0::new();
        sio.write16(Sio0::BASE + 0x8, 0x1234);
        sio.write16(Sio0::BASE + 0xE, 0x5678);
        assert_eq!(sio.read16(Sio0::BASE + 0x8).unwrap(), 0x0034);
        assert_eq!(sio.read16(Sio0::BASE + 0xE).unwrap(), 0x5678);
    }

    #[test]
    fn ctrl_reset_bit_zeroes_everything() {
        let mut sio = Sio0::new();
        sio.write16(Sio0::BASE + 0x8, 0xFFFF);
        sio.write16(Sio0::BASE + 0xE, 0xFFFF);
        sio.ack_input = true;
        sio.irq_latched = true;
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::RESET);
        assert_eq!(sio.mode, 0);
        assert_eq!(sio.baud, 0);
        assert_eq!(sio.ctrl, 0);
        assert!(!sio.ack_input);
        assert!(!sio.irq_latched);
    }

    #[test]
    fn ctrl_ack_bit_is_stripped() {
        let mut sio = Sio0::new();
        // Every bit except RESET (0x40); ACK must not be kept.
        sio.write16(Sio0::BASE + 0xA, 0x001F);
        assert_eq!(sio.ctrl & ctrl_bit::ACK, 0);
        // Other low bits pass through (including SIO0's one-shot CTRL.2).
        assert_eq!(sio.ctrl & 0x000F, 0x000F);
    }

    #[test]
    fn contains_matches_full_window() {
        for off in 0..Sio0::SIZE {
            assert!(Sio0::contains(Sio0::BASE + off));
        }
        assert!(!Sio0::contains(Sio0::BASE - 1));
        assert!(!Sio0::contains(Sio0::BASE + Sio0::SIZE));
    }

    #[test]
    fn digital_pad_full_poll_via_mmio() {
        use crate::pad::{button, ButtonState, DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        // Hold Cross + Start.
        sio.set_port1_buttons(ButtonState::from_bits(button::START | button::CROSS));

        // Simulate the BIOS pad-poll sequence on port 1 (SLOT bit clear).
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::JOYN_OUTPUT); // assert JOYN

        // TX 0x01 → RX 0xFF (dummy select-byte response)
        sio.write8(Sio0::BASE, 0x01);
        sio.tick(DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS);
        assert_eq!(sio.pop_rx(), 0xFF);

        // TX 0x42 → RX 0x41
        sio.write8(Sio0::BASE, 0x42);
        sio.tick(2 * (DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS));
        assert_eq!(sio.pop_rx(), 0x41);

        // TX 0x00 → RX 0x5A
        sio.write8(Sio0::BASE, 0x00);
        sio.tick(3 * (DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS));
        assert_eq!(sio.pop_rx(), 0x5A);

        // TX 0x00 → RX buttons1 (START = bit 3 pressed → wire 0xF7)
        sio.write8(Sio0::BASE, 0x00);
        sio.tick(4 * (DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS));
        assert_eq!(sio.pop_rx(), 0xF7);

        // TX 0x00 → RX buttons2 (CROSS = bit 14 pressed → wire 0xBF)
        sio.write8(Sio0::BASE, 0x00);
        sio.tick(4 * (DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS) + DEFAULT_TRANSFER_TICKS);
        assert_eq!(sio.pop_rx(), 0xBF);
    }

    #[test]
    fn slot_bit_switches_ports() {
        use crate::pad::{DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        // Port 2 stays empty.

        // CTRL.SLOT = 1 → port 2 (empty).
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::SLOT | ctrl_bit::JOYN_OUTPUT);
        sio.write8(Sio0::BASE, 0x01);
        sio.tick(DEFAULT_TRANSFER_TICKS);
        assert_eq!(sio.pop_rx(), 0xFF, "empty port 2 returns 0xFF");

        // CTRL.SLOT = 0 → port 1 (has pad).
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::JOYN_OUTPUT);
        sio.write8(Sio0::BASE, 0x01);
        sio.tick(DEFAULT_TRANSFER_TICKS);
        assert_eq!(
            sio.pop_rx(),
            0xFF,
            "port 1 select byte gets the dummy response"
        );
        sio.write8(Sio0::BASE, 0x42);
        sio.tick(DEFAULT_TRANSFER_TICKS * 2);
        assert_eq!(
            sio.pop_rx(),
            0x41,
            "port 1 pad responds on the command byte"
        );
    }

    #[test]
    fn controller_ack_sets_stat_bits_and_irq_when_enabled() {
        use crate::pad::{DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.write16(
            Sio0::BASE + 0xA,
            ctrl_bit::JOYN_OUTPUT | ctrl_bit::ACK_IRQ_ENABLE,
        );

        sio.write8(Sio0::BASE, 0x01);

        let stat = sio.read32(Sio0::BASE + 0x4).unwrap();
        assert_eq!(
            stat & stat_bit::RX_NOT_EMPTY,
            0,
            "the response byte is still on the wire"
        );
        assert_eq!(
            stat & stat_bit::ACK_INPUT,
            0,
            "ACK must wait for the delayed SIO event"
        );
        assert_eq!(stat & stat_bit::IRQ, 0, "IRQ bit must stay low until ACK");
        assert!(
            !sio.take_pending_irq(),
            "bus IRQ should not fire before ACK"
        );

        sio.tick(DEFAULT_TRANSFER_TICKS - 1);
        assert_eq!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::RX_NOT_EMPTY,
            0,
            "ten bit times have not passed"
        );

        sio.tick(DEFAULT_TRANSFER_TICKS);
        let stat = sio.read32(Sio0::BASE + 0x4).unwrap();
        assert_ne!(stat & stat_bit::RX_NOT_EMPTY, 0, "the byte has arrived");
        assert_eq!(stat & stat_bit::IRQ, 0, "ACK follows the byte");

        sio.tick(DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAYS[0] - 1);
        assert_eq!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::IRQ,
            0,
            "IRQ must not fire before the pad's ACK delay"
        );

        sio.tick(DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAYS[0]);
        let stat = sio.read32(Sio0::BASE + 0x4).unwrap();
        assert_ne!(stat & stat_bit::ACK_INPUT, 0, "ACK input should be visible");
        assert_ne!(stat & stat_bit::IRQ, 0, "STAT IRQ bit should latch");
        assert!(sio.take_pending_irq(), "bus should see one IRQ edge");
        assert!(!sio.take_pending_irq(), "IRQ edge should be single-shot");

        sio.write16(Sio0::BASE + 0xA, ctrl_bit::JOYN_OUTPUT | ctrl_bit::ACK);
        let stat = sio.read32(Sio0::BASE + 0x4).unwrap();
        assert_eq!(stat & stat_bit::IRQ, 0, "CTRL.ACK should clear STAT IRQ");
    }

    #[test]
    fn controller_ack_without_irq_enable_is_visible_but_does_not_raise_irq() {
        use crate::pad::{DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::JOYN_OUTPUT);

        sio.write8(Sio0::BASE, 0x01);
        sio.tick(DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS);

        let stat = sio.read32(Sio0::BASE + 0x4).unwrap();
        assert_ne!(
            stat & stat_bit::ACK_INPUT,
            0,
            "ACK input level is independent from ACK IRQ enable"
        );
        assert_eq!(
            stat & stat_bit::IRQ,
            0,
            "IRQ bit should stay low without enable"
        );
        assert!(
            !sio.take_pending_irq(),
            "bus IRQ must stay quiet without enable"
        );
    }

    #[test]
    fn missing_device_returns_ff_without_dsr_timeout_irq() {
        use crate::pad::PortDevice;

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty());
        sio.write16(
            Sio0::BASE + 0xA,
            ctrl_bit::JOYN_OUTPUT | ctrl_bit::ACK_IRQ_ENABLE,
        );
        sio.write16(Sio0::BASE + 0xE, 0x0088);

        sio.write8_at(Sio0::BASE, 0x01, 10);
        assert_eq!(sio.pop_rx(), 0xFF);
        assert_eq!(sio.debug_ack_deadline(), None);
        assert_eq!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::ACK_INPUT,
            0,
            "empty slots must not expose a visible ACK pulse"
        );

        sio.tick(10 + 0x88 * 8);
        assert_eq!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::IRQ,
            0,
            "no ACK/DSR IRQ is scheduled for a missing device"
        );
        assert!(!sio.take_pending_irq());
    }

    #[test]
    fn controller_final_no_ack_byte_does_not_arm_timeout_irq() {
        use crate::pad::{DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.write16(
            Sio0::BASE + 0xA,
            ctrl_bit::JOYN_OUTPUT | ctrl_bit::ACK_IRQ_ENABLE,
        );
        sio.write16(Sio0::BASE + 0xE, 0x0088);

        let mut now = 10u64;
        for tx in [0x01, 0x42, 0x00, 0x00] {
            sio.write8_at(Sio0::BASE, tx, now);
            sio.tick(now + 0x88 * BYTE_BIT_TIMES + PAD_ACK_DELAY_TICKS);
            assert!(sio.take_pending_irq(), "byte 0x{tx:02x} should ACK");
            sio.write16(
                Sio0::BASE + 0xA,
                ctrl_bit::JOYN_OUTPUT | ctrl_bit::ACK_IRQ_ENABLE | ctrl_bit::ACK,
            );
            let _ = sio.read8(Sio0::BASE);
            now += 0x88 * BYTE_BIT_TIMES + PAD_ACK_DELAY_TICKS + 1;
        }

        sio.write8_at(Sio0::BASE, 0x00, now);
        assert_eq!(
            sio.debug_ack_deadline(),
            None,
            "the final byte of a real controller poll drops ACK without arming a DSR timeout"
        );
        sio.tick(now + 0x88 * BYTE_BIT_TIMES + PAD_ACK_DELAY_TICKS);
        assert!(!sio.take_pending_irq());
        assert_eq!(sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::IRQ, 0);
    }

    #[test]
    fn successive_controller_acks_wait_for_ctrl_ack_before_reinterrupt() {
        use crate::pad::{DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.write16(
            Sio0::BASE + 0xA,
            ctrl_bit::JOYN_OUTPUT | ctrl_bit::ACK_IRQ_ENABLE,
        );

        sio.write8_at(Sio0::BASE, 0x01, 10);
        sio.tick(10 + DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS);
        assert!(sio.take_pending_irq(), "first ACK should raise an IRQ edge");

        let second_start = 10 + DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS + 1;
        sio.write8_at(Sio0::BASE, 0x42, second_start);
        sio.tick(second_start + DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS);
        assert!(
            !sio.take_pending_irq(),
            "no further SIO IRQ is scheduled while STAT.IRQ is still latched"
        );

        sio.write16(
            Sio0::BASE + 0xA,
            ctrl_bit::JOYN_OUTPUT | ctrl_bit::ACK_IRQ_ENABLE | ctrl_bit::ACK,
        );
        sio.write8_at(Sio0::BASE, 0x00, second_start + DEFAULT_TRANSFER_TICKS + 1);
        sio.tick(second_start + 2 * DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS + 1);
        assert!(
            sio.take_pending_irq(),
            "after CTRL.ACK clears STAT.IRQ, a later ACK can raise the next edge"
        );
    }

    #[test]
    fn sdk_style_pad_poll_sequence_completes() {
        use crate::pad::{button, ButtonState, DigitalPad, PortDevice};

        fn wait_for_stat(sio: &mut Sio0, now: &mut u64, mask: u32) {
            const TIMEOUT_TICKS: u64 = 10_000;
            let start = *now;
            while sio.read32(Sio0::BASE + 0x4).unwrap() & mask == 0 {
                *now = now.saturating_add(1);
                sio.tick(*now);
                assert!(
                    *now - start < TIMEOUT_TICKS,
                    "timed out waiting for STAT mask {mask:#x}"
                );
            }
        }

        fn exchange_sdk_style(sio: &mut Sio0, now: &mut u64, tx: u8) -> u8 {
            wait_for_stat(sio, now, stat_bit::TX_READY_1);
            sio.write8(Sio0::BASE, tx);
            wait_for_stat(sio, now, stat_bit::RX_NOT_EMPTY);
            sio.read8(Sio0::BASE).unwrap()
        }

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.set_port1_buttons(ButtonState::from_bits(button::START | button::CROSS));

        let mut now = 0u64;
        // Match psx-pad's init sequence: ACK any stale IRQ, then
        // assert JOYN with TX enabled; selected-port RX is implicit.
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::ACK);
        sio.write16(
            Sio0::BASE + 0xA,
            (1 << 0) | ctrl_bit::JOYN_OUTPUT | (1 << 2),
        );

        assert_eq!(exchange_sdk_style(&mut sio, &mut now, 0x01), 0xFF);
        assert_eq!(exchange_sdk_style(&mut sio, &mut now, 0x42), 0x41);
        assert_eq!(exchange_sdk_style(&mut sio, &mut now, 0x00), 0x5A);
        assert_eq!(exchange_sdk_style(&mut sio, &mut now, 0x00), 0xF7);
        assert_eq!(exchange_sdk_style(&mut sio, &mut now, 0x00), 0xBF);
    }

    #[test]
    fn a_byte_holds_the_shifter_for_ten_bit_times_and_acks_later() {
        use crate::pad::{DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.write16(
            Sio0::BASE + 0xA,
            ctrl_bit::JOYN_OUTPUT | ctrl_bit::ACK_IRQ_ENABLE,
        );
        sio.write16(Sio0::BASE + 0xE, 0x0088);

        sio.write8_at(Sio0::BASE, 0x01, 10);
        let stat = sio.read32(Sio0::BASE + 0x4).unwrap();
        assert_eq!(stat & stat_bit::TX_READY_2, 0, "the byte is on the wire");
        assert_ne!(stat & stat_bit::TX_READY_1, 0, "the TX register is free");
        assert_eq!(stat & stat_bit::RX_NOT_EMPTY, 0, "nothing received yet");

        let end = 10 + 0x88 * BYTE_BIT_TIMES;
        sio.tick(end - 1);
        assert_eq!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::RX_NOT_EMPTY,
            0
        );
        sio.tick(end);
        let stat = sio.read32(Sio0::BASE + 0x4).unwrap();
        assert_ne!(stat & stat_bit::TX_READY_2, 0, "the shifter is idle again");
        assert_ne!(stat & stat_bit::RX_NOT_EMPTY, 0, "the answer arrived");
        assert_eq!(stat & stat_bit::IRQ, 0, "ACK has not come yet");

        sio.tick(end + PAD_ACK_DELAY_TICKS);
        let stat = sio.read32(Sio0::BASE + 0x4).unwrap();
        assert_ne!(stat & stat_bit::IRQ, 0, "IRQ latches with /ACK");
    }

    #[test]
    fn a_byte_written_while_one_is_on_the_wire_waits_in_the_tx_register() {
        use crate::pad::{DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::JOYN_OUTPUT);
        let t = DEFAULT_TRANSFER_TICKS;

        sio.write8_at(Sio0::BASE, 0x01, 10);
        assert_ne!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::TX_READY_1,
            0,
            "TX_READY_1 stays set after the first write"
        );
        sio.write8_at(Sio0::BASE, 0x42, 11);
        assert_eq!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::TX_READY_1,
            0,
            "the second write fills the TX register"
        );

        sio.tick(10 + t);
        assert_eq!(sio.pop_rx(), 0xFF, "the select byte's answer");
        assert_ne!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::TX_READY_1,
            0,
            "the queued byte moved to the shifter"
        );
        sio.tick(10 + 2 * t);
        assert_eq!(sio.pop_rx(), 0x41, "the queued byte went out right behind");
    }

    #[test]
    fn a_byte_schedules_its_arrival_then_its_ack() {
        use crate::pad::{DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.write16(
            Sio0::BASE + 0xA,
            ctrl_bit::JOYN_OUTPUT | ctrl_bit::ACK_IRQ_ENABLE,
        );

        sio.write8_at(Sio0::BASE, 0x01, 10);
        assert_eq!(sio.pop_rx(), 0xFF, "nothing has arrived: DATA reads idle");
        assert_eq!(
            sio.debug_transfer_deadline(),
            Some(10 + DEFAULT_TRANSFER_TICKS),
            "the byte arrives after ten bit times"
        );
        sio.tick(10 + DEFAULT_TRANSFER_TICKS);
        assert_eq!(
            sio.debug_ack_deadline(),
            Some(10 + DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAYS[0]),
            "and /ACK rises the pad's delay behind it"
        );
        assert_eq!(sio.pop_rx(), 0xFF, "select byte answer");

        sio.write8_at(Sio0::BASE, 0x42, 10 + DEFAULT_TRANSFER_TICKS + 1);
        sio.tick(10 + 2 * DEFAULT_TRANSFER_TICKS + 1);
        assert_eq!(sio.pop_rx(), 0x41, "the command byte's answer");
    }

    #[test]
    fn slow_pad_desyncs_without_ack_wait() {
        use crate::pad::{button, ButtonState, DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.set_port1_buttons(ButtonState::from_bits(button::START));
        sio.set_slow_pad(true);
        sio.write16(Sio0::BASE + 0xE, 0x0088); // baud -> transfer_ticks = 1360
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::JOYN_OUTPUT); // select port 1

        let mut now = 100u64;
        // The select byte is always fine (no prior /ACK deadline to beat).
        sio.write8_at(Sio0::BASE, 0x01, now);
        assert_eq!(sio.pop_rx(), 0xFF);

        // The command byte is clocked only a few cycles later -- long before the
        // slow pad's /ACK deadline (now + 1360) -- so the device misses it.
        now += 8;
        sio.write8_at(Sio0::BASE, 0x42, now);
        assert_eq!(
            sio.pop_rx(),
            0xFF,
            "a slow pad must not return its 0x41 id when the host skips the /ACK wait"
        );

        // Every subsequent byte of the packet stays desynced.
        now += 8;
        sio.write8_at(Sio0::BASE, 0x00, now);
        assert_eq!(
            sio.pop_rx(),
            0xFF,
            "packet remains desynced after a missed byte"
        );
    }

    #[test]
    fn slow_pad_reads_clean_with_ack_wait() {
        use crate::pad::{button, ButtonState, DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.set_port1_buttons(ButtonState::from_bits(button::START | button::CROSS));
        sio.set_slow_pad(true);
        sio.write16(Sio0::BASE + 0xE, 0x0088);
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::JOYN_OUTPUT);

        // Mirror the ACK-paced driver: wait past each byte's /ACK deadline
        // (ten bit times plus the pad's delay) before clocking the next byte.
        let gap = DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS + 16;
        let mut now = 100u64;
        let ex = |sio: &mut Sio0, now: &mut u64, tx: u8| -> u8 {
            sio.write8_at(Sio0::BASE, tx, *now);
            sio.tick(*now + DEFAULT_TRANSFER_TICKS);
            let rx = sio.read8(Sio0::BASE).unwrap();
            *now += gap; // host waited for /ACK before clocking the next byte
            rx
        };

        assert_eq!(ex(&mut sio, &mut now, 0x01), 0xFF);
        assert_eq!(ex(&mut sio, &mut now, 0x42), 0x41, "id after /ACK wait");
        assert_eq!(ex(&mut sio, &mut now, 0x00), 0x5A, "magic after /ACK wait");
        assert_eq!(ex(&mut sio, &mut now, 0x00), 0xF7, "buttons1 (START)");
        assert_eq!(ex(&mut sio, &mut now, 0x00), 0xBF, "buttons2 (CROSS)");
    }

    #[test]
    fn ack_input_is_a_pulse_not_a_sticky_level() {
        use crate::pad::{DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.write16(
            Sio0::BASE + 0xA,
            ctrl_bit::JOYN_OUTPUT | ctrl_bit::ACK_IRQ_ENABLE,
        );

        sio.write8_at(Sio0::BASE, 0x01, 100);
        sio.tick(100 + DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS);
        assert_ne!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::ACK_INPUT,
            0,
            "ACK pulse should become visible"
        );

        sio.tick(100 + DEFAULT_TRANSFER_TICKS + PAD_ACK_DELAY_TICKS + PAD_ACK_PULSE_TICKS);
        assert_eq!(
            sio.read32(Sio0::BASE + 0x4).unwrap() & stat_bit::ACK_INPUT,
            0,
            "ACK pulse should self-clear"
        );
    }

    /// A device answers each byte of a transaction in its own time, so the
    /// `/ACK` delay follows the byte's place in it, from the select byte on,
    /// and starts over when the port is deselected.
    #[test]
    fn the_ack_delay_follows_the_bytes_place_in_the_transaction() {
        use crate::pad::{DigitalPad, PortDevice};

        let mut sio = Sio0::new();
        sio.attach_port1(PortDevice::empty().with_pad(DigitalPad::new()));
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::JOYN_OUTPUT);
        let t = DEFAULT_TRANSFER_TICKS;
        let mut now = 100u64;
        for (index, tx) in [0x01u8, 0x42, 0x00, 0x00].into_iter().enumerate() {
            sio.write8_at(Sio0::BASE, tx, now);
            sio.tick(now + t);
            assert_eq!(
                sio.debug_ack_deadline(),
                Some(now + t + PAD_ACK_DELAYS[index]),
                "byte {index}"
            );
            now += t + PAD_ACK_DELAYS[index] + 1;
            sio.tick(now);
        }
        // Deselecting starts the next transaction at its first byte again.
        sio.write16(Sio0::BASE + 0xA, 0);
        sio.write16(Sio0::BASE + 0xA, ctrl_bit::JOYN_OUTPUT);
        sio.write8_at(Sio0::BASE, 0x01, now);
        sio.tick(now + t);
        assert_eq!(
            sio.debug_ack_deadline(),
            Some(now + t + PAD_ACK_DELAYS[0]),
            "the first byte of the next poll"
        );
    }

    /// Past the four header bytes a card answers in the fitted data delay.
    #[test]
    fn a_card_answers_its_data_bytes_in_the_fitted_delay() {
        use crate::pad::{MemoryCard, PortDevice};

        for slot in 0..2usize {
            let mut sio = Sio0::new();
            let card = PortDevice::empty().with_memcard(MemoryCard::new());
            let ctrl = if slot == 0 {
                ctrl_bit::JOYN_OUTPUT
            } else {
                ctrl_bit::JOYN_OUTPUT | ctrl_bit::SLOT
            };
            if slot == 0 {
                sio.attach_port1(card);
            } else {
                sio.attach_port2(card);
            }
            sio.write16(Sio0::BASE + 0xA, ctrl);
            let t = DEFAULT_TRANSFER_TICKS;
            let mut now = 100u64;
            // Select, read command, two ID bytes, then address and data.
            for (index, tx) in [0x81u8, 0x52, 0, 0, 0, 0, 0, 0].into_iter().enumerate() {
                sio.write8_at(Sio0::BASE, tx, now);
                sio.tick(now + t);
                let want = match MEMCARD_ACK_DELAYS[slot].get(index) {
                    Some(&delay) => delay,
                    None => MEMCARD_DATA_ACK_DELAY_TICKS[slot],
                };
                assert_eq!(
                    sio.debug_ack_deadline(),
                    Some(now + t + want),
                    "slot {slot} byte {index}"
                );
                now += t + want + 1;
                sio.tick(now);
            }
        }
    }
}
