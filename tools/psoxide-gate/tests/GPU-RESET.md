# Cross-game drawing-state reset

The GUI keeps its renderer while replacing the emulated machine. A previous
game's GP0 texture window, drawing area, offset and mask must not survive that
replacement. GP1 reset inside one machine must also reach the replay backend
in command order. Reset preserves VRAM; machine replacement restores the new
machine's pixels and active drawing environment before its first batch.

The native GPU tests cover the active environment snapshot and ordered reset
entries without VRAM clearing. The hardware regression
`different_machine_and_gp1_reset_discard_previous_draw_environment` requires
an adapter and checks both replacement and GP1 reset at scales 1, 3 and 4.
Its textured rectangle deliberately omits E2 and E6 after reset, exposing a
stale texture window or mask instead of repairing it in the guest.

The permanent route fixtures reproduce ordinary OoT file selection followed
by WipEout title, Players, race start and mid-race in the same renderer. Supply
the two complete game discs as local inputs; no game memory is modified.

Build the diagnostic with `cargo build --release -p psoxide-gate --example
gui_lifecycle`. For each scale 1 and 3, capture a fresh WipEout process first:

```sh
gui_lifecycle --disc "$WIP_DISC" --route tools/psoxide-gate/tests/routes/gpu-reset-wipeout.toml --batch 4 --scale 3 --out "$FRESH_OUT"
gui_lifecycle --previous-disc "$OOT_DISC" --previous-route tools/psoxide-gate/tests/routes/gpu-reset-oot-prefix.toml --disc "$WIP_DISC" --route tools/psoxide-gate/tests/routes/gpu-reset-wipeout.toml --batch 4 --scale 3 --expect-fresh "$FRESH_OUT" --out "$SWITCH_OUT"
```

Use distinct, empty output directories and repeat with `--scale 1`. The
comparison requires byte-identical CPU and hardware checkpoint images to
their respective fresh captures. It does not equate CPU pixels with enhanced
hardware pixels, waive renderer differences, or qualify gameplay. The
unfixed renderer reproduces a title HW3 mismatch after the OoT prefix while
CPU pixels remain identical. Keep that failure and the fixed captures.
