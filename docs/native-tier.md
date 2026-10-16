# Native CPU tier

The optional AArch64 native tier compiles hot blocks from the interpreter's
decoded-block cache. It preserves the interpreter's instruction timing,
load delays, exceptions, device state and frame boundaries. Unsupported
operations continue through the interpreter.

Build the desktop frontend with:

```sh
cargo build --release -p frontend --features native-jit
```

This also enables the tier for headless `launch` commands. Set
`PSOXIDE_JIT=0` to use only the interpreter in the same binary. The executable-memory backend supports macOS and Linux AArch64. Other
hosts, or hosts where executable memory cannot be mapped, use the interpreter. The
feature does not enable native code in WebAssembly builds.

A fresh compiler is installed when a machine boots and when a save state is
restored. Compiled code is not part of a save state. Library users can call
`psoxide_jit::install_tier(&mut cpu)` after creating or deserializing a CPU.

For headless compatibility validation, `hle_compat` enables the tier only
when `PSOXIDE_JIT=1`. Its default remains the interpreter. The native-tier
`jit_run lockstep` example compares CPU state at every run boundary and
serialized machine state every 100 run calls by default, and at completion.
`--full-every` sets that interval; zero disables the intermediate checks.

`jit_tape` replays a pad tape on a disc (a text file of hex button masks per
frame, or a recorded `.pxtape`) and compares the tier (or, with `run` in place of
`tier`, the cached interpreter) with plain `Cpu::step` after every run call, reporting the first difference with the
instruction, program counter and cycle count; `--refine-at` and `--sub` re-run
the failing stretch in smaller pieces to find the instruction.

The tier is experimental. Performance comparisons must use the same host
loop and game windows, verify that the tier was installed, and compare
final machine state. It is not yet the fastest measured recompiler.
