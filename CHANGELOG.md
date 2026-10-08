# Changelog

## Unreleased

- One texture filter, Edge, replaces the earlier filter set: the Bilinear, JINC2
  and xBR modes are gone, the toolbar and Settings toggle cycles None and
  Edge, and `--texture-filter` takes `none`, `edge` or `all` (`xbr` is an
  alias of `edge`; other values are an error). Edge is edge-directed
  interpolation written from scratch from `docs/texture-filters-spec.md`: it
  smooths diagonal staircases and keeps flat areas and orthogonal pixel-art
  edges crisp.
- Clean-room rewrites from PSX-SPX and console measurements: the event
  scheduler, the DMA register file and DICR logic, the MDEC colour path, and
  the SPU voice engine (ADSR and volume sweeps, noise generator, pitch
  modulation, reverb). `docs/PROVENANCE.md` lists what is rewritten and what
  still carries behaviour matched to PCSX-Redux traces. Behaviour changes:
  MDEC colour output now matches the console frame of ps1-tests more closely
  (FMV frames differ from earlier builds); the SPU noise generator,
  exponentially rising envelopes, reverb channel timing and a modulating
  voice's audibility follow PSX-SPX. Save states move to format 10; older
  save states no longer load.
- MDEC output DMA now costs 24 cycles per word instead of 8, fitted to the
  console's v1.26 FMV player profile (decode time per frame and the share of
  late frames). FMV timing in games moves with it; the
  `PSOXIDE_MDEC_OUT_CYCLES_PER_WORD` override is removed.
- BIOS support is gone: every disc and EXE boots on the built-in HLE kernel.
  The BIOS path setting, `PSOXIDE_BIOS`, the `--bios`, `--bios-boot` and
  `--bios-warmup-steps` options, the web BIOS upload and the real-BIOS
  compatibility modes are removed. A game's `memcard-1.mcd` is used directly;
  a card that existed before is first copied once to `memcard-1.pre-hle.mcd`.
- The HLE kernel serves an original font for Krom2RawAdd (Chrono Cross's
  name entry), plays CD-DA through track pregaps, reads CloneCD pregaps,
  and leaves the drive head where a boot loader does.
- GPU DMA now walks linked lists through the GPU's input FIFO by default, as
  a console measured it (hwtest v1.24). `PSOXIDE_EXPERIMENTAL_DMA_FIFO=0`
  restores the old word-count timing. Save states move to format 7 and keep
  the in-flight transfer; older save states no longer load.
- Refit GPU draw cost to console captures (hwtest v1.23/v1.24): large
  fills cost about twice as much, tiny and textured primitives less.
- An interrupt that lands on a GTE command now runs it and reports EPC on
  it, as a console does; a handler returning to EPC runs it twice.
- Faults in any branch delay slot, taken or not, report Cause.BD and EPC on
  the branch.
- GPU DMA nodes larger than the FIFO no longer drop words or the list's
  final GP0(1Fh); `PSOXIDE_GPU_DMA_OVERFLOW=drop` keeps the old rule.
- Timer 1 keeps its VBlank reset when a counter read straddles the edge, at
  the console's measured phase (26 lines after the interrupt).
- GPUSTAT bit 28 returns once a list's final GP0(1Fh) is in the GPU, before
  the last primitive finishes drawing.
- Add RAM-word watches to headless route logs for profiling unmodified games.
- Add opt-in limit-study oracles (`PSOXIDE_LIMIT_ORACLES=icache,ram,muldiv,
  gte,gpu,cd,mmio`), free code ranges, counted wait ranges and a per-function
  cycle profile, switched on from a chosen pad poll. Off by default; a plain
  run is byte-identical.
- Show game-library subfolders as expandable rows, collapsed at startup, with
  indented contents and game counts. Refresh preserves open folders and selection.
- Keep same-ID disc copies individually selectable and launch the exact file
  chosen in the library.

## Source 2026.09.05

This source snapshot is tagged `source-2026.09.05`. Download versions are
listed separately below; source cleanup does not replace an already published disc.

- Moved the emulator core and desktop/browser frontends into their own repository.
- Pinned the SDK sources used by the emulator and added software Vulkan to CI parity tests.
- Removed unused input-replay and profiler helpers from the standalone frontend.

No new binary download accompanies this source snapshot.
