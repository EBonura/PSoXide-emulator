# Conformance check

The emulator's default model (`real`) does what the console does, and the
console accepts some things that no commercial program ever does and that other
emulators, or other implementations of the hardware, do not accept. A program
that relies on one of them passes every gate here and on the console, then
stops elsewhere. The conformance check counts them instead.

Every run records, per rule, how many times the guest broke it and the guest PC
of the first time (`crates/emulator-core/src/conformance.rs`). Recording changes
no behaviour and no result.

## In the gate

`psoxide-gate run` prints a line for every journey whose guest broke a rule:

    CONFORMANCE nx-installed cd-ack-partial=75 first at 0x80021134 (<psx_io::cd::reader::SectorReader>::send_command_waiting+0x138); cd-ack-param-reset=30 first at 0x80061268 (psx_io::cd::pause::PauseDrive>::status+0x98); gpu-irq-command=6624 first at 0x8007b670 (psx_io::gpu::settle_gpu_channel) (114 CD acknowledges, 6624 GP0(1Fh))

(NitroXide built at SDK 3779d464513f, 60 seconds from boot.) The counts in
brackets are every CD acknowledge and GP0(1Fh) the guest wrote, rule broken or
not, so a clean line also shows the run reached the CD; once the guest drove
the MDEC they also count its commands that took parameter words.

The symbol comes from the journey's `symbols` files (the nearest symbol at or
below the PC). Without `--conformance` that line is all: no journey changes
verdict. With `--conformance` every rule becomes a check of a `conformance`
group, and a rule the guest broke fails the journey; a clean journey prints
`CONFORMANCE <name> clean`.

`--conformance=cd-ack-partial,cd-ack-param-reset` enforces only the rules it
lists. A broken rule outside the list is still counted and printed, marked
`[not enforced]` in the line and as a skipped check in the group; it never
fails the journey and is never dropped. An unknown rule id is an error.
`--conformance` alone is `--conformance=all`. The selection is part of the
receipt key.

A game switches it on by adding `--conformance` (or a list) to its gate
command line, for example the CD rules once its SDK acknowledges with 07h,
while the GP0(1Fh) rule waits for its presenter. The
PCs need the `conformance` cargo feature, which the gate builds by default (the
player binary leaves it out: it costs one check per interpreted instruction).

## Model equivalence (`SIGNALS`)

The hardware models change emulated behaviour in three places only: the GP0(1Fh)
model (`gpu-irq`, which is also all that `mister` changes, with its `seed` and
`trace`) when a GP0(1Fh) executes and when the CPU writes one in command position;
`cd-ack=strict` on a CD acknowledge that breaks a CD rule; `mdec-req=hidden` on
the MDEC status request bits, which rise only after an MDEC command. Two runs on
different models are the same run until the first such event, so a run that met
none of the events a pair of models differs on is the other model's run too.

Every run prints what it met, after `AREAS` and `DIGEST`:

    SIGNALS <journey> <model> gp0-1f=N gp0-1f-written=N cd-ack-broken=N mdec-commands=N golden-on=yes|no

`gp0-1f` counts executed GP0(1Fh), `gp0-1f-written` the words the CPU wrote in
command position (a word a GPU reset drops before it runs still posts its status
latch differently), `golden-on` says the journey limits goldens to some models
(its check list then depends on the model). `--second-hardware` lets the second
model take the first one's result only when the first passed, both GP0(1Fh)
counts are zero, no CD rule broke, the journey names no `golden_on`, and either
both models read the MDEC status alike or no MDEC command was written.

## Adding a rule

A rule needs all three: commercial software never does it (check a few
commercial discs: a commercial disc that trips it means the rule is wrong), at
least one other emulator or hardware implementation is known to reject or
mishandle it, and the detection is exact (no guessing at what the guest meant).
Give it an id, one line in `Rule::summary`, and a section here with the evidence.

## Rules

### `cd-ack-partial`

A write to the CD-ROM interrupt flag register (`1F801803h`, index 1) while a
response interrupt is pending (`HINTSTS` bits 0-2 not zero), whose bits 0-2 are
not zero and whose bits 0-4 are neither `07h` nor `1Fh`. Writing the interrupt
number just read (`03h` for INT3) is the usual case.

Evidence:
- PCSX-Redux (`src/core/cdrom.cc`, the 1F801803.1 write): only `07h` and `1Fh`
  acknowledge; a value with bit 6 resets the parameters; `18h` and its subsets
  are ignored; anything else logs "not available yet" and pauses the emulator.
  The code dates from the CD-ROM rewrite of December 2022; its commits give no
  hardware reason, and Redux's own CD-ROM hardware tests acknowledge only with
  `07h`, `1Fh` or the buffer bits, so this is an unimplemented case rather than
  a claim about the console.
- Commercial code, the BIOS and PSn00bSDK write `07h` or `1Fh`.
- The console accepts any value. The decoder datasheet text that Redux quotes
  (CXD1815Q `HCLRCTL`) and psx-spx both describe bits 0-4 as independent
  "write 1 to clear" bits; psx-spx notes that writing `01h` to an INT3 leaves an
  INT2. Writing back the number just read therefore clears it completely. The
  MiSTer PSX core does the same (`CDROM_IRQFLAG and not value`).
- One way the value can still matter on a console (inferred, not measured):
  psx-spx reports that the flag bits are not synchronised to the CPU clock, so
  a read during the change can see `1` on its way to `3`, or `4` on its way to
  `5`. Writing that glitched value back clears one bit and leaves `2` or `1`
  pending; `07h` cannot.

### `cd-ack-param-reset`

A write to `1F801803h` index 1 that sets bit 6 (reset the parameter FIFO) and
any of bits 0-4 in the same byte, for example `5Fh`.

Evidence: PCSX-Redux takes bit 6 first, resets the parameters and returns
without acknowledging, so the guest waits on an interrupt that never clears.
Commercial code acknowledges and resets the parameters in two writes (`1Fh` or
`07h`, then `40h`). On the console both bits are independent (same sources as
above).

### `gpu-irq-command`

A GP0(1Fh) command (GPU interrupt request, raises GPUSTAT bit 24 and I_STAT
bit 1). Its first PC is the last CPU store to GP0 or to DMA channel 2's control
register before it ran: the store that wrote it, or the one that started the
list that held it.

Evidence:
- PCSX-ReARMed ignores GP0(1Fh) in every GPU plugin and never sets GPUSTAT
  bit 24 (read in its source).
- The MiSTer PSX core on a SuperStation One raised bit 24 for 0 or 1 of 24
  requests in most hardware-test rows (measured; the `mister` target's
  `gpu-irq` models reproduce it). The retail console raised it every time.
- psx-spx: rarely used; the one game it names does not enable the interrupt.
  Sony's library, the BIOS's `GPU_sync` and PSn00bSDK wait for drawing with DMA
  channel 2 and GPUSTAT bits 26 and 28, not GP0(1Fh).

The rule counts the command, not a wait on bit 24: a program that issues it and
never waits on it also trips it. Replace the command rather than argue the case.

### `mdec-status-before-feed`

A CPU read of the MDEC status register (`1F801824h`) after a write to
`1F801820h` that starts a command taking parameter words (decode, quantisation
table, scale table) and before DMA channel 0 is started for it (CHCR written
with the start bit). The wait ends at the DMA0 kick, at an MDEC reset, or when
the CPU has written the command's last parameter word itself, so a status read
between CPU-fed parameter words counts too.

This is how a program waits for the data-in request (status bit 28) before it
feeds the command. The count is of reads in that span, not of a bit test: a
load of the status register returns every bit, and none of the commercial code
checked reads it there at all.

Evidence:
- PCSX-Redux (`src/core/mdec.cc`) and PCSX-ReARMed (`libpcsxcore/mdec.c`)
  define `MDEC1_DREQ` and never set it: status bits 27 and 28 always read 0.
  Both start DMA0 on the CHCR write alone (`0x01000201`). Neither takes CPU
  parameter words: a write to `1F801820h` replaces the command register.
- Commercial code and PSn00bSDK write the command, start DMA0 and let the
  channel wait for the request (PSn00bSDK `libpsn00b/psxpress/mdec.c`:
  `DecDCTPutEnv` and `DecDCTin` write `MDEC0`, then `DecDCTinRaw` starts the
  channel; `DecDCTinSync` polls bit 29 before the next command, never bit 28).
  Ten commercial discs that play movies read the status register between such
  a command and its DMA0 kick 0 times (below).
- The SDK's FMV player waited on bit 28 after each table command and, when it
  never rose, refused to play: no movie of a game built on it started on either
  emulator. The SDK now starts DMA0 straight after the command (`psx-fmv`
  `Mdec::upload`).

`--hardware real,mdec-req=hidden` (see `docs/hardware-targets.md`) makes the
request bits read 0, as on those two emulators, so a journey shows the failure
headless.

## Checked against commercial discs

With `--conformance`, 60 seconds from boot with no input, these are clean
(each wrote between about 1,700 and 8,000 CD acknowledges and no GP0(1Fh)):
Crash Bandicoot, CTR, WipEout, Gran Turismo 2, Final Fantasy IX, Metal Gear
Solid, Soul Reaver, Nightmare Creatures. Only boot, title and attract code ran;
in-game code paths are not covered by that sample.

For `mdec-status-before-feed`, with `--conformance`, 90 seconds from boot with
no input, the discs that drove the MDEC in that time are clean (MDEC commands
with parameters in brackets): Chrono Cross (853), Final Fantasy IX (742),
Formula One 2001 (685), Gran Turismo 2 (1,895), Marvel vs. Capcom (145),
Nightmare Creatures (1,283), Resident Evil 3 (234), Soul Reaver (459),
Spider-Man (237), WipEout (1,546). A trace of every MDEC access on five of them
also showed no parameter word written by the CPU (Resident Evil 3 read the
status register about 4.5 million times, all outside the span). Crash
Bandicoot, CTR, Metal Gear Solid and Valkyrie Profile drove no MDEC in that
time.

## Considered and left out

Polling the controller's /ACK input level (`JOY_STAT` bit 7) instead of the
latched interrupt bit (bit 9): PCSX-ReARMed never sets bit 7. A load of
`JOY_STAT` returns every bit, so the emulator cannot tell which bit the guest
tests without decoding the code that follows the load. Not exact, so not a rule.

## `cd-ack=strict`

`--hardware real,cd-ack=strict` (see `docs/hardware-targets.md`) makes a write
that breaks a CD rule clear nothing, as on PCSX-Redux, so a journey shows the
resulting hang headless.
