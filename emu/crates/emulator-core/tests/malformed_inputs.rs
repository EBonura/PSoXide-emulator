//! Property tests for untrusted inputs that reach the emulator core: a
//! disc's SYSTEM.CNF and boot EXE header, raw disc images, side-loaded
//! EXEs and save states. Each must be accepted or rejected, never panic
//! (a panic aborts the whole wasm module in the browser build).
//! Deterministic fixed-seed PRNG so a failure reproduces.

use std::panic::{catch_unwind, AssertUnwindSafe};

use emulator_core::snapshot::{EmulatorState, EmulatorStateRef};
use emulator_core::{fast_boot_disc, Bus, Cpu};
use psoxide_settings::savestate::SaveStateV1;
use psx_iso::{Disc, Exe, IsoBuilder, EXE_HEADER_BYTES};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
    /// A 32-bit field biased toward the values that stress range checks.
    fn field(&mut self) -> u32 {
        match self.below(6) {
            0 => 0,
            1 => u32::MAX,
            2 => 0x801F_FFF0 + self.below(32) as u32,
            3 => 0x8000_0000 + self.below(0x0020_0000) as u32,
            4 => self.below(0x0040_0000) as u32,
            _ => self.next() as u32,
        }
    }
}

fn no_panic(what: &str, input: &dyn std::fmt::Debug, f: impl FnOnce()) {
    if catch_unwind(AssertUnwindSafe(f)).is_err() {
        panic!("{what} panicked on input: {input:?}");
    }
}

fn exe_with_header(rng: &mut Rng) -> Vec<u8> {
    let payload = rng.below(64) * 4;
    let mut exe = vec![0u8; EXE_HEADER_BYTES + payload];
    exe[..8].copy_from_slice(b"PS-X EXE");
    for offset in [0x10, 0x14, 0x18, 0x28, 0x2C, 0x30, 0x34] {
        exe[offset..offset + 4].copy_from_slice(&rng.field().to_le_bytes());
    }
    // Usually honest about the payload size, sometimes not.
    let t_size = if rng.below(3) == 0 {
        rng.field()
    } else {
        payload as u32
    };
    exe[0x1C..0x20].copy_from_slice(&t_size.to_le_bytes());
    exe
}

#[test]
fn disc_boot_with_hostile_exe_headers_never_panics() {
    let mut rng = Rng(0xb007);
    let cnfs: [&[u8]; 5] = [
        b"BOOT = cdrom:\\GAME.EXE;1\r\n",
        b"BOOT = cdrom:\\GAME.EXE;1\r\nSTACK = 801FFFF0\r\nTCB = 4\r\nEVENT = 10\r\n",
        b"BOOT = cdrom:\\GAME.EXE;1\r\nSTACK = FFFFFFFF\r\nTCB = FFFFFFFF\r\nEVENT = FFFFFFFF\r\n",
        b"BOOT = cdrom:\\MISSING.EXE;1\r\n",
        b"BOOT\r\n",
    ];
    for _ in 0..400 {
        let exe = exe_with_header(&mut rng);
        let cnf = cnfs[rng.below(cnfs.len())];
        let mut builder = IsoBuilder::new();
        builder.add_file("SYSTEM.CNF", cnf.to_vec());
        builder.add_file("GAME.EXE", exe.clone());
        let disc = Disc::from_bin(builder.build_bin());
        let header = (&exe[0x10..0x38], String::from_utf8_lossy(cnf).into_owned());
        no_panic("fast_boot_disc", &header, || {
            let mut bus = Bus::new_without_bios();
            let mut cpu = Cpu::new();
            let _ = fast_boot_disc(&mut bus, &mut cpu, &disc);
        });
    }
}

#[test]
fn side_loaded_exe_headers_never_panic() {
    let mut rng = Rng(0x51de);
    let mut bus = Bus::new_without_bios();
    for _ in 0..2_000 {
        let exe_bytes = exe_with_header(&mut rng);
        no_panic("side-load", &&exe_bytes[0x10..0x38], || {
            if let Ok(exe) = Exe::parse(&exe_bytes) {
                bus.load_exe_payload(exe.load_addr, &exe.payload);
                bus.clear_exe_bss(exe.bss_addr, exe.bss_size);
            }
        });
    }
    // An oversized payload wraps through the RAM mirror: the last 2 MiB
    // written win, as sequential CPU stores would leave them.
    let payload: Vec<u8> = (0..(2 << 20) + 8).map(|i| (i / 4) as u8).collect();
    bus.load_exe_payload(0x8001_0000, &payload);
    let last = *payload.last().unwrap();
    let wrapped_end = (0x0001_0000 + payload.len() - 1) % (2 << 20);
    assert_eq!(bus.read8(wrapped_end as u32), last);
}

#[test]
fn mutated_save_states_never_panic() {
    let mut cpu = Cpu::new();
    let mut bus = Bus::new_without_bios();
    bus.write32(0x1000, 0x3C08_1234);
    cpu.seed_from_exe(0x8000_1000, 0, None);
    for _ in 0..4 {
        let _ = cpu.step(&mut bus);
    }
    let bytes = SaveStateV1::new(
        EmulatorStateRef {
            cpu: &cpu,
            bus: &bus,
        },
        "fuzz",
        cpu.tick(),
    )
    .to_bytes()
    .unwrap();
    let mut rng = Rng(0x5a7e);
    for _ in 0..300 {
        let mut mutated = bytes.clone();
        match rng.below(4) {
            0 => mutated.truncate(rng.below(bytes.len())),
            1 => {
                for _ in 0..1 + rng.below(8) {
                    let at = rng.below(mutated.len());
                    mutated[at] = rng.next() as u8;
                }
            }
            2 => {
                // Corrupt the header region, where the length-prefixed strings live.
                let at = 8 + rng.below(64.min(mutated.len() - 8));
                mutated[at] = 0xFF;
            }
            _ => {
                let at = rng.below(mutated.len());
                mutated.splice(at..at, [0xFF, 0xFF, 0xFF, 0xFF, 0x0F]);
            }
        }
        no_panic("SaveStateV1::from_bytes", &mutated.len(), || {
            if let Ok(state) = SaveStateV1::<EmulatorState>::from_bytes(&mutated) {
                let _ = (state.payload.cpu.pc(), state.payload.bus.cycles());
            }
        });
    }
}
