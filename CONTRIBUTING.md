# Contributing

Run `make bootstrap`, `make check`, `make test`, and `make fmt-check`.
Keep emulator behavior changes separate from dependency pin updates. Add a
focused regression test for timing, CPU, GPU, SPU or CD-ROM changes, and record
whether evidence comes from emulation or original hardware.

SDK source is generated from `components.lock.json`; propose changes in
EBonura/PSoXide and then update the pin here. Editor and game work belongs in
EBonura/PSoXide-editor. Do not commit BIOS files, game images, capture output,
or generated component files.
