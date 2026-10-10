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
release gate: it requires a journey, checkpoints and CPU/HW 1x and 3x at every
checkpoint. A missing named journey always fails. `make gate` uses `--strict`.
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

`index.html` is a contact sheet. For each journey: the disc and its hash, the
timings, and per checkpoint a row of frames: old golden, CPU 1x, CPU 3x,
golden vs new heat map, then the hardware frame at each scale with a heat map
against the corresponding CPU frame. Each checkpoint also writes a full-size
`<checkpoint>.matrix.png` page: CPU 1x and HW 1x across the top, CPU 3x and
HW 3x below. The individual frames remain available for close review. Below
the frames, every check has PASS, FAIL, XFAIL, XPASS or SKIP and its measured
values. One line per journey also goes to stdout.

Heat maps: black is identical, dim blue-grey is a difference inside the
tolerance, red through white is a pixel outside it.

## The render matrix

Every checkpoint with `no_matrix` unset has four reviewable images from the same
emulated state: CPU 1x, CPU 3x presentation by nearest-neighbour enlargement,
hardware 1x, and hardware 3x (`render.scales` changes the hardware list in
diagnostic runs). The CPU rasterizer runs at native resolution, so CPU 3x is an
enlarged reference, not a second rasterization. The hardware renderers run
alongside the CPU one throughout the journey, fed each tick's GP0 log as in the
window, so
the persistent target holds what a player would see.

- **CPU vs golden.** Exact by default. This catches any change to what the
  game draws.
- **HW 1x vs CPU.** The CPU path keeps 15-bit colour and the hardware path
  shades in 8 bits, so smooth surfaces differ by up to 7 levels per channel
  with no fault. The default tolerance is 24 levels on at most 0.5% of pixels.
- **HW 3x vs the CPU frame enlarged.** Edges fall up to a pixel away from where
  nearest-neighbour enlargement puts them, so a pixel passes if a pixel within
  3 of it in the enlarged frame has a matching colour. Wrong colours, missing
  or smeared content, and black frames still fail. Default 48 levels on at most
  4%.
- **Strict regions.** Mark 2D-only rectangles (text, menus, splashes, panels)
  with `strict = [{ x, y, w, h }]`, in native pixels. Each rectangle is judged
  alone against the enlarged CPU frame, tolerating only a one-pixel shift, on
  at most 0.2% of its pixels. This is what sees an upscale bleed: with the
  hardware renderer before commit 5b2b5b7, VoXide's "Built with PSoXide" strict
  rectangle fails at 3x with 96 stray pixels (0.39%), and the inventory,
  crafting and options panels fail too; on the fix every one of them is exact.
  Do not mark regions with 3D showing through, or scenery edges fail them.

Frames the hardware renderer defers to the CPU (24bpp, screen offset, 480i
field rendering) are reported SKIP with the reason in diagnostic runs. A
required renderer SKIP fails the release gate.

Per-checkpoint overrides: `tolerance` (golden), `hw1`, `hwn`, `mask`
(rectangles ignored by every comparison, for clocks and random particles).

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
| `[render]` | `scales`, `hw1`, `hwn`, `strict` defaults |

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
review report beside it shows old golden, new frame and heat map. Commit the
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
`max_ticks` and a 3600-tick step limit. Input tape hashes appear in the report.

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
