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
not, so a clean line also shows the run reached the CD.

The symbol comes from the journey's `symbols` files (the nearest symbol at or
below the PC). Without `--conformance` that line is all: no journey changes
verdict. With `--conformance` every rule becomes a check of a `conformance`
group, and a rule the guest broke fails the journey; a clean journey prints
`CONFORMANCE <name> clean`. The flag is part of the receipt key.

A game switches it on by adding `--conformance` to its gate command line. The
PCs need the `conformance` cargo feature, which the gate builds by default (the
player binary leaves it out: it costs one check per interpreted instruction).

`--second-hardware` treats a journey as the same on both models only when the
guest issued no GP0(1Fh) and broke no CD acknowledge rule (`cd-ack=strict`
differs from the console on exactly those writes).

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

## Checked against commercial discs

With `--conformance`, 60 seconds from boot with no input, these are clean
(each wrote between about 1,700 and 8,000 CD acknowledges and no GP0(1Fh)):
Crash Bandicoot, CTR, WipEout, Gran Turismo 2, Final Fantasy IX, Metal Gear
Solid, Soul Reaver, Nightmare Creatures. Only boot, title and attract code ran;
in-game code paths are not covered by that sample.

## Considered and left out

Polling the controller's /ACK input level (`JOY_STAT` bit 7) instead of the
latched interrupt bit (bit 9): PCSX-ReARMed never sets bit 7. A load of
`JOY_STAT` returns every bit, so the emulator cannot tell which bit the guest
tests without decoding the code that follows the load. Not exact, so not a rule.

## `cd-ack=strict`

`--hardware real,cd-ack=strict` (see `docs/hardware-targets.md`) makes a write
that breaks a CD rule clear nothing, as on PCSX-Redux, so a journey shows the
resulting hang headless.
