//! Log the frequency of each A/B/C BIOS function call while the
//! emulator is running. Used to figure out what the BIOS is
//! spinning on after the PlayStation splash -- if one A-function
//! is called millions of times and the disc-read functions never
//! fire, the BIOS is waiting on a syscall-visible state (a
//! counter, a flag) that we're not updating correctly.
//!
//! ```bash
//! PSOXIDE_DISC=path/to/game.cue \
//! PSOXIDE_PAD1_PULSES="0x0008@600+8,0x4000@650+8" \
//!   cargo run -p emulator-core --example bios_syscall_probe --release -- 2000000000
//! ```
//!
//! Set `PSOXIDE_CENSUS_OUT=<dir>` for the full BIOS usage census (boot
//! vs game phase call counts, exception/IRQ/SYSCALL entries, kernel-RAM
//! accesses from user code, EXE-entry state snapshot); see
//! `support/census.rs`. `PSOXIDE_STOP_VBLANK=<n>` stops after `n`
//! VBlank IRQs instead of the step budget.

#[path = "support/census.rs"]
mod census;
#[path = "support/disc.rs"]
mod disc_support;
#[path = "support/pad.rs"]
mod pad_support;

use emulator_core::{Bus, Cpu};
use pad_support::{parse_pad_pulses, parse_u16_mask, sync_pad_mask};
use std::path::PathBuf;

fn main() {
    let n: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2_000_000_000);

    let bios_path = std::env::var("PSOXIDE_BIOS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("bios/SCPH1001.BIN"));
    let bios = std::fs::read(&bios_path).expect("BIOS readable");
    let mut bus = Bus::new(bios).expect("bus");
    let mut census = census::Census::from_env();
    let stop_vblank = std::env::var("PSOXIDE_STOP_VBLANK")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    let mut stop_reason = "step budget";
    if let Ok(disc_path) = std::env::var("PSOXIDE_DISC") {
        let disc = disc_support::load_disc_path(&PathBuf::from(&disc_path)).expect("disc readable");
        if let Some(c) = census.as_mut() {
            c.set_boot_exe(&disc);
        }
        bus.cdrom.insert_disc(Some(disc));
        eprintln!("[probe] mounted {disc_path}");
    }
    bus.attach_digital_pad_port1();
    if std::env::var_os("PSOXIDE_NO_MEMCARD").is_some() {
        bus.detach_memcard_port1();
        bus.detach_memcard_port2();
    } else {
        bus.attach_memcard_port1(Vec::new());
    }
    let mut cpu = Cpu::new();
    let held_buttons = std::env::var("PSOXIDE_PAD1")
        .ok()
        .and_then(|s| parse_u16_mask(&s))
        .unwrap_or(0);
    let pad_pulses = std::env::var("PSOXIDE_PAD1_PULSES")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| parse_pad_pulses(&s).expect("valid PSOXIDE_PAD1_PULSES"))
        .unwrap_or_default();
    let trace_start = std::env::var("PSOXIDE_SYSCALL_TRACE_START")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(u64::MAX);
    let trace_limit = std::env::var("PSOXIDE_SYSCALL_TRACE_LIMIT")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(128);
    let mut trace_count = 0usize;
    let mut current_pad_mask = None;

    // Histograms: [table][function_index] → call count.
    //   table 0 = A-functions @ 0xa0
    //   table 1 = B-functions @ 0xb0
    //   table 2 = C-functions @ 0xc0
    let mut hist: [[u64; 256]; 3] = [[0; 256]; 3];
    // Keep a short recent-call ring so if the BIOS wedges we can
    // print the last few calls to pinpoint the loop.
    let mut recent: std::collections::VecDeque<(u64, u8, u8, u32)> =
        std::collections::VecDeque::with_capacity(64);
    // Capture every `putchar`-ish byte the BIOS emits.
    // A(0x3C), B(0x3C), and B(0x3D) are all stdout-character
    // calls (different kernel versions reach the TTY differently).
    // If the BIOS printed an error before wedging, this is the
    // fastest way to see it.
    let mut putchar_log: String = String::new();
    // Track CDROM SetLoc MSF writes so we can correlate each
    // ReadN with the target LBA. `(step, m, s, f, lba)`.
    let mut cdrom_setloc_log: Vec<(u64, u8, u8, u8, u32)> = Vec::new();
    let mut last_cdrom_cmd_count: u64 = 0;

    for i in 0..n {
        sync_pad_mask(&mut bus, held_buttons, &pad_pulses, &mut current_pad_mask);
        // Before each step, sample pc -- dispatch happens when we
        // execute at exactly 0xa0 / 0xb0 / 0xc0 (the J to the
        // table dispatcher). `t1` carries the function number.
        if let Some(c) = census.as_mut() {
            c.pre_step(&cpu, &mut bus);
        }
        let pc = cpu.pc();
        // Any KUSEG/KSEG0/KSEG1 mirror of the dispatch vectors.
        let table = match pc & 0x1FFF_FFFF {
            0xa0 => Some(0u8),
            0xb0 => Some(1u8),
            0xc0 => Some(2u8),
            _ => None,
        };
        if let Some(t) = table {
            let t1 = cpu.gprs()[9] as u8;
            hist[t as usize][t1 as usize] = hist[t as usize][t1 as usize].saturating_add(1);
            if recent.len() >= 64 {
                recent.pop_front();
            }
            recent.push_back((i, t, t1, cpu.gprs()[31]));
            if i >= trace_start && trace_count < trace_limit {
                trace_count += 1;
                eprintln!(
                    "[syscall] step={i} cyc={} {}(0x{t1:02X}) {} \
                     a0=0x{:08x} a1=0x{:08x} a2=0x{:08x} a3=0x{:08x} ra=0x{:08x}",
                    bus.cycles(),
                    ["A", "B", "C"][t as usize],
                    function_name(t, t1),
                    cpu.gprs()[4],
                    cpu.gprs()[5],
                    cpu.gprs()[6],
                    cpu.gprs()[7],
                    cpu.gprs()[31],
                );
            }

            // Putchar capture: argument is in $a0.
            let a0 = cpu.gprs()[4];
            match (t, t1) {
                (0, 0x3C) | (1, 0x3D) => {
                    // Single-char putchar.
                    if a0 < 128 {
                        putchar_log.push(a0 as u8 as char);
                    } else {
                        putchar_log.push_str(&format!("\\x{:02x}", a0 & 0xFF));
                    }
                }
                (1, 0x3F) => {
                    // std_out_puts: $a0 is a C-string pointer.
                    let mut addr = a0;
                    for _ in 0..512 {
                        let ch = bus.try_read8(addr).unwrap_or(0);
                        if ch == 0 {
                            break;
                        }
                        if ch < 128 {
                            putchar_log.push(ch as char);
                        } else {
                            putchar_log.push_str(&format!("\\x{:02x}", ch));
                        }
                        addr = addr.wrapping_add(1);
                    }
                    putchar_log.push('\n');
                }
                _ => {}
            }
        }
        if let Err(e) = cpu.step(&mut bus) {
            eprintln!("[probe] step {i} error: {e:?}");
            stop_reason = "step error";
            break;
        }
        if let Some(c) = census.as_mut() {
            c.post_step(&bus);
        }
        if i & 0xFFF == 0 && bus.irq().raise_counts()[0] >= stop_vblank {
            stop_reason = "stop vblank";
            break;
        }
        // Pump the SPU every step, as the frontend's headless loop does.
        // A coarse pump starves CD-XA/SPU timing and stalls FMV intros
        // (seen on Gran Turismo 2 and Resident Evil 3).
        bus.run_spu_to_current_cycle();
        if bus.spu.audio_queue_len() != 0 {
            let _ = bus.spu.drain_audio();
        }

        // After each step, check whether a new CDROM command
        // got dispatched. If so, log its last-seen setloc so we
        // know which LBA the BIOS is targeting.
        let c = bus.cdrom.commands_dispatched();
        if c != last_cdrom_cmd_count {
            last_cdrom_cmd_count = c;
            let op = bus.cdrom.last_command();
            if op == 0x02 || op == 0x15 || op == 0x06 {
                // SetLoc / SeekL / ReadN -- snapshot current MSF.
                let (m, s, f) = bus.cdrom.debug_setloc_msf();
                let lba = psx_iso::msf_to_lba(m, s, f);
                let tag = match op {
                    0x02 => "SetLoc",
                    0x15 => "SeekL",
                    0x06 => "ReadN",
                    _ => "?",
                };
                cdrom_setloc_log.push((i, m, s, f, lba));
                if tag == "ReadN" {
                    // Don't blow up the log for one run.
                    if cdrom_setloc_log.len() < 100 {
                        eprintln!(
                            "[probe] {tag} step={i} MSF={:02x}:{:02x}:{:02x} → LBA {}",
                            m, s, f, lba
                        );
                    }
                }
            }
        }
    }

    if let Some(c) = census.as_ref() {
        c.finish(&cpu, &mut bus, stop_reason);
    }

    let labels = ["A", "B", "C"];
    println!("=== BIOS syscall histogram @ step {} ===", cpu.tick());
    println!("cycles: {}", bus.cycles());
    println!("final pc: 0x{:08x}", cpu.pc());
    println!();
    for (t, table) in hist.iter().enumerate() {
        let mut pairs: Vec<(u8, u64)> = table
            .iter()
            .enumerate()
            .filter(|(_, &c)| c > 0)
            .map(|(i, &c)| (i as u8, c))
            .collect();
        pairs.sort_by_key(|&(_, c)| std::cmp::Reverse(c));
        let total: u64 = pairs.iter().map(|(_, c)| c).sum();
        println!("{}-functions ({total} calls):", labels[t]);
        for (op, c) in pairs.iter().take(20) {
            let name = function_name(t as u8, *op);
            println!(
                "  {:>3}%  {:>9}× {}(0x{:02X}) {}",
                100 * c / total.max(1),
                c,
                labels[t],
                op,
                name
            );
        }
        println!();
    }

    if !putchar_log.is_empty() {
        println!("=== BIOS stdout ({} bytes) ===", putchar_log.len());
        // Print each line so newlines / error messages are readable.
        for line in putchar_log.lines() {
            println!("  | {line}");
        }
        println!();
    }

    println!("=== Last 32 syscalls ===");
    for (step, table, fn_no, ra) in recent.iter().rev().take(32) {
        let name = function_name(*table, *fn_no);
        println!(
            "  step {step:>10}  {}(0x{:02X}) {} ra=0x{:08x}",
            labels[*table as usize], fn_no, name, ra
        );
    }
}

/// Standard BIOS function names; see `emulator_core::bios_names` for their
/// provenance.
fn function_name(table: u8, fn_no: u8) -> &'static str {
    census::function_name(table, fn_no as u32)
}
