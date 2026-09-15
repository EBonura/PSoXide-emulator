# PSoXide Emulator

A Rust PlayStation emulator for playing, debugging and profiling homebrew.
The CPU, GPU, SPU, CD-ROM, controller and memory-card implementation lives
here, alongside the desktop and browser frontends.

[The SDK](https://github.com/EBonura/PSoXide) remains at the original PSoXide
repository. [The editor, engine and Cortex Ignition](https://github.com/EBonura/PSoXide-editor)
live together in their own repository and consume this emulator core.

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
BIOS and game images are supplied locally and are not included.

## Game library

Choose your games directory in Settings. The Games menu follows its subfolders;
folders start collapsed each time you launch PSoXide. Click a folder or press
Enter to expand it. Contents are indented, including nested folders.
Use Refresh library after moving or adding games. Keep each CUE beside its
BIN files when organizing discs.

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

## Recent changes

Source snapshot **2026.09.05**: Moved the emulator core and desktop/browser frontends into their own repository.
See the [changelog](CHANGELOG.md) for the remaining changes.
