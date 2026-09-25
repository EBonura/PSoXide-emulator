# HLE BIOS provenance

PSoXide replaces the PlayStation BIOS with a high-level emulation (HLE) of its
kernel services, written in Rust. This file records where each piece of that
behaviour comes from, so the implementation stays independent of Sony's code.
Update it in the same commit as any HLE change.

## Rules

- No Sony code or data in the repository: no BIOS bytes, disassembly,
  fonts, logos, boot sounds or kernel RAM images, including test fixtures.
- Behaviour is written from public documentation (psx-spx), from the MIT
  OpenBIOS used as a specification (not ported line by line, and never its
  `patches/*.c` disassembly comments), and from black-box observation of a
  real BIOS: register and memory values, call arguments and return values,
  timing. Only those derived facts are committed.
- A real BIOS dump is used only by private development tooling, supplied by
  the developer from their own console, and read from an explicit path:
  `PSOXIDE_BIOS` for the census probe and `PSOXIDE_PARITY_BIOS` for
  `hle_compat --parity`. Outputs of those runs stay local.
- Interface facts (table numbers, function names, RAM addresses, register
  layouts) are used freely.

## Sources

| Short name | Source | Licence |
|---|---|---|
| psx-spx | nocash psx-spx "Kernel (BIOS)" and "CDROM File Formats" chapters | facts only; prose is not copied |
| OpenBIOS | pcsx-redux `src/mips/openbios` | MIT, Copyright (c) PCSX-Redux authors |
| kernellog | pcsx-redux `src/core/kernellog.cc` name tables | GPL-2.0-or-later |
| census | BIOS usage census of 2026-09-24: `bios_syscall_probe` with `PSOXIDE_CENSUS_OUT` under SCPH1001 on 31 discs (38 runs), plus `tools/kcall_scan.py` | measured facts |

## Subsystems

| Area | Code | Source |
|---|---|---|
| A/B/C slot numbering | `hle_bios.rs` `run` | OpenBIOS `kernel/handlers.c` `romA0table`, `B0table`, `C0table` and `patchA0table` aliases; psx-spx function summary |
| memcpy, bcopy, memset, bzero, memchr | `hle_bios.rs` `libc` | psx-spx "BIOS Memory Fill/Copy/Compare": null and length refusals, return values |
| strcat, strcmp, strncmp, strcpy, strncpy, strlen, index/rindex/strchr/strrchr, toupper, tolower, abs, labs | `hle_bios.rs` `libc` | psx-spx "BIOS String Functions" and "Number/String/Character Conversion" |
| puts, puts(NULL) | `hle_bios.rs` | psx-spx A(3Eh)/B(3Fh) |
| SetMem | `hle_bios.rs` `set_mem_size` | psx-spx A(9Fh); OpenBIOS `kernel/misc.c` `setMemSize` |
| Function names | `bios_names.rs` | kernellog, generated mechanically |
| Loud and strict unimplemented calls | `hle_bios.rs`, `bus.rs`, `cpu.rs` | original |
| SYSTEM.CNF keys, defaults, BOOT argument at 0x180, PSX.EXE fallback | `system_cnf.rs` | psx-spx "CDROM File Playstation EXE and SYSTEM.CNF" |
| STACK read as leading hex digits (`0x` prefix gives 0) and the caller stack then kept (sp 801FFDD8h, fp 801FFF00h) | `system_cnf.rs` | census: measured at EXE entry on Metal Gear Solid, Resident Evil 2 and Resident Evil 3; psx-spx for "SP 0 keeps the caller's stack" |
| EXE entry registers (a0=1, a1=0, gp from the header, sp=fp=STACK) | `fastboot.rs` | census, all runs |
| Entry hardware state on the HLE path: DPCR 9099h, DICR 8C8C0000h, I_MASK 000Ch, [0x60]=2, SPU shell profile with main volume 3FFFh/37EFh, CD volume 0, transfer control 0004h, GP1(08h) 640x480i NTSC, GP0(E1h) dither and draw-to-display | `fastboot.rs` `apply_hle_entry_state`, `spu.rs` | census, constant across all 38 runs |
| SPU shell reverb profile | `spu.rs` `apply_retail_bios_shell_audio_profile` | PA5 real-console capture (predates this file) |
| Kernel RAM layout: table of tables at 100h, A0/B0/C0 tables at 200h/874h/674h, C(06h) at C80h, B(5Bh) at 43D0h, FCB 8648h, DCB 6EE0h, kernel heap A000E000h/2000h with ExCB, EvCB, PCB, TCB allocated in that order with 4-byte size headers | `hle_kernel.rs` | psx-spx "BIOS Memory Map" and "Table of Tables"; census: addresses and header words at EXE entry (values only, no code) |
| Trap stubs (BREAK with code B00xxh) and HLE kernel variables at A00h | `hle_kernel.rs` | original |
| InitHeap, malloc, free, calloc, realloc, alloc/free_kernel_memory, SysInitMemory, SetConf, GetConf | `hle_kernel.rs`, `hle_bios.rs` | psx-spx "BIOS Memory Allocation" and A(9Ch)/A(9Dh); allocator algorithm original |
| B(56h)/B(57h) patch recognition: hash, masks, variant table, counterpatch branch lengths and pointer word offsets | `hle_kernel.rs` | OpenBIOS `patches/` (MIT); psx-spx "BIOS Patches" for what each routine does |
| setjmp, longjmp, HookEntryInt r2=1 | `hle_bios.rs`, `cpu.rs` | psx-spx A(13h)/A(14h), B(19h) |
| ChangeClearPAD B(5Bh) returns previous value; _96_remove | `hle_bios.rs` | psx-spx; OpenBIOS `sio0/driver.c` setSIO0AutoAck, `cdrom/cdrom.c` deinitCDRom |
| strtol, strtoul, atoi, atol, atob, rand, srand | `hle_bios.rs` | psx-spx "Number/String/Character Conversion" and "Misc Functions" |
| SendGP1Command, GPU_cw, GPU_cwp, GetGPUStatus, gpu_sync | `hle_bios.rs` | psx-spx "BIOS GPU Functions" |
| Exception vector at 80h and its copy at 0 (first word 3) | `hle_exceptions.rs` | psx-spx "Garbage Area" and C(06h); OpenBIOS `vectors.s` notes on games that read these words |
| Exception handler at C80h: register save to the current TCB, GTE EPC adjust, patch-slot layout, four ExCB priority chains (verifier then handler), longjmp to the exit buffer with r2=1 | `hle_exceptions.rs` | psx-spx "BIOS Interrupt/Exception Handling"; layout and protocol from OpenBIOS `kernel/vectors.s` (MIT) |
| ReturnFromException (k0 not restored, k1 restored last) | `hle_exceptions.rs` | psx-spx B(17h); OpenBIOS `returnFromException` |
| Events: OpenEvent, CloseEvent, WaitEvent, TestEvent, Enable/DisableEvent, DeliverEvent, UnDeliverEvent, EvCB layout, status and mode values, classes | `hle_exceptions.rs`, `hle_bios.rs` | psx-spx "BIOS Event Functions"; OpenBIOS `kernel/events.c` |
| Default chain elements: SYSCALL handler (prio 0), root counters T0-T2 and VBlank (prio 1), default IRQ handler (prio 3); ChangeClearRCnt, SetIrqAutoAck, SysEnqIntRP, SysDeqIntRP (searches the whole chain, unlike the documented retail bug) | `hle_exceptions.rs` | psx-spx "Priority Chains", C(0Ah), C(0Dh), C(02h)/C(03h); OpenBIOS `handlers/irq.c`, `setup.c`, `syscall.c` |
| SYSCALL 0-3 and the unknown-syscall / unresolved-exception events | `hle_exceptions.rs` | psx-spx SYS(01h)-(03h), "Unresolved Exception Events"; OpenBIOS `syscallVerifier` |
| Threads: OpenTh (SR left as is), CloseTh, ChangeTh via SYSCALL(3) | `hle_exceptions.rs`, `hle_bios.rs` | psx-spx "BIOS Thread Functions"; OpenBIOS `kernel/threads.c` |
| HookEntryInt, ResetEntryInt default exit buffer (ReturnFromException, exception stack top minus 4) | `hle_exceptions.rs` | psx-spx B(18h)/B(19h) |
| Unresolved-exception hook for side-loaded homebrew, FlushCache | `hle_bios.rs`, `cpu.rs` | predates this file; to be re-derived from psx-spx and OpenBIOS when the kernel model replaces them |
| Timer helpers B(02h)-B(06h) | `hle_exceptions.rs` | psx-spx "BIOS Timer Functions" (where OpenBIOS disagrees on the init_timer clock bit, psx-spx is followed) |
| Exec A(43h) as guest code: saved registers in the header, memfill, stack, call with (a1, a2), return 1 | `hle_exceptions.rs` | psx-spx A(43h); register protocol from OpenBIOS `kernel/psxexec.s` (MIT) |
| File layer: FCB/DCB layout, open, lseek, read, write, close, AddDevice, RemoveDevice, _get_errno, _get_error, firstfile, nextfile, format, erase, rename, undelete, error numbers, the unmarked firstfile search FCB | `hle_files.rs` | psx-spx "BIOS File Functions" and "BIOS Control Blocks"; call protocol between file functions and drivers from OpenBIOS `fileio/` (MIT) |
| TTY and CD-ROM devices, kernel CD-ROM driver (SetMode, Setloc, ReadN, DMA per sector, Pause), kernel CD IRQ handlers and events, LoadTest, Load, LoadExec | `hle_files.rs`, `hle_bios.rs` | psx-spx "BIOS CDROM Functions" and "BIOS File Execute"; OpenBIOS `cdrom/` (MIT) |
| Pad driver: InitPAD2, StartPAD2, StopPAD2, PAD_init2, PAD_dr, the priority-2 VBlank pad/card handler, the reader's byte sequence, delays and ACK timeout, effect of `_patch_pad`, `_remove_ChgclrPAD`, `_send_pad` | `hle_pad.rs` | psx-spx "BIOS Joypad Functions", "Priority Chains" and the pad patch notes; byte sequence and timeouts from OpenBIOS `sio0/driver.c` and `sio0/pad.c` (MIT). Delay and timeout lengths in cycles are estimates until measured (plan P7). |
| Memory card low level: InitCARD2, StartCARD2, StopCARD2, _card_info, _card_read, _card_write, _new_card, _card_chan, _card_status, _card_wait, _card_load, _card_auto, bufs_cb A(A7h)-A(AAh), HwCARD and SwCARD events, command bytes, one command per VBlank with alternating slots, timeout on the next VBlank | `hle_card.rs` | psx-spx "BIOS Memory Card Functions", "Memory Card Read/Write Commands" and the event lists; order of flags, callbacks and events from OpenBIOS `sio0/card.c`, `sio0/driver.c`, `card/backupunit.c` (MIT) |
| Early card IRQ routine in exception handler slot 1 (lui/ori/jalr layout, +28h replaceable, v1 = 1F800000h, continue at +3Ch) | `hle_exceptions.rs`, `hle_card.rs` | psx-spx "early_card_irq_patch" (what games write there); structure from OpenBIOS `sio0/cardfasttrack.s` (MIT) |
| Memory card device "bu": open/create, read, write (sync and async), close, firstfile/nextfile wildcards, erase (A1h/A2h/A3h), rename, format, directory validation, broken-sector reallocation lookup | `hle_bu.rs` | psx-spx "BIOS Memory Card Functions" and "Memory Card Data Format"; write order and error codes from OpenBIOS `card/device.c` and `card/backupunit.c` (MIT). Retail bugs OpenBIOS documents as hangs or corruption are reported as failures instead. |
| GetConf A(9Dh) as guest code starting with lui/lw on the stack word; config words in the order TCBs, EvCBs, stack | `hle_exceptions.rs`, `hle_kernel.rs` | inferred from how Metal Gear Solid decodes the A0[9Dh] entry (the game's own code, read locally for interoperability); no BIOS bytes |
| memmove, memcmp, bcmp with their documented bugs | `hle_bios.rs` | psx-spx "BIOS Memory Fill/Copy/Compare" |
| Krom2RawAdd B(51h) returns -1 for every character | `hle_bios.rs` | psx-spx B(51h); the Kanji font is Sony ROM data and is not shipped |

## Tooling

| Tool | Notes |
|---|---|
| `emulator-core` example `bios_syscall_probe` (census) | Needs a real BIOS. Its output directory holds kernel RAM images (BIOS-written bytes) and game frames: private, never committed. |
| `tools/kcall_scan.py` | Static scan of a disc image. Emits facts only (offsets, function numbers, hashes). Patch signature hash, masks and known-variant values from OpenBIOS `patches` (MIT). |
| `compat/games.toml` | Facts only: titles, serials, regions, sha256 of the disc image and boot executable, BIOS functions and patch routines per game. |
| `emulator-core` example `hle_compat` | Finds the developer's discs by hash, runs them under HLE with a formatted empty memory card and no BIOS. `--parity` (dev-only) additionally cold-boots a real BIOS from `PSOXIDE_PARITY_BIOS` and diffs the EXE-entry state; nothing from that BIOS is written out except the compared register values. `--reference` (dev-only, same variable) runs the game a second time through that BIOS with the same input and memory card and reports its display hash; with `--shots` its frames are written locally as game imagery, never committed. |

## Not yet done

- The SYSTEM.CNF parser and ISO9660 file lookup live in `emulator-core` for
  now; they should move into `psx-iso` (SDK repository) with a BOOT parser
  that accepts an argument.
- strtok, strstr and strpbrk, undelete on the memory card, LoadExec from
  devices other than the CD-ROM, and the retail broken-sector
  reallocation on write are unimplemented.
- Cycle costs of the kernel routines (pad reader delays, handler
  overhead) are estimates until plan phase P7 measures them.
- The retail initial rand seed is not known; the HLE starts at 0.
