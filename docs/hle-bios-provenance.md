# HLE BIOS provenance

PSoXide replaces the PlayStation BIOS with a high-level emulation (HLE) of its
kernel services, written in Rust. This file records where each piece of that
behaviour comes from, so the implementation stays independent of Sony's code
and of every other kernel implementation. Update it in the same commit as any
HLE change.

## Rules

- No Sony code or data in the repository: no BIOS bytes, disassembly, fonts,
  logos, boot sounds or kernel RAM images, including test fixtures.
- No code from any other kernel implementation or SDK in the HLE kernel. It
  is written from public documentation and from measurements. Other
  kernels and SDKs (OpenBIOS, PCSX-Redux, nugget, psyqo, PSn00bSDK) are not
  read while writing it, and nothing is translated from them. (Other parts
  of the emulator, such as the MDEC, state their own references in the
  files concerned.) Code that was once
  derived from one of them has been deleted and written again from the
  sources below (October 2026); the git history shows the rewrites as
  commits that start with "Rewrite".
- Where psx-spx is silent, the choice is made here, written next to the code
  and checked against the games in `compat/games.toml`. A choice that
  came from comparing with an earlier PSoXide kernel's traffic says so.
- PSoXide has no BIOS path at all (removed 2026-09-25). During development a
  real BIOS dump from the developer's own console was read by private tooling
  only (the census probe, and `hle_compat --parity` and `--reference`), and
  only derived facts were kept: the measured values cited below, and the
  display hashes and frame positions in `compat/reference/` and
  `compat/reference.toml`. That tooling is deleted; git history has it.
- Interface facts (table numbers, function names, RAM addresses, register
  layouts) are used freely.

## Sources

| Short name | Source | Use |
|---|---|---|
| psx-spx | nocash psx-spx, "Kernel (BIOS)", "Controllers and Memory Cards", "Serial Interfaces" and "CDROM File Formats" | facts only; the prose is not copied |
| JIS X 0208 | the character chart (rows, cells, assigned ranges) and the Shift-JIS pairing of rows | the font layout |
| census | the BIOS usage census of 2026-09-24 (`bios_syscall_probe` with `PSOXIDE_CENSUS_OUT`), plus the disc scan | measured facts: which functions games call, RAM addresses, entry state |
| hardware tests | the hardware-tests disc run on a console | register timings, pad and SIO behaviour |
| compat discs | the games in `compat/games.toml`, run with `hle_compat` | the check on everything above |

## Subsystems

| Module | What it is | Basis |
|---|---|---|
| `hle_asm.rs` | a small MIPS encoder for the kernel's guest-side routines | none needed; it only knows the instructions the routines use |
| `hle_kernel.rs` | RAM layout, tables, trap words, heaps, libc-style helpers | psx-spx memory map, memory allocation and string chapters; census addresses; the first-fit allocator is PSoXide's own |
| `hle_exceptions.rs` | exception vector and handler, priority chains, events, threads, root counters, SYSCALL | psx-spx "Interrupt/Exception Handling", "Event Functions", "Thread Functions", "Timer Functions", "Priority Chains" |
| `hle_patch.rs` | what a game's kernel patch changes (the routines that follow B(56h) and B(57h)) | psx-spx "BIOS Patches". The routine after the call runs as written and markers record which documented spot a game overwrote; `identify` only names a routine by what it touches, for diagnostics and the disc scanner, and decides nothing |
| `hle_pad.rs` | InitPAD2, StartPAD2, StopPAD2, PAD_init2, PAD_dr and the VBlank reader | psx-spx "BIOS Joypad Functions", the controller communication sequence, SIO0. Waits are stated in byte times at the 88h reload |
| `hle_card.rs` | the low-level memory card driver and the card side of the early IRQ routine | psx-spx "Memory Card Read/Write Commands", "BIOS Memory Card Functions", "BIOS Event Summary", "BIOS Patches" |
| `hle_bu.rs` | `_bu_init`, `_card_load`, `_card_info`, the "bu" file device and the directory cache | psx-spx "Memory Card Data Format", "BIOS Memory Card Functions", "BIOS File Functions" |
| `hle_files.rs` | files and devices, the TTY and CD-ROM devices, the CD driver, executable loading | psx-spx "BIOS File Functions", "BIOS Control Blocks", "BIOS CDROM Functions", "BIOS More Internal Functions" (device names). The CD read state machine and the ISO 9660 lookup are PSoXide's own |
| `hle_font.rs`, `hle_font_glyphs.txt` | Krom2RawAdd and Krom2Offset over a font drawn for PSoXide | psx-spx "BIOS Character Sets" and the JIS X 0208 chart. The glyphs are hand-drawn, none taken from any font |
| `hle_bios.rs` | dispatch, per-call cycle costs, the host printf and libc functions | psx-spx function descriptions. Cycle costs are measured, see below |
| `bios_names.rs` | the name shown for each A, B and C function | psx-spx "BIOS Function Summary" |
| `system_cnf.rs`, `fastboot.rs` | SYSTEM.CNF parsing and the state a game is entered in | psx-spx, and the entry state measured by the census |

### Choices psx-spx leaves open

These were settled by hand and are stated in the code beside them.

- File layer: a device function answers 0 for success from open, close,
  format, erase, rename and undelete, and a byte count or -1 from read and
  write; a failing one leaves the reason in the FCB's error field. lseek
  whence 2 leaves the position alone. A block device does the alignment
  check itself.
- Memory card: slots take alternate frames and a lone slot gets a command
  every other frame (psx-spx says so). The first frame after StartCARD2 is
  slot 2's turn, a card gets two bit times between /CS and its first byte,
  the backup unit's own event goes out before HwCARD's, `_bu_init` writes
  the test frame and `_card_load` does not, firstfile2 reads sector 0 and
  waits for it, and an asynchronous write ends with an info command and
  then the handle's event, SwCARD 4 and HwCARD 4. These follow the card
  traffic of the earlier PSoXide kernel on the compat discs (command order
  exactly, event times to within about 50 cycles).
- Font: a Shift-JIS code with no cell in either charset answers -1; Krom2Offset
  counts in cells.
- ChangeClearPAD sets the pad and card handler's auto-acknowledge flag and
  leaves v0 alone. `_96_init` leaves the interrupt state alone.
- The unpatched card info command sends one byte more than it reads
  (psx-spx); a game that patches that out stops it.

## Tooling

| Tool | Notes |
|---|---|
| `emulator-core` example `bios_syscall_probe` (census) | Deleted with BIOS support. It needed a real BIOS; its output (kernel RAM images, game frames) stayed private and was never committed. |
| `emulator-core` example `kcall_scan` | Static scan of a disc image or executable. Emits facts only: call sites, function numbers, offsets, the patch routines a game installs. Written from psx-spx's function summary and patch descriptions. |
| `compat/games.toml` | Facts only: titles, serials, regions, sha256 of the disc image and boot executable, BIOS functions and patch routines per game. |
| `emulator-core` example `hle_compat` | Finds the developer's discs by hash and runs them on the HLE kernel with a formatted empty memory card. `--hash-log` writes the display hash of every frame. Its `--parity` and `--reference` modes, which booted a real BIOS from `PSOXIDE_PARITY_BIOS` for comparison, are deleted; their last results are recorded as display hashes and descriptions only (`compat/reference/`, `compat/reference.toml`). |
| `compat/reference/`, `compat/reference.toml` | Facts only: display hashes every 60 frames and final-screen descriptions of the last real-BIOS reference runs, next to the HLE run of the same commit. No frames, no BIOS bytes. |

A change to the kernel is equivalent when every compat game keeps its tier
and its per-frame HLE hashes from the run before the change, or the
differences are looked at in before and after frames and explained.

## Not yet done

- The SYSTEM.CNF parser and ISO 9660 file lookup live in `emulator-core` for
  now; they should move into `psx-iso` (SDK repository) with a boot parser
  that accepts an argument.
- strtok, strstr and strpbrk, LoadExec from devices other than the CD-ROM,
  the retail broken-sector reallocation on write, A(AFh) `card_write_test`,
  B(40h) cd and B(59h) testdevice are unimplemented.
- Cycle costs of the kernel routines (pad reader delays, handler overhead,
  every call but TestEvent, memcpy, memset and bzero) are estimates until
  they are measured on a console. In the card unit test that removes the
  early IRQ routine, the kernel's ordinary handler needs about 5,500 cycles
  a byte, so a sector does not finish inside one frame and the command
  times out; the compat games that carry the patch to remove it still run,
  so this is not seen on real discs here, and a console is faster. The
  hardware-tests disc needs a case that times a VBlank round trip and a
  SYSCALL round trip on a console to calibrate this.
- The retail initial rand seed is not known; the HLE starts at 0.
