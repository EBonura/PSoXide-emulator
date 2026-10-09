<!--
DRAFT, not pushed. Written for the state after plan phases P0 (one-line build) and P1 (release
binaries). Today the clone-and-run block below is true only on the branch
poc/one-liner-build-2026-10-09 (macOS arm64 verified). The Download table is a placeholder until
the first tagged release exists. Replace SCREENSHOT with a current capture of the window.
-->

# PSoXide Emulator

> **Largely written with agentic coding.** I direct the agents and test their work in two places: this emulator, which profiles every cycle, and a real PlayStation, which shows me where the emulator is wrong. Working between them is where the accuracy and the speed come from. [How PSoXide is built](https://ebonura.github.io/PSoXide/how-its-built/)

A PlayStation emulator written in Rust, for playing, debugging and profiling homebrew and the
games you own. It needs no BIOS. Part of [PSoXide](https://ebonura.github.io/PSoXide/).

![PSoXide running a game, with the debugger open](docs/images/SCREENSHOT.png)

## Download

| Platform | File |
| --- | --- |
| macOS, Apple silicon and Intel | `PSoXide-Emulator-<version>-macos.dmg` |
| Windows, 64-bit | `PSoXide-Emulator-<version>-windows-x86_64.zip` |
| Linux, 64-bit | `PSoXide-Emulator-<version>-linux-x86_64.tar.gz` |

All files are on the [Releases page](https://github.com/EBonura/PSoXide-emulator/releases), each
with a checksum and the source revision it was built from. You can also
[play in the browser](https://bonnie-studios.itch.io/psoxide) with no install.

## Build from source

Needs Rust 1.87 or newer. Install it with [rustup](https://rustup.rs), then:

```sh
git clone https://github.com/EBonura/PSoXide-emulator.git
cd PSoXide-emulator
cargo run --release
```

That is the whole build: no Python, no Make, no extra downloads beyond Cargo's own. On Linux,
first install `pkg-config libasound2-dev libudev-dev libxkbcommon-dev`. On Windows, rustup asks
for the Visual Studio Build Tools; on macOS it asks for the Xcode command line tools.

## What it does

- Boots commercial discs and homebrew with its own HLE kernel. No BIOS is needed or accepted.
  [Compatibility list](docs/COMPATIBILITY.md).
- Debugger and profiler in the window: CPU registers, memory, VRAM, frame timing.
- Headless mode for scripted checks: `psoxide-emulator launch --path game.cue --steps 8000000 --dump-hash`.
- Game library, save states and memory cards.
- Runs in the browser as WebAssembly from the same source.
- Import a game: build a supported port, such as Half-Life, from your own Steam copy, on your own
  machine. Nothing is downloaded except the port's public source and Rust dependencies. [P5; off until
  the legal review in the plan is done]
- Optional MCP server (`--features mcp`) so an agent can drive a running emulator.

Point the Library menu at your games folder. Games are not included and none are downloaded.

## More

[Website](https://ebonura.github.io/PSoXide/) ·
[Compatibility](docs/COMPATIBILITY.md) ·
[Frontend notes](docs/frontend.md) ·
[Web player](docs/web-player.md) ·
[Changelog](CHANGELOG.md)

Related repositories: the [SDK](https://github.com/EBonura/PSoXide) for writing PlayStation programs
in Rust, and the [editor](https://github.com/EBonura/PSoXide-editor) for making games.

## Licence and provenance

[GPL-2.0-or-later](LICENSE). Where the code comes from is recorded, not summarised away:
[docs/PROVENANCE.md](docs/PROVENANCE.md) lists what is and is not rewritten from PCSX-Redux, and
[docs/hle-bios-provenance.md](docs/hle-bios-provenance.md) covers the HLE kernel. Development used
heavy AI assistance and this is not a clean-room implementation as a whole; the wider picture is in
[downstream licensing](https://github.com/EBonura/PSoXide/blob/main/docs/downstream-licensing.md).

PlayStation is a trademark of Sony Interactive Entertainment Inc. PSoXide is not affiliated with or
endorsed by Sony. Game names and screenshots belong to their owners.
