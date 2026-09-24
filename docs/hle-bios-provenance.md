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
| Events always ready, HookEntryInt, unresolved-exception hook, FlushCache | `hle_bios.rs`, `cpu.rs` | predates this file; to be re-derived from psx-spx and OpenBIOS when the kernel model replaces them |

## Tooling

| Tool | Notes |
|---|---|
| `emulator-core` example `bios_syscall_probe` (census) | Needs a real BIOS. Its output directory holds kernel RAM images (BIOS-written bytes) and game frames: private, never committed. |
| `tools/kcall_scan.py` | Static scan of a disc image. Emits facts only (offsets, function numbers, hashes). Patch signature hash, masks and known-variant values from OpenBIOS `patches` (MIT). |
| `compat/games.toml` | Facts only: titles, serials, regions, sha256 of the disc image and boot executable, BIOS functions and patch routines per game. |
| `emulator-core` example `hle_compat` | Finds the developer's discs by hash, runs them under HLE with a formatted empty memory card and no BIOS. `--parity` (dev-only) additionally cold-boots a real BIOS from `PSOXIDE_PARITY_BIOS` and diffs the EXE-entry state; nothing from that BIOS is written out except the compared register values. |

## Not yet done

- The SYSTEM.CNF parser and ISO9660 file lookup live in `emulator-core` for
  now; they should move into `psx-iso` (SDK repository) with a BOOT parser
  that accepts an argument.
- Stateful libc (malloc family, rand, strtok), the functions psx-spx
  documents as buggy (memcmp, bcmp, memmove, strstr, strpbrk), setjmp and
  longjmp, file and device I/O, memory card, CD and pad services, and the
  event, exception and thread model are unimplemented and report loudly.
