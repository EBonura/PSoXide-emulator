# PSoXide distribution plan

Date: 2026-10-09. Status: design plus one proof of concept. Round 2 (names, parity audit, import library) folded in.

This plan covers four things: a build-from-source path as short as the one on getartcraft.com/apps,
prebuilt downloads for every platform, an Import feature that builds the Half-Life and Hollow Knight
ports from the user's own game files inside both apps, a clean split of the emulator and editor into
two products, and the product names. It is not legal advice, and neither is the fact sheet in section 5. It records facts with
sources and lists the questions a lawyer should answer.

Evidence tags used throughout:

- **[host-tested]** I ran it on this Mac (macOS arm64, Rust stable 1.92.0) and saw the result.
- **[source-inspected]** I read the code or config and did not run it.
- **[CI]** GitHub Actions result for a pushed commit.
- **[web]** Stated by a page I fetched or searched, URL given. Secondary sources are marked.
- **[not tested]** Reasoned from the above and not exercised. Treat as a hypothesis.

## 0. Summary

1. **The emulator one-liner works on macOS from a fresh clone** [host-tested], on branch
   `poc/one-liner-build-2026-10-09`. `git clone`, then `cargo run --release` builds and starts the
   `psoxide-emulator` binary with no Python, no Make and no hydration step. Fresh clone to finished release
   binary took 3 min 47 s with the machine at load average ~40. The binary is 13.3 MB. It booted
   Celeste headless, and a separately cross-built x86_64 binary run under Rosetta printed the same
   frame hashes. Windows and Linux were **not run**; see 1.4 for what is and is not evidenced.
2. **A self-hydrating `build.rs` cannot work.** Cargo reads every workspace member and path
   dependency manifest before any build script runs. I hit that exact error when I left one crate
   out (`failed to read .../sdk/crates/psx-spu/Cargo.toml`) [host-tested]. The fix is to put the
   sources where Cargo expects them. The emulator needs only 12 SDK crates, about 1.7 MB, so the PoC
   vendors them. Git dependencies would drag a 517 MB repository ([web] GitHub API size field).
3. **The editor cannot be a downloadable binary today.** Its Play button runs `make`, a `sh`
   script, `rsync`, and a nightly cross-compile of a guest program against a source tree whose path
   is baked in at compile time [source-inspected]. It also carries its own fork of the emulator
   frontend: the two source trees differ by 6,806 added or removed lines in the files they share, and the editor's tree has another 10.9k lines the emulator lacks (37.0k lines against 25.4k in total) [host-tested, diff]. It is a developer
   tool whose download must provision a toolchain, and the product split should remove the fork
   (section 4).
4. **Half-Life is import-ready in design, Hollow Knight is not.** `cargo hl-build build` is a
   Rust-only, cross-OS-aware builder that finds Steam installs itself. `cargo hk-build` shells out
   to 281 Python files, needs a venv, finds only Windows installs inside CrossOver bottles, and
   writes discs to a hard-coded `~/Downloads/ps1 games` [source-inspected].
5. **Provenance has gaps that block a public Import feature regardless of engineering**: the HL
   clean-room branch is unmerged and diverged (53 ahead, 43 behind main), the website legal page
   still describes Redux derivation that the emulator's own PROVENANCE.md says is rewritten, and
   nobody has asked a lawyer the questions in 5.9.

6. **Names (decided):** products are `psoxide-emulator`, `psoxide-editor`, `psoxide-sdk`,
   `psoxide-engine`; helper crates `psoxide-app` and `psoxide-import`. Only the emulator package is
   renamed now. The repo `EBonura/PSoXide` and the `psx-*` crates should not be renamed yet (4.4).
7. **Parity audit:** 13 things the emulator frontend has that the editor's fork lacks, 6 the other
   way, 7 already at parity. The editor's fork can be deleted once the six editor-side rows move
   into `psoxide-app`; the riskiest merges are frame pacing, the menu layout, fonts, the texture
   filter pin and launch/save identity (4.5).
8. **Import (decided):** a `psoxide-import` library shown through the shared app library in both
   apps, HL first. No standalone launchpad (section 3).

Decisions that need Manny are collected in section 8.

## 1. Build from source today

### 1.1 Emulator (EBonura/PSoXide-emulator, main c743674)

What a fresh clone does on main today, and every blocker I found:

| # | Blocker | Evidence | Status in the PoC |
|---|---|---|---|
| B1 | The 12 SDK crates the frontend needs are git-ignored (`/sdk`, `/crates`, ...). `cargo` cannot load the workspace until `tools/bootstrap-components.py` has downloaded a tarball of the SDK repo from api.github.com and unpacked 423 files. | [source-inspected] `.gitignore`, `tools/bootstrap-components.py`; [host-tested] bootstrap takes 5 s | Fixed: vendored, about 1.7 MB, 111 tracked files in `sdk/` and `crates/` |
| B2 | Python 3 is required, for B1 and for two CI audit scripts. The org rule is Rust only. | README "Build and run", `ci.yml` | Bootstrap no longer needed. The two CI audits and `tools/build-web-player.py` are still Python. |
| B3 | `make bootstrap` must run before any cargo command, or cargo fails to load the workspace. | [host-tested] error on a missing member | Removed from Makefile and CI |
| B4 | `rust-toolchain.toml` pins `nightly-2026-03-25` plus four components, so a first `cargo` run downloads a nightly. Nothing in the emulator needs nightly: the check and the full release build pass on stable 1.92.0 [host-tested]. The nightly is for the MIPS device builds, which this repo no longer does. `rust-version` is 1.87 in the manifest; I only tested 1.92. | `rust-toolchain.toml`, no `#![feature]` in emu/crates, crates | Toolchain file removed. Needs a CI job on 1.87 before the README says "1.87 or newer" with confidence. |
| B5 | The package is called `frontend` and is not a default member, so there is no obvious `-p` name and the binary is `target/release/frontend`. | `emu/crates/frontend/Cargo.toml` | Renamed `psoxide`; `default-members` set, so bare `cargo run --release` works |
| B6 | Profile overrides name the package `frontend` (cargo warns after a rename). | [host-tested] warning | Fixed |
| B7 | `cargo test` fails on a fresh clone if the SDK `examples` source dirs are missing: `public_example_placeholders_are_discovered_from_source_dirs`. Found by running the suite on the fresh clone. | [host-tested] 1 failure in 112, `app.rs:3695` | Six example dirs (392 KB) vendored. Root cause below. |
| B8 | The README says a C/C++ toolchain is needed. For rustc that means the platform linker, and on Linux four apt packages. | README, `ci.yml` | README rewritten on the branch |

Root cause of B7, and a design smell to surface: the standalone emulator frontend resolves paths
from `CARGO_MANIFEST_DIR` at compile time (`app.rs` lines ~1411 to 1428) to find `sdk/examples`,
`build/examples/...` and `editor/projects`, and its "Build examples" action runs `make examples`
there (`app.rs:1449`). In a downloaded binary those paths point at the build machine's checkout.
These are editor and SDK concerns living in the player; they should go in the product split.

Things that are not blockers [source-inspected unless noted]: no Git LFS and no `.gitattributes` in
either repo, no submodules, `Cargo.lock` is committed so `--locked` works, and the build needs no
network beyond crates.io. A checkout path containing a space ("Application Support") built and ran
fine [host-tested]. Other repos do have space bugs: hk-psx's `pgo` needs a path without spaces
(hk-psx README), and the editor's staged guest build has a comment about spaces splitting the
RUSTFLAGS list (`tools/build_guest_staged.sh:130`). I do not know which path-with-spaces bugs were
seen overnight; none reproduced in the emulator.

### 1.2 The proof of concept

Branch `poc/one-liner-build-2026-10-09` in the work dir clone `emulator/`, six commits on main
c743674, pushed. Summary of the diff: 121 files, almost all vendored sources.

- Vendors `crates/{psx-hw,psx-iso,psx-trace,psxed-format}` and
  `sdk/crates/{psx-gpu,psx-gte,psx-gte-core,psx-io,psx-math,psx-spu,psx-telemetry,psx-vram}` plus a
  trimmed `sdk/Cargo.toml` (it must stay, because the crates inherit package fields from it) and six
  example dirs, at SDK revision 4e97cf3 as in `components.lock.json`. The lock file now lists
  exactly these paths and says `"vendored": true`.
- Drops `tools/mkisopsx` and `tools/psoxide-link` from the workspace (the frontend does not use
  them), `rust-toolchain.toml`, and the bootstrap steps in `Makefile` and `ci.yml`.
- Renames the package and binary to `psoxide-emulator` (the first product name in the scheme of section 4.4), adds `default-members`, fixes profile names, rewrites the README
  build block.

Verification, all from a fresh `git clone` of the branch into `fresh-emulator/` [host-tested]:

| Check | Result |
|---|---|
| `cargo run --release -- --help` (cold, includes crate downloads) | exit 0, 227 s wall, load ~40 |
| Binary | `target/release/psoxide-emulator`, 13,305,424 bytes, arm64 (measured before the final rename; same code) |
| `psoxide-emulator launch --path <Celeste cue> --steps 8000000 --dump-hash` | `vram_fnv1a_64=0xff5327f5b858460d`, `display_fnv1a_64=0xc2779d4f09200444` |
| x86_64 cross build on the arm64 Mac, then `arch -x86_64` run | built in 3 min 35 s, identical hashes under Rosetta |
| `lipo -create` of both | universal binary 27.6 MB (12.9 MB as tar.gz), ad-hoc linker-signed only |
| `cargo check --locked --workspace --all-features` (includes the `mcp` feature) | pass |
| `cargo fmt --all -- --check` | pass after trimming `sdk/Cargo.toml` (it failed before: missing members) |
| `cargo test --locked --workspace --no-fail-fast` on the final branch head | exit 0, 29 test binaries pass, 222 s. A first run before the example dirs were vendored failed B7 (plain `cargo test` stops at the first failing binary, so that run did not reach every crate). |

Not done: I did not open the GUI window (agent rules forbid launching GUI apps), so "starts the
app" rests on `--help` and the headless `launch` path through the same binary. Plain
`cargo run --release` will open the window.

### 1.3 Ways to get the SDK crates into a fresh clone

| Option | Fresh-clone cost | Drift handling | Verdict |
|---|---|---|---|
| Vendor the used crates (PoC) | +1.4 MB | `components.lock.json` pin plus a refresh with the SDK's Rust `psoxide-components` tool; add a CI job that diffs vendored files against the pinned revision | **Recommended for the emulator and the SDK-consuming games** |
| Cargo `git` dependency on EBonura/PSoXide | repo is 517 MB on GitHub; Cargo fetches the history [not tested] | `Cargo.lock` pins the rev | Worth it only if the SDK repo is slimmed or split |
| Publish the host crates to crates.io | tiny | semver | Cleanest long term, but needs name claims and a stability promise on hardware-contract crates. Not now. |
| `build.rs` or `xtask` that hydrates | n/a | n/a | `build.rs` is impossible (see 0.2). An `xtask` only helps `cargo xtask run`, not `cargo run`. |

The SDK already ships the Rust refresh tool: `tools/psoxide-link` provides `psoxide-components`, and
`hl-build` calls `psoxide_link::components::materialize` directly (SDK README; `hl-build` main.rs
`prepare_psoxide`). The emulator's Python `bootstrap-components.py` is the older copy and can be
deleted once the vendor refresh uses the Rust one.

### 1.4 Windows, Linux, and macOS Intel

| Platform | What is evidenced | What is still unknown |
|---|---|---|
| macOS arm64 | Fresh clone, build, headless run [host-tested] | Window opens (not launched by me; Manny's own runs of main cover it) |
| macOS x86_64 | Cross-built; runs under Rosetta with identical frame hashes [host-tested] | Real Intel hardware, Metal path |
| Linux x86_64 | `Emulator` workflow on ubuntu-latest: check and test pass on main c7436746 [CI], with `pkg-config libasound2-dev libudev-dev libxkbcommon-dev mesa-vulkan-drivers` and `WGPU_BACKEND=vulkan` | A real desktop session on X11 and Wayland, audio, gamepad. CI is headless. |
| Windows x86_64 | `cargo check --locked -p psoxide --target x86_64-pc-windows-msvc` passes on the PoC branch (1 min 45 s) [host-tested]. It type-checks only: no link, no run. | Build with MSVC, DX12 or Vulkan window, WASAPI audio, XInput pad, `rfd` dialogs, the JIT feature is mac/linux only by cfg. `psoxide-settings/disc_image.rs` has a `cfg(windows)` arm, so someone has handled paths. |

What Windows and Linux need that I could not provide: a real run on each. The plan adds CI jobs that
build and test on `windows-latest` and `ubuntu-22.04`, and a short human smoke test list (boot a
disc, audio plays, pad works, window resize, file dialog) that someone with those machines runs
once per release. On Windows, rustup prompts for the Visual Studio Build Tools; on Linux the four
packages above.

### 1.5 Editor (EBonura/PSoXide-editor, main 6613c59d)

A fresh clone of the editor was 1.1 GB with `--depth 1` (360 MB `.git`, 710 MB of `editor/`)
[host-tested]. `editor/projects` is 694 MB: `default` 227 MB, `graybox-reach` 417 MB,
`cortex-ignition-0.5` 51 MB. GitHub reports the repo at 648 MB.

| # | Blocker | Evidence |
|---|---|---|
| E1 | Same Python hydration as B1, but for the SDK (full `sdk/`, `crates/`, four tool crates and two scripts) and 6 emulator crates. | `components.lock.json`, `Makefile` `bootstrap` |
| E2 | Pinned `nightly-2026-03-25`. Needed for guest builds (`build-std`, `-Zunstable-options`, `panic=immediate-abort`). The host editor itself passes `cargo check -p frontend` on stable 1.92 after bootstrap, 82 s [host-tested]. | `rust-toolchain.toml`, `tools/build_guest_staged.sh` |
| E3 | Play, Export and "Build examples" run `make build-editor-playtest` / `make examples`, which call `tools/build_guest_staged.sh`: POSIX `sh`, `rsync`, a stage at `/tmp/psoxide-psx-guest-v1`, an isolated Cargo home. None exist on stock Windows. | `app.rs:1449, 2793`, `Makefile:923`, the script |
| E4 | `repo_root_dir()` is `CARGO_MANIFEST_DIR/../../..`, fixed at compile time. A prebuilt editor looks for `engine/`, `sdk/`, `logs/` at the build machine's path. | `main.rs:135` |
| E5 | The guest cannot be prebuilt generically: the playtest program `include!`s a manifest generated from the project (`env!("PSXED_PLAYTEST_MANIFEST")`), and the notes say content headroom is a few KiB of static RAM. | `engine/examples/editor-playtest/src/main.rs:162` |
| E6 | 1.1 GB clone, dominated by sample projects. | measured above |
| E7 | The README still says MIPS binutils are required; the SDK README says the hazard checks are Rust now (`tools/psoxide-hazard`, no objdump). The README is stale. | both READMEs |
| E8 | Editor CI on main 6613c59d is red as of this morning [CI]. I did not investigate the cause. | `gh run list` |
| E9 | Pin lag: the editor pins emulator ca42a30 (10 commits behind main) and SDK c0d35c4 (16 behind). | `gh api compare` |

So the editor's honest one-liner is "needs rustup; the repo pins its nightly, so the first build
downloads it", and even then Play works only on macOS and Linux until E3 is ported to Rust. A
realistic editor target is in section 7, phase P3. I did not run a release build of the editor.

## 2. Prebuilt downloads

### 2.1 Targets

| Artifact | Built on | Contents | Size evidence |
|---|---|---|---|
| macOS universal `.dmg` with `PSoXide.app` | `macos-14` (arm64), x86_64 slice cross-built and joined with `lipo` | the app, licence, source note | 27.6 MB universal binary, 12.9 MB compressed [host-tested] |
| Windows x86_64 `.zip` | `windows-latest` | `psoxide-emulator.exe`, licence, source note | not measured |
| Linux x86_64 `.tar.gz` | `ubuntu-22.04` (lowest glibc of the hosted LTS images) | binary, licence, source note | not measured |
| Browser player | already exists | itch.io page and GitHub Pages | n/a |
| Later: Linux aarch64 | `ubuntu-24.04-arm` | for the Pi roadmap | not planned yet |

Runner facts [web, GitHub docs and changelog via search]: `macos-14` and `macos-15` are Apple
silicon; `macos-13` was retired in December 2025; Intel macOS is `macos-15-intel`, and GitHub says
Intel macOS runner support ends in fall 2027. That is why the draft cross-builds x86_64 from arm64
rather than depending on the Intel runner. `windows-11-arm` and `ubuntu-24.04-arm` exist if wanted.

Linux packaging: ship a plain tarball first. AppImage needs FUSE 2 on the user's machine, and on
Ubuntu 24.04 that means installing `libfuse2t64` [web, itsfoss and the AppImage wiki, secondary
sources], which is a worse first-run than unpacking a tarball. Revisit AppImage or Flatpak once
there is demand.

### 2.2 Backends per platform

From `emu/crates/frontend/Cargo.toml` [source-inspected]: window `winit` 0.30 (egui-winit with
`wayland` and `x11` on native), GPU `wgpu` 24, audio `cpal` 0.15, gamepads `gilrs` 0.11, dialogs
`rfd` 0.15, `objc2` for the Dock icon on macOS only. wgpu 24 runs on Vulkan, Metal, D3D12 and
OpenGL [web, docs.rs wgpu 24.0.5]: Metal on macOS, D3D12 or Vulkan on Windows, Vulkan on Linux.
I did not check which wgpu backend features are enabled by default in v24; check
`Cargo.lock` features before promising Windows 10 or older GPUs. Linux needs ALSA, udev and
xkbcommon development packages at build time; at run time it needs a Vulkan driver.

### 2.3 Signing and notarisation: what needs Manny

| Platform | What it takes | Cost / constraint | Without it |
|---|---|---|---|
| macOS | Apple Developer Program membership, a "Developer ID Application" certificate exported as `.p12`, an app-specific password, then `codesign --options runtime --timestamp`, `notarytool submit --wait`, `stapler staple` | $99 per year ([web], secondary sources; verify on Apple's site) | Downloaded app is quarantined; on Sequoia the user must approve it in System Settings, Privacy and Security (the right-click Open shortcut is gone) [web, secondary]. A binary fetched with `curl` or built locally with `cargo run` is not quarantined, so the build-from-source path is unaffected. |
| Windows | Authenticode signing. Microsoft's recommended route is Azure Artifact Signing, which needs no hardware token and works from GitHub Actions. | Individuals only in USA and Canada; organisations in USA, Canada, EU, UK. About $9.99 per month [web, Microsoft Learn via search; no primary price page found]. Otherwise an OV certificate, about $150 to 300 per year from DigiCert or Sectigo. | SmartScreen warns on unsigned and on new signed files; signing alone does not clear it quickly, EV no longer bypasses it since 2024 [web, Microsoft Learn]. Users click "More info, Run anyway". |
| Linux | Nothing required. Publish SHA256 sums, optionally a signature. | free | n/a |

Manny's inputs: Apple ID and the $99 enrolment, creating the Developer ID certificate, the
app-specific password; for Windows, a decision on entity (Bonnie Studios as a registered
organisation, or an individual outside US/Canada, which excludes Azure's individual path) and the
spend. I cannot create accounts or enter payment details. The draft workflow signs only when the
secrets exist, so releases can start unsigned.

### 2.4 Source obligations for binaries

The legal page already says the source for a distributed build must be identified and a moving
`main` is not enough (website `legal.md`, "Source for distributed builds"). Vendoring helps: a
release tag then contains the complete corresponding source, including the SDK crates. The draft
workflow writes the commit SHA and a tag URL into `SOURCE.txt` inside every archive and into the
release notes, and ships `LICENSE`. This follows the project's own stated policy; whether it fully
satisfies GPL-2.0 section 3 for every artifact is a question for 5.9.

### 2.5 Draft workflow

`drafts/release.yml` (also at `docs/distribution/release.yml.draft` on the docs branch, so GitHub
will not run it). It triggers on `v*` tags or manual dispatch, builds three jobs, signs only when
secrets are present, and creates a **draft** release with `SHA256SUMS.txt`. It uses first-party
actions only. It is YAML-valid [host-tested, parsed with PyYAML] and has never run. Known gaps: the
macOS bundle and icns steps are inline shell (should become a Rust `xtask package`), Windows has no
icon or version resource yet, and `Cargo.toml` still says version `0.1.0` (`psoxide-emulator --version`
prints it). The repo's only release today is `source-2026.09.05` with no assets, and the web demo
disc sits in the SDK repo's `web-disc` release.

## 3. Import: building ports from the user's own files

### 3.1 Concept

One interface, two apps. A library crate, recommended name `psoxide-import`, does the work. Its
screens live in the shared app library (4.3, recommended name `psoxide-app`), so each app shows the
same "Import a game" entry in its Library menu:

- **psoxide-emulator** shows the emulator plus Import.
- **psoxide-editor** shows the editor plus the emulator plus the same Import.

There is no standalone launchpad program. For a port such as HL or HK, Import does this:

1. Find the user's own install of the game (3.2).
2. Verify it is a version the cookers were tested on (3.3).
3. Fetch the port's source at a pinned tag (public GPL source, no game data) and make sure the
   needed Rust toolchain is present, asking before installing anything.
4. Run the port's builder locally with progress output (3.4, 3.5).
5. Put the resulting `.cue`/`.bin` in the user's game library and offer to boot it.

Nothing copyrighted is downloaded, uploaded or redistributed: the game files are read from where
Steam put them, converted data and the disc image stay on the user's disk. That matches the
existing HL policy ("generated data and BIN/CUE images are local outputs and must not be
redistributed", hl-psx README) and the HK notice ("Nothing was uploaded, published or burned",
hk-psx THIRD_PARTY_NOTICES).

### 3.2 Finding installs

| OS | Steam locations to check | Implemented today |
|---|---|---|
| macOS | `~/Library/Application Support/Steam/steamapps/common/<game>` | HL: yes (hard-coded path only). HK: no, macOS installs are excluded on purpose. |
| macOS, CrossOver | `~/Library/Application Support/CrossOver/Bottles/*/drive_c/Program Files (x86)/Steam` (also `Program Files`), plus the bottle's `libraryfolders.vdf`, mapping `C:` to `drive_c` and other letters to `dosdevices/x:` | HK only (`tools/doctor.py`), in Python |
| Linux | `~/.local/share/Steam`, `~/.steam/steam`; Flatpak Steam at `~/.var/app/com.valvesoftware.Steam/.local/share/Steam` [not tested; path from general knowledge, verify] | HL: first two, not Flatpak. HK: no. |
| Windows | `%ProgramFiles(x86)%\Steam`, registry `HKCU\Software\Valve\Steam` SteamPath [not tested; verify] | HL: `ProgramFiles(x86)` only. Not tested on Windows. |
| All | every library listed in `steamapps/libraryfolders.vdf`, and `steamapps/appmanifest_<appid>.acf` for the app | Only the CrossOver code reads `libraryfolders.vdf`; neither builder reads the registry or other-drive libraries |

App ids: Half-Life is 70 (Steam store link in the Xash3D FWGS README [web]); Hollow Knight is 367520
(`APP_ID` in hk-psx `tools/doctor.py`). `psoxide-import` should implement
`libraryfolders.vdf` parsing once, with the HL candidate list, the CrossOver scan and the OS-specific
roots as its sources, and a manual "choose folder" fallback (both builders already accept
`--half-life` / `--hollow-knight`).

### 3.3 Verifying versions

Today neither builder verifies a known-good version. HL checks that three paths exist
(`maps/c0a0.bsp`, `models/v_crowbar.mdl`, `sound`) and records a tree digest of the input after the
fact. HK checks the Unity data layout and the exe, and reports `installation_complete: "not
verified"`; HK accepts only the tested Windows format. To make verification real:

- Read `buildid` and `StateFlags` from the `appmanifest_<appid>.acf` (HK's doctor already extracts
  these keys) and compare to a table of tested build ids.
- For files the cookers depend on, compare SHA-256 against a manifest of tested files. Mechanism
  precedent: Ship of Harkinian tells users to check their dump's SHA-1 against a supported-hashes
  list (its README [web]). Whether publishing hash lists of copyrighted files has any legal weight
  is in 5.9.
- Record the build ids Manny's own installs have, because the README for neither port states which
  game build was used. That is a needed input from Manny.

### 3.4 What hl-psx needs to build today [source-inspected]

From the hl-psx README and `host/hl-build/main.rs`:

- Rust through rustup; the repo's `rust-toolchain.toml` selects a pinned nightly (so the first
  build downloads it), the platform linker, and internet for the first build.
- Explicitly not needed: a PSoXide checkout, Make, FFmpeg, Python, MIPS binutils.
- `cargo hl-build build` does the whole job: locate Half-Life (`valve/maps/c0a0.bsp`), hydrate the
  pinned SDK, emulator support crates and editor/engine into `.psoxide` using the Rust
  `psoxide_link::components::materialize`, cook 103 maps, models, sprites, sound and dialogue,
  audit residency, compile the guest, patch and scan hazards, pack the disc to `dist/hl-psx.cue`.
- The PGO profile is committed under `game/pgo/`; the guest does not fit RAM without it.
- The guest binary cannot be prebuilt for all users: `game/build.rs` sizes static arenas from the
  cooked data. So the user's machine needs the Rust toolchain.
- Cross-platform: `executable()` adds `.exe` on Windows and the path list includes
  `ProgramFiles(x86)`, so the author designed for it, but I have no evidence of a Windows or Linux
  run [not tested].
- Time: the README says only that the first build "takes longer"; PGO is about four minutes. I have
  no cook-time measurement and will not guess one.

### 3.5 What hk-psx needs to build today [source-inspected]

- Python 3.11+, a venv with pinned packages (`host/requirements.lock`, UnityPy and others),
  Rust, Git, `mipsel-none-elf-objdump` (per the README, which may be stale as for the editor), and a
  sibling PSoXide checkout.
- 281 `.py` files in the public tree (104 under `host/`). The Rust migration has begun
  (`host/hk-unity`, `hk-dotnet`, `hk-pil`, `hk-lz4`, `hk-cook`) but the driver still runs Python
  steps for regions, menu, guest build and reports (`host/hk-build/main.rs`).
- Input is the Windows Hollow Knight install only, found through CrossOver bottles on macOS, or
  given with `--hollow-knight` / `HK_DIR`. "The macOS copy is never a fallback."
- Output paths are hard-coded to `~/Downloads/ps1 games/hk-psx.{bin,cue}` (`host/paths.py`).
- Builds replay 26 route tapes against the emulator pinned in `emulator.lock.json`, which it builds
  once; validation is part of `build` unless `--no-validate`.
- `pgo` needs a checkout path without spaces.

So HK cannot be an Import target until the pipeline is Rust-only (Manny's stated goal, memory
note `rust-only-codebase`), Windows-install discovery works without CrossOver, and the output
directory is a parameter. I will not put a number on the Python port; 281 files is the scale.

### 3.6 Architecture

- **`psoxide-import`** (library, no UI): `discover()` returns installs (3.2); `verify()` checks them
  against a port manifest (3.3); `plan()` lists the steps and what each will download or write;
  `run()` executes a plan and emits progress events (step started, line of output, step finished,
  failed with the log path). The UI only renders events and answers consent prompts. A small
  `psoxide-import` CLI wraps the same calls for scripts and tests.
- **Port manifests** are data files shipped with the import crate: id, display name, source repo and
  pinned commit, game app id, tested build ids, tested file hashes, builder command, output name.
  Adding a port is a manifest change, and an app build with an empty manifest list shows no
  Import entry at all (relevant to 5.9 and to phase P6).
- **Where it runs**: the builders already print step banners (`==> bootstrap PSoXide components`);
  wrap them with line-oriented capture first and ask builders for a `--json-progress` flag later.
  HL's builder is `cargo hl-build`; the import crate calls it as a subprocess against the fetched
  source. Linking a builder into the app instead would still need `cargo` for the guest build
  (3.4), so a subprocess is simpler and keeps the app small.
- **Trust**: the build runs `cargo` on pinned remote source, which is executing downloaded code. The
  UI must show the repo and commit before running, pin by commit, and get explicit consent. It must
  not run anything from a manifest that was not shipped with the app release.
- **Toolchain provisioning**: check `rustup` and `git`; if absent, say so and link rustup.rs rather
  than silently installing it. If present, the repo's `rust-toolchain.toml` downloads the nightly
  on first use; tell the user the size beforehand (I have not measured it).
- **Network**: Git fetches of the pinned port source, crates.io, and the rustup toolchain download.
  No telemetry. If no `git` is installed, a tarball download needs an HTTP client dependency; that
  is an implementation decision for the first milestone.
- **Output location**: a parameter, not `~/Downloads/ps1 games` (HK hard-codes it today). Default to
  the user's configured games folder.

### 3.7 Consequences of embedding Import in both apps

Manny's decision (round 2) replaces my earlier recommendation of a separate launchpad. What follows
from it:

- The emulator stays a player at run time: Import is optional, only needs a toolchain when a user
  chooses to import, and can be compiled out with a cargo feature (`import`, default on) for
  packagers. It is never compiled for wasm; the web player cannot read local installs or run Cargo.
- The editor gets Import for free once both apps share the app library, so there is one
  implementation and one set of legal text.
- Legal isolation moves from "separate repo" to "separate crate with data-only manifests": the
  import crate is the only place that names Half-Life or Hollow Knight. Question 8 in 5.9 covers
  whether that is enough exposure separation.
- Because Import lives inside the shared shell, it depends on the parity merge in 4.5: it can only
  reach the editor after the editor's frontend fork is gone.
- The first release can ship the Import screens with an empty manifest list (so no port is named in
  any binary) until the legal gate in P6 clears.

## 4. Emulator and editor as separate products

### 4.1 How coupled they are today

| Fact | Evidence |
|---|---|
| Two repos since 2026-09-05 | `docs/sdk-separation.md` in the editor repo |
| The editor does not depend on the emulator frontend package; it carries its own `emu/crates/frontend`, binary `frontend`, with `editor` as a default feature | editor `emu/crates/frontend/Cargo.toml` |
| The two frontends are forks of one codebase: `src/` is 37.0k lines in the editor and 25.4k in the emulator; `diff -r` of the shared files shows 6,806 added or removed lines; `app.rs` alone is 4,906 vs 4,056 lines | [host-tested] diff of both trees |
| Editor-only frontend files: `editor_assets`, `editor_preview` (+ dir), `editor_textures`, `embedded_playtest`, `playtest_disc`: 10.9k lines, with 124 `feature = "editor"` gates across `app.rs`, `cli.rs`, `main.rs`, `ui/` | [source-inspected] |
| Emulator-only files: `web_*`, `debug_ui_png` | diff |
| The editor consumes only emulator *core* crates (emulator-core, debug-ui, settings, validation, gpu-render, plus `psoxide-vmcook` uses them) through `components.lock.json`; `psxed-ui` (the editor UI crate) does not depend on the emulator | `grep` of the manifests |
| Pins drift: editor at emulator ca42a30, 10 commits behind main | `gh api compare` |
| Play = the emulator in-process, inside the editor window, fed a disc made by `make build-editor-playtest` | `embedded_playtest.rs`, `app.rs` |
| Which download for what: to *play* discs, the emulator; to *make* a game, the editor (which includes an emulator) | README texts |

The cost of the fork is real: frontend fixes land in one repo and are re-applied by hand to the
other, and the editor sits ten emulator commits behind.

### 4.2 Options

| Option | Description | Verdict |
|---|---|---|
| A. Shared app library | Move the window/UI shell (`app`, `gfx`, library, debugger UI, toolbar, input, audio, cli core) into a library crate in the emulator repo with a small hooks trait. `psoxide-emulator` is a thin bin. The editor repo depends on that library (vendored or pinned like the core crates) and adds `editor_*`, `embedded_playtest`, `playtest_disc` behind the hooks. | **Recommended.** One frontend, no fork, both products share fixes. Cost: untangling 124 feature gates into a trait, in the 4k-line `AppState`. The old web-build note called this "moderate surgery". |
| B. Editor spawns the emulator | Play launches the standalone `psoxide-emulator` process with the disc; the editor drops its embedded emulator. | Simplest split and smallest editor, but loses the in-editor Play viewport and quick iteration. Reasonable as a stopgap or a `--play-external` mode, not as the end state. |
| C. Keep the fork, add a drift check | CI diffs shared files. | Cheapest, keeps the problem. Not recommended. |

### 4.3 Recommended product shape

- **psoxide-emulator**: play discs, debugger, profiler, library, web build, plus Import. Download:
  one app per OS (section 2). Contains no editor code, no `make`, no SDK examples scan, no
  repo-relative paths. This is what the one-liner and the release workflow in this plan produce.
- **psoxide-editor**: makes games; shows the editor, the emulator and the same Import. Download: the
  editor app plus a source bundle of `engine/`, `sdk/` and the pinned tool sources, and a first-run
  check for rustup and the pinned nightly (E2, E5 show Play compiles a program per project). It is
  built on the same app library as the emulator through option A. Its Play and Export steps move
  from `make` + `sh` + `rsync` to a Rust `xtask`, so they run on Windows, and its root-path logic
  reads a data directory instead of `CARGO_MANIFEST_DIR`.
- **psoxide-app** (new library, recommended name): the window, menus, library, debugger, input,
  audio and CLI shell now duplicated across the two frontends, with hooks for the editor. It also
  hosts the Import screens. It lives in the emulator repo and the editor repo consumes it the way it
  consumes the core crates today (vendored at a pinned revision).
- **psoxide-import** (new library): discovery, verification, plan and run (section 3). No UI of its
  own.
- **What moves**: from the emulator repo, the SDK-examples scan, `make examples` action and
  `editor/projects` lookup in `app.rs` (these belong to the editor or are deleted); into the
  emulator repo, a library target for the shell. In the editor repo, delete its copy of the shared
  frontend files and depend on the app library, after the parity items in 4.5 are merged. Sample
  projects (694 MB) move to release assets or a sparse-checkout path so the editor clone shrinks.

### 4.4 Names

Decision (Manny, round 2): "PSoXide" is the project. The products are **psoxide-emulator**,
**psoxide-editor**, **psoxide-sdk** and **psoxide-engine**. No package is called just `psoxide`.
Two helper crates in this plan follow the same scheme: **psoxide-app** (the shared window and UI
shell, 4.2 option A) and **psoxide-import** (section 3).

Map from today's names. "Cost" is what a rename breaks; "Recommend" is my advice. This round renames
only the emulator package and binary (done on the PoC branch, verified: `cargo run --release -p
psoxide-emulator -- --version` prints `psoxide-emulator 0.1.0`; fmt and check pass; the headless
Celeste hashes are unchanged).

| Today | Scheme name | Cost | Recommend |
|---|---|---|---|
| Package and binary `frontend` in PSoXide-emulator | `psoxide-emulator` | Done on the PoC. Downstream path and flag references listed below. | Do it (P0). |
| Binary `frontend` in PSoXide-editor (the fork) | `psoxide-editor` | None until the fork is replaced (4.5). | Name it when the editor binary is built on `psoxide-app`. |
| Repo `EBonura/PSoXide-emulator`, `EBonura/PSoXide-editor` | already `psoxide-emulator`, `psoxide-editor` apart from case | GitHub URLs are case-insensitive. | No action. |
| Repo `EBonura/PSoXide` (SDK, website, docs, shared tools) | `psoxide-sdk` | Old URLs, clones and API calls redirect, but the Pages site does not: it is served at `ebonura.github.io/PSoXide/` ([web] GitHub docs: "Project site URLs are the exception"; use a custom domain before renaming). Every game's `components.lock.json` names `EBonura/PSoXide`; tarball and `cargo install --git` URLs rely on the redirect. Redirects die if anyone creates a new repo under the old name. | Do not rename now. Get a custom domain for the site first, then rename, and never reuse the old name. The SDK is also the project hub today (website, docs), so a clean split of the hub from the SDK may come first. |
| `engine/` inside PSoXide-editor (`psx-bsp`, `psx-engine`, `psx-game-runtime`, `psx-render-contract`, `psx-chainloader`, `psx-goldsrc`, `psx-level`, `psx-carousel`, `psx-disc-toc`) | `psoxide-engine` | Games consume it through the editor repo's lock paths (`engine`, `editor/crates`), e.g. hl-psx's lock. A separate repo changes those paths in every game. | Use the name now in docs and READMEs. Split the repo later, only when something consumes the engine without the editor. |
| `emulator-core` (package) | `psoxide-emulator-core` | 47 files in the emulator repo import `emulator_core`; the editor and every game's lock list `emu/crates/emulator-core` (directory names can stay; only the package name changes). A generic name like `emulator-core` would also be a poor crates.io claim. | Rename the package when the first crates.io publish is planned, not before. |
| `psx-gpu-render` | `psoxide-gpu-render` (it is emulator-side, not device code) | 5 files in the emulator repo, plus pins. | Rename with the next pin bump. |
| `psoxide-debug-ui`, `-jit`, `-settings`, `-validation`, `-link`, `-pgo`, `-hazard`, `-dev`, `-vmcook` | already in scheme | none | Keep. |
| `psx-*` SDK and shared crates (`psx-hw`, `psx-rt`, `psx-gpu`, `psx-gte`, `psx-io`, `psx-math`, `psx-spu`, `psx-pad`, `psx-mc`, `psx-iso`, `psx-trace`, ...) | keep | Every game's source says `use psx_gpu::...`; every manifest and lock names them. | **Do not rename.** `psx-` is the platform vocabulary, like `psx-spx`. If they are ever published, Cargo's `package = "..."` key lets a crate be published as `psoxide-sdk-gpu` while games keep writing `psx-gpu` (Cargo feature, not tested here). |
| `psxed-*` editor crates, binary `psxed` | keep for now | Editor lock paths in every game. | Keep; revisit with the engine split. |

crates.io facts [web, crates.io API, 2026-10-09; "free" is not a reservation]: `psoxide`,
`psoxide-emulator`, `psoxide-editor`, `psoxide-sdk`, `psoxide-engine`, `psoxide-import` and most of
the `psx-*` names I probed (`psx-hw`, `psx-gpu`, `psx-rt`, `psx-gte`, `psx-iso`, `psx-io`, `psx-math`,
`psx-asset`, `psx-font`, `psx-engine`, `psx-trace`, `psxed`) returned 404, so are unpublished. **`psx-spu`
is taken** (another project, version 0.1.1, updated 2026-04-12) and so is `psx` (0.1.8). Publishing the
SDK crates under their current names would therefore collide at least once.

Downstream breakage from the emulator rename (references to update in P0):

- hl-psx: `--frontend`/`PSOXIDE` build paths that name `target/release/frontend`, and the README
  regress command (`cargo hl-build regress --psoxide ...`) docs.
- hk-psx: `cargo hk-build validate --frontend ../PSoXide-emulator/target/release/frontend`, the
  `python3 tools/validate.py --emulator .../frontend` line, and the emulator build it does itself from
  `emulator.lock.json` (it looks for the built binary by name).
- SDK: `make run-tri FRONTEND=/absolute/path/to/frontend` in the README and Makefile.
- demo-disc and the other games' validation scripts, plus the `psoxide-debug` skill, which name `-p frontend` or
  `target/release/frontend`.
- The editor repo is unaffected until its fork goes: it builds its own `frontend`, and its Makefile
  keeps `-p frontend`.
- `psoxide-validation` already accepts the runner names `psoxide` and `frontend`; I did not touch it
  (it names a validation runner, not the package).

I searched the emulator repo only. The other repos need a grep before P0 lands; I have not done that.

### 4.5 Parity audit: what the two frontends have that the other lacks

Method [source-inspected unless noted]: the two frontend trees were byte-identical on 2026-09-05
(tree hash 7c95974 in both repos: emulator commit e1f10617, editor commit d4f77eb3). I diffed each
side against that base, then diffed the two current trees (emulator main c743674, editor main
6613c59d), ignoring everything behind `feature = "editor"` and the editor-only files
(`editor_*`, `embedded_playtest`, `playtest_disc`), which stay with the editor. Commit SHAs differ
between the repos, so I matched commits by date and subject. 30 frontend commits exist only in the
editor and 45 only in the emulator, all after the base. Of 6,806 changed lines in shared files, most
sit in `app.rs` (1,950), `ui/menu.rs` (1,484), `cli.rs` (1,347), `main.rs` (505) and `ui/toolbar.rs`
(494). Dates are commit dates. I read the diffs and grepped both trees for each item; I did not run
the editor build, so no behaviour below is run-tested. A first row-by-row read could have missed a
small item; the "Parity" list is where I would look for mistakes.

**Emulator has it, editor lacks it (13 rows).** Home: `psoxide-app` unless noted.

| # | Feature, fix or difference | Dated | Newer/better | Where it should live | Risk |
|---|---|---|---|---|---|
| EM1 | Menu redesign: Library, Game, Settings; developer tools in the sidebar; toolbar slimmed. Editor still has Games, Examples, Projects, Editor, System, Settings. | 2026-09-26 | Emulator | Menu as data; editor adds its categories | High (tests index categories by position) |
| EM2 | Collapsible library folder tree; launch ids from paths; folder field on library rows. Editor has a flat list and its own shared-disc-id fix (2026-09-15). | 09-06, 09-25 | Emulator | Shared | High (save identity) |
| EM3 | Disc images read on demand (`load_disc_from_bin`, `Disc::from_source`), a frame waits for pending sectors (`disc_ready_for_frame`), `disc_waits` profile column. Editor reads the whole BIN into memory. | 09-25 | Emulator | Shared | Medium |
| EM4 | Opt-in "smooth slow host" setting and `HostPace` adaptive frames-per-paint. | 09-25 | Emulator | Shared setting | High (conflicts with ED1) |
| EM5 | Native AArch64 tier hook (`install_native_tier`, feature `native-jit`). The editor lock already carries `psoxide-jit`. | 09-27 | Emulator | Shared, feature-gated | Low |
| EM6 | Audio underrun counter in the output callback, shown on the performance panel's host line. | 09-25 | Emulator | Shared | Low |
| EM7 | Smooth and Edge texture filters; xBR and JINC2 removed (clean-room provenance work). Editor still has the xBR path. | 10-08 | Emulator | Shared; shader is in `psx-gpu-render` | High (pin plus provenance) |
| EM8 | Phosphor-only icon subset (fonts 176 KB). Editor ships lucide 742 KB plus full Phosphor (1.8 MB). | 09-25 | Emulator, but editor needs lucide | Shared; lucide under the editor feature | High (startup panic) |
| EM9 | Headless CLI: `--route-watch-u32` (09-06), `--route-log-host-ns` (09-24), `--debug-ui-png` with width, window, pointer and csv options (09-25). | 09-06 to 09-25 | Emulator | `psoxide-app` cli | Low |
| EM10 | CLI hardening: malformed disc returns a boot error; unknown or removed filter names are rejected. | 09-25, 10-08 | Emulator | Shared | Low |
| EM11 | Web player stack: `web_disc`, `web_embed`, `web_bench`, slice reads and streaming, WebAssembly SIMD, demo disc at page open, iframe embed. | 09-25 to 09-30 | Emulator (editor never builds wasm) | Shared, `cfg(wasm)` | Medium (must stay wasm-clean) |
| EM12 | Reuse of the frame-start VRAM snapshot buffer for the hardware renderer. | 09-25 | Emulator | Shared | Low |
| EM13 | Core-crate drift that arrives with a pin bump, not frontend code. The editor pins emulator ca42a30 (2026-10-08 15:47 UTC); the 10 commits after it are the Smooth/Edge filters (3), XA 44.1 kHz conversion, a CD underrun repeat, console-measured CD timing, SWC2 timing, an HBlank counter fix and a README edit [`gh api compare`]. | 10-08 | Emulator | Pin bump | Medium |

**Editor has it, emulator lacks it, outside `feature = "editor"` (6 rows).**

| # | Feature, fix or difference | Dated | Newer/better | Where it should live | Risk |
|---|---|---|---|---|---|
| ED1 | Hard frame cap: one guest frame per redraw (two if the monitor refresh is too slow), backlog dropped. Emulator catches up to 4 frames unless EM4 is on. | 2026-09-24 | Editor, tuned for Play beside a heavy editor UI | Shared, as a per-workspace pacing policy | High |
| ED2 | One `shut_down_for_exit` path with an `editor_close_allowed` veto. The emulator repeats the quit sequence three times in `main.rs`. | 09-25 | Editor | Shared, with a hook for the veto | Medium (data loss if a path is missed) |
| ED3 | `Gfx::take_close_requested`: an egui close command goes through the window-close path. | 09-25 | Editor | Shared | Low |
| ED4 | Lucide font registered in `theme.rs`; the code says the editor panics at startup without it. | before the split | Editor requirement | Under the editor feature | High (see EM8) |
| ED5 | Input tape `stop_replay` and `is_replaying`. Used only by embedded Play. | before the split; emulator removed them as unused 09-05 [inferred] | Editor | Editor feature | Low |
| ED6 | Profiler `live_average`, `average_recent_ms`, `latest`. Used only by `editor_play_metrics`. | before the split; emulator removed 09-05 [inferred] | Editor | Editor feature | Low |

**Parity (no action beyond keeping one copy), 7 items:** BIOS removal and HLE boot (emulator
2026-09-15 to 09-25, editor 09-15 and 09-25); clock-driven audio drain (both 09-22); the guest
performance panel and its F3 toggle (both 09-25); LibCrypt `.sbi` loading; per-launch memory card path
(`port1_memcard_for_launch`); shared-disc-id launch fix, done differently on each side (EM2); and four
features the emulator moved out of the toolbar without loss (save-state buttons, controller routing,
debug toggles, single-step), which now sit in the Game menu, `ui::controller_ports` and the debug
sidebar.

**Counts:** 13 emulator-ahead rows, 6 editor-ahead rows, 7 parity items. Nothing in the editor's
fork is lost by deleting it if rows ED1 to ED6 land in `psoxide-app` or the editor feature first, and EM1,
EM2, EM4, EM7 and EM8 are resolved by test, not by default.

**Riskiest to merge, in order:**

1. **Frame pacing (ED1 against EM4).** Two policies with different goals. Pick one mechanism with a
   per-workspace setting, and re-measure editor Play and plain emulator speed. Neither was measured here.
2. **Menu architecture (EM1).** Editor tests assert menu positions (`categories[5]` is System) and the
   editor workspace toggle, Projects and Editor categories depend on the old layout.
3. **Fonts (EM8, ED4).** Drop lucide from the shared lib and the editor panics at startup.
4. **Filters and the `psx-gpu-render` pin (EM7, EM13).** The frontend and the crate must move together.
   The filter is not persisted in settings (it is a runtime cycle), so there is no saved-setting
   migration, but the shader uniform values differ (xBR was 3, Edge is 1) and the editor's preview and
   Play use the same renderer. The pin bump also needs the editor's red CI fixed first (E8).
5. **Launch ids and save identity (EM2, EM3).** The two sides fix the same shared-disc-id bug
   differently. Check that existing memory cards and save states still resolve for both, and test
   huge BINs with the on-demand reader inside the editor's Play.
6. **Quit path (ED2).** Merge the three emulator copies into the single path before adding the editor
   veto, or a quit can skip the project save.


## 5. Legal and provenance fact sheet

Facts only, each with a source. No conclusion about permissibility is drawn.

### 5.1 PSoXide licences

- Emulator, SDK, editor and engine are GPL-2.0-or-later; the Quake port is GPL-2.0-only. The same
  statement says a top-level licence does not grant rights its contributors do not hold. Source:
  website `content/legal.md` ("Code and licences", last reviewed 30 September 2026).
- GitHub's licence detector reports `GPL-2.0` for hl-psx, hk-psx, the emulator and the editor
  [host-tested, `gh api`].
- Distributing a program that incorporates or links PSoXide's GPL runtime or SDK generally requires
  licensing the combination consistently and providing corresponding source (`legal.md`;
  `docs/downstream-licensing.md`, "What this means if you build on PSoXide").

### 5.2 Emulator provenance

- Not claimed clean-room as a whole; heavy AI assistance disclosed (`legal.md`, README "How This
  Was Built", `downstream-licensing.md`).
- Current state per the emulator's `docs/PROVENANCE.md`: scheduler, DMA, SIO0, interrupt
  controller, pads, XA-ADPCM, MDEC, SPU and most CD-ROM timing rewritten from nocash PSX-SPX and
  console measurements, "no remaining derivation known"; CPU, bus, GPU and hardware renderer not
  rewritten and keep behaviour first matched to PCSX-Redux traces, marked `gate-pinned`; three CD
  delay values (GetID, Pause, Stop) are still Redux's.
- **Inconsistency to fix before shipping binaries**: website `legal.md` (30 September) still says the
  scheduler, DMA, SPU envelopes, MDEC and SIO derive from PCSX-Redux. `docs/PROVENANCE.md` and the
  emulator README at c743674 say those were rewritten (the rewrite landed 2026-10-08, campaign log).
- No BIOS: the HLE kernel is written from public documentation and measurements; external BIOS
  loading was removed in September 2026 (`legal.md` "No BIOS"; `docs/hle-bios-provenance.md`).

### 5.3 hl-psx

- Public repo with a source-only history (consolidated 2026-09-21, memory note), noncommercial,
  source-only, bring your own assets (hl-psx README, `LICENSING.md`).
- Code derivation, from `PROVENANCE.md` (audit refreshed 2026-09-07): "source-informed Rust
  adaptation work, not a clean-room implementation" of Valve's public Half-Life 1 SDK; fixed-point
  adaptations of Quake's `SV_RecursiveHullCheck` and `SV_FlyMove` in `game/src/phys.rs` (Quake is
  GPL-2.0-or-later compatible); Xash3D FWGS consulted as a behavioural oracle, no copied code known;
  a similarity scan on 2026-07-16 found no meaningful verbatim blocks and states its own limits.
  Valve SDK reference audited: ValveSoftware/halflife commit b1b5cf5.
- No Valve binary assets are tracked; the repo does contain Half-Life identifiers (maps, models,
  entities, weapons, chapter titles) and UI references (`PROVENANCE.md`).
- **Clean-room branch**: `cleanroom/hl-2026-10-03` exists on origin. Compared with main it is
  53 commits ahead and 43 behind (diverged), last commit `c22d118d` on 2026-10-04
  ("Reword comments next to the rewritten items so they stop naming SDK classes"). It is not
  merged; the 10-04 queue lists phases 2 to 4 as running, then work was parked at the 10-04 wrap-up
  [host-tested, `gh api compare`; work-dir campaign logs]. The project owner's recorded position
  (memory note, 2026-10-03) is that translated Valve SDK code in hl-psx gets clean-room treatment;
  game-rule constants and mechanics stay.
- Build-time fact: the build reads the user's local Half-Life install and writes converted data under
  git-ignored `data/`, `dist/`, `.hlpsx/`; "The build orchestration contains no network upload
  client" (`PROVENANCE.md`).
- The generated disc omits Sony's licensed system area (`PROVENANCE.md`).

### 5.4 hk-psx

- Public repo, history squashed to one root commit on 2026-10-08 (memory note). Licence per GitHub:
  GPL-2.0. No public build; the website calls it "an experiment" (`legal.md`).
- Source tooling credited in `THIRD_PARTY_NOTICES.md`: UnityPy (MIT), TypeTreeGeneratorAPI (MIT),
  dnfile (MIT), dncil (Apache-2.0), AssetsTools.NET rules ported to Rust (MIT), Pillow ops ported
  (MIT-CMU/HPND), liblz4 HC ported (BSD-2-Clause), and emulator triangle-raster equations copied
  from PSoXide-emulator (GPL-2.0-or-later).
- Behaviour comes from game data and game code read on the user's machine: the cookers read
  Unity files and `Assembly-CSharp.dll`, and `host/inspect_il.py` inspects managed IL (README, file
  list). "14 native controllers written against the source numbers" (README). These are facts about
  what the tool does, relevant to the reverse-engineering clause in 5.5.
- "Hollow Knight files, Unity data, game code, sprites and audio remain the property of their
  respective owners. The project license grants no rights to redistribute those inputs or their
  converted forms." (`THIRD_PARTY_NOTICES.md`). Hollow Knight's own end-user licence terms were
  **not located** in this pass.
- An external collaborator has write access to the repo (memory note, 2026-10-06; Actions disabled).

### 5.5 Valve terms

- Half-Life SDK licence (ValveSoftware/halflife `LICENSE`) [web, fetched]: free use of the SDK to
  develop a modified Valve game that runs on the Half-Life 1 engine; copy, modify and distribute
  the SDK and modifications in source and object form; distribution must be free of charge;
  distributions of the SDK or a substantial portion keep Valve's notice and the warranty text;
  commercial use requires contacting Valve (sourceengine@valvesoftware.com). Playing Valve games is
  governed by the Steam Subscriber Agreement.
- Steam Subscriber Agreement [web, fetched, section numbers as the fetch reported them]: 2.A a
  non-exclusive licence to use Content and Services for personal, non-commercial purposes, licensed
  not sold; 2.G no copying, modifying, decompiling, reverse engineering or creating derivative
  works of the Content and Services without Valve's written consent "unless ... applicable law
  permits otherwise"; 2.C use of Valve developer tools such as the Source Engine SDK, and
  distribution of what you create, on a non-commercial basis; 2.D fan art incorporating Valve game
  content may be distributed non-commercially.
- The hl-psx audit records that the SDK licence "does not expressly describe a source-informed
  implementation on a separate runtime", that Codename: Gordon is licensed by Valve, and that
  Valve's Video Policy covers videos, not asset distribution (`PROVENANCE.md`).

### 5.6 Sony

- PlayStation is Sony's trademark. hl-psx omits BIOS, SDK and system-area material and records that
  this "reduces the Sony-content issue but does not create a Sony licence" (`PROVENANCE.md`).
- PSoXide has no BIOS and no Sony SDK in the emulator or SDK (`legal.md`, firmware-cleanup record).

### 5.7 How comparable projects describe bring-your-own-data

All [web], fetched from each project's README on 2026-10-09:

| Project | What it says |
|---|---|
| sm64 decomp (n64decomp/sm64) | Repo does "not include all assets necessary for compiling the ROMs"; you supply your own copy and place a `baserom.<VERSION>.z64`; decompiled source and tools are distributed. |
| Ship of Harkinian (HarbourMasters) | Game "does not include copyrighted assets"; "You are required to provide a supported copy"; assets are extracted from your ROM into an `.o2r` archive; hash check against a supported list; "we do not condone piracy"; no affiliation statement. |
| OpenMW | "You need to own the game for OpenMW to play Morrowind." |
| Xash3D FWGS | Needs the `valve` folder from the user's Half-Life; issues accepted only for a legally acquired copy; no Valve disclaimer in the README. |
| DevilutionX | Needs `DIABDAT.MPQ` from the user's copy, or the shareware `spawn.mpq`; "in no way associated with or endorsed by Blizzard". |

A counter-example on enforcement, [web, news coverage via search, not primary]: Take-Two sent DMCA
notices against the re3 and reVC GTA reverse-engineering projects in 2021, GitHub removed them,
a counter-notice restored them, and Take-Two then sued and GitHub removed them again. Those projects
required users to own the games. I found no final court ruling. This is a fact about an outcome, not
a prediction about PSoXide.

### 5.8 Wording in use today

- Trademark line (hl-psx README): "Half-Life is a trademark of Valve Corporation. PlayStation is a
  trademark or registered trademark of Sony Interactive Entertainment Inc. HL-PSX is unofficial and
  is not affiliated with or endorsed by Valve or Sony."
- Emulator README: "PlayStation is a trademark of Sony Interactive Entertainment Inc.; PSoXide is not
  affiliated with or endorsed by Sony." `legal.md` adds Valve and "the other rights holders named
  here".
- `legal.md` states: requiring your own copy "does not, by itself, grant permission to adapt or
  redistribute that data, including converted files and completed disc images", and that
  noncommercial status, source availability and a rights holder's silence do not establish
  permission.

### 5.9 Questions for a lawyer before a public HL/HK Import feature

1. Does a tool that reads a user's lawfully installed game, converts it locally, and never
   transmits it, raise a different question from publishing a cooker? Does the answer differ when
   the vendor of the tool pins and fetches the cooker source for the user?
2. SSA 2.G (no reverse engineering or derivative works without consent, "unless applicable law
   permits") against a cooker that converts Valve maps and models into a new format on the user's
   machine. Does the user's local conversion count, and who is the actor, the user or the app
   publisher? Same question for Hollow Knight's own licence, which still has to be located.
3. hl-psx's Valve-SDK-informed code: is the Half-Life 1 SDK licence relevant to a port that runs on
   a different runtime, given the licence text covers "modified Valve games running on the
   Half-Life 1 engine"? Does the planned clean-room rewrite (merge status 5.3) change the answer, and
   what evidence of independence would a lawyer want?
4. Reading and decoding managed game code (Hollow Knight's IL) to extract numbers: acceptable
   under the relevant jurisdiction's interoperability or research exceptions, or not? Which
   jurisdictions matter (UK, US, EU)?
5. Are hash lists of copyrighted game files, build ids, and Half-Life or Hollow Knight file names in a
   public manifest a problem?
6. GPL: an app that runs GPL port code to produce a disc image that contains GPL-derived code
   plus converted proprietary data. Does the local output ever need to be treated as a combined
   work, and does it matter that the user never redistributes it?
7. Trademark and affiliation wording for a tool that names "Half-Life" and "Hollow Knight" in its UI
   and manifests. Is nominative use enough, and is more disclaimer text needed?
8. Platform exposure: the re3 precedent suggests rights holders act through GitHub takedowns. What
   is the exposure of the PSoXide repos, the releases and the itch pages, and does it differ now that Import ships inside the emulator and editor apps
   rather than in a separate program?
9. Whether the binary release process (SOURCE.txt, tag, vendored crates) meets GPL-2.0 section 3 for
   each artifact type, including the macOS app bundle and any future AppImage.
10. Sony: anything further needed beyond omitting BIOS and system-area material for generated discs
    and for the emulator's HLE kernel.
11. The "other account" and collaborator situation: hk-psx has an external writer and a history
    that was squashed. Is any attribution, contributor-licence or copyright-holder record missing?

## 6. READMEs

Drafts in `drafts/` and on the docs branch under `docs/distribution/`: `README.emulator.md`,
`README.editor.md`, `README.sdk.md`. Each follows one structure: the agentic-coding callout copied
unchanged from the current READMEs, one line saying what it is, a screenshot, a Download table, a
short "Build from source" block, a feature list, links, then licence, provenance and trademark
lines. They use the product names from 4.4 (titles "PSoXide Emulator", "PSoXide Editor", "PSoXide
SDK"; binary `psoxide-emulator`). They are written for the state after the phases they depend on,
and each draft has a header comment marking lines that are not true yet:

- Emulator: the clone-and-run block is true on the PoC branch only; the Download table needs the
  first release; the Import line needs P5.
- Editor: both the one-liner [P3] and the Download table [P3] are not true yet, it says plainly that
  Play compiles a program and needs rustup, and the Import line needs P4 and P5 (the editor only
  gets Import after the fork is replaced).
- SDK: no Download table, because the SDK is distributed as a pinned revision; the quick start
  assumes a Make-free `cargo xtask disc hello-tri` that does not exist yet.

Screenshots are placeholders: the emulator has `docs/images/compat-grid.png`, but a window capture
of the app with the debugger open is a better lead image. The itch and website links are
`bonnie-studios.itch.io/psoxide` and the GitHub Pages site as used in the current READMEs.

## 7. Phased plan

Efforts are my estimates in engineer-days of focused work, as ranges, assuming the repo owner
reviews. They exclude waiting on accounts and legal. Hardware smoke tests need someone with the
machines. Round 2 changed: P0 now includes the `psoxide-emulator` rename and its downstream edits;
P4 absorbs the parity audit's merge work and the shared `psoxide-app` library; the standalone
launchpad is gone, so P5 builds `psoxide-import` and its screens inside `psoxide-app`.

| Phase | Scope | Effort | Needs Manny |
|---|---|---|---|
| P0 One-line build and names, emulator | Review and merge the PoC (package already renamed `psoxide-emulator`); add CI jobs on `windows-latest` and `ubuntu-22.04` and an MSRV job; use the Rust `psoxide-components` for the vendor refresh and delete the Python bootstrap; grep every repo and update references to `target/release/frontend`, `-p frontend` and `--frontend` (hl-psx, hk-psx, SDK `FRONTEND=`, demo-disc, `psoxide-debug` skill; list in 4.4); decide the version number. No SDK crate or repo renames. | 2 to 4 | One real Windows run and one Linux desktop run |
| P0b One-line build, SDK | `cargo xtask disc <example>` so the SDK needs no Make; hello-tri quick start. | 2 to 4 | none |
| P1 Release binaries, unsigned | Move the draft workflow in, dry-run on a test tag, fix what breaks; `xtask package` for the app bundle and icon; Windows icon resource; checksums; SOURCE.txt; website download page; smoke test checklist run on all three OSes. | 4 to 8 | Test machines or testers for Windows and Linux |
| P2 Signing | Apple enrolment, certificate, notarisation in CI; Windows signing service decision and setup. | 1 to 3 of work, plus enrolment lead time | Apple Developer account ($99/yr), certificate and app password; entity decision and spend for Windows |
| P3 Editor as a buildable product | Vendor the SDK subset into the editor, shrink the clone (sample projects out of the default checkout), port the `build_guest_staged.sh`/`make` Play path to a Rust `xtask`, replace compile-time root paths with a data-dir resolver, first-run toolchain check, fix editor CI, bring pins current (the pin bump carries the filter change, so do it with P4's EM7 work). Editor release workflow. | 15 to 30 | Which sample projects stay in the default clone; a Windows tester |
| P4 Shared app library and parity merge | Create `psoxide-app` in the emulator repo from the emulator frontend (option A, 4.2); land the six editor-side rows ED1 to ED6 (4.5) as hooks and features; resolve the five riskiest items by test (frame pacing, menu data, fonts, filter pin, launch and save identity); rebuild the editor binary `psoxide-editor` on it; delete the editor's fork; remove SDK-example and `editor/projects` code from the player. | 20 to 40 | Approval of option A over B; a call on the pacing policy |
| P5 Import, HL first | `psoxide-import` crate (discovery on all OSes, `libraryfolders.vdf`, version check against tested build ids, pinned fetch, run `cargo hl-build`, progress events, library install) plus the Import screens in `psoxide-app`, shown by both apps. Ships with an empty port manifest list until P6 clears. Needs P4 for the editor; the emulator can ship it earlier. | 10 to 20 | Tested HL build ids and file hashes from his installs; consent text |
| P5b Import, HK | Blocked by the Python-to-Rust port of the HK pipeline, Windows-install discovery without CrossOver, and a parameterised output directory. | not estimated; 281 Python files is the measure | The Rust-only port is already his stated goal |
| P6 Legal gate | Merge `cleanroom/hl-2026-10-03` (rebase, regate), reconcile `legal.md` with `PROVENANCE.md`, answer 5.9 with a lawyer, then enable the HL (and later HK) manifests. | engineering 5 to 15 for the HL clean-room finish; lawyer time unknown | A lawyer, the decision to ship, and for HK the Team Cherry terms |

Order I would take: P0, then P1 unsigned; P2 alongside; P4 (with P3's editor-build work) in
parallel; P5 as soon as the emulator side exists; P6 gating only the manifests, not the screens. P0
and P1 give a visible win (downloads for the emulator) without touching the editor or anything legally
sensitive.

## 8. Needs from Manny

1. Merge approval for the PoC and the `psoxide-emulator` rename (P0), and a go-ahead to update the
   other repos' references to the old binary name.
2. Apple Developer enrolment and the signing secrets; the Windows signing route and entity (P2).
3. A Windows and a Linux machine or a tester for the smoke checklist (P0, P1).
4. The pacing policy call (4.5, ED1 against EM4) and which editor sample projects ship in the default clone (P3, P4).
5. The Half-Life and Hollow Knight build ids and file hashes from his installs, and which game
   versions the cookers were validated on (P5).
6. A lawyer and the questions in 5.9; the Team Cherry terms for Hollow Knight (P6).
7. A custom domain for the website before any rename of `EBonura/PSoXide` (4.4); until then the repo keeps its name.
8. Whether to claim crates.io names early (4.4); not needed for any phase here.

## Appendix A. Repositories and artefacts

- Work dir: `~/Library/Application Support/PSoXide-perf/work/dist-plan-2026-10-09`
- PoC clone and branch: `emulator/` on `poc/one-liner-build-2026-10-09` (base main c743674), pushed.
- Docs branch: `docs/distribution-plan-2026-10-09` in the same clone, off main: this file,
  `docs/distribution/release.yml.draft`, and the three README drafts; pushed.
- Fresh-clone proof: logs `fresh-run.log`, `fresh-check.log`, `fresh-test*.log`, `fresh-x86.log`,
  `fresh-win.log` and `rename-check.log` in the work dir (the fresh clone itself was cleaned up).
- Fetched sources: `src/` (READMEs, PROVENANCE, LICENSING, THIRD_PARTY_NOTICES, legal.md, hk
  doctor, hl-build main.rs).
- Parity audit working material: `editor/` is a partial clone with the editor's full history and a
  sparse checkout of `emu/crates/frontend`; `e.log` and `m.log` list each side's frontend commits.

## Appendix B. Things I noticed outside the brief

- The editor's `README.md` still lists MIPS binutils as a prerequisite (E7).
- Editor main CI is red (E8).
- The standalone emulator frontend scans repo-relative SDK and editor paths (B7).
- `psoxide-emulator --version` reports 0.1.0 for all builds.
- `tools/build-web-player.py` (Python) builds the web player bundle, so the web path is not Rust-only yet.
- The website `legal.md` still describes Redux derivation the emulator's PROVENANCE.md says is rewritten (5.2).
