# Changelog

## Unreleased

- The shell moves. The tab highlight is one pill (phone bar) or marker (desktop) that
  slides to the page you pick on a small spring, and the Controls selector slides the
  same way. Pages arrive part by part (title, then rows or cards), a list's focus frame
  glides between rows, and anything you press glows under your finger. Reduced motion
  turns all of it into a plain switch. Slider values, the Debug summary and the plot
  readouts use the interface type instead of the pixel face.
- A phone no longer says it has a keyboard: the Controls card reads "Not connected"
  until a key has been pressed. Over a paused game the controller's own buttons step
  aside; the shell header has a close button, the running game is the first case with a
  RUNNING chip, Resume and Start over, and the game behind is dimmed so the shell reads.
  Inside another page's frame (itch.io) the phone tab bar leaves that host's fullscreen
  corner clear.

- The desktop home screen always shows the PSoXide Demo Disc as its first case, with the
  disc's own cover. The newest public release in your games folder (or next to the
  program) is the one that starts; older releases and the private and diagnostic builds
  are cases of their own after "Open a games folder", newest first, and a cue sheet whose
  images are gone is not listed. With no public release it points at the download.

- Stall-budget tooling. Two data-address limit oracles (`PSOXIDE_RAM_DATA_LOG`,
  `PSOXIDE_LIMIT_FAST_DATA`) rank main-RAM lines by load stall and price
  "these bytes live in the scratchpad", and `tools/stall-tools` (no
  dependencies) turns a replay's logs into a stall-budget report with wait
  loops removed, a hot-line range file, and a spill gate for registered
  kernels. Both oracles are inert unless set: a Half-Life replay is
  cycle-identical bucket by bucket with them off.

- A new home screen replaces the cross media bar. It opens at once with the PSoXide
  Demo Disc as one low-poly case on a quiet floor, a title, one line and one button
  ("Start disc"); "What's on it" opens only when asked. Your own games get an "Open a
  games folder" case and smaller cases for recent discs. On a first visit a line under
  the disc counts three seconds and starts it, and any input cancels that and keeps
  you home; later visits stay home. Library, Settings (real sliders, toggles and
  choices), a pause list, calm welcome pages and a Debug sidebar with tabs and a
  pinned summary all use the same controller-first focus. Box art for your own discs
  is fetched at run time from public community collections (by boot serial, then by
  file name), cached, credited and switchable off. The phone "tap to play" gate is
  gone. Desktop, phone portrait, small phone and phone landscape are covered by new
  journeys.
- Web player menus, fullscreen and controller polish. The texture filter is one
  control (Settings), "Reconnect saved games" became "Reopen your games folder"
  and shows only after a folder was connected, and a few labels now say what they
  do ("If the device is too slow: Skip frames / Slow the game"). The fullscreen
  button no longer does nothing on an iPhone: inside another site's frame it opens
  the player's own page in its own tab, and on a page of its own it explains Add to
  Home Screen (a web manifest makes that install fullscreen). SELECT and START fit
  their pills with room to spare on every phone, and the status toast clears the
  controller's buttons under a notch. The journey suite checks all of it, and no
  longer races the pad negotiation or another run's ports.
- The web player's Reset, disc pick and menu now behave like a console's. Reset
  restarts the loaded game in place instead of reseating the CPU at a BIOS
  reset vector the browser machine does not have (which faulted and dropped the
  player into the menu over a black screen); picking the disc that is already
  loaded restarts it from the image in hand with no download, and picking it
  again after another game loads it again instead of doing nothing; closing the
  menu with Back resumes the game it paused; the keyboard, touchscreen and
  gamepads present an analog DualShock from the first poll in the browser, so
  the demo disc needs no F9 step. The transport rules live in a pure state
  machine (`session.rs`) with transcript tests, and `tools/web-journeys` drives
  every control of the player in headless Chrome (desktop, phone portrait and
  landscape, a cross-origin iframe, a faked gamepad). Also: the save-states
  panel closes with Esc and Back, browser settings persist in `localStorage`
  instead of toasting "settings save failed", the toolbar comes back when touch
  controls are set to Auto, the iOS silent-mode notice no longer covers the
  controller, and a browser without WebGPU gets an explanation instead of a
  panic. See `docs/web-player-journeys.md`.
- The web build reads XA discs in a compact format that is 44% smaller than the
  raw image and starts sooner. A new crate, `psx-webdisc`, keeps only what a
  sector cannot regenerate (2048 or 2324 user bytes, plus four subheader bytes
  in the index) and deflates it in chunks of 32 sectors; sync, header, EDC and
  ECC are rebuilt exactly on read, and the packer falls back to storing a sector
  verbatim when a rebuild would differ, so every disc round-trips byte for byte
  (its `webdisc pack` reads the files back and compares each sector with the
  image). The demo disc v0.48 goes from 318 MiB to 178 MiB, its index from 330
  KB of sector headers to 30 KB, and a launcher boot reads 0.68 MB instead of
  1.43 MB. The page fetches the index and the boot chunks while the wasm
  downloads, then fetches chunks by range request as the drive reaches for
  them, keeps a few seconds of lookahead, and caches the chunks it has seen in
  IndexedDB, so a return visit downloads no disc data it already has. The
  native bin/cue path is unchanged, and so is the drive's timing: a chunk that
  has not arrived holds the frame back, never the emulated drive.
- The gate's HW3 comparison is now meaningful for 3D content. Every 3D pixel
  used to be compared against the nearest-neighbour enlargement of the CPU
  frame in all nine sub-pixels, which no textured or shaded surface can pass,
  so every HW3 check of a 3D game failed (HL's full renderer matrix: 54 of 91
  checks passed). Unclassified pixels now compare the centre sub-pixel of each
  3x3 block exactly, which is the CPU rasterizer's own sample point, and report
  how many sub-pixels were not compared. Declared 2D regions still compare all
  nine. HW1 is unchanged (exact). Not a tolerance; see TESTING.md.
- The hardware renderer now reproduces the CPU rasterizer's 5-bit arithmetic
  where it showed. The four semi-transparency modes blend on whole colour
  codes (the source is cut to its code and scaled per mode, so the fixed
  blender lands on the CPU's result for every pair of codes; HL's HUD,
  lightning and glows were up to three codes too bright). Lines follow the
  console's own pixel walk, so they no longer sit a pixel off on rounding ties
  (on a scaled target they now step in whole native pixels, like the console's,
  instead of being smoothed). Sprite-shaped Gouraud textured quads keep one
  colour per native pixel at any scale. A scaled target's centre sub-pixel is
  now exactly the CPU's pixel for polygons whose attributes differ across the
  quad diagonal. At 1x the whole frame matches CPU1 on every journey checked.
- The web build plays on phones and tablets. A PlayStation controller is drawn
  on the page (D-pad with diagonals, face buttons, shoulders, Start and Select,
  and two floating analog sticks while the game's pad is in analog mode) in
  landscape beside the game and in portrait under it, inside the notch and home
  bar. It is a device like any other in the input layer, so the menu, port
  assignment and the guest need no special case; fingers slide across the D-pad
  and between buttons, and any number work at once. It shows on touch devices
  until a keyboard or gamepad is used, and Settings has its mode, opacity and
  size sliders, haptics and a left-handed layout, kept per device. The menu
  has big rows, flick scrolling and swipes between columns, an on-screen menu
  button and a fullscreen button; the first visit shows one tap to start that
  unlocks audio, goes fullscreen and keeps the screen awake.
  Also fixed: the menu panicked on the first frame of a high-density phone
  screen (a list narrower than its minimum width).
- Controllers and the keyboard now share one input path. Every source is an
  adapter that produces a per-device snapshot in the PS1 pad layout, and the
  menu, the hold-to-repeat, the noise guard, port assignment and the guest feed
  read only that, so a device cannot work for the keyboard and not for a pad.
  On macOS, Xbox, DualShock 4, DualSense and Switch Pro controllers are read
  through Apple's GameController framework (gilrs stays for other platforms and
  for HID pads the framework does not claim). A pad takes the first free
  controller port on its first button press; the Controls panel lists each
  device with its live state and a port picker, and connects, disconnects and
  noise quarantines are logged with the backend and vid:pid. Holding a menu
  direction scrolls with an initial delay and accelerating repeat.
  `psoxide-emulator input-probe` shows the input layer's view with no window.
- A headless run can plug a pad into port 2 (`--pad2`) and set how late the
  controller pulses `/ACK` after each byte and how long it holds it
  (`--pad-ack-delay`, `--pad-ack-pulse`, CPU cycles; each replaces the measured
  per-byte figure for every pad byte and leaves memory cards alone), so a pad
  driver that paces on `/ACK` can be read against an early and a late
  controller. Without the flags nothing changes.
- Profile-guided release builds: `make build-pgo` and `make gate-pgo` build the
  emulator, the gate and the benchmarks from a recorded profile
  (`pgo/psoxide.profdata.gz`, refreshed by `make pgo-profile`). Every emulated
  game ends in the identical state as with a plain build (`bench_suite`
  compares state, display, cycles and instructions), and runs about 17 to 22%
  more frames per host cycle. A plain `cargo build` is unchanged and needs no
  profile. `bench_suite` is the new fixed-game speed and exactness scoreboard.
- A cache-control / DMA tripwire (`PSOXIDE_CHECK_CC_DMA`, on for every
  `psoxide-gate` run). Changing `cache_control` bits 13..16, 7 or 3, or setting
  `SR.IsC` (which is what the BIOS `FlushCache` does), while any DMA channel is
  busy now fails the run with the PC, the old and new values and every busy
  channel's MADR, BCR and CHCR. On the console that sequence made the next
  instruction fetch return the DMA's list word (the Quake PSX freeze); the
  emulator never reproduced it. `warn` counts instead of failing, and `inject`
  also hands the guest the DMA's word as its next bus fetch. See
  docs/cache-control-dma-tripwire.md.
- The SPU main volume is applied as the console documents it. A fixed
  register word holds half the level, so 0x3FFF is 0x7FFE (about unity), and a
  main volume sweep now moves the output. The mixer used the register word
  itself, so every game was 6.02 dB quieter than the documentation and
  DuckStation give (checked on three discs: the loudness rose by exactly
  6.0 dB). The reverb output volume is now Q15 rather than Q14, so the wet
  level is unchanged; a write of 0x7FFF to the main volume is now a level of
  about zero, as on the other emulator, not full scale. Not yet confirmed on
  the console.
- Pause now takes the time the console takes. Measured on the console (hwtest
  v2.7, run FB89, records 0x340 to 0x3B8): from the command, a Pause out of a
  read completes after 3.80 to 3.84 million clocks at single speed and 1.69
  to 1.93 million at double with the head settled (the twelfth sector), 2.42
  to 2.46 and 1.22 to 1.24 million at the first sector, 2.79 to 2.84 and 1.42
  to 1.44 million at the first sector after a far seek; an idle drive
  answers in 37,720 to 156,544 clocks. The emulator answered all of them in
  about 26,300 (an earlier unlanded estimate said 120,000). The figures scale
  with the sector period, so with the drive speed, though not in whole
  periods: counted from the acknowledge, 5.33, 6.17 and 8.39 periods. The model keeps
  them (first sector, first sector after a head travel of 512 sectors or
  more, settled), and adds a period per sector delivered between the first
  and the settled figure (a guess: the console was timed at 1 and 12
  sectors only). The Pause acknowledge takes the console's 32.9 thousand
  clocks (GetStat keeps 19.2 thousand) and says 0x22, still reading, which
  a GetStat sent within a few milliseconds also says; the second response
  says 0x02. A Pause from an idle drive follows its acknowledge by 8.3
  thousand clocks (was 7,000), which is the console's gap in all six idle
  trials. Two behaviours that were absent: a command written between the
  Pause's two responses cancels the second (GetStat, in four of four
  console trials; the model cancels on any command, an assumption), and a
  Pause while the head is still travelling to the first sector of a read is
  refused with INT5, status 0x03, and no INT3 (six of six trials; the error
  byte was not recorded, 0x80 is a placeholder), the read carrying on. Stop,
  Init, Reset, SeekL, Play and an eject end the short "reading" window.
  Every compat game still boots to the same stop reason, final frames move
  with the timing (17 of 20 display hashes).
- The native CPU tier now matches the interpreter's cycle count while a GPU
  DMA streams from main RAM. Its inline RAM load path skipped the wait a load
  owes a GPU block transfer or list walk (19 clocks a load for a block
  transfer, 12 for a walk), so a WipEout Rapier/Altima journey replayed with
  identical pad input drifted from the interpreter by one controller poll
  within about 3000 ticks, and open-loop replays then diverged for good. The
  inline path is now taken only when no GPU DMA transfer is in flight. The
  tier also no longer leaves a cache-hit note on the bus when it stops before
  running the first op of a block. `jit_tape` replays a pad tape (text or
  `.pxtape`) on a disc and compares the tier with plain `Cpu::step` run call
  by run call, and the lockstep fuzzer now ends runs at clock deadlines and
  compares the fetch bookkeeping too.
- The desktop package and binary are now `psoxide-emulator` (the directory is
  still `emu/crates/frontend`). `cargo run --release` from a fresh clone builds
  and starts it: the SDK crates the emulator links are vendored under `crates/`
  and `sdk/`, and `make bootstrap`, Python and the separate download step are
  gone. A small `frontend` binary is built beside it and runs
  `psoxide-emulator` with the same arguments, so scripts that call
  `target/release/frontend` by path keep working; `cargo build -p frontend`
  becomes `cargo build -p psoxide-emulator`. `make refresh-sdk` re-exports the
  vendored crates from the revision pinned in `components.lock.json` and
  `make check-sdk` (CI) fails when they drift. `tools/mkisopsx`,
  `tools/psoxide-link` and `make examples` are no longer part of this tree; they
  live in the SDK repository.
- A lazily read web disc image can be split into numbered files. The manifest's
  `image` line takes an optional last field, `PART_BYTES`, and the image is then
  `FILE.001`, `FILE.002`, ... of that many bytes each (a whole number of
  sectors; the last one shorter), so a host with a per-file size cap (itch.io
  refuses 200 MB) can still serve an XA disc over Range requests. A read that
  crosses a file boundary becomes one request per file. `index.html` also
  prefetches the header sidecar of a lazy disc while the wasm downloads.
- The web page offers the demo disc only. The 20 example EXEs are no longer
  copied next to the page or listed in its Homebrew folder (2.3 MB less in the
  uploaded bundle, 20 files fewer); the download still bakes and lists them.
- The web build boots the demo disc on its data track alone. The first song is
  fetched and patched into the mounted disc like the others instead of gating
  the first frame, FLAC decode runs on a per-frame time budget instead of three
  blocks per frame, and `index.html` starts the manifest, data track and first
  song while the wasm is still downloading. Measured on a local 25 Mbit link
  the first guest frame moved from 19.7 s to 7.4 s, at 100 Mbit from 9.3 s to
  2.2 s. Pieces are requested as `file?v=<checksum>` so a cached manifest can
  never meet another build's data track, and the menu subtitle no longer
  quotes a download size.
- The fleet gate can rebase a poll-bound tape to the current controller poll
  on a journey step. Its default absolute timing is unchanged, and reports
  retain the tape hash plus recorded and effective poll boundaries.
- A block-mode GPU DMA of NOPs is not held up behind busy drawing. The pacing
  change made every block past the 16 words the FIFO holds wait for the
  drawing in progress, NOPs included: hwtest v2.1 and v2.2 rows 0x67 to 0x6A
  (64 to 256 NOP words) went from 133 to 237 clocks and from 458 to 562 for
  the 16 x 16 block, where the console reads 124 to 126 and 436 to 466. A NOP
  draws nothing and leaves the FIFO as it reaches the head.
- Textured triangles pay when their texels outgrow the texture cache. The
  console's cost for sixteen 32 x 32 triangles was flat at 5.4k clocks until
  the texture coordinates spanned more than the cache holds, then climbed to
  14.7k and 24.8k (4-bit, spans 128 and 255), 12.1k, 26.5k and 44.0k (8-bit,
  64, 128 and 255) and 22.4k then 45.2k twice (15-bit, 64, 128 and 255); the
  emulator read 9,959 for all of them (hwtest v2.4, records 0x8E0 to 0x8F7).
  The extra clocks per drawn pixel follow the bytes the texture step
  advances per pixel (about 1.2 clocks a byte up to 9.7, one miss in two or
  so), ramping in over footprints of 1 to 2.5 KB; a triangle that fits is
  charged nothing. With the 480i rule on, the per-pixel read charge for 8 and
  15-bit texels is dropped as well: the console took 5.4k for triangles of
  every depth that fit. With the 480i rows change below, 32 of the 38
  texture-window rows read within 5% of the console and the cliffs land
  (4-bit span 128 and 255: 15,393 and 25,143 against 14,661 and 24,796; 8-bit
  64, 128 and 255: 12,091, 25,221 and 44,708 against 12,051, 26,549 and
  44,044; 15-bit 128 and 255: 44,864 and 45,306 against 45,185 and 45,167);
  the 15-bit span 64 row reads 25,221 against 22,410. A texture window (GP0
  E2h) is not modelled; only the 15-bit 64-texel one cost anything on the
  console (record 0x92B, 16.3k).
- Draws that the 480i rule skips rows of cost only the rows they reach. With
  drawing to the display area prohibited in 480i, a 256 x 240 fill took 5,872
  clocks and a rectangle 33,592 on the console with the rule off, and 3,096
  and 17,040 with it on (hwtest v2.4, records 0x843 to 0x84A): the skipped
  rows are nearly free. The cost model counted every row. Fills, rectangles
  and triangles now take the skipped rows' pixels off their cost (not their
  per-line term or their setup): 364 and 2,145 clocks for the same pair, 6%
  under and 0.7% over. The other rows the console timed in that mode read
  about half what the emulator gave: the in-cache 4-bit triangle rows 0x8E0
  to 0x8E6 fall from 9,959 to 5,578 against the console's 5.4k, and a block
  DMA waiting for a 512 x 256 rectangle (record 0x86B) reads 35,458 against
  35,630. 8 and 15-bit triangles read 12 to 14% over (see the texture cache
  entry).
- The hardware renderer draws axis-aligned quads (sprites, glyphs, UI boxes)
  as exact blocks at internal scales above 1. They were shifted half a pixel
  like slanted polygons, which at 3x moved a gradient-shaded font half a pixel
  and left a ghost column on one side and a missing sub-row on the other, most
  visible on text such as the WipEout title and VoXide's splash. A scaled
  sprite now covers the same pixels as the CPU image enlarged, with the CPU's
  texel in every sub-pixel; 1x output is unchanged byte for byte.
- A memory card answers its data bytes faster and its read's seek byte late.
  hwtest v2.4 timed every byte of a sector read on the console: the 128 data
  bytes drew `/ACK` 1,468 (slot 1) and 1,730 (slot 2) clocks after their
  write, where the emulator gave 1,689 and 1,897, and the byte after the
  sector address (the `5C` reply) drew none within the probe's 1,200-poll
  window, its `/ACK` arriving 11,530 clocks after the next byte's write on
  slot 2. The data delays now read 1,468 and 1,728 in the same probe, the
  seek byte is acknowledged about 30,000 (slot 1) and 23,000 (slot 2) clocks
  late, the values that keep a whole frame read at the console's 124 and 138
  to 139 HBlanks (records 0x641, 0x649, 0x693), and slot 2's data `/ACK`
  pulse is 68 clocks where the console showed 68 to 80. The late `/ACK` is an
  estimate fitted to the frame totals (the probe lost sync after it, so the
  console gave no clean figure); the emulator's card still takes the bytes
  clocked before it, where the console's seems to have dropped them.
- A block-mode GPU DMA that backs up behind the drawing waits for it, as the
  request line holds the channel on the console. The words already landed
  (since the FIFO pacing change); the transfer still finished in a few
  hundred clocks. It now ends when the GPU has taken the command at the head
  of the FIFO: a 64-word upload behind a 512x256 flat rectangle ran 160
  clocks and now runs 70,272, the rectangle's own cost. hwtest v2.4 records
  0x868 to 0x873 read 35,630 to 35,974 on the console, the same rectangle
  drawn with half its rows skipped (the 480i rule was on for that step).
  Main RAM loads wait only through the clocks the words move, not through
  the wait.
- The hardware renderer takes colour and texture gradients from the CPU
  rasterizer's own attribute planes instead of interpolating in f32. Each
  Gouraud and textured polygon now carries the plane constants
  (`tri_raster_setup`: `(base + x*dadx + y*dady) >> 24`, truncated to 1/4096)
  and the fragment shader evaluates them at the pixel, so at 1x its colours
  and texels are the CPU's byte for byte (a scaled target adds the gradient
  over the sub-pixel, so it stays smooth). A flat textured quad laid out as a
  sprite takes the CPU's four-edge Q12 walk instead, as the CPU does, and a
  quad whose half is too large is dropped whole. Random Gouraud, textured and
  sprite-quad tests now compare equal where they allowed a texel or a step.
  In 35 boots (800M instructions each) the hardware frame moves nearer the
  CPU's in 10 and away in none that could be compared (the CPU display of
  two others, oot and the WipEout PSoXide disc, differs between the builds
  compared); Crash, Quake, VoXide, the arcade, the demo disc and the
  courtyard now match to the 5-bit level, and Counter-Strike and Half-Life
  fell from 582 and 472 pixels apart to 39.
- The full-screen VRAM blit shows the VRAM's display codes unchanged. It
  rendered through an sRGB view of its target, so every code was encoded a
  second time: a grey ramp of 5-bit levels 1 to 31 read back as 49 to 255
  where the VRAM says 8 to 255, and the wireframe view (which rebuilds the
  target with it) and the VRAM debug view came out too bright. They now
  render through the raw-byte view, as the draw passes do, so the blit, the
  hardware renderer's own draws and the CPU display all agree byte for byte.
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
  the display area prohibited (GP0 E1h bit 10 clear) in 480i, fills and
  primitives skip the lines of the field on display (GPUSTAT
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
  WipEout 3 frame) change display hashes. hwtest v2.4 measured the rule on
  the console (fill, rectangle, triangles, textured quad and line follow it,
  copies and uploads do not, 480p and 240i are exempt) and it holds with the
  display blanked, so a blanked display no longer turns it off. Chrono Cross
  builds its menu background blanked and draws it with textured rectangles
  only, which the console test did not cover. With the rule applied to them
  the first frame shown of that menu (frame 2690 of the headless run) has
  every other line of the background, with the texture storage in the
  displayed area between them; the next frame is whole, as it was. Gran
  Turismo 2, Chrono Cross and WipEout 3 also run a little differently now,
  as the 480i draw cost is charged for those blanked draws too.
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
