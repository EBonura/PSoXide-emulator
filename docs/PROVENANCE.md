# Provenance of the emulator core

This document says where each part of `emulator-core` comes from, as exactly
as the repository history and the tests allow. It replaces the earlier
statement that parts of the core are "derived from PCSX-Redux". Source file
headers carry the same information for their own module; this file is the
overview and the list of what is still open.

Nothing here is a legal opinion. "Clean-room" below means: written from
public hardware documentation (nocash PSX-SPX), this project's own console
measurements and the public ps1-tests captures, and checked against tests;
not translated from another emulator's source. The old implementation was
read where an interface had to be kept (field names, call sites, the tests
that pin behaviour), and its history stays in git.

## What was checked against

- **nocash PSX-SPX**, the public hardware document, for registers, bit
  layouts, formulas and tables.
- **This project's console**: the hardware-tests records (CPU, GPU, CD, SPU
  PA/SB series, MDEC v1.26), kept under `docs/hardware-refs/`.
- **ps1-tests** by Jakub Czekanski (MIT), build 158: console logs and VRAM
  captures for GPU, DMA, MDEC, SPU memory transfer, GTE and CD cases. The
  emulator passes 58 of its 61 self-checking cases; that number is a gate
  for every change in this area.
- **Compat and boot gates**: display hashes of 20 commercial titles
  (`compat/reference/`), the per-30-tick hashes of 16 library boots, and the
  emulator-core test suite. These pin behaviour that no document or
  measurement covers; such behaviour is marked `gate-pinned` in the code.

## Rewritten from the documentation (no remaining derivation known)

| Area | Written from | Notes |
|---|---|---|
| Event scheduler (`scheduler.rs`) | PSoXide's own requirements | Structure and names are original. The strict-deadline rule and the slot order are behaviour inherited from the earlier engine and gate-pinned; the slot order's influence on any gate has not been measured. |
| DMA register file and DICR (`dma.rs`) | PSX-SPX "DMA Channels" | Completion latencies are scheduled by the bus (see below). |
| SIO0 (`sio.rs`) | PSX-SPX "Serial Interfaces (SIO)", the SCPH-1200 ACK measurement | The synchronous byte exchange, the no-IRQ-for-a-missing-device rule and the no-second-IRQ-while-latched rule are gate-pinned. |
| Interrupt controller, pads (`irq.rs`, `pad.rs`) | PSX-SPX | Done earlier (EMU-R13). |
| XA-ADPCM decoder (`spu/xa.rs`) | PSX-SPX "CDROM XA Audio ADPCM Compression" | Done earlier (EMU-R04). The 4-fractional-bit history is pinned by the compat hashes. |
| MDEC (`mdec.rs`) | PSX-SPX "MDEC", this project's v1.26 reset measurements, the ps1-tests `mdec` captures | The AAN IDCT and its colour conversion are gone. Colour blocks use the same run-length decode, dequantisation and uploaded-matrix IDCT as mono blocks, and PSX-SPX's conversion coefficients in 16.16 fixed point, rounded once at the output depth. |
| SPU voice engine, envelopes, noise, reverb (`spu.rs`, `spu/envelope.rs`) | PSX-SPX "SPU ADPCM Samples/Pitch", "SPU Volume and ADSR Generator", "SPU Noise Generator", "SPU Reverb Formula" | See the SPU notes below. |
| CD-ROM delays (`cdrom/timing.rs`) | spec arithmetic, console records, PSX-SPX figures | Three values are still pinned, see below. |
| HLE kernel | See `hle-bios-provenance.md` | |

### MDEC notes

Measured against the console frame captured in ps1-tests `mdec/frame` (24-bit
output, byte for byte), the previous AAN-based path had a mean absolute byte
error of 0.54 over the decoded macroblocks and the new path 0.38. The 4-bit
and 8-bit programs match the console at its 5-bit precision before and after.
The new path is not byte-exact: roughly a quarter of the 24-bit bytes still
differ by one from the console, and the hardware's internal precision between
the IDCT and the colour conversion is not known. The 15-bit decode matches
the console frame in about 96% of pixels when rounded per field and 92% when
rounded to a byte first (offline comparison of the decoded blocks against the
console image); the final VRAM image of the 15-bit programs also passes through
the GPU, which adds a further plus or minus one per channel in this emulator
(a separate GPU question, not an MDEC one).

### SPU notes

A review during this rewrite found comments in the old SPU that cited names
such as `VolumeEnvelope::Tick`, `ADPCMBlock::GetShift` and `IsRAMIRQTriggerable`
as "PSX-SPX". Those names do not occur in PSX-SPX and match the source of
other open-source emulators, so the code they annotated (the volume sweep,
the ADSR rate tables, the noise generator, the interpolation window, the
pitch-modulation formula, the reverb network and its address wrap) was
rewritten from the PSX-SPX descriptions, not just re-commented.

Behaviour that changed as a consequence, all towards PSX-SPX: the noise
generator's rate and bit pattern; exponentially rising envelopes above 0x6000
run four times slower instead of using a rate offset; the pitch step is capped
at 0x4000 and the modulated step uses the documented factor; a voice that
modulates the next is no longer muted from the mix; the reverb alternates
left and right passes on successive samples and its addresses wrap by a true
modulo. The compat display hashes do not see audio. Against the previous mixer,
Crash Bandicoot's first 35 seconds differ in 4.9% of the samples with a
root-mean-square difference of 0.2 LSB on a signal of 1062 (about -74 dB);
seven of the 20 compat titles produce byte-identical audio for their whole run.

Legacy scalings kept because nothing measured contradicts them (flagged in
`spu.rs`): the reverb output volume is applied as Q14, and the main volume
register word is applied as a signed Q15 number (the documented fixed-volume
encoding would double it). A reverb work area at or below 0x0200, or 0xFFFF,
is treated as off. ADPCM prediction rounds each weighted term separately.

## Not rewritten: behaviour first matched to PCSX-Redux

These modules still contain behaviour that was originally chosen to match
PCSX-Redux traces while the project had a real-BIOS path. The source text is
the project's own; the behaviours are listed here because they are not
backed by a document or a console measurement, only by the compat gates.
Each is marked `gate-pinned` in the code.

- **CPU** (`cpu.rs`): the interrupt line is sampled only at branch
  boundaries; CAUSE is rewritten, not merged, at exception entry; an
  instruction's issue cost is charged before it executes.
- **Bus** (`bus.rs`): CD-ROM and SIO0 events are serviced at branch
  boundaries; the VBlank already scheduled keeps its line cadence across a
  GP1 display-mode switch; DMA completion costs (one cycle per word for the
  GPU linked list and the CD burst rule); MDEC DMA1 at 8 cycles per word.
  The v1.26 console profile suggests about 24 for DMA1 (see
  `emulator-accuracy-from-silicon.md`). Running the 20 compat titles with
  `PSOXIDE_MDEC_OUT_CYCLES_PER_WORD=24` changes the frame hashes of 15 of
  them (RE2 and Marvel vs. Capcom most) and leaves five
  untouched; all 20 still run to the end. Making 24 the default is a
  deliberate re-baseline that has not been approved, so 8 stays.
- **Video timing** (`bus/timing.rs`): the scanline of the first VBlank.
- **GPU** (`gpu.rs`, `psx-gpu-render`): the GPUSTAT reset value (apart from
  bit 31, which follows the console), the zero power-on display ranges and
  the empty-image rule of the display hash, the order in which quads are
  split into triangles, the four-edge walker for flat textured sprites.
- **CD-ROM** (`cdrom.rs`): see below.

## CD-ROM delay values

`cdrom/timing.rs` tags every constant as spec, console, PSX-SPX or pinned.
Three values are pinned and, until measured on a console, mirror the
earlier engine's choice:

| Constant | Value | PSX-SPX figure (PAL PSone) | Gate that moves |
|---|---|---|---|
| `GETID_SECOND_RESPONSE_CYCLES` | 20,480 | 0x4A00 = 18,944 | Legacy of Kain: Soul Reaver |
| `PAUSE_COMPLETE_CYCLES_STANDBY` / `_ACTIVE` | 7,000 / 1,000,000 | 0x1DF2 idle; 0x21181C single and 0x10BD93 double speed when reading | all 20 compat titles |
| `STOP_SECOND_RESPONSE_CYCLES` | 1,806,336 | 0xD38ACA single, 0x18A6076 double, 0x1D7B stopped | both WipEout titles |

The held-INT1 delay (500) and the IRQ reschedule granularity (0x100) are also
pinned or arbitrary, as `timing.rs` records. Ten timing records that measure
the three delays and the held-INT1 case on a console are specified in
`hwtest-cd-commands.patch` (records 0xF0 to 0xF9 of the hardware-tests suite);
once a console run exists, the measured median replaces the pinned value if the
gates hold, and the compat references are re-recorded otherwise.

The GetID response bytes return the ID string `PCSX` rather than the licensed
response PSX-SPX documents, because a real-BIOS boot is the only thing that
reads it and no gate covers that path.

## Other credits and non-derivations

PCSX-Redux, DuckStation, Mednafen and the MiSTer PSX core were consulted as
behavioural references at various points in the project's history. The
modules rewritten above no longer contain their code as far as the history
and the review can show; the modules listed under "Not rewritten" still
carry behaviour matched to PCSX-Redux traces. The ps1-tests captures (MIT) are used
as external oracles and are not redistributed.
