# Frontend Font Provenance

## VT323-Regular.ttf

- **Designer**: Peter Hull
- **Source**: https://fonts.google.com/specimen/VT323
- **License**: SIL Open Font License 1.1
  (https://openfontlicense.org/) - full text embedded in the TTF's
  metadata.
- **Used for**: monospace / debugger / register-panel typography in
  the PSoXide frontend.

## Phosphor.ttf / Phosphor-Fill.ttf

- **Source**: https://phosphoricons.com/
  (`@phosphor-icons/web`, regular + fill weights).
- **License**: MIT
  (https://github.com/phosphor-icons/homepage/blob/master/LICENSE).
- **Used for**: emulator UI iconography (codepoints listed in
  `emu/crates/frontend/src/icons.rs`). Regular is the default weight;
  fill is used for active toggle buttons.
- **Subset**: both files hold only the codepoints in `icons.rs` (about
  8 KB each instead of 450-490 KB, which every web visitor downloaded).
  Outlines are unchanged. To add an icon, subset the upstream TTFs again
  with fontTools, listing every codepoint in `icons.rs`:

  ```sh
  U=$(grep -o '\\u{e[0-9a-f]*}' emu/crates/frontend/src/icons.rs \
      | sed 's/\\u{\(.*\)}/U+\1/' | sort -u | paste -sd, -)
  pyftsubset Phosphor.ttf --unicodes="$U" --output-file=Phosphor.subset.ttf \
      --no-hinting --desubroutinize --layout-features='' \
      --name-IDs='*' --name-languages='*' --notdef-outline
  ```

  (same for `Phosphor-Fill.ttf`). A frontend test fails while an icon is
  missing from either file.

All fonts are bundled as binary `.ttf` files. Their licenses are
GPL-compatible: the SIL Open Font License is explicitly compatible
with GPL when fonts are bundled with software, and MIT is a
permissive license.
