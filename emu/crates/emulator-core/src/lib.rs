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
pub mod cpu;
pub mod dma;
pub mod fastboot;
pub mod freelook;
pub mod gpu;
pub mod hle_asm;
pub mod hle_bios;
pub mod hle_bu;
pub mod hle_card;
pub mod hle_exceptions;
pub mod hle_files;
pub mod hle_font;
pub mod hle_kernel;
pub mod hle_pad;
pub mod input_tape;
pub mod irq;
pub mod limits;
pub mod mdec;
pub mod mmio_trace;
pub mod pad;
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
pub use fastboot::fast_boot_disc;
pub use freelook::FreelookState;
pub use gpu::{DisplayArea, Gpu};
pub use input_tape::{
    game_image_hash, game_image_hash_parts, read_tape, tape_from_bytes, tape_from_csv, tape_to_csv,
    write_tape, PadSample,
};
pub use pad::{button, ButtonState};
pub use psx_gte_core::GteProfileSnapshot;
pub use snapshot::{EmulatorState, EmulatorStateRef};
pub use telemetry::{GuestTelemetry, GuestTelemetryEvent, GuestTelemetryKind};
pub use vram::{Vram, VRAM_HEIGHT, VRAM_WIDTH};
