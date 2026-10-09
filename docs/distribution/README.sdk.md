<!--
DRAFT, not pushed. The SDK is a library and toolchain, so there is no Download table: the
distribution unit is a pinned Git revision. The quick-start block assumes an xtask target that
builds an example disc without Make (`cargo xtask disc hello-tri`), which does not exist yet [P0].
Today the working command is `make hello-tri-disc`.
-->

# PSoXide SDK

> **Largely written with agentic coding.** I direct the agents and test their work in two places: PSoXide's emulator, which profiles every cycle, and a real PlayStation, which shows me where the emulator is wrong. Working between them is where the accuracy and the speed come from. [How PSoXide is built](https://ebonura.github.io/PSoXide/how-its-built/)

Bare-metal Rust for the original PlayStation: runtime, GPU and GTE, audio, input, disc and
memory-card APIs, fixed-point math, a disc packer and small homebrew examples. No Sony SDK and no
BIOS needed.

![The hello-tri example running](docs/images/SCREENSHOT.png)

## Build a triangle

Needs [rustup](https://rustup.rs); the repository pins the nightly toolchain it needs, and the
first build downloads it.

```sh
git clone https://github.com/EBonura/PSoXide.git
cd PSoXide
cargo xtask disc hello-tri        # [P0] today: make hello-tri-disc
```

The result is `hello-tri.cue` and `hello-tri.bin` under `build/examples/mipsel-sony-psx/release/`.
Keep the two files together. Open the CUE in the [emulator](https://github.com/EBonura/PSoXide-emulator),
or burn it for a console that can boot it. No editor is needed.

## What is in it

- `sdk/`: the device crates (`psx-rt`, `psx-gpu`, `psx-gte`, `psx-spu`, `psx-pad`, `psx-mc` and more),
  linker script and examples.
- `crates/`: hardware, disc and cooked-format contracts shared with the host tools.
- `tools/mkisopsx`: BIN/CUE mastering. `tools/psoxide-hazard`: post-link checks for CPU hazards and
  stack bounds. `tools/psoxide-pgo`: profile-guided builds from emulator runs.
- No Python anywhere in the repository.

## More

[Website](https://ebonura.github.io/PSoXide/) · [SDK crate docs](sdk/README.md) ·
[Downstream licensing](docs/downstream-licensing.md) · [Asset provenance](docs/asset-provenance.md)

The [emulator](https://github.com/EBonura/PSoXide-emulator) runs what you build, and the
[editor](https://github.com/EBonura/PSoXide-editor) makes whole games on top of the SDK. Games pin
a full Git revision of this repository and commit their lockfiles.

## Licence and provenance

[GPL-2.0-or-later](LICENSE). This is not a clean-room implementation; development used heavy AI
assistance, and where code comes from is tracked in [docs/license-audit.md](docs/license-audit.md).
PlayStation is a trademark of Sony Interactive Entertainment Inc. PSoXide is not affiliated with or
endorsed by Sony.
