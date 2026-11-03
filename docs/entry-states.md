# Entry states: booting a disc as another BIOS would leave it

PSoXide has no BIOS. Every disc boots on the built-in HLE kernel, which hands
the program the state a retail Sony BIOS leaves at the executable's first
instruction (docs/hle-bios-provenance.md). A program that quietly depends on
that state therefore passes every gate here and fails on a console whose BIOS
leaves something else: PCSX-Redux's OpenBIOS (a SuperStation One boots it), or
any of the Sony revisions, which differ from each other in ways nobody has
documented in full.

The rule (Manny, 2026-11-03): every PSoXide program works on every Sony BIOS,
v1.0 to v4.5 in all regions, and on OpenBIOS, and depends on no state a BIOS
leaves. Entry states are the test for that. They are opt-in; the default boot is
untouched and stays the reference for goldens.

No BIOS is loaded and no BIOS byte is in the repository. An entry state is a
list of numbers and one seed. The only code it writes to RAM is the exception
handler PSoXide already assembles, at another address.

## Using them

```sh
# Environment variable, honoured by every tool that boots a disc
# (frontend, psoxide-gate, hle_compat, the jit examples):
PSOXIDE_ENTRY_STATE=openbios       psoxide-emulator launch game.cue --headless ...
PSOXIDE_ENTRY_STATE=hostile:42     psoxide-gate run quake

# Flags:
psoxide-emulator --entry-state openbios launch ...     # GUI and every subcommand
psoxide-gate run hl --entry-state hostile:7,skip=gpu   # gate
```

Spec: `NAME[:SEED][,key=value]...`

| Name | State |
|---|---|
| `default` (or unset) | retail-modelled HLE state, as before |
| `openbios` | modelled on what PCSX-Redux OpenBIOS leaves |
| `hostile[:SEED]` | every item below that BIOSes may differ on, randomised from SEED (decimal or `0x` hex, default 1) |

| Key | Meaning |
|---|---|
| `dpcr=`, `dicr=`, `imask=` | replace one register after the mode is built (for example `openbios,dpcr=0x9099` isolates the DMA question) |
| `skip=CATEGORY` | leave a category at the default state; repeatable. Categories: `vector`, `dma`, `irq`, `spu`, `cd`, `ram`, `vram`, `scratch`, `gpu`, `timers`, `cpu`. This is the bisect tool: a program that fails under `hostile:9` and passes with `skip=spu` depends on the SPU state |

A run prints one `[entry-state]` line with the values it used. Under a
non-default state a golden mismatch is a finding about the program, not a
regression; `psoxide-gate bless` refuses to run with one set. The same seed gives
the same state on every host (splitmix64), and every category draws from its own
stream, so skipping one does not move the others.

`emulator_core::fast_boot_disc_with(bus, cpu, disc, &EntryState)` is the API;
`fast_boot_disc` reads `PSOXIDE_ENTRY_STATE`.

## What changes

All values are at the program's first instruction. Anything not listed is the
default state.

| Item | Default (retail-modelled) | `openbios` | `hostile` range |
|---|---|---|---|
| Exception vector (80h and its copy at 0) | `lui k0,0; addiu k0,k0,0C80h; jr k0; nop` | same form, target 5E00h (see below) | one of `lui/addiu/jr`, `lui/ori/jr`, `j`; target one of 7 free blocks (retail 0C80h among them) |
| Handler | at 0C80h; C(06h) says so | at 5E00h; C(06h) says so; 0C80h..0EFFh spins (`b .`) | as the vector; the retail region spins whenever the handler moved |
| DPCR | 9099h | 076FC321h | each channel: priority 0..7, enable on or off |
| DICR | 8C8C0000h | 00880000h | enables, master enable and flags random (master flag computed) |
| I_MASK | 000Ch | 000Ch (assumed) | any of the 11 bits |
| SPU main volume | 3FFFh / 37EFh | 3800h / 3800h | 0..3FFFh each |
| SPUCNT | C085h | C000h | enable and unmute random, CD, external and reverb routing bits random, transfer stopped |
| SPU CD / external volume | 0 | 0 | 0 or random |
| SPU reverb (volume, base, EON, 32 config words) | the shell's preset | power-on zeros | random |
| SPU transfer control, address | 0004h, 0 | 0004h, 0 | 0 or 4; random |
| CD mode byte | 80h | 00h | random byte |
| CD INT enable | 1Fh | 1Fh | random 5 bits |
| CD head | sector after the executable | same | random sector on the disc |
| RAM from 80010000h up | zero up to the stack, then power-on zeros | AAh (the `PSOXIDE_POISON_MEM` pattern; the variable's own byte if set), bss cleared by the loader as always | random bytes |
| Scratchpad | power-on zeros | same | random bytes |
| VRAM | power-on zeros | same | random halfwords |
| GPU | 640x480 interlaced NTSC 15-bit, display on, draw mode E1h = 600h | same | display mode (width, height, depth, interlace; never PAL), display on or off, display start, E1h..E5h words random |
| Root counters | power-on | same | counter, target, reset-on-target and clock source; never a sync mode or an interrupt enable |
| General registers | the loader's, others zero | same | every register except `zero`, `a0`, `a1`, `gp`, `sp`, `fp`, `ra`; HI and LO |

`a0`, `a1`, `sp`, `fp`, `gp` and the program's own header fields are defined at
entry and are never touched.

### Where the OpenBIOS numbers come from

Campaign lane hwtest-1103 listed every memory-mapped load and store in the
OpenBIOS ROM image the SuperStation One boots (a straight-line constant tracker
over the ROM, values only); the DMA, SPU and CD init routines were then
disassembled to read the read-modify-write constants. The numbers in the `openbios` column are values that
traffic stores, plus the power-on values of registers it only modifies; only
numbers are recorded. OpenBIOS source was not read, and nothing here is
derived from it.

- Exception vector words: `lui k0,0; addiu k0,k0,27D0h; jr k0; nop`, a
  different handler address from the retail 0C80h.
- SPU: the init routine stores 3800h to both main volume registers, zeros the
  voice, reverb-on, CD and external volume registers, stores 0004h to the
  transfer control, and ends by writing SPUCNT 8000h and then C000h. It writes no
  reverb preset.
- DMA: DPCR starts at its power-on value 07654321h. The CD init routine ORs
  8000h into it and sets DICR to `(DICR & FFFFFFh) | 880000h`; the SPU init
  ORs B0000h into DPCR. The result is 076FC321h.
- CD: the init routine issues Pause, Init and Setmode with parameter 0, then
  sets the INT enable register to 1Fh. Mode 00h is single speed, 2048-byte
  sectors, no CD-DA, no XA, no auto-pause.

### What is assumed (not settled by the ROM traffic)

- DPCR: the ROM also holds a `CdInit`-style routine that stores 9099h outright
  (the retail value). The boot may or may not call it. If it does, DPCR at entry
  is 9099h and the MDEC channels are enabled. Run `openbios,dpcr=0x9099` against
  plain `openbios` to see whether a program cares; a program that does depends on
  state OpenBIOS may not leave.
- DPCR: the GPU's read-modify-write (ORs 800h, channel 2 enable) is in the
  LoadImage-style helpers, assumed not to run during boot.
- DICR flags: assumed clear (the executable load's channel 3 completion
  acknowledged).
- I_MASK: no constant store found; kept at the retail 000Ch.
- Nothing pending in the CD controller (the boot acknowledges its last
  response); no interrupt latched in I_STAT.
- The handler address. OpenBIOS keeps its handler at 27D0h. That address is
  inside the HLE's own kernel routines (ReturnFromException, DeliverEvent and
  the rest, 2600h to 3000h), which cannot move without a layout change that
  touches every kernel routine, so the handler sits at 5E00h and the vector
  names that. A program that decodes the vector and chains to its target
  works; a program that tests the target for 27D0h (or for 0C80h) does not
  see either. A program that jumps to 0C80h or writes the retail vector
  words hangs in the loop at 0C80h, which is what the SuperStation shows
  for the hardware tests' KERNEL TIMING step (it writes the retail words
  itself).
- RAM contents: real OpenBIOS leaves its own data in low RAM and power-on noise
  above it. The HLE keeps its own kernel layout in low RAM and `openbios` fills
  the rest with one byte.

## What `hostile` does not model

- Sony revision differences nobody has documented. The range covers them; it is
  not a list of versions.
- The video standard (PAL vs NTSC) at entry. A PAL console's BIOS starts in PAL
  timing; the run is 50 Hz, which changes every frame count. Test it with a PAL
  disc, not with a seed.
- Kernel RAM below 10000h beyond the vector and handler: A0/B0/C0 table
  contents, heap pointers and the rest are the HLE's. Randomising them tests the
  HLE kernel, not the program.
- Pending interrupts: I_STAT and the CD controller's response queue start empty.
  A BIOS that leaves an unacknowledged interrupt is a hazard this does not
  reproduce.
- SPU voices: none playing, no pending key-on or key-off; sound RAM contents.
- Pad and memory card (SIO0) register state, and the pad and card the BIOS leaves
  initialised or not.
- The CPU's COP0 registers (SR, CAUSE) and the instruction cache contents.
- Timing: how many cycles the BIOS spent before the program (the VBlank phase at
  entry, the cycle counter).
