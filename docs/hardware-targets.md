# Hardware targets

The emulator is calibrated against a retail PS1 (`real`, the default). A second
target, `mister`, models the MiSTer PSX core (480094b) as it runs on a
SuperStation One, so a program's behaviour on that core can be reproduced and a
fix checked here. Nothing changes under `real`.

Select with `--hardware SPEC` on `psoxide-emulator` (GUI and every subcommand)
and `psoxide-gate run`, or with `PSOXIDE_HARDWARE=SPEC`:

    real
    mister[,gpu-irq=never|core|parity|exact|reliable][,seed=N][,trace=N]

`mister` also answers to `mister-fpga`, `fpga` and `superstation`.

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
