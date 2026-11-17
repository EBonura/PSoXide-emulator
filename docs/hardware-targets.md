# Hardware targets

The emulator is calibrated against a retail PS1 (`real`, the default). A second
target, `mister`, models the MiSTer PSX core (480094b) as it runs on a
SuperStation One, so a program's behaviour on that core can be reproduced and a
fix checked here. Nothing changes under `real`.

Select with `--hardware SPEC` on `psoxide-emulator` (GUI and every subcommand)
and `psoxide-gate run`, or with `PSOXIDE_HARDWARE=SPEC`:

    real[,cd-ack=any|strict][,mdec-req=shown|hidden]
    mister[,gpu-irq=never|core|parity|exact|reliable][,cd-ack=any|strict][,mdec-req=shown|hidden][,seed=N][,trace=N]

`mister` also answers to `mister-fpga`, `fpga` and `superstation`.

## `cd-ack=strict`

Not a console: the CD-ROM interrupt acknowledge as PCSX-Redux has it. A write
to `1F801803h` index 1 that breaks a CD rule of the conformance check
(`docs/conformance.md`: a partial acknowledge of a pending response, or an
acknowledge combined with the parameter reset in one write) clears no response
bit; the parameter reset still happens. The guest then waits for an interrupt
that never goes away, as it does on Redux (which pauses there). Every other
write behaves as on `real`. Use it to reproduce that failure headless; it
changes nothing for a program that passes the conformance check. The default,
`cd-ack=any`, is the console (write 1 to clear, any value).

## `mdec-req=hidden`

Not a console: the MDEC status register as PCSX-Redux and PCSX-ReARMed have it.
Bits 27 (data-out request) and 28 (data-in request) always read 0. The
transfers run as on `real`: DMA0 still waits for the request inside the
emulator and DMA1 still drains, so only a guest that reads those bits from the
CPU sees a difference. A program that waits for bit 28 before feeding a
command (`mdec-status-before-feed` in `docs/conformance.md`) never sees it, as
on those emulators. The default, `mdec-req=shown`, is the console.

## What `mister` changes

GP0(1Fh) (see `crates/emulator-core/src/gpu/irq_quirk.rs`). On the core the
request pulse is missed by the `clk1x` process that sets GPUSTAT bit 24 and
I_STAT bit 1 unless a draw ahead of the word shifted its phase. Measured on the
core with the GPU DONE hardware-test rows (campaign lanes fpga-irq, fpga-core):
a 1Fh popped on an idle GPU is lost every time (also confirmed in GHDL on
`gpu.vhd`), one big triangle or a rect of 16 pixels or more ahead of it passes,
small rects and short fills fail, long chains almost always fail, and VRAM
copies pass about half the time. `gpu-irq=exact` (the default of `mister`) is the one fitted to the GHDL rules and the rows; `gpu-irq=core` is a simpler fit to those rows,
`parity` is the physical reading (chains XOR draw parities), `exact` takes the
per-command parities from cycle-accurate GHDL runs of the core's `gpu.vhd`
(rects pass at an even pixel count, fills and triangles add an odd duration,
copies are a coin, commands too short to still be running when the next word
arrives lose the request), `never` is the extreme. A word a program sends right after polling GPUSTAT counts as sent to an
idle GPU. Coin flips use a fixed seed.

Other core differences join the same target as further fields of
`HardwareTarget` (`crates/emulator-core/src/hardware_target.rs`).

Gate goldens are recorded on `real`; a difference under `mister` is a finding
about the program on that core. `bless` refuses to run under it.

## Strict Expansion 2 stores

A retail console maps 128 bytes of Expansion 2 after the BIOS has run (the
size field, bits 16 to 20, of the delay/size register at `0x1F80101C` holds
`0x00070777`). A store to `0x1F802080`, the debug text port that PCSX-Redux and
this emulator print, is outside that window and is a data bus error on the
console: a disc whose program stored there stopped at boot on its fault panel
with exception code 7, after passing every emulator run.

Strict mode is a cargo feature, `strict-exp2`, compiled out of the default build
so the default emulator carries no trace of it. `make build-strict` builds the
binaries that have it into `target/strict/release/`; `make test-strict` runs its
tests. In those binaries `--strict-exp2` (or `PSOXIDE_STRICT_EXP2=1`; also
`psoxide-gate run --strict-exp2`) models it. A default build asked for strict
mode stops at start with a message naming the build command.

A store beyond the configured window, to any
address of the 8 KiB Expansion 2 region, takes the exception: CAUSE.ExcCode 7
(with BD in a delay slot), EPC at the store or at its branch, BadVaddr left
alone, the store not performed. The window follows the size register. It is
off by default because the emulator's debug text goes through the same port
and the gates read it; run a program that is meant for a console under
`--strict-exp2` to prove it never reaches the port.

Only stores are modelled: the one console observation is a store. Whether a
load from outside the window faults too is not known, so loads are untouched.
The telemetry port at `0x1F802F00` is beyond the window too and faults.

What a program does with the fault is up to its runtime. The SDK runtime's
exception handler halts on a bus error only when the store sat in a branch
delay slot (CAUSE.BD set); otherwise it skips the instruction and carries on.
The console's first disc faulted at boot because its store was in a delay slot;
the emulator-only build that prints through the port is not, so under
`--strict-exp2` it takes the fault about thirty times and continues. Judge a
strict run by the exceptions taken (`PSOXIDE_TRACE_EXC=1` prints each), not by
whether the program stops.
The console's CAUSE also had bit 30 set (`0xC000001C`); the emulator does not
model that bit for any exception and leaves it clear.
