# Cache-control / DMA tripwire

`PSOXIDE_CHECK_CC_DMA` fails a run (or, with `warn`, prints and counts) when the
CPU reconfigures its bus while a DMA channel is busy:

- a write to `cache_control` (`0xFFFE0130`, word, half or byte) whose old and new
  values differ in bits 13..16, 7 or 3;
- a write to COP0 Status that sets `IsC` (bit 16) when it was clear;
- the HLE kernel's `A(44h) FlushCache`. The emulator is HLE-BIOS only, so that call
  stands for the real BIOS routine, which writes `cache_control = 0x804` and sets
  `IsC`.

A channel is busy when its CHCR bit 24 is set.

Why: on a console, with DMA2 walking a GPU list, the next instruction fetch after
such a write returned the DMA's list word instead of the ROM word (Quake PSX
bisect disc, 2026-10-22: BIOS `FlushCache` under a live walk executed a GPU list
header as code). The emulator ignores those bits and does not arbitrate CPU and
DMA words, so it runs the same program happily.

Values: `1` or `fail` panics at the first hit (message: PC, old and new
`cache_control` or SR, and MADR, BCR and CHCR of every busy channel); `warn`
prints the first few hits and counts all (`Bus::cc_dma_hits`); `inject` is `warn`
plus a model of the fault: the CPU's next instruction fetch that reaches the bus (a
cache hit never does) returns the word at the busy channel's MADR (DMA2 first)
and the guest executes a list header as code. For the HLE `FlushCache` the glitch
lands on the instruction at the return address, since the BIOS routine itself is
not run. Inject turns the block cache off from the first hit. Anything else is
off. `psoxide-gate` sets `1` unless the variable is already set.

The fix in a guest is to mask interrupts, wait for every DMA channel (and the
present queue's walk) to go idle, and flush inside the same masked window, or to
flush by eviction without touching `cache_control` or `IsC`.
