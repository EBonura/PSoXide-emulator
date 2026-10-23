# Testing: the fleet gate

Every game in the library is tested by playing it. `psoxide-gate` drives the
game headlessly through the route a player takes (boot, splash, menus, new
game, each major feature), captures frames along the way, and judges them on
the CPU renderer and on the hardware renderer the app uses. It lives in
`tools/psoxide-gate`.

It exists because narrower gates let real bugs through. A New Game freeze after
the first cutscene passed every check that jumped straight to gameplay. A
garbled splash text passed every check that compared CPU frames, because the
fault was in the hardware renderer's upscale. Ships that bounced after a jump
passed a parity check that only measured ship 0. No kickoff countdown shipped
because nothing asserted anything after a goal. A person had to find each one.
The rules below close those holes.

## The rules

1. **A journey is required before a library swap or a main landing.** A game's
   `tests/journey.toml` must be green on the disc being shipped, and the contact
   sheet must have been looked at (frames, not just the exit code).
2. **New features ship with journey steps or asserts.** A change that adds a
   screen, a mechanic or a mode adds the steps that reach it and the checks that
   prove it works. A bug fix adds the check that would have caught it.
3. **Parity and remake work measures every entity.** A comparison against a
   reference covers each car, ship, enemy and actor, never only the first one.
   Use `ram` asserts with `each` for arrays.
4. **Goldens change deliberately.** A frame changes only through
   `psoxide-gate bless`, after reading the diff report.
5. **Known failures are marked, not hidden.** `expect_fail = true` reports XFAIL
   and passes for an evaluated assertion. XPASS fails until its marker is removed.
   A broken symbol or unreadable RAM fails even with the marker.

## Running it

Every run checks GPU list nodes against the 16-word FIFO (`PSOXIDE_CHECK_GPU_LL_FIFO=1`, set by the gate unless you set the variable; `=0` turns it off), because silicon freezes on a longer node and the emulator does not. It also fails a run that changes `cache_control` bus bits 13..16, 7 or 3, or sets `SR.IsC` (the BIOS `FlushCache`), while any DMA channel is busy (`PSOXIDE_CHECK_CC_DMA=1`, same default and `=0` switch; `warn` prints and counts instead of failing; see `docs/cache-control-dma-tripwire.md`).

```sh
cargo build --release -p psoxide-gate        # or: make gate
psoxide-gate list                            # each game, journey and whether it exists
psoxide-gate run nitroxide                   # one game, library disc
psoxide-gate run tests/journey.toml --disc dist/game.cue   # a journey file, a specific disc
psoxide-gate run all --jobs 2 --strict       # release gate (at most 2 emulator runs at once)
psoxide-gate bless oot                       # rewrite goldens, with a review report
```

Exit code 0 means every selected journey passed. 1 means a check failed or a
journey aborted. 2 means bad usage or an unreadable file. `--strict` is the
release gate: it requires a journey, checkpoints and CPU1, HW1 and HW3 at every
checkpoint. A missing named journey always fails. `make gate` uses `--strict`.
`make gate-pgo` runs the same gate from the profile-guided build (`target/pgo/release/psoxide-gate`, built by `make build-pgo` from `pgo/psoxide.profdata.gz`): identical results and frames, about 10 to 18% less CPU time per journey. Use it for long runs; a plain build stays the reference.

`--no-hw` and reduced `--scales` are diagnostic options and cannot pass the
release gate. The report goes to `gate-report/index.html` (or `--out`).

Which disc runs: a fleet name or `all` boots the library disc, the one a player
launches. A journey path boots the journey's own `disc` list, the game's normal
build output. `--library` and `--build` force either, `--disc` names one. The
`--repo name=/path` flag points a game at a different checkout (for example a
worktree) while its fleet entry stays the same.
An explicit `--library` or `--build` fails if that disc is absent; it never
substitutes the other one.

The fleet manifest is `tools/psoxide-gate/fleet.toml`: library entry, repo, and
journey path per first-party game. A game without a journey shows as MISSING.
Third-party titles are covered by the emulator's `validate` manifests instead.

## The report

`index.html` is a contact sheet. Each checkpoint writes a labeled matrix PNG:
CPU1 (reference), HW1 and HW3 across the first row at their native image sizes.
The second row shows HW1 versus CPU1 at 3x presentation size, then the surviving
HW3 differences. Both paths first map each RGB channel to its high five bits,
then use the same 5-to-8 bit expansion. This is deterministic normalization,
not channel tolerance. Cells print raw, normalized exact and surviving counts.
Magenta marks surviving differences over a dimmed normalized reference. Embedded labels identify mode,
scale, checkpoint, recorded frame identity and tick. Where the capture schema
has no video-frame counter, the label explicitly says `frame unrecorded`;
the emulation tick is a separate clock and is never substituted for a frame.

Raw captures and historical golden images are retained separately as data.
Nearest-neighbour CPU1 enlargement is comparison-reference data, not a render
mode. The only release render modes are CPU1, HW1 and HW3.

Every HW3 capture has paired reference/HW3 crops at 1:1 source-pixel size:
full-frame text/HUD coverage, declared strict regions, and every 64x64 tile
containing any surviving difference, including partial edge tiles. There is no
ranking, crop limit or omitted differing region. `<checkpoint>.crop-coverage.tsv`
records each rectangle and projected surviving count; non-overlapping tile
counts must sum to all surviving hardware pixels, with `omitted=0`. A native
3D error projects to its complete 3x3 hardware box for the display and crop audit.
`<checkpoint>.exact-diffs.tsv` separates raw counts, normalized exact counts,
2D hardware-pixel survivors and 3D native-pixel survivors. Raw captures and
raw zero-threshold diff PNGs remain separate data. Each game's sheet ends with
its established differences and unresolved buckets, or `clean`; incomplete
hardware captures cannot be clean. Never sum hardware and native pixel counts.

## The render matrix

Every checkpoint with `no_matrix` unset captures CPU1, HW1 and HW3 from the
same emulated state (`render.scales` changes the hardware list in diagnostic
runs). The CPU rasterizer runs at native resolution. The hardware renderers
run alongside it throughout the journey, fed each tick's GP0 log as in the
window, so the persistent target holds what a player would see.

- **CPU vs golden.** Exact by default. This catches any change to what the
  game draws.
- **2D content.** Text, HUD, sprites and axis-aligned textured quads compare
  exactly after normalization, at HW3 against nearest-neighbour CPU1: all nine
  hardware pixels of a native pixel equal the CPU pixel. No
  radius, channel or fraction allowance can hide shifted/missing glyphs.
  Declare native bounds with `strict = [{ x, y, w, h }]` or
  `comparison.two_d`. Conservative bounds include transparent holes and
  override polygon classes; they never hide underlying text/HUD errors.
  The renderer keeps the colour of an axis-aligned textured quad constant over
  each native pixel (a sheen over text steps per pixel), so such a region is
  exact at 3x even with Gouraud colours. A strict region must hold only 2D
  content: 3D surfaces (a held item, scenery) shade and rasterize at finer
  than native pixels at 3x by design and do not belong in one.
- **HW1 content.** Normalized channels compare exactly, including polygon edges.
  The hardware renderer reproduces the console's 5-bit colour arithmetic where
  it is visible: the four semi-transparency modes blend on 5-bit codes (the
  source is cut to its code and scaled so the fixed-function blender lands on
  the CPU's result for every pair of codes), and lines follow the console's
  own pixel walk, so HW1 equals CPU1 and any difference is a renderer fault.
- **3D content.** Only explicit final visible polygon runs qualify at HW3. Box-average
  canonical normalized 8-bit hardware channels, round once to an 8-bit channel,
  and compare to native CPU1. Never requantize the average to 5-bit codes.
  Interiors remain exact. A polygon's declared
  `edge_channel` (at most 24 display levels) applies only within half a native
  pixel of its finite actual boundary, and only on its final visible pixels.
  Hidden edges and overlapping final ownership are invalid. Axis-aligned
  textured quad opcodes are always 2D even if supplied as polygon metadata.
  Metadata vertices use a simple convex perimeter. Convert GP0 quad packet
  order (a triangle strip) to perimeter indices 0,1,3,2; malformed/bowtie
  metadata is rejected. Unsupported geometry stays unresolved and exact.
- **Unclassified content.** Everything not declared above. At HW1 every pixel
  compares exactly. At HW3 the **centre** hardware pixel of each 3x3 block
  compares exactly with the CPU pixel, and the other eight are not compared.
  This is not a tolerance. The CPU rasterizer samples a polygon, its colour
  and texture planes at the corner of each native pixel; the hardware renderer
  shifts polygons by half a native pixel and evaluates the same planes at the
  sub-pixel it shades, so the centre sample of a block is the CPU's own sample
  point: same coverage, same texel, same colour, same blend arithmetic. Any
  difference there is a hardware renderer fault, at every scale. The other
  eight sub-pixels are what the upscale adds (finer edges, texel boundaries
  and gradients inside the pixel); a 1x frame holds no value to compare them
  with, and requiring them to equal the CPU pixel could never pass a textured
  or shaded surface. (Until 2026-10-23 they were compared exactly, which made
  every 3D HW3 comparison fail. Bleed or shift of 2D content is still caught,
  by the 2D rule.) The count of uncompared sub-pixels is printed with every
  such result. Survivors are reported as unresolved; never declare them 3D
  errors just because they differ. Final contribution data for a 3D class
  must come from observable primitive/state evidence or an explicit reviewed
  region declaration; historical triangle unions and image gradients are
  insufficient. Invalid geometry, ownership, bounds or provenance fail closed.

`comparison.polygons` records the observed GP0 `opcode`, perimeter-ordered
native `vertices`, non-overlapping final `visible` pixel runs, `evidence` and
`edge_channel`. Capture manifests bind this declaration to full raw-image,
source-report and BIN hashes. A 3D mask must exclude occluded contributions;
mixed transparency remains exact unless its visible classes are established.

Frames the hardware renderer defers to the CPU (24bpp, screen offset, 480i
field rendering) are reported SKIP with the reason in diagnostic runs. A
required renderer SKIP fails the release gate.

`tolerance` and `mask` continue to apply to CPU golden/assert comparisons.
Hardware comparisons do not ignore masked pixels. The old `hw1`, `hwn` and
`render.strict` channel tolerances are gone (the gate stopped reading them when
comparison became exact) and a journey that still names them is rejected as an
unknown key. Hardware tolerance exists only at explicit visible polygon edges. A capture refresh reports new presentation analysis,
preserves prior acceptance receipts, and is not a fresh installed-disc run.

## Journey files

`tests/journey.toml` in the game repo, beside `tests/golden/<name>/*.png`.
TOML, unknown keys are errors. Top level:

| key | meaning |
|---|---|
| `name`, `title` | id for the golden folder and report; display name |
| `disc` | disc images relative to the repo root, first that exists |
| `symbols` | ELF, lld link map or `address name` list, repo-relative, all loaded |
| `addr` | table of fixed addresses by name |
| `pad`, `pad2` | `analog` (default), `digital`, `dualshock`; port 2 defaults to `none` |
| `memcard` | `fresh` (default) or `none` |
| `max_ticks` | hard stop |
| `[render]` | `scales` default |

Time is in route ticks, the headless frontend's clock: one vblank period of
emulated cycles, about 60 a second. `launch --press` and the editor's tape use
the same tick, so a route found with one reproduces in the other.

Steps are tables, run in order:

- **Input.** `press = ["cross"]` (cross circle square triangle start select up
  down left right l1 r1 l2 r2 l3 r3), `lstick = [x, y]`, `rstick = [x, y]`
  (0..255, 128 centred, y 0 up), `hold` ticks (default 4, one pad poll), `wait`
  ticks of rest afterwards, `repeat` to do hold + wait several times.
- **Port.** `port = 2` sends the step's input to pad 2 (the journey needs
  `pad2`). `[step.other]` holds a second pad's `press` / `lstick` / `rstick` on
  the other port during the same hold, for two-player play. Every step says
  which port it uses; the default is 1.
- **Plain wait.** `wait = 300`.
- **Conditional wait.** `wait_until = { kind = "...", ... }` with `timeout`
  ticks (default 3600). A timeout aborts the journey and reports the frozen
  frame. Allowed kinds: `ram`, `pixels`, `flat`, `not_flat`, `dark`,
  `not_dark`. Prefer these to long fixed waits: they keep a journey aligned
  when a build loads faster or slower, and a freeze becomes a precise failure.
- **Conditional input hold.** `lstick = [128, 0]` with
  `hold_until = { kind = "ram", sym = "PLAYER_POS", offset = 8, signed = true, op = "le", value = 690 }`
  and a required positive `timeout` keeps the ordinary pad input down until
  the condition succeeds or the tick bound expires. The pad is released on
  success, timeout and guest error; `wait` can add rest after release. RAM and
  pixel conditions use the same fields as assertions. `hold`, `repeat`,
  `wait_until`, tapes and card actions cannot share this step.
- **Checkpoint.** `checkpoint = "name"` captures the frame and runs the step's
  asserts. `golden = false` skips the golden comparison (animated or
  timing-dependent frames still get liveness and matrix checks).
- **Asserts.** `[[step.assert]]` tables, also usable on a step with no
  checkpoint.

| kind | passes when |
|---|---|
| `presenting` | the display showed a new frame in at least `min_changes` of the last `window` ticks (buffer flip or changed pixels): catches freezes |
| `not_flat` | no single colour covers more than `max_dominant` (0.98) of the frame: catches black-screen hangs |
| `pc_not_stuck` | at least `min_distinct` (24) 16-byte code lines executed over the last `window` ticks: catches a loop |
| `audio` | the SPU output peaked at `min_peak` or more over the last `window` ticks |
| `pixels` | `min_count` / `max_count` pixels of `color` (within `color_tol`) in `region`: a HUD heart, a cursor, a countdown digit |
| `frame_diff` | the share of pixels that differ from an earlier checkpoint (`from`) is within `min_changed` / `max_changed`: "nothing moved", "the screen changed" |
| `flat`, `dark`, `not_dark` | the frame is one colour or dark: anchors for cuts to black between scenes |
| `ram` | a word at `sym` (or `addr`) plus `offset` satisfies `op` (`eq ne lt le gt ge between zero nonzero`) and `value`; `size` 1, 2 or 4, `signed`, and `each = { count, stride }` for arrays, all elements must hold |

Any assert takes `expect_fail = true` and a `note`.

`ram` reads emulated main RAM without disturbing timing. Symbols resolve by
exact name or by `::` path suffix against an ELF symbol table (legacy Rust
mangling is undone), an lld map, or an `address name` list. An unresolved
symbol fails the assert with the reason.

### Checkpoint rules of thumb

- Take goldens at still moments: menus, panels, a held pose. Animated scenes
  depend on exact load and boot timing; take liveness and matrix checks there.
- Anchor on the screen, not the clock, wherever a load or cutscene precedes the
  checkpoint (`dark` then `not_dark`).
- Assert what a person would notice: the HUD is there, the countdown shows,
  nothing moves while it should hold, the game is still presenting.
- Give every checkpoint `presenting` and `not_flat` unless it is meant to be
  still or black.

## Goldens

CPU frame at native resolution, PNG, under `tests/golden/<name>/`, a few tens of
kilobytes each. `psoxide-gate bless <game>` runs the journey, refuses if any
assert (not golden, not matrix) failed unless `--force`, writes the PNGs, and
prints new / changed / unchanged per checkpoint with the diff statistics. The
review report beside it shows old golden, new frame and exact magenta difference. Commit the
PNGs with the change that caused them.

A golden belongs to one build. A different build that changes pixels (a
fidelity round, a perf change that shifts load times) fails its goldens until
re-blessed, which is the point. `--golden-dir` reads or writes another folder,
for checking an older disc against its own goldens.

## Known gaps

- **Stack-resident state has no symbol.** `ram` asserts need an address.
  Statics have one in the ELF or link map; a struct that lives on `main`'s
  stack (NitroXide's whole match state) does not. See NitroXide's journey for
  how it is located. Long-term fix: give gate-visible state a named static, or
  teach the gate to read DWARF member offsets (`gimli` and `object` are already
  in the dependency cache).
- **Port 2 is implemented and parsed, but no journey drives it yet.** WipEout's
  2-player split and NitroXide's versus mode need a `pad2` journey; the harness
  has what they need (`pad2`, `port`, `[step.other]`).
- **Audio is a level check.** `audio` proves sound is playing, not that it is
  the right sound.
- **GUI-only paths.** Window sizing, the egui chrome and real-time pacing are
  not exercised; the gate runs the emulation and the hardware renderer's draw,
  not the window.
- **No journey yet** for most of the fleet. `psoxide-gate list` shows which.

## Card save journeys

A save journey can boot a raw 131,072-byte card through `card_fixture =
{ path = "tests/cards/save.mcd", sha256 = "..." }`. Supply the complete SHA256
of that exact fixture. The source file is only read. A missing, shortened or
changed fixture aborts; the gate never falls back to a fresh card.

Set `card_observe = true` to observe real guest card traffic one instruction
at a time while a card transaction is active. `card_irq_ack` requires an
observed interrupt vector entry and return spanning an entire actual card ACK
pulse. The report includes IRQ and ACK cycle spans, EPC, command, frame, byte
count and CTRL. No IRQ is injected and card timing is unchanged. A journey
whose save never overlaps an IRQ fails this assertion, so choose a recorded
route that exercises the overlap. Ordinary journeys keep their usual batching.

A step with `tape = "tests/tapes/save.pxtape"` plays a poll-bound tape through
the guest's normal pad interface, ending after all samples have been consumed.
Video-frame tapes and empty tapes are rejected. Playback is bounded by
`max_ticks` and a 3600-tick default step limit (`timeout` overrides it).
By default the tape keeps its recorded absolute `start_poll`. Set
`tape_rebase = true` on the tape step to play sample zero at the current
completed port-1 poll after earlier journey steps. This requires a tape and
does not change its bytes. The report records its SHA256, recorded and
effective start polls, sample count and end poll. Both modes reject an end
poll that overflows. The final sample remains live through the remainder of
its route tick, then both pads are released after completion or timeout.

Card actions need their own steps:

```toml
[[step]]
card = { action = "snapshot" }
[[step]]
card = { action = "reboot" }
[[step]]
tape = "tests/tapes/save.pxtape"
card = { action = "power_cut", command = 87, byte_index = 70, occurrence = 3, timeout = 1800 }
```

`snapshot` persists the current card. `reboot` persists and syncs it, reads it
back, and boots a new CPU/bus with those bytes. `power_cut` plays its optional
tape, or holds its optional buttons, until the selected command occurrence
accepts the requested byte, then persists and reboots immediately. The command
occurrence is counted from the start of that step, not the boot. The trigger
must occur within its timeout or the journey aborts. Write command 87 uses
accepted byte counts 7..134 for data and 135 for the checksum that commits a
sector in the model. The model accepts bytes at transfer start, so this is a
protocol interruption boundary, not an analog power-loss simulation.

Each action writes a new `cards/card-step-NNN.mcd` under the journey's report
folder. Existing files are refused, so use a new report directory for each run.
The HTML links the exact images and their full hashes. Reboot preserves the
journey's total tick budget, resets CPU/bus history, clears pads and recreates
the hardware renderers. Follow it with a normal load route and RAM assertions
for the selected save, game state and error counters. A card's committed sector
count alone cannot prove the guest finished its save.

`card_writes` checks successful sector commits in the card's recent protocol
events (up to 256 events), with `min_count` defaulting to 1. `card_unchanged`
compares the complete live card with the initial journey fixture. Always pair
these transport checks with guest save/load assertions. Early, middle and
pre-checksum cuts should retain the previous valid save; a cut after publishing
the new directory entry may load the complete new save. Check that exact
outcome through the game, and retain a live-ACK guest as a negative control for
`card_irq_ack` plus guest save success. Fault-only journeys may use `--no-hw`;
they supplement the normal strict visual journey and its reviewed contacts.

For a diagnostic IRQ fault, use `card = { action = "irq_during_ack",
command = 82, byte_index = 82, occurrence = 1, ack_lead = 64, timeout = 1800 }`
on a tape or input step, with `card_observe = true`. This raises one VBlank
pending interrupt through the normal interrupt controller before the selected
scheduled ACK. It neither changes ACK width nor guest RAM/PC. The action fails
unless that specific pulse is completely covered by the actual guest IRQ
handler, and reports the injected source/cycle separately from natural overlaps.
Run old and fixed guests with identical emulator binary, execution tier, tape,
card and injection settings. An unrelated overlap elsewhere in the journey
cannot satisfy the injected event's completion check.

## Refresh retained evidence

`psoxide-gate refresh-report captures.toml --out <new-directory>` composes
reports from verified raw captures without starting an emulator. Each
`[[capture]]` record supplies `game`, `title`, `checkpoint`, `tick`, absolute
`bin`, `report`, `cpu`, `hw1`, `hw3` paths and a full SHA256 in the matching
`*_sha256` field. `strict` is the journey's array of native text/HUD rectangles;
`receipt` and `notes` can identify the external artifact binding evidence.
The command validates file hashes, renderer dimensions, the source report's
BIN prefix and checkpoint tick. Historical reports do not record a video-frame
counter, so omit `frame`. Artifact identity must first be verified against the
installed library receipt; a report hash alone does not certify installation.
It requires a new output directory and preserves the original reports.
The refreshed report shows SKIP for acceptance and links the original report;
only exact visual counts are recomputed.

`psoxide-gate before-after pair.toml` writes a deterministic review PNG with
one 3x presentation row with readable embedded labels. Set top-level `out` to a new absolute PNG
path; `[before]` and `[after]` each have absolute `image`, full `sha256`, `build`,
`checkpoint`, `mode`, `scale`, and recorded `tick`. Both sides must have matching
checkpoint, mode, scale and image dimensions. Embedded labels and stdout give
the exact changed-pixel count, or `no visible change` with zero. Raw screenshots
remain unmodified. Retain this PNG for every visible landing/library swap and
show it inline with the previous installed and new build identities.
