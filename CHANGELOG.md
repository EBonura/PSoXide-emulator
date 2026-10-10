# Changelog

## Unreleased

- Reading one of the chip's own registers (SIO0, GPUSTAT, the interrupt, DMA
  and timer registers, memory control) no longer holds back the instructions
  behind it. Its wait overlaps them the way a RAM load's does: the first two
  pay in full, the next ones that need neither the bus nor the loaded value
  run free. The SDK's spin loop (`lw` of SIO0 STAT, `addiu`, `bne`, `nop`)
  cost 8.0 clocks a turn where the console's costs 6.85 (hardware tests v2.2,
  records 0xD1 to 0xDB), and costs 7.0 now. The pad setup and poll rows
  (0xB7 to 0xB9, 0xD1 to 0xDB) come 100 to 1,500 clocks nearer the console's,
  and a missing pad's timeout (records 0x64A, 0x64E, 0x767, 0x768) is exact.
  The fitted memory card data-byte delays were refitted for the faster loop
  (325 clocks in slot 1, 528 in slot 2), so a frame read still takes 124
  and 139 HBlanks.
- A controller port answers each byte of a transaction in its own time
  (hardware tests v2.2). `/ACK` rises 1665, 1678, 1627, 1629, 1468, 1695,
  1695 and 1692 clocks after the bytes of a pad poll, not 1677 after every
  one, and a memory card's first four bytes take 1717, 1682, 1523 and 1516
  clocks in slot 1 and 1691, 1552, 1465 and 1497 in slot 2, with the pulse
  widths the console showed. The bytes after those were never timed one by
  one; their delay is fitted so a whole 140-byte frame read takes the 124
  HBlanks (slot 1) and 139 (slot 2) the console took, where the emulator took
  113 and 117. A card read or write in a game is that much slower, and the
  card frame reads that failed under GPU load in the hardware tests (records
  0x6D3 to 0x6D5, all clean on the console) now pass. The pad poll record
  0x642 reads 1765 against the console's 1757 (it was 1807). Save states move
  to format 17.
- A block-mode DMA to the GPU waits for the GPU instead of losing words. The
  channel is paced by the GPU's request line, and the emulator pushed a whole
  block at once, dropping every word that arrived while the input FIFO was
  full: an A0h upload or a command stream sent behind busy drawing lost its
  tail, and the pixels or commands that followed were read out of step. The
  commands at the head of the FIFO now run to make room, with their drawing
  time still charged as busy credit.
- The HLE kernel's memory card driver no longer takes a byte before it has
  arrived. Its early data-phase routine, which games patch to wait for the
  card's `/ACK` line, runs on every exception of a sector's data phase, not
  only on the byte's IRQ7. With a byte on the wire for ten bit times it then
  read an empty DATA register and sent the next byte behind it, so a card
  read or write lost its sync: Legacy of Kain: Soul Reaver reported "Invalid
  memory card" with a good card and never left "Reading memory card" with a
  fresh one. The routine now waits for RX not empty and leaves the byte to the
  next pass. `frontend launch --no-memcard` starts with no card in either
  slot, and `hle_compat --no-card` does the same for the compatibility runs,
  which also print the card commands each game sent and can start from
  (`--card-in`) and save (`--card-out`) the card images.
- 480-line interlaced frames drawn field by field show whole. With drawing to
  the display area prohibited (GP0 E1h bit 10 clear) in 480i and the display
  on, fills and primitives skip the lines of the field on display (GPUSTAT
  bit 31) and draw only the other field's. Valkyrie Profile redraws its title
  and menus this way: the screen was white whenever the frame was sampled just
  after the clear. The picture is put together from the live lines of the
  shown field and the held lines of the other, by the new Settings >
  Deinterlace choice: Blend (the default, averages the fields), Bob (shows the
  field on display only) or Weave (interleaves them, which combs moving
  objects). Headless captures and display hashes always weave. In the GUI,
  these frames now come from the CPU display like 24bpp ones, since the HW
  renderer does not apply the rule. Games that run in this mode (Valkyrie
  Profile, Gran Turismo 2, Tekken 3, Chrono Cross, Formula One 2001, one
  WipEout 3 frame) change display hashes. The rule is not measured on the
  console yet, and whether it holds with the display blanked is a guess (it
  does not, here: Chrono Cross builds its menu background that way).
- The hardware renderer now matches the CPU rasterizer on polygons. It draws
  each triangle half a pixel down and right, so the host's centre sampling
  lands where the PS1 samples (a pixel's top-left corner): the same pixels are
  lit, with the same texels and colours, including on one-pixel slivers. That
  removes the row of blue dots along the Quake shotgun, where the host lit
  pixels the CPU skips and read the texture's border column; a Quake frame's
  mean difference from the CPU image drops from about 8 to under 1.
  Rectangles, fills and lines are unchanged. Texture coordinates wrap in
  eight bits below zero as the CPU's do instead of clamping at zero.
- Semi-transparency in the hardware renderer blends the stored colour values
  instead of linear light. It added the source to the decoded destination and
  re-encoded the sum, so VoXide's BRIGHTER 3 rectangle (+24 per channel) came
  out near +3; all four blend modes now land within one 5-bit step of the CPU.
- Stopping a GPU linked-list DMA now delivers the node the walker had already
  admitted. A DMA burst holds the bus, so a CPU write that clears CHCR cannot
  land inside a node; the walker interleaves a word per cycle with the CPU and
  could stop with half a packet in the GPU FIFO. The words the CPU sent next
  (a clear-cache and an A0h upload header) were absorbed into that packet,
  every pixel of the upload that followed was read as a command, and Legacy of
  Kain: Soul Reaver showed corrupted textures after an area change.
- Textured primitives pay for their texels (hardware tests v2.1, records 0x200
  to 0x223). An 8 or 15-bit texture costs more a pixel than a 4-bit one, a
  texture page change refills the texture cache in proportion to the pixels
  drawn (a 32x32 15-bit triangle alternating between two pages costs about 1280
  clocks more, where the emulator charged none), and a new 8-bit CLUT costs
  about 270 clocks to load (25 at 4 bits, none at 15). A moving texture window
  inside one page, which the console also charges for, is not modelled.
  Games that switch pages and palettes a lot draw slower in emulated time.
- The expansion buses cost what their delay registers say. A write to a root
  counter no longer shortens the first external read behind it (an overlap
  measured on a late PSone): through warm code 64 reads of EXP1 now take 2174
  clocks, the BIOS ROM's figure and the console's 2176, and EXP3 830 against
  832 (they were 2144 and 821). A cold I-cache line fill and the
  write buffer share the RAM bus: a store behind a fill reaches RAM after it,
  a fill waits for the stores the buffer already holds, and an internal
  register read gives way to a fill for up to three clocks. That moves 64
  stores through evicted code from 143 to 319 clocks (329 on the console) and
  32 GPUSTAT reads from 239 to 287 (284), while a store with room in the
  buffer is never held. Save states move to format 16.
- SIO0 follows the console's byte timing (hardware tests v2.1). A byte takes
  ten bit times, not eight: it reaches DATA and sets RX not empty when it has
  been through the wire (1360 clocks at the BIOS's BAUD, where the emulator
  answered at once), and a DATA write behind it waits in the TX register.
  `/ACK` rises 1677 clocks after a pad byte and 1511 (slot 1) or 1529 (slot 2)
  after a card byte, for 92, 38 and 74 clocks. A full pad poll, which took
  about 1100 clocks a byte, now takes about 1700, so games that poll the pad
  see their input a few frames later in emulated time. Save states move to
  format 15.
- The GPU's DMA competes with the CPU for main RAM as it does on a console
  (hardware tests v2.1). While a linked-list walk is moving, CPU loads and
  stores to main RAM start at least 12 clocks apart (64 loads with a nop each
  took 770 clocks against 513 idle, 64 stores 797 against 141; a walk parked on
  a full FIFO leaves the CPU alone). While a block transfer streams RAM into
  the GPU, loads wait 19 clocks more (1738 clocks for 64 against 510). An empty
  list node costs 10.25 clocks, where the emulator charged 12 against a
  console's 10.23. Scratchpad accesses are unaffected.
- A CD sector DMA holds the CPU off the bus for the whole burst, 68.3 clocks a
  word (34.9k clocks for 2048 bytes), and CHCR reads 0 once it ends. On a
  console 64 loads or stores after the kick finished 35k clocks late at both
  drive speeds (hardware tests v2.1); the emulator let the CPU run on and took
  66k clocks to report the channel idle. Chopped bursts (CHCR 0x11400100) keep
  their windows. Games that stream from the disc now spend that time in the
  transfer, as on a console, so their frame timing shifts.
- GPU and SPU block DMA (sync mode 1) count their blocks down in BCR and walk
  MADR as the transfer runs, and a kick with a block count of zero is 65,536
  blocks, not none: hardware tests v2.1 re-kicked a finished transfer without
  rewriting BCR and the channel ran away on a console (count wrapped below
  zero, 250 blocks in for the SPU, which the emulator now reproduces to the
  register). A software write to a busy channel's MADR, BCR or CHCR supersedes
  the transfer. MDEC DMA leaves MADR past the last word it moved. The CD and
  MDEC channels keep their block-count handling: only the GPU and SPU were
  measured. Save states move to format 14.
- MTHI and MTLO cancel a running multiply or divide with its interlock: a
  `multu; mtlo; mflo` triple costs three instructions, as on a console (hardware
  tests v2.1, records 0x1D5 to 0x1D8), where the emulator waited out the
  multiply.
- A root counter polled in a tight loop no longer loses ticks. A read still
  returns the count from before its own bus wait, but the wait's clocks come
  back at the next advance instead of being dropped: hardware tests v2.1 on a
  console counted every clock of a second of Timer 2 and Timer 0 polling
  (2172.3 ticks a line), where the emulator lost 22 to 40 percent. Save states
  move to format 13; older save states no longer load.
- Input tapes are easier to find and keep. The Game menu has "Record input
  tape" (F8), "Replay last recording" (F4) and the texture filter (F6), and the
  debug sidebar has an Input tape section with the same buttons. Recording now
  restarts the game first, so every tape starts at a cold boot and replays to
  the same frames. Stopping saves `latest.pxtape` plus a timestamped
  `recording-<UTC time>.pxtape` beside it, in place of the old `archive/`
  folder. The texture filter is saved in `settings.ron` (`video.texture_filter`)
  and restored on the next launch. `ui-png` renders the menu or the window
  chrome to a PNG without opening a window.
- The library list shows one row per game. It lists only what is under the
  games folder (the editor's project bakes and example builds, which share
  `library.ron`, no longer appear), skips `.bin` files that are RAM dumps rather
  than discs, and shows a game's own folder as a plain row; folders remain as
  groups of two or more games.
- The desktop audio output waits for 50 ms of sound before it starts and again
  after it runs dry, and an underrun fades the last sample out over about 2 ms
  and back in over the refill instead of cutting to silence (a click). A
  backlog past 160 ms drops its oldest samples with the seam crossfaded. The
  web player keeps its shorter 20 ms and 64 ms limits.
- The dev profile builds the workspace crates on the frame path (`psx-gpu-render`,
  `psx-iso`, `psx-hw`, `psx-trace`, `psoxide-jit`) at opt-level 3 and the debug
  UI and settings crates at 2, so `make run` is no longer limited by crates
  that the `"*"` override does not reach.
- `--press` takes analog stick entries, `tick:lstick=X/Y[:hold]` and
  `tick:rstick=X/Y[:hold]` (0..=255, 128 centred), held for a number of route
  ticks like a button. A stick token forces the pad to analog mode and
  conflicts with `--digital-pad`; scripts without one behave as before.
- One texture filter, Edge, replaces the earlier filter set: the Bilinear, JINC2
  and xBR modes are gone, the toolbar and Settings toggle cycles None and
  Edge, and `--texture-filter` takes `none`, `edge` or `all` (`xbr` is an
  alias of `edge`; other values are an error). Edge is edge-directed
  interpolation written from scratch from `docs/texture-filters-spec.md`: it
  smooths diagonal staircases and keeps flat areas and orthogonal pixel-art
  edges crisp.
- XA audio is converted from 37.8 or 18.9 kHz to 44.1 kHz with the PSX-SPX
  25-point zigzag filter instead of by repeating the nearest sample, which put
  the images of the source band up to 22 kHz and cost about 14 dB of
  signal-to-error ratio on music. The SPU repeats the last CD frame over an
  underrun of up to four samples instead of dropping to zero (a click). The
  converter's history and that repeat state are part of save states, which
  move to format 11; older save states no longer load.
- CD sectors chain from their own deadline instead of from the cycle the CPU
  serviced the previous one. A streaming read ran about 50 ppm slower than
  the disc turns, so the XA audio it fed the SPU ran dry about twice a second
  and each gap was a click. A tick more than a sector period late restarts the
  cadence from the current cycle. Sector arrival times move by a few cycles
  per sector, so frame hashes of games that stream from the disc move with it.
- The CD drive's audio, data and motor transitions follow console measurements
  (hardware tests v1.28, medians): Pause from CD-DA completes 123.1 ms after
  the command; Stop shows the motor until 606 ms; a data read on a stopped
  drive pays the spin-up (four sectors in 1978 ms) and a read issued during
  the spin-down waits it out first (first sector 2721 ms); the first data
  sector after CD-DA arrives 945 ms after the read, and the first Play after
  data reaches PLAYING in 1006 ms. A CD-DA Pause used to complete in 3.9 ms
  and a read after audio in 356 ms. Games that pause or stop audio and read
  the disc afterwards see the new latencies, and with the sector change above
  14 of the 20 compat games render some frames differently (their final
  frame hash moves in 8). The drive's spin-down deadline and its
  head-on-a-CD-DA-track flag are part of save states, which move to format
  12; older save states no longer load.
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
