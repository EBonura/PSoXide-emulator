# Changelog

## Unreleased

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
