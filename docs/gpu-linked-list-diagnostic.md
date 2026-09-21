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
