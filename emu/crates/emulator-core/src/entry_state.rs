//! Opt-in entry states for disc boot.
//!
//! PSoXide has no BIOS, so every disc boots on the HLE kernel, which hands the
//! program the state a retail Sony BIOS leaves behind (see [`crate::fastboot`]).
//! That makes a program that quietly depends on that state invisible to every
//! gate: the exception vector words, the DMA priorities, the SPU volumes, the
//! CD drive's mode, zeroed RAM. This module describes other states the same
//! HLE boot can hand over, so those programs fail here the way they fail on a
//! console with another BIOS:
//!
//! * `openbios` is modelled on what PCSX-Redux's OpenBIOS leaves, from the
//!   register traffic of its ROM (campaign lane hwtest-1103). Values that
//!   traffic does not settle are marked "assumed" below.
//! * `hostile:SEED` randomises, from a fixed seed, every item below that real
//!   BIOSes are allowed to differ on. Sony revisions 1.0 to 4.5 in every
//!   region differ from each other in ways nobody has documented in full, so
//!   the range, not a list of versions, is the claim: a program that works
//!   across many seeds does not depend on any of these.
//!
//! The default boot is untouched: nothing here runs unless
//! `PSOXIDE_ENTRY_STATE` names a mode (or a caller passes one to
//! [`crate::fast_boot_disc_with`]).
//!
//! No BIOS bytes are involved. Every value is a number or a seed; the only
//! code written to RAM is the exception handler PSoXide already assembles,
//! at another address.
//!
//! Spec syntax: `NAME[:SEED][,key=value]...`
//!
//! | Name | Meaning |
//! |---|---|
//! | `default` | the retail-modelled HLE entry state (also: unset or empty) |
//! | `openbios` | the OpenBIOS-modelled state |
//! | `hostile[:SEED]` | randomised; SEED is decimal or `0x` hex, default 1 |
//!
//! | Key | Meaning |
//! |---|---|
//! | `dpcr=`, `dicr=`, `imask=` | override one register after the mode is built |
//! | `skip=CATEGORY` | leave a category at the default state (repeatable): `vector`, `dma`, `irq`, `spu`, `cd`, `ram`, `vram`, `scratch`, `gpu`, `timers`, `cpu` |

use crate::hle_exceptions::VectorForm;

/// Environment variable [`crate::fast_boot_disc`] reads.
pub const ENV_VAR: &str = "PSOXIDE_ENTRY_STATE";

/// Where the HLE exception handler sits in OpenBIOS mode. OpenBIOS keeps its
/// handler at 0x27D0, which is inside the HLE's own kernel routines
/// (ReturnFromException, DeliverEvent, ...), so the handler goes to the
/// first free block after the B(5Bh) patch zone instead.
pub const OPENBIOS_HANDLER_BASE: u32 = 0x0000_5e00;

/// Free blocks of at least [`crate::hle_exceptions::handler_len_bytes`]
/// bytes a hostile boot may put the handler in (see the layout table in
/// [`crate::hle_kernel`]): after the patch zone and before the device
/// control blocks, and between those and the file control blocks.
const HANDLER_SPOTS: [u32; 7] = [
    crate::hle_kernel::EXCEPTION_HANDLER,
    0x0000_5e00,
    0x0000_6000,
    0x0000_6400,
    0x0000_6800,
    0x0000_7400,
    0x0000_7800,
];

/// An entry state, before it is resolved against a disc.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum EntryState {
    /// The retail-modelled state; [`crate::fast_boot_disc`] does what it did
    /// before this module existed.
    Default,
    /// The OpenBIOS-modelled state.
    OpenBios(Options),
    /// Randomised from a seed.
    Hostile {
        /// PRNG seed; the same seed gives the same state.
        seed: u64,
        /// Per-run options.
        options: Options,
    },
}

/// Register overrides and category skips of a spec.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Options {
    /// Replaces DPCR.
    pub dpcr: Option<u32>,
    /// Replaces DICR.
    pub dicr: Option<u32>,
    /// Replaces I_MASK.
    pub imask: Option<u32>,
    /// Categories left at the default state.
    pub skip: Vec<String>,
}

const CATEGORIES: [&str; 11] = [
    "vector", "dma", "irq", "spu", "cd", "ram", "vram", "scratch", "gpu", "timers", "cpu",
];

impl EntryState {
    /// Parse a spec (see the module docs). An empty spec is
    /// [`EntryState::Default`].
    ///
    /// # Errors
    ///
    /// A message naming the part of the spec that does not parse.
    pub fn from_spec(spec: &str) -> Result<Self, String> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Ok(Self::Default);
        }
        let mut parts = spec.split(',').map(str::trim);
        let head = parts.next().unwrap_or("");
        let (name, seed) = match head.split_once(':') {
            Some((name, seed)) => (name, Some(seed)),
            None => (head, None),
        };
        let mut options = Options::default();
        for part in parts {
            let (key, value) = part
                .split_once('=')
                .ok_or_else(|| format!("entry state: `{part}` is not key=value"))?;
            match key {
                "dpcr" => options.dpcr = Some(parse_number(key, value)? as u32),
                "dicr" => options.dicr = Some(parse_number(key, value)? as u32),
                "imask" => options.imask = Some(parse_number(key, value)? as u32),
                "skip" => {
                    if !CATEGORIES.contains(&value) {
                        return Err(format!(
                            "entry state: unknown category `{value}` (one of {})",
                            CATEGORIES.join(", ")
                        ));
                    }
                    options.skip.push(value.to_string());
                }
                _ => return Err(format!("entry state: unknown key `{key}`")),
            }
        }
        match (name, seed) {
            ("default", None) if options == Options::default() => Ok(Self::Default),
            ("default", _) => Err("entry state: `default` takes no seed or options".to_string()),
            ("openbios", None) => Ok(Self::OpenBios(options)),
            ("openbios", Some(_)) => Err("entry state: `openbios` takes no seed".to_string()),
            ("hostile", seed) => {
                let seed = match seed {
                    Some(seed) => parse_number("seed", seed)?,
                    None => 1,
                };
                Ok(Self::Hostile { seed, options })
            }
            (other, _) => Err(format!(
                "entry state: unknown mode `{other}` (default, openbios, hostile[:SEED])"
            )),
        }
    }

    /// The state `PSOXIDE_ENTRY_STATE` names. A spec that does not parse
    /// stops the run: silently booting the default would defeat the point.
    ///
    /// # Panics
    ///
    /// When the variable is set to a spec that does not parse.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(ENV_VAR) {
            Ok(spec) => Self::from_spec(&spec).unwrap_or_else(|e| panic!("{ENV_VAR}: {e}")),
            Err(_) => Self::Default,
        }
    }

    /// The spec that parses back to this state.
    #[must_use]
    pub fn spec(&self) -> String {
        let (head, options) = match self {
            Self::Default => return "default".to_string(),
            Self::OpenBios(o) => ("openbios".to_string(), o),
            Self::Hostile { seed, options } => (format!("hostile:{seed}"), options),
        };
        let mut out = head;
        for (key, value) in [
            ("dpcr", options.dpcr),
            ("dicr", options.dicr),
            ("imask", options.imask),
        ] {
            if let Some(value) = value {
                out.push_str(&format!(",{key}={value:#x}"));
            }
        }
        for category in &options.skip {
            out.push_str(&format!(",skip={category}"));
        }
        out
    }

    /// Resolve against a disc: `None` for [`EntryState::Default`].
    pub(crate) fn plan(&self, disc_sectors: u32) -> Option<Plan> {
        match self {
            Self::Default => None,
            Self::OpenBios(options) => Some(Plan::openbios(options)),
            Self::Hostile { seed, options } => Some(Plan::hostile(*seed, options, disc_sectors)),
        }
    }
}

fn parse_number(what: &str, text: &str) -> Result<u64, String> {
    let text = text.trim();
    let parsed = match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => text.parse(),
    };
    parsed.map_err(|_| format!("entry state: `{what}` needs a number, got `{text}`"))
}

/// SPU registers an entry state sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SpuEntry {
    pub main_l: u16,
    pub main_r: u16,
    pub reverb_l: u16,
    pub reverb_r: u16,
    pub reverb_base: u16,
    pub eon: u32,
    pub ext_l: u16,
    pub ext_r: u16,
    pub cd_l: u16,
    pub cd_r: u16,
    pub transfer_addr: u16,
    pub transfer_ctrl: u16,
    pub spucnt: u16,
    pub reverb_cfg: [u16; 32],
}

/// CD-ROM controller state an entry state sets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CdEntry {
    /// Setmode byte.
    pub mode: u8,
    /// INT enable register.
    pub irq_mask: u8,
    /// Sector the head rests on, when not the one after the executable.
    pub head: Option<u32>,
}

/// GPU state an entry state sets, as the commands that set it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct GpuEntry {
    /// GP1(08h) display mode.
    pub display_mode: u32,
    /// GP1(03h) display enable (0 on, 1 off).
    pub display_off: bool,
    /// GP1(05h) display area start, x | y << 10.
    pub display_start: u32,
    /// GP0 E1h..E5h words.
    pub draw_state: [u32; 5],
}

/// One root counter's registers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TimerEntry {
    pub counter: u16,
    pub mode: u16,
    pub target: u16,
}

/// General registers a BIOS leaves with whatever its last code put there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CpuEntry {
    pub regs: Vec<(u8, u32)>,
    pub hi: u32,
    pub lo: u32,
}

/// How a block of memory is filled.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Fill {
    /// Left as the default boot leaves it.
    Default,
    /// One byte everywhere (the `PSOXIDE_POISON_MEM` pattern).
    Byte(u8),
    /// Pseudo-random bytes from a seed.
    Random(u64),
}

/// An entry state resolved to values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub label: String,
    /// Handler address and the form of the vector that reaches it.
    pub vector: Option<(u32, VectorForm)>,
    /// DPCR and DICR.
    pub dma: Option<(u32, u32)>,
    pub i_mask: Option<u32>,
    pub spu: Option<SpuEntry>,
    pub cd: Option<CdEntry>,
    pub gpu: Option<GpuEntry>,
    pub timers: Option<[TimerEntry; 3]>,
    pub cpu: Option<CpuEntry>,
    pub ram: Fill,
    pub vram: Fill,
    pub scratch: Fill,
}

/// splitmix64: small, seedable, and the same on every host.
pub(crate) struct Rng(u64);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }

    fn u16(&mut self) -> u16 {
        (self.next_u64() >> 48) as u16
    }

    fn below(&mut self, n: u32) -> u32 {
        ((u64::from(self.u32()) * u64::from(n)) >> 32) as u32
    }

    fn chance(&mut self) -> bool {
        self.next_u64() >> 63 == 1
    }
}

impl Plan {
    /// OpenBIOS, from the ROM's register traffic. Each item says where it
    /// comes from.
    fn openbios(options: &Options) -> Self {
        let spu = SpuEntry {
            // Main volume 3800h/3800h, SPUCNT C000h (enabled, unmuted, no CD
            // audio), CD and external input volume 0, transfer control 4
            // (the SPU init routine's stores). Everything else is the
            // power-on value: the init does not program a reverb preset.
            main_l: 0x3800,
            main_r: 0x3800,
            reverb_l: 0,
            reverb_r: 0,
            reverb_base: 0,
            eon: 0,
            ext_l: 0,
            ext_r: 0,
            cd_l: 0,
            cd_r: 0,
            transfer_addr: 0,
            transfer_ctrl: 4,
            spucnt: 0xc000,
            reverb_cfg: [0; 32],
        };
        let mut plan = Plan {
            label: "openbios".to_string(),
            // Assumed: OpenBIOS's handler lives at 0x27D0 (the vector's
            // `lui/addiu/jr`), which the HLE cannot give it; see
            // OPENBIOS_HANDLER_BASE. The Sony address holds a tight loop.
            vector: Some((OPENBIOS_HANDLER_BASE, VectorForm::LuiAddiuJr)),
            // DPCR: power-on 07654321h with the ROM's read-modify-writes
            // applied: CD init ORs 8000h (channel 3 enable), SPU init ORs
            // B0000h (channel 4 enable at priority 3). Assumed: the GPU
            // helpers' OR of 800h runs only when a program draws, and the
            // `CdInit` routine that stores 9099h outright (the retail
            // value) is not part of the boot. DICR: the CD init's
            // `(DICR & FFFFFFh) | 880000h`: master enable and channel 3
            // enable, no flags (assumed: none left by the executable load).
            dma: Some((0x076f_c321, 0x0088_0000)),
            // Assumed: unchanged from the retail value (CD and DMA).
            i_mask: Some(0x0000_000c),
            spu: Some(spu),
            // The ROM's CD init issues Pause, Init and Setmode with a zero
            // parameter and enables all five INT bits: single speed, 2048
            // byte sectors, nothing else. The motor is on (Init), nothing
            // pending (assumed: the boot acknowledges its last response).
            cd: Some(CdEntry {
                mode: 0x00,
                irq_mask: 0x1f,
                head: None,
            }),
            gpu: None,
            timers: None,
            cpu: None,
            // Assumed byte: RAM outside the program is not cleared. The
            // pattern is PSOXIDE_POISON_MEM's.
            ram: Fill::Byte(0xaa),
            vram: Fill::Default,
            scratch: Fill::Default,
        };
        plan.apply_options(options);
        plan
    }

    /// Every item that BIOSes may differ on, drawn from `seed`. The ranges
    /// stay inside what the hardware can hold; nothing here is a value no
    /// BIOS could plausibly leave.
    fn hostile(seed: u64, options: &Options, disc_sectors: u32) -> Self {
        let mut rng = Rng::new(seed);
        // Each category draws from its own stream, so skipping one leaves the
        // others on the same values.
        let mut stream = |tag: u64| Rng::new(rng.next_u64() ^ tag);

        let vector = {
            let mut r = stream(1);
            let base = HANDLER_SPOTS[r.below(HANDLER_SPOTS.len() as u32) as usize];
            let form = match r.below(3) {
                0 => VectorForm::LuiAddiuJr,
                1 => VectorForm::LuiOriJr,
                _ => VectorForm::Jump,
            };
            (base, form)
        };

        let dma = {
            let mut r = stream(2);
            let mut dpcr = 0u32;
            for channel in 0..7 {
                let nibble = r.below(8) | if r.chance() { 8 } else { 0 };
                dpcr |= nibble << (4 * channel);
            }
            let enables = (r.u32() & 0x7f) << 16;
            let master_enable = if r.chance() { 1 << 23 } else { 0 };
            let flags = (r.u32() & 0x7f) << 24;
            let mut dicr = enables | master_enable | flags;
            if master_enable != 0 && (enables >> 16) & (flags >> 24) != 0 {
                dicr |= 1 << 31;
            }
            (dpcr, dicr)
        };

        let i_mask = stream(3).u32() & 0x07ff;

        let spu = {
            let mut r = stream(4);
            let enable = if r.chance() { 0x8000 } else { 0 };
            let unmute = if r.chance() { 0x4000 } else { 0 };
            let mut cfg = [0u16; 32];
            for word in &mut cfg {
                *word = r.u16();
            }
            let volume = |r: &mut Rng| if r.chance() { 0 } else { r.u16() & 0x7fff };
            SpuEntry {
                main_l: r.u16() & 0x3fff,
                main_r: r.u16() & 0x3fff,
                reverb_l: volume(&mut r),
                reverb_r: volume(&mut r),
                reverb_base: r.u16(),
                eon: r.u32() & 0x00ff_ffff,
                ext_l: volume(&mut r),
                ext_r: volume(&mut r),
                cd_l: volume(&mut r),
                cd_r: volume(&mut r),
                transfer_addr: r.u16(),
                transfer_ctrl: if r.chance() { 4 } else { 0 },
                // Enable, unmute, CD audio, external audio, the two reverb
                // routings and the reverb master; the transfer mode stays
                // stopped.
                spucnt: enable | unmute | (r.u16() & 0x008f),
                reverb_cfg: cfg,
            }
        };

        let cd = {
            let mut r = stream(5);
            CdEntry {
                mode: r.u16() as u8,
                irq_mask: (r.u16() & 0x1f) as u8,
                head: Some(r.below(disc_sectors.max(1))),
            }
        };

        let gpu = {
            let mut r = stream(6);
            // Horizontal resolution (bits 0, 1 and 6), vertical (2), colour
            // depth (4) and interlace (5). The video standard (bit 3) is not
            // randomised: PAL changes the frame rate, which is a different
            // test (`video=` is not a key yet).
            let display_mode = (r.u32() & 0x57) | ((r.u32() & 1) << 5);
            let x = r.below(512) & !1;
            let y = r.below(256);
            let draw_state = [
                0xe100_0000 | (r.u32() & 0x7ff),
                0xe200_0000 | (r.u32() & 0x000f_ffff),
                0xe300_0000 | (r.below(1024) | (r.below(512) << 10)),
                0xe400_0000 | (r.below(1024) | (r.below(512) << 10)),
                0xe500_0000 | (r.below(2048) | (r.below(2048) << 11)),
            ];
            GpuEntry {
                display_mode,
                display_off: r.chance(),
                display_start: x | (y << 10),
                draw_state,
            }
        };

        let timers = {
            let mut r = stream(7);
            let mut one = || TimerEntry {
                counter: r.u16(),
                // Reset on target and the clock source. Sync modes stay off (a
                // counter gated or paused until a sync event is not a state a
                // BIOS leaves: CTR waits on one forever) and so do the
                // interrupt enables (a stray timer interrupt tests the kernel,
                // not the program).
                mode: r.u16() & 0x0308,
                target: r.u16(),
            };
            [one(), one(), one()]
        };

        let cpu = {
            let mut r = stream(8);
            // Not a0/a1 (argc and argv), gp, sp, fp or ra: those have
            // defined values at entry.
            let regs = (1u8..=27)
                .filter(|i| !matches!(i, 4 | 5))
                .map(|i| (i, r.u32()))
                .collect();
            CpuEntry {
                regs,
                hi: r.u32(),
                lo: r.u32(),
            }
        };

        let mut plan = Plan {
            label: format!("hostile:{seed}"),
            vector: Some(vector),
            dma: Some(dma),
            i_mask: Some(i_mask),
            spu: Some(spu),
            cd: Some(cd),
            gpu: Some(gpu),
            timers: Some(timers),
            cpu: Some(cpu),
            ram: Fill::Random(stream(9).next_u64()),
            vram: Fill::Random(stream(10).next_u64()),
            scratch: Fill::Random(stream(11).next_u64()),
        };
        plan.apply_options(options);
        plan
    }

    fn apply_options(&mut self, options: &Options) {
        for category in &options.skip {
            match category.as_str() {
                "vector" => self.vector = None,
                "dma" => self.dma = None,
                "irq" => self.i_mask = None,
                "spu" => self.spu = None,
                "cd" => self.cd = None,
                "ram" => self.ram = Fill::Default,
                "vram" => self.vram = Fill::Default,
                "scratch" => self.scratch = Fill::Default,
                "gpu" => self.gpu = None,
                "timers" => self.timers = None,
                "cpu" => self.cpu = None,
                _ => {}
            }
        }
        if options.dpcr.is_some() || options.dicr.is_some() {
            let (dpcr, dicr) = self.dma.unwrap_or((RETAIL_DPCR, RETAIL_DICR));
            self.dma = Some((options.dpcr.unwrap_or(dpcr), options.dicr.unwrap_or(dicr)));
        }
        if let Some(mask) = options.imask {
            self.i_mask = Some(mask);
        }
    }

    /// One line for the log: what differs from the default boot.
    pub(crate) fn describe(&self) -> String {
        let mut out = format!("[entry-state] {}:", self.label);
        if let Some((base, form)) = self.vector {
            out.push_str(&format!(" vector={form:?}->{base:#x}"));
        }
        if let Some((dpcr, dicr)) = self.dma {
            out.push_str(&format!(" dpcr={dpcr:#010x} dicr={dicr:#010x}"));
        }
        if let Some(mask) = self.i_mask {
            out.push_str(&format!(" imask={mask:#05x}"));
        }
        if let Some(spu) = &self.spu {
            out.push_str(&format!(
                " spu(main={:#x}/{:#x} cnt={:#06x} cd={:#x}/{:#x})",
                spu.main_l, spu.main_r, spu.spucnt, spu.cd_l, spu.cd_r
            ));
        }
        if let Some(cd) = &self.cd {
            out.push_str(&format!(
                " cd(mode={:#04x} mask={:#04x}",
                cd.mode, cd.irq_mask
            ));
            if let Some(head) = cd.head {
                out.push_str(&format!(" head={head}"));
            }
            out.push(')');
        }
        if self.gpu.is_some() {
            out.push_str(" gpu");
        }
        if self.timers.is_some() {
            out.push_str(" timers");
        }
        if self.cpu.is_some() {
            out.push_str(" cpu");
        }
        for (name, fill) in [
            ("ram", self.ram),
            ("vram", self.vram),
            ("scratch", self.scratch),
        ] {
            match fill {
                Fill::Default => {}
                Fill::Byte(b) => out.push_str(&format!(" {name}={b:#04x}")),
                Fill::Random(s) => out.push_str(&format!(" {name}=random({s:#x})")),
            }
        }
        out
    }
}

/// DPCR and DICR of the default (retail-modelled) boot.
const RETAIL_DPCR: u32 = 0x0000_9099;
const RETAIL_DICR: u32 = 0x8c8c_0000;

fn fill_bytes(fill: Fill, mut write: impl FnMut(&mut dyn FnMut() -> u8)) {
    match fill {
        Fill::Default => {}
        Fill::Byte(byte) => write(&mut || byte),
        Fill::Random(seed) => {
            let mut rng = Rng::new(seed);
            let mut word = 0u64;
            let mut left = 0;
            write(&mut || {
                if left == 0 {
                    word = rng.next_u64();
                    left = 8;
                }
                left -= 1;
                let byte = word as u8;
                word >>= 8;
                byte
            });
        }
    }
}

impl Plan {
    /// Memory that must be filled before the executable is loaded: RAM
    /// above the kernel, VRAM and the scratchpad. Returns whether RAM was
    /// filled (so the default zeroing is skipped).
    pub(crate) fn fill_memory(&self, bus: &mut crate::Bus) -> bool {
        let poison = std::env::var("PSOXIDE_POISON_MEM")
            .ok()
            .map(|spec| crate::Bus::poison_byte(&spec));
        let ram = match (self.ram, poison) {
            (Fill::Byte(_), Some(byte)) => Fill::Byte(byte),
            (ram, _) => ram,
        };
        fill_bytes(ram, |next| {
            bus.fill_ram_with(0x8001_0000, 0x1f_0000, next);
        });
        fill_bytes(self.scratch, |next| bus.fill_scratchpad_with(next));
        fill_bytes(self.vram, |next| {
            bus.fill_vram_with(|| {
                let lo = next();
                u16::from(lo) | (u16::from(next()) << 8)
            });
        });
        ram != Fill::Default
    }

    /// The hardware registers, with the default boot's own values for the
    /// categories the plan leaves alone.
    pub(crate) fn apply(&self, bus: &mut crate::Bus) {
        const DPCR: u32 = 0x1f80_10f0;
        const I_MASK: u32 = 0x1f80_1074;
        const GP0: u32 = 0x1f80_1810;
        const GP1: u32 = crate::gpu::GP1_ADDR;

        let (dpcr, dicr) = self.dma.unwrap_or((RETAIL_DPCR, RETAIL_DICR));
        bus.write32(DPCR, dpcr);
        bus.set_dicr_raw(dicr);
        bus.write32(I_MASK, self.i_mask.unwrap_or(0x0c));
        match &self.spu {
            Some(spu) => bus.spu.apply_entry_profile(spu),
            None => bus.apply_hle_entry_audio_profile(),
        }
        if let Some(cd) = &self.cd {
            bus.cdrom.apply_entry_state(cd.mode, cd.irq_mask);
            if let Some(head) = cd.head {
                bus.cdrom.park_head(head);
            }
        }
        match &self.gpu {
            Some(gpu) => {
                bus.write32(GP1, 0x0800_0000 | gpu.display_mode);
                bus.write32(GP1, 0x0500_0000 | gpu.display_start);
                for word in gpu.draw_state {
                    bus.write32(GP0, word);
                }
                bus.write32(GP1, 0x0300_0000 | u32::from(gpu.display_off));
            }
            None => {
                bus.write32(GP1, 0x0800_0027);
                bus.write32(GP0, 0xe100_0600);
                bus.write32(GP1, 0x0300_0000);
            }
        }
        if let Some(timers) = &self.timers {
            for (n, timer) in timers.iter().enumerate() {
                let base = 0x1f80_1100 + 0x10 * n as u32;
                bus.write32(base + 4, u32::from(timer.mode));
                bus.write32(base, u32::from(timer.counter));
                bus.write32(base + 8, u32::from(timer.target));
            }
        }
    }

    /// After the CPU is seeded at the entry point.
    pub(crate) fn apply_cpu(&self, cpu: &mut crate::Cpu) {
        if let Some(entry) = &self.cpu {
            cpu.seed_entry_scratch(&entry.regs, entry.hi, entry.lo);
        }
    }

    /// After the HLE kernel is installed: move the exception handler and
    /// write the vector.
    pub(crate) fn apply_vector(&self, bus: &mut crate::Bus) {
        if let Some((base, form)) = self.vector {
            if base != crate::hle_kernel::EXCEPTION_HANDLER {
                crate::hle_exceptions::relocate_handler(bus, base);
            }
            crate::hle_exceptions::install_vector_form(bus, base, form);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn specs_parse_and_print_back() {
        for spec in [
            "default",
            "openbios",
            "openbios,dpcr=0x9099",
            "hostile:7",
            "hostile:7,imask=0x4,skip=gpu,skip=cpu",
        ] {
            assert_eq!(EntryState::from_spec(spec).unwrap().spec(), spec);
        }
        assert_eq!(EntryState::from_spec("").unwrap(), EntryState::Default);
        assert_eq!(
            EntryState::from_spec("hostile").unwrap(),
            EntryState::Hostile {
                seed: 1,
                options: Options::default()
            }
        );
        for bad in [
            "nope",
            "openbios:3",
            "hostile:x",
            "hostile,skip=sound",
            "hostile,dpcr",
        ] {
            assert!(EntryState::from_spec(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_seed_gives_the_same_state_and_other_seeds_differ() {
        let one = Plan::hostile(5, &Options::default(), 1000);
        assert_eq!(one, Plan::hostile(5, &Options::default(), 1000));
        assert_ne!(one, Plan::hostile(6, &Options::default(), 1000));
    }

    #[test]
    fn skipping_a_category_leaves_the_others_on_their_values() {
        let all = Plan::hostile(9, &Options::default(), 1000);
        let options = Options {
            skip: vec!["gpu".to_string()],
            ..Options::default()
        };
        let some = Plan::hostile(9, &options, 1000);
        assert_eq!(some.gpu, None);
        assert_eq!(some.spu, all.spu);
        assert_eq!(some.cpu, all.cpu);
        assert_eq!(some.dma, all.dma);
    }

    #[test]
    fn hostile_values_stay_in_range() {
        for seed in 0..200 {
            let plan = Plan::hostile(seed, &Options::default(), 5000);
            let (base, _) = plan.vector.unwrap();
            assert!(HANDLER_SPOTS.contains(&base));
            let (_, dicr) = plan.dma.unwrap();
            assert_eq!(dicr & 0x0000_ffff, 0);
            let master = dicr >> 31 == 1;
            let flags = (dicr >> 24) & 0x7f;
            let enables = (dicr >> 16) & 0x7f;
            assert_eq!(master, dicr & (1 << 23) != 0 && flags & enables != 0);
            let spu = plan.spu.unwrap();
            assert!(spu.main_l <= 0x3fff && spu.main_r <= 0x3fff);
            assert_eq!(spu.spucnt & 0x30, 0);
            assert!(plan.cd.unwrap().head.unwrap() < 5000);
            assert_eq!(plan.gpu.unwrap().display_mode & 0x08, 0, "NTSC");
            for timer in plan.timers.unwrap() {
                assert_eq!(timer.mode & !0x0308, 0, "no sync mode, no timer interrupts");
            }
            for (index, _) in plan.cpu.unwrap().regs {
                assert!(!matches!(index, 0 | 4 | 5 | 28..=31));
            }
        }
    }

    #[test]
    fn handler_spots_hold_the_handler_and_miss_the_kernel() {
        let len = crate::hle_exceptions::handler_len_bytes();
        assert!(len <= 0x280, "{len:#x}");
        for base in HANDLER_SPOTS {
            let end = base + len;
            // Everything the layout table names in 0x0F00..0x8648 that is
            // not free.
            let used: [(u32, u32); 5] = [
                (0x0f00, 0x4176),
                (0x43d0, 0x5e00),
                (0x6ee0, 0x7200),
                (0x8648, 0x8908),
                (0xdf80, 0x10000),
            ];
            for (lo, hi) in used {
                if base == crate::hle_kernel::EXCEPTION_HANDLER {
                    continue;
                }
                assert!(
                    end <= lo || base >= hi,
                    "{base:#x}..{end:#x} meets {lo:#x}..{hi:#x}"
                );
            }
        }
    }
}
