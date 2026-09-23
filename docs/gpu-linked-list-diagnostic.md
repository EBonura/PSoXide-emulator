# GPU linked-list packing diagnostic

Set `PSOXIDE_CHECK_GPU_LL_FIFO=1` when launching the emulator to stop at a GPU
linked-list DMA node whose payload exceeds 16 words. Unset it (or set it to `0`)
for the existing permissive behavior. The setting is read when constructing or
restoring a bus, not on every node. A failure reports the node address, transfer
number, payload count, and DMA mode before submitting that node's payload.

This is an **opt-in conformance diagnostic**, not finite-FIFO emulation and not a
prediction of which words real hardware loses. It must not become a default rule
for arbitrary games: variable-length primitives and transfers need more detailed
hardware treatment.

Sony's [SCEA Advanced GPU presentation, March 1996](https://psx.arthus.net/sdk/Psy-Q/DOCS/CONF/SCEA/adv_gpu.pdf)
describes a 64-byte (16-word) FIFO (page index 2), source-chain requests when the
FIFO becomes empty (14 and 18), and merging primitives to fit that FIFO (44).
It also explicitly permits LoadImage primitives in an ordering table (46).
A0 image uploads in a list therefore are not inherently invalid.

The diagnostic counts **GPU payload words only**: the DMA tag is consumed by the
DMA controller. Passing this 16-word envelope does not establish compliance with
Sony's stricter MargePrim total-size convention (16 words including the tag).
A guest can conservatively use at most 15 payload words for that convention.
Neither convention proves a specific overflow/drop/wrap behavior.

## Reproduction and scope

The unchanged Celeste Collection executable with SHA-256
`0d7526c9d88fc086ea19cb934dea2b18c6f6da54e7f83c6176e1aecf6afaae09`
passes through its immediate-GP0 collection menu, then fails this diagnostic on
its first active-frame linked-list node: physical address `0x0009c5a0`,
252 payload words. A captured complete active frame contains 995 payload words
in four nodes of 254, 253, 252, and 236 words. The individual GP0 packets are
well formed and at most 11 words; regrouping the same packets changes transport,
not drawing commands. Paused frames use the immediate-GP0 path.

Normal emulation still consumes the list synchronously and renders it cleanly.
This diagnostic makes that previously accepted packing discrepancy observable;
it does **not** reproduce the corrupt hardware pixels. Modeling FIFO consumption,
DREQ arbitration, and overflow requires independent hardware evidence rather than
inventing corruption to match a video.

Run the focused real-DMA-path regression tests with:

```
cargo test -p emulator-core gpu_linked_list_fifo_guard
```

They cover an A0 upload in a 16-payload-word node, rejection before side effects,
and byte-identical VRAM/command execution when the same valid upload stream is
split into smaller nodes. The disabled diagnostic retains the larger-node behavior.

## Separate request-gating correction

Request/block and linked-list DMA now leave CHCR busy without reading RAM while
GPUSTAT DREQ is deasserted. Bus time advancement retries that waiting channel
when the GPU's existing direction latch/readiness model asserts DREQ. Cancellation
clears the wait; manual DMA remains CPU-triggered. The focused tests change RAM
while blocked and verify that resumption fetches the new value exactly once.

This fixes an independently reproduced missing request gate. Celeste sets direction
2 before its list submissions, so this correction alone is not evidence for the
hardware corruption's cause. List execution after admission remains synchronous;
it does not implement per-node requests or finite FIFO consumption.

## Timed FIFO model (default since hwtest v1.24)

The FIFO model is the default for every `Bus` since the hwtest v1.24 console
capture of 2026-09-23 (PSoXide-editor
`docs/emulator-accuracy-from-silicon.md`). Cases 211-226 time channel 2 on
lists that draw: on silicon CHCR stays busy until the last packet is in the
GPU (expensive list 586,354 clocks, cheap 2,996) and GP0(1Fh) follows the
drawing. This model reproduces that shape; the word-count model clears CHCR
at 283 clocks for both lists and raises the 1Fh 6 clocks after the kick.
`PSOXIDE_EXPERIMENTAL_DMA_FIFO=0` selects the word-count model for
comparison; the variable keeps its old name so existing scripts that set `=1`
still mean the same thing. A standalone `Gpu` driven by host code (renderer
tests, replay tools) still executes words immediately, since it has no clock
to drain a queue.

The rest of this section is the original description of the model. Leave the
packing guard unset to observe its rendering instead of stopping
at an oversized node. CPU GP0 and DMA traffic share an ordered input queue.
Linked-list headers are admitted by DREQ; each admitted payload is fetched from
RAM one word at a time as bus time advances, without rechecking DREQ mid-node.
DPCR suspension and CHCR cancellation remain effective. GPU work consumes the
queue using the existing command cost model, permitting drawing-area/offset
commands while drawing and consuming active A0 image data without a drawing gate.

The structural reference is [Beetle/Mednafen at
05261cede8ad70dd48081d27b878622b67517a6d](https://github.com/libretro/beetle-psx-libretro/tree/05261cede8ad70dd48081d27b878622b67517a6d/mednafen/psx):
`dma.c` admits a whole node at request boundaries; `gpu.c` accounts for a
16-word FIFO plus a front-command staging allowance and rejects excess incoming
words while drawing prevents consumption. The implementation here is independent;
reference behavior is corroborating evidence, not a new silicon measurement.

The model deliberately remains experimental:

- Existing raster costs are reused; no costs were fitted to the Celeste footage.
- DMA setup uses a reference-derived 15/10-clock countdown for nonempty/empty
  nodes and one payload word per bus clock, plus explicit state-transition ticks.
  Exact hardware arbitration and CPU bus stealing are not newly calibrated.
- Primitive execution remains the existing complete-packet rasterizer, including
  whole-quad processing rather than a fully modeled hardware command sequencer.
- Front-command staging is approximated from packet size, with the explicit
  two-word E1/E2/E6/A0 allowance. This is not full command-by-command silicon
  characterization.
- Save states carry the queue, the in-flight list walk and the model flag
  (format version 7), so a save taken mid-walk resumes the walk.
- Raster costs were recalibrated separately against silicon; see the
  emulator-accuracy document for the records used.

Transport tests demonstrate actual timed RAM fetches, tick-chunk equivalence,
request suspension, valid A0 uploads (including a node larger than 16 words), and
an overflowing long draw burst that becomes lossless when regrouped. Thus the
experiment does not simply truncate every large node or inject game-specific
corruption. Visual results and remaining discrepancies belong in the investigation
report, separately from these transport assertions.
