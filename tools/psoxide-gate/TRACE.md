# Failed comparison traces

`psoxide-gate run JOURNEY --disc DISC --strict --scales 1,3 --trace-failures`
opts into diagnostic recording. Default runs do not allocate owner buffers or
retain packet streams. All comparison tolerances, classification and pass/fail
checks remain unchanged. Trace failures become report notes, never passes.

Start with a journey containing one failing checkpoint. Tracing increases host
work and is not a performance measurement. Use the existing shared executable,
serial build lock and disk floor. Do not run a full sixteen-checkpoint matrix to
diagnose one pixel region.

For a passing baseline, add `--trace-checkpoint NAME` together with
`--trace-failures`. This selects the checkpoint without introducing a failed
assertion or changing any comparison result.

Each failed or explicitly selected checkpoint writes `JOURNEY/trace/checkpoint-NNNN/`:

* `packets.bin`: recent GP0 snapshots, bounded to 4 MiB plus the eight-byte header.
* `owners.bin`: 1024x512 native VRAM CPU owner IDs, u64 little endian.
* `ram.bin`, `vram.bin`: complete guest main RAM and CPU VRAM at the checkpoint.
* `receipt.toml`: full SHA256 hashes, tick range, eviction status, disc track
  hashes, exact journey hash, executable hash, source HEAD and dirty status,
  symbol source hashes, PC/GPRs, display origin and bounded named RAM state.

At most sixteen failed checkpoints are retained, about 176 MiB total. Packet
snapshots older than the window are evicted; their owners are unknown. A tick
larger than the window is omitted explicitly. `truncated` means history was
omitted, never that a comparison passed. Reboots clear ownership and increment
an epoch; journey ticks continue, so stable owner IDs cannot alias an older boot.

Packet format is `PXGP0V1\0`, then repeated records: little-endian u64 tick,
u32 epoch, u32 local packet index, u32 word count, and that many little-endian
u32 FIFO words. Snapshot capture occurs before the hardware renderer drains the
log. Pending uploads and polylines may recur on later ticks with more words.
Owner IDs are `(tick << 32) | local_index`; u64::MAX means unknown. Per-tick CPU
stamps are cleared without clearing command logs or disrupting pending packets.

`psoxide-gate trace-pixel CHECKPOINT_DIRECTORY X Y` resolves a **native VRAM**
coordinate to a hash-verified CPU packet snapshot. Add display origin to native
screen coordinates first. For HW3, divide screen coordinates by three to find
candidate CPU ownership, then inspect neighboring packets and geometry. CPU
ownership does not establish the HW winner at a subpixel edge. Upload/copy
operations and untraced writes can also have unknown ownership.

Default named-state requests are `SIM_NOW:4`, `DEFERRED_OVERLAYS:128`,
`LIVE_CHECKPOINT:256` and `SHOW_VIEWMODEL:1`. Missing optimized symbols are recorded
as unavailable. These are raw byte requests, not asserted struct layouts.
`--trace-state SYMBOL:BYTES` adds at most 32 requests of 1..256 bytes. Only resident
main RAM addresses are accepted. The complete RAM and GPR dumps also preserve
stack/register state for source-bound analysis of local animation state; no
specific pose is claimed merely because a dump exists.

A dirty source is explicitly flagged and must be reviewed before attributing
behavior to its HEAD. The historical short `disc_id` is only a twelve-hex BIN
identifier; the trace records full CUE and each referenced track SHA256 instead.
