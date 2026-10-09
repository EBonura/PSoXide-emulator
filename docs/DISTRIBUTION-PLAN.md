# PSoXide distribution plan

Date: 2026-10-09. Status: design plus one proof of concept. Nothing here is pushed.

This plan covers four things: a build-from-source path as short as the one on getartcraft.com/apps,
prebuilt downloads for every platform, a launchpad that builds the Half-Life and Hollow Knight
ports from the user's own game files, and a clean split of the emulator and editor into two
products. It is not legal advice, and neither is the fact sheet in section 5. It records facts with
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
   `psoxide` binary with no Python, no Make and no hydration step. Fresh clone to finished release
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
4. **Half-Life is launchpad-ready in design, Hollow Knight is not.** `cargo hl-build build` is a
   Rust-only, cross-OS-aware builder that finds Steam installs itself. `cargo hk-build` shells out
   to 281 Python files, needs a venv, finds only Windows installs inside CrossOver bottles, and
   writes discs to a hard-coded `~/Downloads/ps1 games` [source-inspected].
5. **Provenance has gaps that block a public launchpad regardless of engineering**: the HL
   clean-room branch is unmerged and diverged (53 ahead, 43 behind main), the website legal page
   still describes Redux derivation that the emulator's own PROVENANCE.md says is rewritten, and
   nobody has asked a lawyer the questions in 5.9.

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

Branch `poc/one-liner-build-2026-10-09` in the work dir clone `emulator/`, five commits on main
c743674, not pushed. Summary of the diff: 121 files, almost all vendored sources.

- Vendors `crates/{psx-hw,psx-iso,psx-trace,psxed-format}` and
  `sdk/crates/{psx-gpu,psx-gte,psx-gte-core,psx-io,psx-math,psx-spu,psx-telemetry,psx-vram}` plus a
  trimmed `sdk/Cargo.toml` (it must stay, because the crates inherit package fields from it) and six
  example dirs, at SDK revision 4e97cf3 as in `components.lock.json`. The lock file now lists
  exactly these paths and says `"vendored": true`.
- Drops `tools/mkisopsx` and `tools/psoxide-link` from the workspace (the frontend does not use
  them), `rust-toolchain.toml`, and the bootstrap steps in `Makefile` and `ci.yml`.
- Renames the package to `psoxide`, adds `default-members`, fixes profile names, rewrites the README
  build block.

Verification, all from a fresh `git clone` of the branch into `fresh-emulator/` [host-tested]:

| Check | Result |
|---|---|
| `cargo run --release -- --help` (cold, includes crate downloads) | exit 0, 227 s wall, load ~40 |
| Binary | `target/release/psoxide`, 13,305,424 bytes, arm64 |
| `psoxide launch --path <Celeste cue> --steps 8000000 --dump-hash` | `vram_fnv1a_64=0xff5327f5b858460d`, `display_fnv1a_64=0xc2779d4f09200444` |
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
| Windows x86_64 `.zip` | `windows-latest` | `psoxide.exe`, licence, source note | not measured |
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
icon or version resource yet, and `Cargo.toml` still says version `0.1.0` (`psoxide --version`
prints it). The repo's only release today is `source-2026.09.05` with no assets, and the web demo
disc sits in the SDK repo's `web-disc` release.

## 3. The launchpad

### 3.1 Concept

A small tool, not part of the emulator core, that does this for a port such as HL or HK:

1. Find the user's own install of the game (section 3.2).
2. Verify it is a version the cookers were tested on (3.3).
3. Fetch the port's source at a pinned tag (public GPL source, no game data) and make sure the
   needed Rust toolchain is present, asking before installing anything.
4. Run the port's builder locally with progress output (3.4, 3.5).
5. Put the resulting `.cue`/`.bin` in the user's game library and offer to launch it in PSoXide.

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
(`APP_ID` in hk-psx `tools/doctor.py`). The launchpad should be one Rust crate that implements
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

So HK cannot be a launchpad target until the pipeline is Rust-only (Manny's stated goal, memory
note `rust-only-codebase`), Windows-install discovery works without CrossOver, and the output
directory is a parameter. I will not put a number on the Python port; 281 files is the scale.

### 3.6 Architecture

- A `psoxide-launchpad` crate: library plus CLI, then a window. Port manifests are data files
  (id, display name, source repo and pinned commit, game app id, expected build ids, expected file
  hashes, builder command, output name). Adding a port is a manifest change.
- Progress: the builders already print step banners (`==> bootstrap PSoXide components`, ...).
  Wrap them with line-oriented capture first; ask builders for a `--json-progress` flag later.
- The build runs `cargo` on pinned remote source, which is executing downloaded code. The tool must
  show the repo and commit before running, pin by commit, and get explicit consent. It must not run
  anything from a manifest that was not shipped with the launchpad release.
- Toolchain provisioning: check `rustup`; if absent, say so and link rustup.rs rather than silently
  installing it. If present, the repo's `rust-toolchain.toml` downloads the nightly on first use;
  tell the user the size beforehand (I have not measured it).
- No telemetry, no network calls except Git fetches of the pinned port source, crates.io, and the
  rustup toolchain download.

### 3.7 Where the launchpad lives

| Option | For | Against |
|---|---|---|
| Inside the emulator | Players are the audience; one app to open | The emulator becomes a build orchestrator needing a Rust toolchain at runtime, and carries port-specific, legally sensitive code into the core product |
| Inside the editor | The editor already shells out to Cargo | Wrong audience: makers, not players; the editor has no Windows story yet |
| **Separate crate and binary** | Optional, removable, independently released and reviewed; legal text sits next to the code that needs it; works without either app | Third download unless bundled |

Recommendation: a separate `psoxide-launchpad` that the **emulator's Library menu opens** (a "Get
games" entry that starts the launchpad if installed, otherwise links to its page). The emulator
stays a player that never builds anything; the editor does not need to know about it.

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
| A. Shared app library | Move the window/UI shell (`app`, `gfx`, library, debugger UI, toolbar, input, audio, cli core) into a library crate in the emulator repo with a small hooks trait. `psoxide` is a thin bin. The editor repo depends on that library (vendored or pinned like the core crates) and adds `editor_*`, `embedded_playtest`, `playtest_disc` behind the hooks. | **Recommended.** One frontend, no fork, both products share fixes. Cost: untangling 124 feature gates into a trait, in the 4k-line `AppState`. The old web-build note called this "moderate surgery". |
| B. Editor spawns the emulator | Play launches the standalone `psoxide` process with the disc; the editor drops its embedded emulator. | Simplest split and smallest editor, but loses the in-editor Play viewport and quick iteration. Reasonable as a stopgap or a `--play-external` mode, not as the end state. |
| C. Keep the fork, add a drift check | CI diffs shared files. | Cheapest, keeps the problem. Not recommended. |

### 4.3 Recommended product shape

- **PSoXide (emulator)**: play discs, debugger, profiler, library, web build. Download: one app per
  OS (section 2). Contains no editor code, no `make`, no SDK examples scan, no repo-relative paths.
  This is what the one-liner and the release workflow in this plan produce.
- **PSoXide Editor**: makes games. Download: the editor app plus a source bundle of `engine/`, `sdk/`
  and the pinned tool sources, and a first-run check for rustup and the pinned nightly (E2, E5 show
  Play compiles a program per project). It embeds the same app library as the emulator through
  option A. Its Play and Export steps move from `make` + `sh` + `rsync` to a Rust `xtask`, so they
  run on Windows, and its root-path logic reads a data directory instead of `CARGO_MANIFEST_DIR`.
- **Launchpad**: separate, opened from the emulator (3.7).
- **What moves**: from the emulator repo, the SDK-examples scan, `make examples` action and
  `editor/projects` lookup in `app.rs` (these belong to the editor or are deleted); into the
  emulator repo, a library target for the shell. In the editor repo, delete its copy of the shared
  frontend files and depend on the emulator's app library. Sample projects (694 MB) move to release
  assets or a sparse-checkout path so the editor clone shrinks.

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

### 5.9 Questions for a lawyer before a public HL/HK launchpad

1. Does a tool that reads a user's lawfully installed game, converts it locally, and never
   transmits it, raise a different question from publishing a cooker? Does the answer differ when
   the vendor of the tool pins and fetches the cooker source for the user?
2. SSA 2.G (no reverse engineering or derivative works without consent, "unless applicable law
   permits") against a cooker that converts Valve maps and models into a new format on the user's
   machine. Does the user's local conversion count, and who is the actor, the user or the launchpad
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
6. GPL: a launchpad that runs GPL port code to produce a disc image that contains GPL-derived code
   plus converted proprietary data. Does the local output ever need to be treated as a combined
   work, and does it matter that the user never redistributes it?
7. Trademark and affiliation wording for a tool that names "Half-Life" and "Hollow Knight" in its UI
   and manifests. Is nominative use enough, and is more disclaimer text needed?
8. Platform exposure: the re3 precedent suggests rights holders act through GitHub takedowns. What
   is the exposure of the PSoXide repos, the releases and the itch pages, and does it differ if the
   launchpad lives in its own repository?
9. Whether the binary release process (SOURCE.txt, tag, vendored crates) meets GPL-2.0 section 3 for
   each artifact type, including the macOS app bundle and any future AppImage.
10. Sony: anything further needed beyond omitting BIOS and system-area material for generated discs
    and for the emulator's HLE kernel.
11. The "other account" and collaborator situation: hk-psx has an external writer and a history
    that was squashed. Is any attribution, contributor-licence or copyright-holder record missing?

## 6. READMEs

Drafts, not pushed, in `drafts/`: `README.emulator.md`, `README.editor.md`, `README.sdk.md`. Each
follows one structure: the agentic-coding callout copied unchanged from the current READMEs, one
line saying what it is, a screenshot, a Download table, a short "Build from source" block, a
feature list, links, then licence, provenance and trademark lines. They are written for the state
after the phases they depend on, and each draft has a header comment marking lines that are not true
yet:

- Emulator: the clone-and-run block is true on the PoC branch only; the Download table needs the
  first release.
- Editor: both the one-liner [P0 for the editor] and the Download table [P2] are not true yet, and it
  says plainly that Play compiles a program and needs rustup.
- SDK: no Download table, because the SDK is distributed as a pinned revision; the quick start
  assumes a Make-free `cargo xtask disc hello-tri` that does not exist yet.

Screenshots are placeholders: the emulator has `docs/images/compat-grid.png`, but a window capture
of the app with the debugger open is a better lead image. The itch and website links are
`bonnie-studios.itch.io/psoxide` and the GitHub Pages site as used in the current READMEs.

## 7. Phased plan

Efforts are my estimates in engineer-days of focused work, as ranges, assuming the repo owner
reviews. They exclude waiting on accounts and legal. Hardware smoke tests need someone with the
machines.

| Phase | Scope | Effort | Needs Manny |
|---|---|---|---|
| P0 One-line build, emulator | Review and merge the PoC; add CI jobs on `windows-latest` and `ubuntu-22.04` and an MSRV job; use the Rust `psoxide-components` for the vendor refresh and delete the Python bootstrap; update every downstream reference to `target/release/frontend` and `-p frontend` (hl-psx, hk-psx `--frontend`, demo-disc, `psoxide-debug` skill, SDK `FRONTEND=` examples); decide the version number. | 2 to 4 | OK to rename the package; one real Windows run and one Linux desktop run |
| P0b One-line build, SDK | `cargo xtask disc <example>` so the SDK needs no Make; hello-tri quick start. | 2 to 4 | none |
| P1 Release binaries, unsigned | Move the draft workflow in, dry-run on a test tag, fix what breaks; `xtask package` for the app bundle and icon; Windows icon resource; checksums; SOURCE.txt; website download page; smoke test checklist run on all three OSes. | 4 to 8 | Test machines or testers for Windows and Linux |
| P2 Signing | Apple enrolment, certificate, notarisation in CI; Windows signing service decision and setup. | 1 to 3 of work, plus enrolment lead time | Apple Developer account ($99/yr), certificate and app password; entity decision and spend for Windows |
| P3 Editor as a buildable product | Vendor the SDK subset into the editor, shrink the clone (sample projects out of the default checkout), port `build_guest_staged.sh`/`make` Play path to a Rust `xtask`, replace compile-time root paths with a data-dir resolver, first-run toolchain check, fix editor CI, bring pins current. Editor release workflow. | 15 to 30 | Which sample projects stay in the default clone; a Windows tester |
| P4 Product split | Option A in section 4.2: library target for the shared shell, hooks trait, delete the editor's frontend fork, remove SDK-example and `editor/projects` code from the player. | 15 to 30 | Approval of option A over B |
| P5 Launchpad MVP, HL only | `psoxide-launchpad` crate: install discovery on all OSes, `libraryfolders.vdf`, version check against tested build ids, pinned fetch, run `cargo hl-build`, progress, library install, launch; emulator Library entry. Not released publicly until P6 clears. | 10 to 20 | Tested HL build ids and file hashes from his installs; consent text |
| P5b Launchpad, HK | Blocked by the Python-to-Rust port of the HK pipeline, Windows-install discovery without CrossOver, and a parameterised output directory. | not estimated; 281 Python files is the measure | The Rust-only port is already his stated goal |
| P6 Legal gate | Merge `cleanroom/hl-2026-10-03` (rebase, regate), reconcile `legal.md` with `PROVENANCE.md`, answer 5.9 with a lawyer, then publish the launchpad. | engineering 5 to 15 for the HL clean-room finish; lawyer time unknown | A lawyer, the decision to ship, and for HK the Team Cherry terms |

Order I would take: P0, P1 unsigned, P2 in parallel with P3 and P4, P5 in parallel, P6 gating the
public launchpad only. P0 and P1 give a visible win (downloads for the emulator) without touching
the editor or anything legally sensitive.

## 8. Needs from Manny

1. Yes or no to renaming the emulator package to `psoxide` and to vendoring the SDK crates (P0).
2. Apple Developer enrolment and the signing secrets; the Windows signing route and entity (P2).
3. A Windows and a Linux machine or a tester for the smoke checklist (P0, P1).
4. Product split choice: option A, B or C (P4), and which editor sample projects ship in the default clone.
5. The Half-Life and Hollow Knight build ids and file hashes from his installs, and which game
   versions the cookers were validated on (P5).
6. A lawyer and the questions in 5.9; the Team Cherry terms for Hollow Knight (P6).
7. Whether the launchpad is a separate repository (recommended) and under which account.
8. Permission to push: the PoC branch and the docs branch are local only.

## Appendix A. Repositories and artefacts

- Work dir: `~/Library/Application Support/PSoXide-perf/work/dist-plan-2026-10-09`
- PoC clone and branch: `emulator/` on `poc/one-liner-build-2026-10-09` (base main c743674).
- Docs branch: `docs/distribution-plan-2026-10-09` in the same clone, off main: this file,
  `docs/distribution/release.yml.draft`, and the three README drafts.
- Fresh-clone proof: `fresh-emulator/` and logs `fresh-run.log`, `fresh-check.log`, `fresh-test.log`,
  `fresh-test2.log`, `fresh-x86.log`, `fresh-win.log`.
- Fetched sources: `src/` (READMEs, PROVENANCE, LICENSING, THIRD_PARTY_NOTICES, legal.md, hk
  doctor, hl-build main.rs).
- Editor clone `editor/` on `poc/editor-fresh-build-probe` (bootstrapped, nothing committed).

## Appendix B. Things I noticed outside the brief

- The editor's `README.md` still lists MIPS binutils as a prerequisite (E7).
- Editor main CI is red (E8).
- The standalone emulator frontend scans repo-relative SDK and editor paths (B7).
- `psoxide --version` reports 0.1.0 for all builds.
- `tools/build-web-player.py` (Python) builds the web player bundle, so the web path is not Rust-only yet.
