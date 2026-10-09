# Contributing

Run `make check`, `make test`, and `make fmt-check`.
Keep emulator behavior changes separate from dependency pin updates. Add a
focused regression test for timing, CPU, GPU, SPU or CD-ROM changes, and record
whether evidence comes from emulation or original hardware.

SDK crates under `crates/` and `sdk/` are vendored copies of the revision in
`components.lock.json`; propose changes in EBonura/PSoXide and then refresh the
copies here. Editor and game work belongs in
EBonura/PSoXide-editor. Do not commit BIOS files, game images, capture output,
or generated component files.
