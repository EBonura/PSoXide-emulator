//! Named hardware targets: what the emulated console is, where a retail PS1
//! and other implementations differ.
//!
//! The default, [`HardwareTarget::real`], is the retail console the emulator
//! is calibrated against, and nothing here changes it. [`HardwareTarget::mister`]
//! stands for the MiSTer PSX core (480094b, as run on a SuperStation One):
//! an FPGA re-implementation whose differences from silicon were measured with
//! the hardware-test rows (campaign lanes fpga-core, fpga-irq). It exists to
//! reproduce, in the emulator, what a program does on that core, so a fix can
//! be checked here. Each difference lives in one field of the target, so
//! further core differences can join it without a new switch.
//!
//! Selected by `PSOXIDE_HARDWARE` (or `--hardware` on `psoxide-emulator` and
//! `psoxide-gate run`), a spec of the form
//!
//! ```text
//! real[,cd-ack=MODEL][,mdec-req=MODEL][,CD DRIVE OPTIONS]
//! mister[,gpu-irq=MODEL][,cd-ack=MODEL][,mdec-req=MODEL][,seed=N][,trace=N]
//! ```
//!
//! CD drive options (any target, all off by default, see [`CdDriveModel`]):
//! `cd-pause-seek=read|any`, `cd-pause-refuse=MS`, `cd-speed-change=MS` and
//! `cd-marginal=FIRST-LAST:DELAY_MS:FAIL_FRACTION[:SEED]` (up to four areas,
//! one option each). They make the drive
//! harsher than the calibrated default where the console has failed and the
//! mechanism is unmeasured, so a fix can be checked before a disc is burned.
//!
//! `mister` defaults to `gpu-irq=exact`.
//! `cd-ack=strict` (any target) ignores a CD-ROM interrupt acknowledge that
//! commercial code never writes, as PCSX-Redux does, see [`CdAckModel`].
//! `mdec-req=hidden` (any target) never shows the MDEC's DMA requests in its
//! status register, as PCSX-Redux and PCSX-ReARMed do, see [`MdecRequestModel`].
//!
//! `mister` also answers to `mister-fpga`, `fpga` and `superstation`.
//! `gpu-irq` overrides the target's GP0(1Fh) model, see [`GpuIrqModel`];
//! `seed` seeds its coin flips (default 1); `trace=N` prints the first N
//! model events to stderr.

/// Environment variable naming the target.
pub const ENV_VAR: &str = "PSOXIDE_HARDWARE";

/// Which implementation of the console is being emulated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Implementation {
    /// A retail console (the calibrated default).
    Real,
    /// The MiSTer PSX core on a SuperStation One.
    MisterFpga,
}

/// How GP0(1Fh) behaves: whether GPUSTAT bit 24 and I_STAT bit 1 rise.
///
/// On the MiSTer core the request is a pulse one `clk2x` edge wide or two
/// wide depending on the phase the word is popped at (`gpu.vhd` 633-634, 687,
/// 818, 936-939), and only the wide one crosses into the `clk1x` process that
/// sets the bit. An idle GPU always pops on the narrow phase; a draw ahead of
/// the word shifts the phase by the draw's duration parity.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum GpuIrqModel {
    /// Every 1Fh raises the request (retail console).
    Reliable,
    /// No 1Fh ever raises it. The extreme case.
    Never,
    /// Fitted to the core's measured rows (see `gpu/irq_quirk.rs`): idle
    /// pops lost, one big rect or polygon ahead passes, small rects and fills
    /// fail, long chains almost always fail, VRAM copies pass about half.
    Core,
    /// The physical reading of the same evidence: each draw has a parity,
    /// a chain XORs them, and a 1Fh passes behind an odd chain.
    Parity,
    /// Parity per command class from cycle-accurate GHDL runs of `gpu.vhd`
    /// (fpga-irq, coordinator 11-03): an opaque rect passes when its pixel
    /// count is even, fills and triangles each add an odd duration, copies
    /// are a coin, and a command that ends before the next word arrives (the
    /// short ones) loses the request. Size cut-offs stand in for the arrival
    /// gap and are calibrated against the v3 rows, not measured.
    Exact,
}

/// How a write to the CD-ROM interrupt flag register (`1F801803h` index 1)
/// clears the pending response.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum CdAckModel {
    /// Write 1 to clear, any value (retail console, CXD1199/CXD1815 datasheet).
    Any,
    /// Only what commercial code writes acknowledges: a write that breaks a
    /// CD rule of [`crate::conformance`] (a partial acknowledge, or one
    /// combined with the parameter reset) clears no response bit, so the
    /// guest waits as it does on PCSX-Redux (`cdrom.cc`: 07h and 1Fh
    /// acknowledge, bit 6 alone resets the parameters, anything else pauses).
    Strict,
}

/// Whether the MDEC status register shows the DMA requests (bits 27 and 28).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum MdecRequestModel {
    /// Bit 28 reads 1 while the MDEC wants data in, bit 27 while it has
    /// data out for DMA1 (retail console).
    Shown,
    /// Both always read 0, as on PCSX-Redux (`mdec.cc`) and PCSX-ReARMed
    /// (`mdec.c`), which define the request bits and never set them. The
    /// transfers themselves still run: only a guest that reads the bits
    /// from the CPU sees a difference.
    Hidden,
}

/// Which drive activity refuses a Pause with INT5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum PauseSeekModel {
    /// Only the head travel ahead of a read's first sector (measured: hwtest
    /// v2.7 run FB89 and both v2.18 runs of 2026-11-14, records 0x35C-0x35E
    /// and 0x370-0x372, six of six each).
    Read,
    /// Also the travel of a plain SeekL/SeekP. Not measured on a console.
    Any,
}

/// Sectors of a disc area that read slowly or not at all, as on a weak burn.
///
/// A failing sector is silence: no INT1 and no INT5, the drive keeps the read
/// going and never delivers it. That is what the radius sweep saw on this
/// console (hwtest v2.8 run F8BB: INT5 zero at every radius, the raw 2x read
/// timed out at 77:48). The fail roll is seeded by LBA and by the count of
/// read commands, so a retry rolls again and a run repeats exactly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarginalArea {
    /// First LBA of the area.
    pub first: u32,
    /// Last LBA of the area, inclusive.
    pub last: u32,
    /// Extra time each sector of the area takes to arrive, in milliseconds.
    pub delay_ms: u32,
    /// Chance that a sector read is never delivered, in millionths.
    pub fail_ppm: u32,
    /// Seed of the fail rolls.
    pub seed: u64,
}

impl MarginalArea {
    /// Parse `FIRST-LAST:DELAY_MS:FAIL_FRACTION[:SEED]` (fraction 0 to 1).
    ///
    /// # Errors
    ///
    /// Names the part that does not parse.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = |what: &str| format!("cd-marginal `{text}`: {what}");
        let mut parts = text.split(':');
        let range = parts.next().ok_or_else(|| bad("expected FIRST-LAST"))?;
        let (first, last) = range
            .split_once('-')
            .ok_or_else(|| bad("expected FIRST-LAST"))?;
        let first: u32 = first.trim().parse().map_err(|_| bad("bad first LBA"))?;
        let last: u32 = last.trim().parse().map_err(|_| bad("bad last LBA"))?;
        if last < first {
            return Err(bad("last LBA before first"));
        }
        let delay_ms: u32 = parts
            .next()
            .ok_or_else(|| bad("expected DELAY_MS"))?
            .trim()
            .parse()
            .map_err(|_| bad("bad delay"))?;
        let fraction: f64 = parts
            .next()
            .ok_or_else(|| bad("expected FAIL_FRACTION"))?
            .trim()
            .parse()
            .map_err(|_| bad("bad fail fraction"))?;
        if !(0.0..=1.0).contains(&fraction) {
            return Err(bad("fail fraction outside 0 to 1"));
        }
        let seed = match parts.next() {
            Some(seed) => seed.trim().parse().map_err(|_| bad("bad seed"))?,
            None => 1,
        };
        if parts.next().is_some() {
            return Err(bad("too many fields"));
        }
        Ok(Self {
            first,
            last,
            delay_ms,
            fail_ppm: (fraction * 1_000_000.0).round() as u32,
            seed,
        })
    }

    /// Whether `lba` lies in the area.
    #[must_use]
    pub fn contains(&self, lba: u32) -> bool {
        (self.first..=self.last).contains(&lba)
    }

    /// Whether the read of `lba` by read command number `attempt` fails.
    #[must_use]
    pub fn fails(&self, lba: u32, attempt: u32) -> bool {
        if self.fail_ppm == 0 || !self.contains(lba) {
            return false;
        }
        let mut x = self.seed ^ (u64::from(lba) << 32) ^ u64::from(attempt);
        // splitmix64 finaliser.
        x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
        x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        x ^= x >> 31;
        (x % 1_000_000) < u64::from(self.fail_ppm)
    }
}

/// CD drive behaviours beyond the calibrated default. Every field off means
/// the retail console as measured; each one is an unmeasured or extrapolated
/// harshness for reproducing a console failure headless.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CdDriveModel {
    /// Which seeks refuse a Pause (`cd-pause-seek`).
    pub pause_seek: PauseSeekModel,
    /// After a refused Pause, every Pause in the next this-many milliseconds is
    /// refused too (`cd-pause-refuse`). The next ReadN ends it: on the console
    /// the trial after each refusal set up and paused with no failure (record
    /// 0x342 setup failures 0). How long it lasts otherwise is not measured.
    pub pause_refuse_ms: u32,
    /// A SetMode that changes the speed bit keeps the drive busy this many
    /// milliseconds (`cd-speed-change`): GetStat shows seeking, Pause is
    /// refused, and sectors still come at the old speed. Not measured.
    pub speed_change_ms: u32,
    /// Weak disc areas (`cd-marginal`, once per area, or `PSOXIDE_CD_MARGINAL`
    /// with areas joined by `+`). The first area holding an LBA applies.
    pub marginal: [Option<MarginalArea>; MARGINAL_AREAS],
}

impl CdDriveModel {
    /// The calibrated drive.
    #[must_use]
    pub const fn calibrated() -> Self {
        Self {
            pause_seek: PauseSeekModel::Read,
            pause_refuse_ms: 0,
            speed_change_ms: 0,
            marginal: [None; MARGINAL_AREAS],
        }
    }

    /// Add a weak area.
    ///
    /// # Errors
    ///
    /// When [`MARGINAL_AREAS`] are already set.
    pub fn add_marginal(&mut self, area: MarginalArea) -> Result<(), String> {
        let slot = self
            .marginal
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or_else(|| format!("cd-marginal: at most {MARGINAL_AREAS} areas"))?;
        *slot = Some(area);
        Ok(())
    }

    /// The weak area holding `lba`, if any.
    #[must_use]
    pub fn marginal_area(&self, lba: u32) -> Option<&MarginalArea> {
        self.marginal
            .iter()
            .flatten()
            .find(|area| area.contains(lba))
    }

    /// Whether any option is set.
    #[must_use]
    pub fn differs_from_calibrated(&self) -> bool {
        *self != Self::calibrated()
    }
}

/// Most weak areas one drive model holds.
pub const MARGINAL_AREAS: usize = 4;

/// Environment variable that sets [`CdDriveModel::marginal`] for any target.
pub const MARGINAL_ENV_VAR: &str = "PSOXIDE_CD_MARGINAL";

/// A named hardware target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HardwareTarget {
    /// Which console implementation this is.
    pub implementation: Implementation,
    /// GP0(1Fh) model.
    pub gpu_irq: GpuIrqModel,
    /// CD-ROM interrupt acknowledge model.
    pub cd_ack: CdAckModel,
    /// MDEC status request bits model.
    pub mdec_req: MdecRequestModel,
    /// CD drive options.
    pub cd_drive: CdDriveModel,
    /// Seed of the model's coin flips.
    pub seed: u64,
    /// Number of model events to print to stderr.
    pub trace: u32,
}

impl HardwareTarget {
    /// A retail console: the emulator's calibrated behaviour.
    #[must_use]
    pub const fn real() -> Self {
        Self {
            implementation: Implementation::Real,
            gpu_irq: GpuIrqModel::Reliable,
            cd_ack: CdAckModel::Any,
            mdec_req: MdecRequestModel::Shown,
            cd_drive: CdDriveModel::calibrated(),
            seed: 1,
            trace: 0,
        }
    }

    /// The MiSTer PSX core (480094b) on a SuperStation One.
    #[must_use]
    pub const fn mister() -> Self {
        Self {
            implementation: Implementation::MisterFpga,
            gpu_irq: GpuIrqModel::Exact,
            cd_ack: CdAckModel::Any,
            mdec_req: MdecRequestModel::Shown,
            cd_drive: CdDriveModel::calibrated(),
            seed: 1,
            trace: 0,
        }
    }

    /// Parse a spec (see the module docs).
    ///
    /// # Errors
    ///
    /// Names the unknown target, option or value.
    pub fn from_spec(spec: &str) -> Result<Self, String> {
        let mut parts = spec.split(',').map(str::trim);
        let mut target = match parts.next().unwrap_or("") {
            "" | "real" | "default" => Self::real(),
            "mister" | "mister-fpga" | "fpga" | "superstation" => Self::mister(),
            other => {
                return Err(format!(
                    "hardware target `{other}`: expected `real` or `mister`"
                ))
            }
        };
        for option in parts.filter(|p| !p.is_empty()) {
            let (key, value) = option
                .split_once('=')
                .ok_or_else(|| format!("hardware option `{option}`: expected KEY=VALUE"))?;
            match key {
                "gpu-irq" => {
                    target.gpu_irq = match value {
                        "reliable" => GpuIrqModel::Reliable,
                        "never" => GpuIrqModel::Never,
                        "core" => GpuIrqModel::Core,
                        "parity" => GpuIrqModel::Parity,
                        "exact" => GpuIrqModel::Exact,
                        _ => {
                            return Err(format!(
                                "hardware option gpu-irq={value}: expected reliable, never, core, parity or exact"
                            ))
                        }
                    }
                }
                "cd-ack" => {
                    target.cd_ack = match value {
                        "any" => CdAckModel::Any,
                        "strict" => CdAckModel::Strict,
                        _ => {
                            return Err(format!(
                                "hardware option cd-ack={value}: expected any or strict"
                            ))
                        }
                    }
                }
                "mdec-req" => {
                    target.mdec_req = match value {
                        "shown" => MdecRequestModel::Shown,
                        "hidden" => MdecRequestModel::Hidden,
                        _ => {
                            return Err(format!(
                                "hardware option mdec-req={value}: expected shown or hidden"
                            ))
                        }
                    }
                }
                "cd-pause-seek" => {
                    target.cd_drive.pause_seek = match value {
                        "read" => PauseSeekModel::Read,
                        "any" => PauseSeekModel::Any,
                        _ => {
                            return Err(format!(
                                "hardware option cd-pause-seek={value}: expected read or any"
                            ))
                        }
                    }
                }
                "cd-pause-refuse" => {
                    target.cd_drive.pause_refuse_ms = value.parse().map_err(|_| {
                        format!("hardware option cd-pause-refuse={value}: not a number of ms")
                    })?;
                }
                "cd-speed-change" => {
                    target.cd_drive.speed_change_ms = value.parse().map_err(|_| {
                        format!("hardware option cd-speed-change={value}: not a number of ms")
                    })?;
                }
                "cd-marginal" => {
                    target.cd_drive.add_marginal(MarginalArea::parse(value)?)?;
                }
                "seed" => {
                    target.seed = value
                        .parse()
                        .map_err(|_| format!("hardware option seed={value}: not a number"))?;
                }
                "trace" => {
                    target.trace = value
                        .parse()
                        .map_err(|_| format!("hardware option trace={value}: not a number"))?;
                }
                _ => return Err(format!("unknown hardware option `{key}`")),
            }
        }
        Ok(target)
    }

    /// The target `PSOXIDE_HARDWARE` names; [`HardwareTarget::real`] when unset.
    ///
    /// # Panics
    ///
    /// On a malformed spec: a typo must not run as a retail console.
    #[must_use]
    pub fn from_env() -> Self {
        match std::env::var(ENV_VAR) {
            Ok(spec) => Self::from_spec(&spec).unwrap_or_else(|e| panic!("{ENV_VAR}: {e}")),
            Err(_) => Self::real(),
        }
    }

    /// Whether this target changes any behaviour from the retail console.
    #[must_use]
    pub fn differs_from_real(&self) -> bool {
        self.gpu_irq != GpuIrqModel::Reliable
            || self.cd_ack != CdAckModel::Any
            || self.mdec_req != MdecRequestModel::Shown
            || self.cd_drive.differs_from_calibrated()
    }

    /// Short name for logs.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self.implementation {
            Implementation::Real => "real hardware",
            Implementation::MisterFpga => "MiSTer FPGA core",
        }
    }
}

impl Default for HardwareTarget {
    fn default() -> Self {
        Self::real()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_real_and_changes_nothing() {
        assert_eq!(HardwareTarget::default(), HardwareTarget::real());
        assert!(!HardwareTarget::real().differs_from_real());
        assert_eq!(
            HardwareTarget::from_spec("").unwrap(),
            HardwareTarget::real()
        );
    }

    #[test]
    fn mister_models_the_core() {
        let t = HardwareTarget::from_spec("superstation").unwrap();
        assert_eq!(t.implementation, Implementation::MisterFpga);
        assert_eq!(t.gpu_irq, GpuIrqModel::Exact);
        assert!(t.differs_from_real());
    }

    #[test]
    fn options_override() {
        let t = HardwareTarget::from_spec("mister,gpu-irq=never,seed=7,trace=3").unwrap();
        assert_eq!((t.gpu_irq, t.seed, t.trace), (GpuIrqModel::Never, 7, 3));
        assert!(HardwareTarget::from_spec("mister,gpu-irq=sometimes").is_err());
        assert!(HardwareTarget::from_spec("pc").is_err());
        assert!(HardwareTarget::from_spec("mister,bogus=1").is_err());
        let t = HardwareTarget::from_spec("real,cd-ack=strict").unwrap();
        assert_eq!(
            (t.implementation, t.cd_ack),
            (Implementation::Real, CdAckModel::Strict)
        );
        assert!(t.differs_from_real());
        assert!(HardwareTarget::from_spec("real,cd-ack=loose").is_err());
        let t = HardwareTarget::from_spec("mister,mdec-req=hidden").unwrap();
        assert_eq!(t.mdec_req, MdecRequestModel::Hidden);
        let t = HardwareTarget::from_spec("real,mdec-req=hidden").unwrap();
        assert!(t.differs_from_real());
        assert!(HardwareTarget::from_spec("real,mdec-req=never").is_err());
    }

    #[test]
    fn cd_drive_options_parse_and_default_off() {
        let t = HardwareTarget::from_spec(
            "real,cd-pause-seek=any,cd-pause-refuse=500,cd-speed-change=300,cd-marginal=335881-341440:20:0.5:7",
        )
        .unwrap();
        assert_eq!(t.cd_drive.pause_seek, PauseSeekModel::Any);
        assert_eq!(t.cd_drive.pause_refuse_ms, 500);
        assert_eq!(t.cd_drive.speed_change_ms, 300);
        assert_eq!(
            t.cd_drive.marginal[0],
            Some(MarginalArea {
                first: 335_881,
                last: 341_440,
                delay_ms: 20,
                fail_ppm: 500_000,
                seed: 7
            })
        );
        assert!(t.differs_from_real());
        assert!(!HardwareTarget::from_spec("real,cd-pause-seek=read")
            .unwrap()
            .differs_from_real());
        assert!(HardwareTarget::from_spec("real,cd-pause-seek=never").is_err());
        assert!(HardwareTarget::from_spec("real,cd-speed-change=fast").is_err());
        assert!(HardwareTarget::from_spec("real,cd-marginal=9-1:0:0").is_err());
        assert!(HardwareTarget::from_spec("real,cd-marginal=1-9:0:2").is_err());
        let t =
            HardwareTarget::from_spec("real,cd-marginal=1-9:5:0,cd-marginal=20-29:7:0").unwrap();
        assert_eq!(t.cd_drive.marginal_area(25).map(|a| a.delay_ms), Some(7));
        assert_eq!(t.cd_drive.marginal_area(5).map(|a| a.delay_ms), Some(5));
        assert!(t.cd_drive.marginal_area(15).is_none());
        let five = "real,cd-marginal=1-1:0:0,cd-marginal=2-2:0:0,cd-marginal=3-3:0:0,cd-marginal=4-4:0:0,cd-marginal=5-5:0:0";
        assert!(HardwareTarget::from_spec(five).is_err());
    }

    #[test]
    fn marginal_rolls_are_seeded_and_bounded() {
        let area = MarginalArea::parse("100-199:0:0.25").unwrap();
        let fails = (100..200).filter(|&lba| area.fails(lba, 0)).count();
        assert!((10..=45).contains(&fails), "{fails}");
        assert_eq!(
            (100..200).map(|l| area.fails(l, 3)).collect::<Vec<_>>(),
            (100..200).map(|l| area.fails(l, 3)).collect::<Vec<_>>()
        );
        assert!(!area.fails(99, 0) && !area.fails(200, 0));
        let all = MarginalArea::parse("100-199:0:1").unwrap();
        assert!((100..200).all(|l| all.fails(l, 9)));
    }

    // The gate (`tools/psoxide-gate`, `--second-hardware`) takes the first model's result for the
    // second when the guest issued no GP0(1Fh) command and no CD acknowledge that breaks a
    // conformance rule. That holds only while the models differ in GP0(1Fh) handling and the
    // CD acknowledge model alone, and `cd-ack=strict` differs from `any` only on writes that
    // break a CD rule. The MDEC request model is the same on both targets; the gate refuses the
    // shortcut when the two specs set it differently. The next two tests fail when that stops being true, so the shortcut cannot
    // go stale silently: change the gate's rule (`receipt::model_equivalent`) first, then update
    // these.

    #[test]
    fn real_and_mister_differ_in_the_gp0_1f_model_alone() {
        // Naming every field: a new field is a compile error here.
        let HardwareTarget {
            implementation: _,
            gpu_irq: real_irq,
            cd_ack: real_cd_ack,
            mdec_req: real_mdec_req,
            cd_drive: real_cd_drive,
            seed: real_seed,
            trace: real_trace,
        } = HardwareTarget::real();
        let HardwareTarget {
            implementation: _,
            gpu_irq: mister_irq,
            cd_ack: mister_cd_ack,
            mdec_req: mister_mdec_req,
            cd_drive: mister_cd_drive,
            seed: mister_seed,
            trace: mister_trace,
        } = HardwareTarget::mister();
        assert_eq!((real_seed, real_trace), (mister_seed, mister_trace));
        assert_eq!(
            (real_cd_ack, mister_cd_ack),
            (CdAckModel::Any, CdAckModel::Any)
        );
        assert_eq!(
            (real_mdec_req, mister_mdec_req),
            (MdecRequestModel::Shown, MdecRequestModel::Shown)
        );
        assert_eq!(real_cd_drive, CdDriveModel::calibrated());
        assert_eq!(mister_cd_drive, CdDriveModel::calibrated());
        assert_eq!(real_irq, GpuIrqModel::Reliable);
        assert_ne!(mister_irq, GpuIrqModel::Reliable);
        // `implementation` is a label: only `name` reads it.
        assert_eq!(
            HardwareTarget::mister().differs_from_real(),
            mister_irq != real_irq
        );
    }

    #[test]
    fn only_the_gp0_1f_model_reads_the_target() {
        const READERS: [&str; 7] = [
            "emu/crates/emulator-core/src/hardware_target.rs",
            "emu/crates/emulator-core/src/lib.rs",
            "emu/crates/emulator-core/src/gpu/irq_quirk.rs",
            // The CD acknowledge model; it differs only on writes that break a CD rule.
            "emu/crates/emulator-core/src/cdrom.rs",
            // The MDEC request model; the gate compares it between the two specs.
            "emu/crates/emulator-core/src/mdec.rs",
            // Parse the spec and set the variable, or print the target; read no field.
            "emu/crates/frontend/src/main.rs",
            "emu/crates/frontend/src/cli.rs",
        ];
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../..");
        let mut users = Vec::new();
        for dir in ["emu/crates", "sdk/crates", "crates", "tools"] {
            sources_naming_the_target(&root.join(dir), &root, &mut users);
        }
        users.sort();
        let outside: Vec<&String> = users
            .iter()
            .filter(|u| !READERS.contains(&u.as_str()) && !u.starts_with("tools/psoxide-gate/"))
            .collect();
        assert!(
            outside.is_empty(),
            "another reader of the hardware target: {outside:?}; see the comment above"
        );
        assert!(
            users.iter().any(|u| u.ends_with("gpu/irq_quirk.rs")),
            "scan found no reader"
        );
    }

    fn sources_naming_the_target(
        dir: &std::path::Path,
        root: &std::path::Path,
        out: &mut Vec<String>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if path.file_name().is_some_and(|n| n != "target") {
                    sources_naming_the_target(&path, root, out);
                }
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap_or_default();
                let named = [
                    "HardwareTarget",
                    "hardware_target",
                    "PSOXIDE_HARDWARE",
                    "GpuIrqModel",
                ]
                .iter()
                .any(|token| text.contains(token));
                if named {
                    let relative = path.strip_prefix(root).unwrap_or(&path);
                    out.push(relative.to_string_lossy().replace('\\', "/"));
                }
            }
        }
    }
}
