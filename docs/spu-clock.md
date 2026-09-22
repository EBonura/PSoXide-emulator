# SPU clock ownership

The bus advances the SPU on its existing absolute 768-cycle sample deadline. The deadline is inclusive and participates in the event-drain fast path even when no `SpuAsync` scheduler slot exists. It is already serialized in save states; no format or extra clock state is introduced.

Catch-up precedes SPU register access, SPU DMA, interrupt observation and interrupt acknowledgement. This includes cycles charged during a CPU instruction before its MMIO access. A DMA write must not replace sample data used by elapsed audio, and an acknowledgement must not precede an already-due SPU interrupt. Sample production raises the SPU IRQ directly without re-entering scheduler dispatch.

Frontends drain output samples. Calling `run_spu_to_current_cycle` additionally is safe, but calling it once per instruction versus once per video frame must not change PCM or guest-visible state. Previously, synthesis was deliberately deferred to the frontend for an old external parity-oracle workaround. A mid-frame volume write could therefore retroactively mute earlier samples, and a due SPU IRQ remained invisible until the frontend pumped audio.

`spu_frontend_clock_repro` covers these failures, the exact sample boundary, repeated calls, access widths, DMA capture ordering, acknowledgement ordering and restored deadlines. A bus unit test covers architectural narrow stores separately from the host byte-write helper. The cadence of display flips and host playback underruns are separate properties; this correction does not impose a game frame rate or change host audio buffering.
