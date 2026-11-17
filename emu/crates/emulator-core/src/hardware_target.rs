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
//! real[,cd-ack=MODEL][,mdec-req=MODEL]
//! mister[,gpu-irq=MODEL][,cd-ack=MODEL][,mdec-req=MODEL][,seed=N][,trace=N]
//! ```
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
            seed: real_seed,
            trace: real_trace,
        } = HardwareTarget::real();
        let HardwareTarget {
            implementation: _,
            gpu_irq: mister_irq,
            cd_ack: mister_cd_ack,
            mdec_req: mister_mdec_req,
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
