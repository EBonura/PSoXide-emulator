# PSoXide Emulator

> **Largely written with agentic coding.** I direct the agents and test their work in two places: this emulator, which profiles every cycle, and a real PlayStation, which shows me where the emulator is wrong. Working between them is where the accuracy and the speed come from. [How PSoXide is built](https://ebonura.github.io/PSoXide/how-its-built/)
>
> **AI-generated content:** code. The fonts are third-party and credited in [emu/crates/frontend/assets/fonts/PROVENANCE.md](emu/crates/frontend/assets/fonts/PROVENANCE.md).

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

Install Rust through rustup, Python 3, and your host's C/C++ build tools.
On Ubuntu, install `pkg-config libasound2-dev libudev-dev libxkbcommon-dev`.
The checked-in toolchain file selects the Rust version.

```sh
git clone https://github.com/EBonura/PSoXide-emulator.git
cd PSoXide-emulator
make bootstrap
make check
make test
make build
./target/release/frontend
```

Headless verification uses the same core:

```sh
./target/release/frontend launch --path /path/to/game.cue --steps 8000000 --dump-hash
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

`components.lock.json` pins the SDK by full Git commit. `make bootstrap`
materializes its source at Cargo's expected paths and records file hashes in
`.components-receipt.json`. These generated directories are ignored by Git.
Do SDK development in the SDK repository and update the lock; the bootstrap
refuses to overwrite modified imported files. `make verify-components` checks
the lock and hashes without a network request.

For local verification, export the exact locked commit from an existing clone:

```sh
python3 tools/bootstrap-components.py --source sdk=/path/to/PSoXide
```

This uses committed content at the lock's revision, not the checkout's working
files. The original source history is retained. Emulator verification cannot
substitute for original-console validation of SDK or game behavior.

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
