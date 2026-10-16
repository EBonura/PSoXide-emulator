# PSoXide Emulator

> **Largely written with agentic coding.** I direct the agents and test their work in two places: this emulator, which profiles every cycle, and a real PlayStation, which shows me where the emulator is wrong. Working between them is where the accuracy and the speed come from. [How PSoXide is built](https://ebonura.github.io/PSoXide/how-its-built/)

A Rust PlayStation emulator for playing, debugging and profiling homebrew.
The CPU, GPU, SPU, CD-ROM, controller and memory-card implementation lives
here, alongside the desktop and browser frontends.

[The SDK](https://github.com/EBonura/PSoXide) remains at the original PSoXide
repository. [The editor, engine and Cortex Ignition](https://github.com/EBonura/PSoXide-editor)
live together in their own repository and consume this emulator core.

## Runs without a BIOS

PSoXide boots commercial discs and homebrew with its own clean-room HLE kernel;
no BIOS is needed or accepted. All 20 commercial games in the compatibility
list reach gameplay. Per-game status: [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md).

![Twenty commercial games and five homebrew games running in PSoXide](docs/images/compat-grid.png)

Screenshots are headless captures from the compatibility runs. The games and
their screenshots belong to their respective owners.

## Build and run

You need [Rust](https://rustup.rs) (the checked-in toolchain file selects the
version) and your host's C/C++ build tools. On Ubuntu, first install
`pkg-config libasound2-dev libudev-dev libxkbcommon-dev`. Then:

```sh
git clone https://github.com/EBonura/PSoXide-emulator.git
cd PSoXide-emulator
cargo run --release
```

That builds and starts `psoxide-emulator`. Nothing else is needed: no Python,
no Make, no bootstrap step, and no BIOS. `make check`, `make test` and
`make build` are shortcuts for the matching cargo commands, and the built
binary is `target/release/psoxide-emulator`.

The binary was called `frontend` before 2026-10-16. A tiny `frontend` binary is
still built beside it and runs `psoxide-emulator` with the same arguments, so
scripts that call `target/release/frontend` by path keep working. Scripts that
build with `cargo build -p frontend` must change to `-p psoxide-emulator`.

Headless verification uses the same core:

```sh
./target/release/psoxide-emulator launch --path /path/to/game.cue --steps 8000000 --dump-hash
```

For recorded runs, `--route-log route.csv` measures emulated cycles and display
flips. Add `--route-watch-u32 0x80010000` to sample an aligned RAM word in each
row without changing guest timing; repeat it for additional addresses. Resolve
addresses from the exact game's link map. A loading-state word can distinguish
map transitions from slow gameplay without filtering by frame rate.

The optional `mcp` feature enables the native debugging server. Browser code
remains under `emu/crates/frontend`; it does not depend on the editor.
PSoXide boots every disc, commercial games included, and every homebrew EXE
with its own clean-room HLE kernel. It needs no BIOS, and none can be
supplied. What runs, and how far, is in
[docs/COMPATIBILITY.md](docs/COMPATIBILITY.md).

## Game library

Open the Library menu and pick "Choose games folder" to set your games
directory. The Library lists its subfolders; folders start collapsed each time
you launch PSoXide. Click a folder or press Enter to expand it. Contents are
indented, including nested folders. Use "Refresh library" (also in Library)
after moving or adding games. Keep each CUE beside its BIN files when
organizing discs.

The menu has three categories: Library, Game (shown only while a game is
running) and Settings.

## Source dependencies

The nine SDK crates and four shared crates the emulator links, plus the six
SDK example sources its menu lists, are vendored (committed) under `sdk/` and
`crates/`, so a plain clone builds without a
download step. `components.lock.json` pins the SDK revision they were exported
from and lists the vendored paths. Do SDK development in the SDK repository,
change the pinned revision, then run `make refresh-sdk` to re-export and stage
the copies; `make check-sdk` fails when the committed copies differ from the pin
and runs in CI. To export from a local clone instead of GitHub:

```sh
make refresh-sdk COMPONENTS_ARGS="--source sdk=/path/to/PSoXide"
```

This uses committed content at the pinned revision, not the checkout's working
files. `sdk/Cargo.toml` is the SDK workspace manifest with its member list cut
to the vendored crates. Emulator verification cannot substitute for
original-console validation of SDK or game behavior.

## License

[GPL-2.0-or-later](LICENSE). Existing source and asset attribution is preserved.
Where the HLE kernel's behaviour comes from is recorded in
[docs/hle-bios-provenance.md](docs/hle-bios-provenance.md).

PlayStation is a trademark of Sony Interactive Entertainment Inc.; PSoXide is not affiliated with or endorsed by Sony.

## How This Was Built

PSoXide was developed with heavy use of AI coding assistants, with a human
directing the architecture, debugging and hardware verification. A large part
of the code was written by an AI assistant under human direction, review and
integration.

This is not a clean-room implementation as a whole, and disclosing AI
assistance is not a warranty of clean-room provenance or of non-infringement.
PCSX-Redux (GPL-2.0-or-later) was the parity oracle early in the project. The
scheduler, DMA register file, SIO model, MDEC, SPU and CD-ROM timing constants
have since been rewritten from nocash PSX-SPX and console measurements; the
CPU, bus, GPU and hardware renderer have not, and keep a short list of
behaviours matched to Redux traces. [docs/PROVENANCE.md](docs/PROVENANCE.md)
lists exactly what is and is not rewritten. The wider picture is in PSoXide's
[downstream licensing](https://github.com/EBonura/PSoXide/blob/main/docs/downstream-licensing.md)
document.

## Recent changes

Source snapshot **2026.09.05**: Moved the emulator core and desktop/browser frontends into their own repository.
See the [changelog](CHANGELOG.md) for the remaining changes.

## Firmware policy

PSoXide does not bundle or load console firmware. Discs and homebrew run
on its own HLE kernel, written from public documentation and black-box
observation (see [docs/hle-bios-provenance.md](docs/hle-bios-provenance.md)). See the [cleanup audit](docs/firmware-cleanup.md)
for the source, binary-header and history checks.
