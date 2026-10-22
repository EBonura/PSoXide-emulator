# Rust style

We follow [Google's Rust style guide](https://google.github.io/styleguide/rustguide.html).
The canonical statement, rule 0 and the approved project deltas live in PSoXide's
`sdk/docs/STYLE.md`; this file only records how this repository applies them.

Rule 0: games run well first. Frame time, RAM, VRAM, binary size and load time outrank
style. Formatting and other mechanical changes must not change behaviour; a change that can
alter codegen needs a measurement showing no cost. This is an emulator, so the cycle-model
and frame-hash gates are the measurement.

## Enforcement

- `rustfmt.toml`: the guide's settings (three options are unstable, hence the pinned
  nightly). `make fmt` formats, `cargo fmt --all -- --check` runs in CI. The vendored SDK
  crates under `crates/` and `sdk/` are formatted upstream and excluded here.
- `[workspace.lints.clippy]` in `Cargo.toml`: lints that are clean today are `deny`.
- `tools/style-ratchet.sh`: counts every other guide lint, and re-checks the clean ones in
  crates that have not opted into the workspace lints, against
  `tools/style-ratchet.baseline`. A count may fall, never rise. `--report` prints,
  `--update` lowers the baseline.
