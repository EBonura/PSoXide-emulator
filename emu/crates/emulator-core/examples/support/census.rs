//! BIOS usage census for `bios_syscall_probe` (opt-in via
//! `PSOXIDE_CENSUS_OUT=<dir>`).
//!
//! Records, split into a boot phase (before the disc's boot EXE entry
//! point first executes) and a game phase (from then on):
//!
//! * every arrival at the A0/B0/C0 dispatch vectors (any KUSEG/KSEG0/KSEG1
//!   mirror), keyed by table and `$t1`, with caller class (ROM, kernel RAM,
//!   or user code, classified from `$ra`), user call sites, a few distinct
//!   argument tuples, and the `$v0` observed when execution first comes back
//!   to `$ra`;
//! * every arrival at the general exception vector (phys `0x80`) and the
//!   boot vector (phys `0x1FC0_0180`), with Cause.ExcCode, the pending and
//!   enabled IRQ sources for interrupts, and `$a0` for SYSCALL;
//! * every CPU load/store to kernel RAM (phys `0x0000..0xFFFF` and its
//!   2 MiB mirrors) executed from a user PC (not ROM, not kernel RAM),
//!   aggregated per 32-bit word.
//!
//! At the EXE entry it also writes a boot-state snapshot (GPRs, COP0, I_STAT,
//! I_MASK, GPUSTAT, SPU register bank, DMA control, CD drive status) and the
//! 64 KiB kernel RAM image.
//!
//! PRIVATE OUTPUT. The census needs a real BIOS, so `kernel_ram_*.bin` hold
//! BIOS-written bytes and the frame dumps hold game imagery. Keep the output
//! directory local and never commit or publish it; only derived facts
//! (function numbers, counts, register values) may leave it.

#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::PathBuf;

use emulator_core::{Bus, Cpu};

pub use emulator_core::bios_names::function_name;

const KERNEL_RAM_END: u32 = 0x1_0000;
const CALLER_ROM: usize = 0;
const CALLER_KERNEL: usize = 1;
const CALLER_USER: usize = 2;

fn phys(addr: u32) -> u32 {
    addr & 0x1FFF_FFFF
}

/// 0 = ROM, 1 = kernel RAM, 2 = user (RAM >= 64 KiB, scratchpad, other).
fn class_of(addr: u32) -> usize {
    let p = phys(addr);
    if (0x1FC0_0000..0x1FC8_0000).contains(&p) {
        CALLER_ROM
    } else if p < 0x80_0000 && (p & 0x1F_FFFF) < KERNEL_RAM_END {
        CALLER_KERNEL
    } else {
        CALLER_USER
    }
}

#[derive(Default)]
struct CallStat {
    count: u64,
    by_caller: [u64; 3],
    returned: u64,
    v0s: Vec<u32>,
    args: Vec<[u32; 4]>,
    user_sites: BTreeMap<u32, u64>,
    first_tick: u64,
}

#[derive(Default)]
struct ExcStat {
    vector_entries: u64,
    bev_entries: u64,
    by_code: [u64; 32],
    irq_sources: [u64; 11],
    irq_nothing_pending: u64,
    syscalls: BTreeMap<u32, u64>,
    epc_class: [u64; 3],
}

#[derive(Default, Clone)]
struct KAccess {
    reads: u64,
    writes: u64,
    pcs: Vec<u32>,
}

#[derive(Default)]
struct Phase {
    calls: BTreeMap<(u8, u32), CallStat>,
    exc: ExcStat,
    kram: BTreeMap<u32, KAccess>,
    isolated_cache_stores: u64,
    steps: u64,
}

struct Pending {
    ra_phys: u32,
    key: (u8, u32),
    phase: usize,
}

pub struct Census {
    out: PathBuf,
    entry_phys: Option<u32>,
    entry_word: u32,
    entry_info: String,
    entry_tick: Option<u64>,
    entry_vblank: Option<u64>,
    phases: [Phase; 2],
    pending: Vec<Pending>,
    frame_every: u64,
    next_frame: u64,
}

impl Census {
    pub fn from_env() -> Option<Self> {
        let out = PathBuf::from(std::env::var_os("PSOXIDE_CENSUS_OUT")?);
        std::fs::create_dir_all(&out).expect("census out dir");
        let frame_every = std::env::var("PSOXIDE_CENSUS_FRAME_EVERY")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        Some(Self {
            out,
            entry_phys: None,
            entry_word: 0,
            entry_info: String::new(),
            entry_tick: None,
            entry_vblank: None,
            phases: [Phase::default(), Phase::default()],
            pending: Vec::new(),
            frame_every,
            next_frame: frame_every,
        })
    }

    /// Remember the boot EXE entry so the phase split can fire.
    pub fn set_boot_exe(&mut self, disc: &psx_iso::Disc) {
        match emulator_core::system_cnf::load_disc_boot(disc) {
            Ok(boot) => {
                let exe = &boot.exe;
                let off = exe.initial_pc.wrapping_sub(exe.load_addr) as usize;
                self.entry_word = exe
                    .payload
                    .get(off..off + 4)
                    .map(|b| u32::from_le_bytes(b.try_into().unwrap()))
                    .unwrap_or(0);
                self.entry_phys = Some(phys(exe.initial_pc));
                self.entry_info = format!(
                    "\"boot_path\":\"{}\",\"initial_pc\":\"0x{:08x}\",\"initial_gp\":\"0x{:08x}\",\
                     \"load_addr\":\"0x{:08x}\",\"payload_len\":{},\"sp_base\":\"0x{:08x}\",\
                     \"sp_offset\":\"0x{:08x}\",\"cnf_stack\":\"0x{:08x}\",\"bss_addr\":\"0x{:08x}\",\"bss_size\":{}",
                    boot.cnf.boot_path.replace('\\', "\\\\"),
                    exe.initial_pc,
                    exe.initial_gp,
                    exe.load_addr,
                    exe.payload.len(),
                    exe.initial_sp_base,
                    exe.initial_sp_offset,
                    boot.cnf.stack,
                    exe.bss_addr,
                    exe.bss_size,
                );
                eprintln!(
                    "[census] boot exe {} entry 0x{:08x}",
                    boot.cnf.boot_path, exe.initial_pc
                );
            }
            Err(e) => {
                eprintln!("[census] no boot exe: {e:?}");
                self.entry_info = format!("\"boot_error\":\"{e:?}\"");
            }
        }
    }

    fn phase(&self) -> usize {
        usize::from(self.entry_tick.is_some())
    }

    /// Call before every `cpu.step`.
    #[inline]
    pub fn pre_step(&mut self, cpu: &Cpu, bus: &mut Bus) {
        let pc = cpu.pc();
        let p = phys(pc);

        if self.entry_tick.is_none()
            && Some(p) == self.entry_phys
            && bus.peek_instruction(pc) == Some(self.entry_word)
        {
            self.entry_tick = Some(cpu.tick());
            self.entry_vblank = Some(bus.irq().raise_counts()[0]);
            self.snapshot(cpu, bus);
        }
        let ph = self.phase();
        self.phases[ph].steps += 1;

        // Returns: first arrival back at a pending call's $ra.
        if !self.pending.is_empty() {
            let n = self.pending.len();
            let lo = n.saturating_sub(4);
            if let Some(i) = (lo..n).rev().find(|&i| self.pending[i].ra_phys == p) {
                let v0 = cpu.gpr(2);
                let done = self.pending.split_off(i);
                let hit = &done[0];
                if let Some(stat) = self.phases[hit.phase].calls.get_mut(&hit.key) {
                    stat.returned += 1;
                    if stat.v0s.len() < 8 && !stat.v0s.contains(&v0) {
                        stat.v0s.push(v0);
                    }
                }
            }
        }

        match p {
            0xA0 | 0xB0 | 0xC0 => {
                let table = ((p - 0xA0) >> 4) as u8;
                let func = cpu.gpr(9);
                let ra = cpu.gpr(31);
                let caller = class_of(ra);
                let args = [cpu.gpr(4), cpu.gpr(5), cpu.gpr(6), cpu.gpr(7)];
                let tick = cpu.tick();
                let stat = self.phases[ph].calls.entry((table, func)).or_default();
                if stat.count == 0 {
                    stat.first_tick = tick;
                }
                stat.count += 1;
                stat.by_caller[caller] += 1;
                if stat.args.len() < 4 && !stat.args.contains(&args) {
                    stat.args.push(args);
                }
                if caller == CALLER_USER
                    && (stat.user_sites.len() < 24 || stat.user_sites.contains_key(&ra))
                {
                    *stat.user_sites.entry(ra).or_insert(0) += 1;
                }
                if self.pending.len() >= 64 {
                    self.pending.drain(..32);
                }
                self.pending.push(Pending {
                    ra_phys: phys(ra),
                    key: (table, func),
                    phase: ph,
                });
            }
            0x80 | 0x1FC0_0180 => {
                let cop0 = cpu.cop0();
                let cause = cop0[13];
                let epc = cop0[14];
                let code = ((cause >> 2) & 0x1F) as usize;
                let exc = &mut self.phases[ph].exc;
                if p == 0x80 {
                    exc.vector_entries += 1;
                } else {
                    exc.bev_entries += 1;
                }
                exc.by_code[code] += 1;
                exc.epc_class[class_of(epc)] += 1;
                if code == 0 {
                    let live = bus.irq().stat() & bus.irq().mask() & 0x7FF;
                    if live == 0 {
                        exc.irq_nothing_pending += 1;
                    }
                    for bit in 0..11 {
                        if live & (1 << bit) != 0 {
                            exc.irq_sources[bit] += 1;
                        }
                    }
                } else if code == 8 {
                    *exc.syscalls.entry(cpu.gpr(4)).or_insert(0) += 1;
                }
            }
            _ => {}
        }

        // Kernel-RAM loads/stores from user code.
        if class_of(pc) == CALLER_USER {
            if let Some(word) = bus.peek_instruction(pc) {
                let op = word >> 26;
                let is_load = matches!(op, 0x20..=0x26 | 0x32);
                let is_store = matches!(op, 0x28..=0x2B | 0x2E | 0x3A);
                if is_load || is_store {
                    let rs = ((word >> 21) & 31) as u8;
                    let ea = cpu.gpr(rs).wrapping_add(word as u16 as i16 as i32 as u32);
                    let ep = phys(ea);
                    let isolated = cpu.cop0()[12] & (1 << 16) != 0;
                    if isolated {
                        // SR.IsC: the access hits the i-cache, not RAM
                        // (cache invalidation loop). Count, don't map.
                        self.phases[ph].isolated_cache_stores += 1;
                    } else if ep < 0x80_0000 && (ep & 0x1F_FFFF) < KERNEL_RAM_END {
                        let a = &mut self.phases[ph]
                            .kram
                            .entry((ep & 0x1F_FFFF) & !3)
                            .or_default();
                        if is_store {
                            a.writes += 1;
                        } else {
                            a.reads += 1;
                        }
                        if a.pcs.len() < 4 && !a.pcs.contains(&pc) {
                            a.pcs.push(pc);
                        }
                    }
                }
            }
        }
    }

    /// Call after every `cpu.step` (cheap; handles periodic frame dumps).
    pub fn post_step(&mut self, bus: &Bus) {
        if self.frame_every == 0 {
            return;
        }
        let vb = bus.irq().raise_counts()[0];
        if vb >= self.next_frame {
            self.next_frame = vb + self.frame_every;
            self.dump_frame(bus, &format!("frame_{vb:06}.ppm"));
        }
    }

    fn dump_frame(&self, bus: &Bus, name: &str) {
        let (rgba, w, h) = bus.gpu.display_rgba8();
        if w == 0 || h == 0 {
            return;
        }
        let mut ppm = format!("P6\n{w} {h}\n255\n").into_bytes();
        for px in rgba.chunks_exact(4) {
            ppm.extend_from_slice(&px[..3]);
        }
        let _ = std::fs::write(self.out.join(name), ppm);
    }

    fn snapshot(&self, cpu: &Cpu, bus: &mut Bus) {
        let mut s = String::from("{\n");
        let _ = writeln!(s, "  \"tick\": {},", cpu.tick());
        let _ = writeln!(s, "  \"bus_cycles\": {},", bus.cycles());
        let _ = writeln!(s, "  \"vblanks\": {},", bus.irq().raise_counts()[0]);
        let _ = writeln!(s, "  \"pc\": \"0x{:08x}\",", cpu.pc());
        let g: Vec<String> = cpu
            .gprs()
            .iter()
            .map(|v| format!("\"0x{v:08x}\""))
            .collect();
        let _ = writeln!(s, "  \"gprs\": [{}],", g.join(","));
        let _ = writeln!(
            s,
            "  \"hi\": \"0x{:08x}\", \"lo\": \"0x{:08x}\",",
            cpu.hi(),
            cpu.lo()
        );
        let c = cpu.cop0();
        let _ = writeln!(
            s,
            "  \"cop0\": {{\"sr\":\"0x{:08x}\",\"cause\":\"0x{:08x}\",\"epc\":\"0x{:08x}\",\"badvaddr\":\"0x{:08x}\",\"dcic\":\"0x{:08x}\"}},",
            c[12], c[13], c[14], c[8], c[7]
        );
        let _ = writeln!(
            s,
            "  \"i_stat\": \"0x{:04x}\", \"i_mask\": \"0x{:04x}\",",
            bus.irq().stat(),
            bus.irq().mask()
        );
        // Observational: read32_at only applies GP1 status updates that
        // are already due at the current cycle.
        let now = bus.cycles();
        let gpustat = bus
            .gpu
            .read32_at(emulator_core::gpu::GP1_ADDR, now)
            .unwrap_or(0);
        let _ = writeln!(s, "  \"gpustat\": \"0x{gpustat:08x}\",");
        let _ = writeln!(
            s,
            "  \"spucnt\": \"0x{:04x}\", \"spustat\": \"0x{:04x}\",",
            bus.spu.spucnt(),
            bus.spu.spustat()
        );
        let spu: Vec<String> = (0..0x100u32)
            .map(|i| format!("\"{:04x}\"", bus.spu.read16(0x1F80_1C00 + i * 2)))
            .collect();
        let _ = writeln!(s, "  \"spu_regs_1f801c00\": [{}],", spu.join(","));
        let _ = writeln!(
            s,
            "  \"dma\": {{\"dpcr\":\"0x{:08x}\",\"dicr\":\"0x{:08x}\"}},",
            bus.debug_dma_read32(0x1F80_10F0),
            bus.debug_dma_read32(0x1F80_10F4)
        );
        let _ = writeln!(
            s,
            "  \"cd\": {{\"stat\":\"0x{:02x}\",\"mode\":\"0x{:02x}\",\"read_lba\":{}}},",
            bus.cdrom.debug_stat_byte(),
            bus.cdrom.debug_mode(),
            bus.cdrom.debug_read_lba()
        );
        let mem_ctrl: Vec<String> = (0..9u32)
            .map(|i| {
                let a = 0x1F80_1000 + i * 4;
                let w = (0..4)
                    .map(|k| bus.try_read8(a + k).unwrap_or(0) as u32)
                    .enumerate()
                    .fold(0u32, |acc, (k, b)| acc | (b << (8 * k)));
                format!("\"0x{w:08x}\"")
            })
            .collect();
        let _ = writeln!(
            s,
            "  \"mem_ctrl_1f801000_io_shadow\": [{}],",
            mem_ctrl.join(",")
        );
        let ram_size = (0..4)
            .map(|k| bus.try_read8(0x1F80_1060 + k).unwrap_or(0) as u32)
            .enumerate()
            .fold(0u32, |acc, (k, b)| acc | (b << (8 * k)));
        let _ = writeln!(s, "  \"ram_size_1f801060_io_shadow\": \"0x{ram_size:08x}\"");
        s.push_str("}\n");
        let _ = std::fs::write(self.out.join("entry_snapshot.json"), s);
        let kram: Vec<u8> = bus.ram()[..KERNEL_RAM_END as usize].to_vec();
        let _ = std::fs::write(self.out.join("kernel_ram_at_entry.bin"), kram);
        self.dump_frame(bus, "frame_at_entry.ppm");
        eprintln!("[census] EXE entry reached at tick {}", cpu.tick());
    }

    pub fn finish(&self, cpu: &Cpu, bus: &mut Bus, stop_reason: &str) {
        self.dump_frame(bus, "frame_final.ppm");
        let _ = std::fs::write(
            self.out.join("kernel_ram_final.bin"),
            &bus.ram()[..KERNEL_RAM_END as usize],
        );
        let memcard_cmds: u64 = bus
            .port1_memcard_command_histogram()
            .map(|h| h.iter().map(|&c| c as u64).sum())
            .unwrap_or(0);
        let mut s = String::from("{\n");
        let _ = writeln!(s, "  \"stop_reason\": \"{stop_reason}\",");
        let _ = writeln!(s, "  \"final_tick\": {},", cpu.tick());
        let _ = writeln!(s, "  \"final_vblanks\": {},", bus.irq().raise_counts()[0]);
        let _ = writeln!(s, "  \"final_pc\": \"0x{:08x}\",", cpu.pc());
        let _ = writeln!(s, "  \"entry\": {{{}}},", self.entry_info);
        let _ = writeln!(
            s,
            "  \"entry_tick\": {}, \"entry_vblank\": {},",
            self.entry_tick
                .map(|t| t.to_string())
                .unwrap_or("null".into()),
            self.entry_vblank
                .map(|t| t.to_string())
                .unwrap_or("null".into())
        );
        let _ = writeln!(s, "  \"memcard_port1_commands\": {memcard_cmds},");
        s.push_str("  \"phases\": {\n");
        for (pi, name) in ["boot", "game"].iter().enumerate() {
            let ph = &self.phases[pi];
            let _ = writeln!(s, "    \"{name}\": {{");
            let _ = writeln!(s, "      \"steps\": {},", ph.steps);
            let _ = writeln!(
                s,
                "      \"isolated_cache_stores\": {},",
                ph.isolated_cache_stores
            );
            s.push_str("      \"calls\": [\n");
            let mut first = true;
            for ((t, f), st) in &ph.calls {
                if !first {
                    s.push_str(",\n");
                }
                first = false;
                let v0s: Vec<String> = st.v0s.iter().map(|v| format!("\"0x{v:x}\"")).collect();
                let args: Vec<String> = st
                    .args
                    .iter()
                    .map(|a| {
                        format!(
                            "[\"0x{:x}\",\"0x{:x}\",\"0x{:x}\",\"0x{:x}\"]",
                            a[0], a[1], a[2], a[3]
                        )
                    })
                    .collect();
                let mut sites: Vec<(&u32, &u64)> = st.user_sites.iter().collect();
                sites.sort_by_key(|(_, c)| std::cmp::Reverse(**c));
                let sites: Vec<String> = sites
                    .iter()
                    .take(12)
                    .map(|(ra, c)| format!("[\"0x{ra:08x}\",{c}]"))
                    .collect();
                let _ = write!(
                    s,
                    "        {{\"table\":\"{}\",\"func\":\"0x{:02x}\",\"name\":\"{}\",\"count\":{},\
                     \"from_rom\":{},\"from_kernel\":{},\"from_user\":{},\"returned\":{},\
                     \"first_tick\":{},\"v0\":[{}],\"args\":[{}],\"user_sites\":[{}]}}",
                    ["A", "B", "C"][*t as usize],
                    f,
                    function_name(*t, *f),
                    st.count,
                    st.by_caller[0],
                    st.by_caller[1],
                    st.by_caller[2],
                    st.returned,
                    st.first_tick,
                    v0s.join(","),
                    args.join(","),
                    sites.join(",")
                );
            }
            s.push_str("\n      ],\n");
            let e = &ph.exc;
            let codes: Vec<String> = e
                .by_code
                .iter()
                .enumerate()
                .filter(|(_, c)| **c > 0)
                .map(|(i, c)| format!("\"{i}\":{c}"))
                .collect();
            let irqs: Vec<String> = e.irq_sources.iter().map(|c| c.to_string()).collect();
            let sys: Vec<String> = e
                .syscalls
                .iter()
                .map(|(k, v)| format!("\"0x{k:x}\":{v}"))
                .collect();
            let _ = writeln!(
                s,
                "      \"exceptions\": {{\"vector_80\":{},\"vector_bfc00180\":{},\"by_exccode\":{{{}}},\
                 \"irq_sources_vblank_gpu_cd_dma_t0_t1_t2_sio0_sio1_spu_lp\":[{}],\"irq_nothing_pending\":{},\
                 \"syscall_a0\":{{{}}},\"epc_rom_kernel_user\":[{},{},{}]}},",
                e.vector_entries,
                e.bev_entries,
                codes.join(","),
                irqs.join(","),
                e.irq_nothing_pending,
                sys.join(","),
                e.epc_class[0],
                e.epc_class[1],
                e.epc_class[2]
            );
            s.push_str("      \"kernel_ram_user_access\": [\n");
            let mut first = true;
            for (addr, a) in &ph.kram {
                if !first {
                    s.push_str(",\n");
                }
                first = false;
                let pcs: Vec<String> = a.pcs.iter().map(|p| format!("\"0x{p:08x}\"")).collect();
                let _ = write!(
                    s,
                    "        {{\"addr\":\"0x{addr:04x}\",\"reads\":{},\"writes\":{},\"pcs\":[{}]}}",
                    a.reads,
                    a.writes,
                    pcs.join(",")
                );
            }
            s.push_str("\n      ]\n");
            let _ = writeln!(s, "    }}{}", if pi == 0 { "," } else { "" });
        }
        s.push_str("  }\n}\n");
        let _ = std::fs::write(self.out.join("census.json"), s);
        eprintln!("[census] wrote {}", self.out.join("census.json").display());
    }
}
