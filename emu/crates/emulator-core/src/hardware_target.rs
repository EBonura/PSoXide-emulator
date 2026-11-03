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
//! real
//! mister[,gpu-irq=MODEL][,seed=N][,trace=N]
//! ```
//!
//! `mister` defaults to `gpu-irq=exact`.
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

/// A named hardware target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct HardwareTarget {
    /// Which console implementation this is.
    pub implementation: Implementation,
    /// GP0(1Fh) model.
    pub gpu_irq: GpuIrqModel,
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
    }
}
