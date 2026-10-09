<!--
DRAFT, not pushed. Describes the editor after plan phases P0 to P5 (self-contained clone, shared app library
into its own product, installer that provisions the toolchain). Marked lines do not hold today:
  [P3] the single clone-and-run command
  [P3] the Download table and the first-run toolchain setup; [P4, P5] the Import line
Today the editor needs Python 3 for `make bootstrap`, `make`, `sh`, `rsync` and a pinned nightly
Rust, and a clone is about 1.1 GB.
-->

# PSoXide Editor

> **Largely written with agentic coding.** I direct the agents and test their work in two places: PSoXide's emulator, which profiles every cycle, and a real PlayStation, which shows me where the emulator is wrong. Working between them is where the accuracy and the speed come from. [How PSoXide is built](https://ebonura.github.io/PSoXide/how-its-built/)

Make PlayStation games: author levels, materials, animation, audio and UI, cook them into a disc
image, and playtest on the built-in emulator. The editor, the cookers and the PlayStation runtime are all Rust.

![The editor with a level open](docs/images/SCREENSHOT.png)

## Download [P3]

| Platform | File |
| --- | --- |
| macOS, Apple silicon and Intel | `PSoXide-Editor-<version>-macos.dmg` |
| Windows, 64-bit | `PSoXide-Editor-<version>-windows-x86_64.zip` |
| Linux, 64-bit | `PSoXide-Editor-<version>-linux-x86_64.tar.gz` |

Just want to play a disc? Use the [PSoXide Emulator](https://github.com/EBonura/PSoXide-emulator); the
editor is for making games. Playtesting compiles your project into a PlayStation program, so on
first launch the editor checks for [rustup](https://rustup.rs) and installs the pinned nightly
toolchain it needs.

## Build from source [P3]

Needs [rustup](https://rustup.rs). The repository pins its nightly toolchain, so the first build
downloads it, then:

```sh
git clone https://github.com/EBonura/PSoXide-editor.git
cd PSoXide-editor
cargo run --release
```

On Linux, first install `pkg-config libasound2-dev libudev-dev libxkbcommon-dev`.

## What it does

- BSP level editor with a 3D viewport, materials, lighting and prefabs.
- Animation, audio and UI authoring, with the cookers that turn them into PlayStation formats.
- Play button: cooks the project, builds the PlayStation program and boots it in the embedded
  emulator. Export builds a CUE/BIN disc image.
- Command-line cookers and an MCP server for scripting the editor.
- Everything the emulator does, plus Import: build a supported port, such as Half-Life, from your
  own Steam copy, on your own machine [P4 and P5].
- The runtime engine (`psoxide-engine`) the games run on, and Cortex Ignition, a game made with it.

The engine and every game it produces link the GPL-licensed SDK; see
[downstream licensing](https://github.com/EBonura/PSoXide/blob/main/docs/downstream-licensing.md)
before shipping.

## More

[Website](https://ebonura.github.io/PSoXide/) ·
[Demo disc](https://bonnie-studios.itch.io/psoxide-demo-disc) ·
[Dependency matrix](docs/demo-disc-dependencies.md) ·
[Repository split](docs/sdk-separation.md) ·
[Changelog](CHANGELOG.md)

Related repositories: the [SDK](https://github.com/EBonura/PSoXide) (psoxide-sdk) and the
[emulator](https://github.com/EBonura/PSoXide-emulator) (psoxide-emulator).

## Licence and provenance

[GPL-2.0-or-later](LICENSE), with existing asset attribution and provenance preserved. This is not a
clean-room implementation; development used heavy AI assistance, and provenance is tracked in the
open ([downstream licensing](https://github.com/EBonura/PSoXide/blob/main/docs/downstream-licensing.md)).
Hardware-sensitive changes need original-console evidence; emulator checks alone do not establish
hardware correctness. PSoXide does not bundle or load external console firmware.

PlayStation is a trademark of Sony Interactive Entertainment Inc. PSoXide is not affiliated with or
endorsed by Sony.
