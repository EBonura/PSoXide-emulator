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

## Gate baseline after the MDEC DMA1 change

The gates were re-run with MDEC DMA1 at 24 cycles per word. The ps1-tests
result is unchanged (58 of 61 cases; the three MDEC programs only end on a
different idle cycle count). Of the 20 compat titles, five keep their display
hashes (Crash Bandicoot, Crash Team Racing, Metal Gear Solid, Metal Slug X,
Valkyrie Profile) and fifteen change from the first FMV frame on, because the
decode now takes about three times as long. The 16 library boots and the
v0.43 demo discs are unchanged. The HLE columns of `compat/reference/` and
`compat/reference.toml` still hold the earlier numbers and are re-run when the
kernel's timing is next re-recorded.

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
| MDEC DMA1 cost (`bus.rs`, `MDEC_OUT_CYCLES_PER_WORD`) | this project's v1.26 console FMV profile | 24 cycles per word. It is a throughput fit to one workload (the v1.26 FMV player: 43% of frames late, as on the console), not a measured per-word latency. The earlier value of 8 was a constant inherited from the previous engine and is gone. |
| Root counter read latch (`timers.rs`) | hwtest v2.1 records 0x6B0 to 0x6B5, the public access-time suite | A read returns the count from before its own bus wait; the wait's clocks come back at the next advance. Polled for a second the counters lose nothing, as on the console (2172.3 ticks a line). The Timer 0 dot-clock case is not covered: the console's figure for it is not usable. |
| GPU and SPU block DMA registers (`bus.rs`, `DmaBlockRun`) | hwtest v2.1 records 0x780 to 0x785 | BCR's block count falls as blocks finish and MADR walks, a kick with count 0 runs 65,536 blocks. Measured on channels 2 and 4 only; CD and MDEC keep their old handling. The SPU's end state reproduces record 0x781 exactly. The console's GPU runaway reads source memory past the buffer as commands, so its block count at the bound (274) is not reproducible. |
| CD DMA bus hold (`bus.rs`, `cd_dma_clocks`) | hwtest v2.1 records 0x700 to 0x707, 0x790 to 0x797 | The CPU waits 68.3 clocks a word. Measured with CHCR 0x11000000 only, at both drive speeds; the chopped-burst CHCR 0x11400100 keeps its windows. |
| GPU DMA against the CPU on main RAM (`bus.rs`, `GPU_LIST_RAM_ACCESS_SPACING`, `GPU_BLOCK_RAM_READ_WAIT`) | hwtest v2.1 records 0xFE, 0x145, 0x1E1 to 0x1E7 | While a list walk moves, CPU main-RAM accesses start 12 clocks apart (loads fit, stores come out 4 percent low). A block transfer into the GPU makes loads wait 19 clocks more; stores and the SPU/OTC channels are not modelled this way. |
| SIO0 byte and `/ACK` timing (`sio.rs`) | hwtest v2.1 records 0x610 to 0x618, 0x6C0 to 0x6CB | Ten bit times a byte; the ACK delays and widths are the medians of one pad (id 0x73) and two memory cards, not a range over pads. |
| Cold code fill against stores and registers (`bus.rs`, `WRITE_AFTER_FILL`, `CODE_FILL_MMIO_WAIT_CAP`) | hwtest v2.1 records 0x1B8, 0x1BA | Stores: a mechanism, one fitted constant. The fill and the write buffer share the RAM bus, so a write waits for the fill in flight and a fill waits for the writes the buffer already holds; the core is held only when the four slots are full, so an isolated store costs nothing (the earlier rule charged every store behind a fill and added about 4.9 percent to Graybox Reach's cycles; this one adds about 2.9). Record 0x1B8 reads 319 against 329 on the console. `WRITE_AFTER_FILL` is the only fit. Registers: the three-clock cap is fitted to 0x1BA. |
| Texture cost model (`gpu.rs`, `texture_timing_surcharge`) | hwtest v2.1 records 0x200 to 0x223 | The per-pixel read rates, the page-change refill (1.0, 1.55 and 2.8 clocks a pixel past the first 24, at 4, 8 and 15 bits) and the CLUT reload (25 and 270 clocks) are fits to three triangle sizes (8x8, 16x32, 32x32) on one console, within about 15 percent of them. The cause of the small-triangle behaviour (cache capacity, line size) is not identified, and a moving UV window is not modelled. |
| Expansion bus waits (`bus/memory_timing.rs`) | hwtest v2.1 records 0x1C0 to 0x1CB | Derived from the delay registers alone. The counter-write overlap that the SCPH-9902 captures showed is gone: the project's console shows none through warm code (records 0x55 to 0x5D, a single cold pass, still read 1 to 2 percent over). |
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
  GPU linked list and the CD burst rule).
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

## Texture filter (Edge)

The hardware renderer's upscaling texture filter, Edge
(`psx-gpu-render/src/shaders/prim.wgsl`, functions from `filter_tap` to
`filter_edge`), is original work. It replaces two earlier filters that were
ports of third-party shaders and were removed in 56e7aff, and the bilinear
mode that sat beside them (also removed; the toggle is now None and Edge). The
specification `docs/texture-filters-spec.md` was written first, from the
mathematics alone, and the shader was written from that specification.

What was consulted, all of it mathematics and none of it source code:
barycentric coordinates in a triangle, the smoothstep polynomial and a plain L1
colour distance. The diagonal-chain orientation test, the isolated-texel
support test, the transparent-tap rule and the way the axis and diagonal
renderings share their cell edges are this project's own design. No shader,
upscaler or filter source from DuckStation, beetle-psx, libretro or slang
shader packs, ReShade, xBRZ, ScaleFX or any other project was read, and the
removed implementations were not opened while writing this one.

## Other credits and non-derivations

PCSX-Redux, DuckStation, Mednafen and the MiSTer PSX core were consulted as
behavioural references at various points in the project's history. The
modules rewritten above no longer contain their code as far as the history
and the review can show; the modules listed under "Not rewritten" still
carry behaviour matched to PCSX-Redux traces. The ps1-tests captures (MIT) are used
as external oracles and are not redistributed.
