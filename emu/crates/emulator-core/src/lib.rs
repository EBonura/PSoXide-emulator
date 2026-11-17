// SPDX-License-Identifier: GPL-2.0-or-later
//! PSoXide emulator core.
//!
//! CPU, peripherals and a built-in runtime for homebrew executables and discs.
//! No external firmware images are accepted by the public bus API.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(missing_docs)]

/// A diagnostic environment switch, read once per process. Several trace
/// switches sat on per-instruction paths (MFC2, timer writes, GP0 overflow)
/// and paid a libc `getenv` scan on every call.
#[macro_export]
macro_rules! env_flag {
    ($name:literal) => {{
        static FLAG: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        *FLAG.get_or_init(|| std::env::var_os($name).is_some())
    }};
}

pub mod bios_names;
pub mod bus;
pub mod cdrom;
pub mod conformance;
pub mod cpe;
pub mod cpu;
pub mod dma;
pub mod entry_state;
pub mod fastboot;
pub mod freelook;
pub mod gpu;
pub mod guest_stats;
pub mod hardware_target;
pub mod hle_asm;
pub mod hle_bios;
pub mod hle_bu;
pub mod hle_card;
pub mod hle_exceptions;
pub mod hle_files;
pub mod hle_font;
pub mod hle_kernel;
pub mod hle_pad;
pub mod hle_patch;
#[cfg(test)]
mod hle_testkit;
pub mod input_tape;
pub mod irq;
pub mod limits;
pub mod mdec;
pub mod mmio_trace;
pub mod pad;
pub mod pcdrv;
pub mod ram;
pub mod scheduler;
pub(crate) mod serde_big_array;
pub mod sio;
mod sio1;
pub mod snapshot;
pub mod spu;
pub mod system_cnf;
pub mod telemetry;
pub mod timers;
pub mod vram;

// Root re-exports: only what the two consumers (frontend and
// psx-gpu-render) actually reach by name. Everything else stays on
// its module path.
pub use bus::Bus;
pub use cpu::{
    Cpu, CpuCycleProfileSnapshot, InstructionCacheMissKind, InstructionCacheProfileSnapshot,
    InstructionCacheRefillEvent, InstructionClassProfileSnapshot,
};
pub use entry_state::EntryState;
pub use fastboot::{fast_boot_disc, fast_boot_disc_with};
pub use freelook::FreelookState;
pub use gpu::{Deinterlace, DisplayArea, Gpu};
pub use hardware_target::HardwareTarget;
pub use input_tape::{
    game_image_hash, game_image_hash_parts, read_tape, tape_from_bytes, tape_from_csv, tape_to_csv,
    write_tape, GameImageHasher, PadSample,
};
pub use pad::{button, ButtonState};
pub use psx_gte_core::GteProfileSnapshot;
pub use snapshot::{EmulatorState, EmulatorStateRef};
pub use telemetry::{GuestTelemetry, GuestTelemetryEvent, GuestTelemetryKind};
pub use vram::{Vram, VRAM_HEIGHT, VRAM_WIDTH};

/// Whether this build can model strict Expansion 2 stores (cargo feature
/// `strict-exp2`, `make build-strict`).
pub const STRICT_EXP2_COMPILED: bool = cfg!(feature = "strict-exp2");

/// Whether the environment asks for strict Expansion 2 stores
/// (`PSOXIDE_STRICT_EXP2` set to anything but `0` or empty).
#[must_use]
pub fn strict_exp2_requested() -> bool {
    std::env::var("PSOXIDE_STRICT_EXP2").is_ok_and(|value| value != "0" && !value.is_empty())
}

/// For a binary's start-up: stop with a clear message when strict Expansion 2
/// stores are asked for (`requested`, or `PSOXIDE_STRICT_EXP2`) in a build
/// without the `strict-exp2` feature, so a run is never silently non-strict.
///
/// # Errors
/// The message to print, when strict mode is asked for and not compiled in.
pub fn check_strict_exp2(requested: bool) -> Result<(), String> {
    if (requested || strict_exp2_requested()) && !STRICT_EXP2_COMPILED {
        return Err(
            "strict Expansion 2 stores are not compiled into this build; \
build the strict binary with `make build-strict` (cargo build --release --features strict-exp2)"
                .into(),
        );
    }
    Ok(())
}
