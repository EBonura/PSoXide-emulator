#!/usr/bin/env python3
"""Build the slim web player bundle that other sites embed with ?embed=1.

The output directory gets index.html, the JS glue and wasm, Trunk's
snippets/, the favicon, and a build record (psoxide-player-build.json:
emulator revision, tool versions, and a sha256 per file). No demo-disc
delivery and no bundled example programs: the host serves whatever it
embeds and names it with ?disc=. The page loads from a relative base, so the
bundle can be served from any path (for example /PSoXide/player/). See
docs/web-player.md for the embed protocol.

Toolchain, same on macOS and ubuntu-latest:
  - rustup. The nightly pinned in rust-toolchain.toml installs on first use;
    this script adds its wasm32-unknown-unknown target.
  - trunk, exactly TRUNK_VERSION below
    (cargo install trunk --version <TRUNK_VERSION> --locked, or a release binary).
  - python3 (3.9 or newer) and git.
  - Network on a cold run: make bootstrap builds the SDK's psoxide-components,
    which fetches the locked SDK
    sources from GitHub, and trunk downloads the wasm-bindgen CLI matching
    Cargo.lock and the wasm-opt pinned in Trunk.toml unless the ones on PATH
    already match.

Usage: python3 tools/build-web-player.py --out DIR   (DIR must be new or empty)
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
FRONTEND = ROOT / "emu/crates/frontend"
TRUNK_VERSION = "0.21.14"
# Same flags as the public itch.io build. RUSTFLAGS replaces the simd128 flag
# from .cargo/config.toml, so it is repeated here.
RUSTFLAGS = "-C target-feature=+simd128 -C link-arg=-zstack-size=16777216"
RECORD = "psoxide-player-build.json"
# Copied into Trunk's output for the full web page; the player does not use it.
NOT_SHIPPED = ("examples",)


def run(*cmd, cwd=ROOT, env=None):
    print("+", " ".join(str(c) for c in cmd), flush=True)
    subprocess.run([str(c) for c in cmd], cwd=cwd, env=env, check=True)


def output(*cmd, cwd=ROOT):
    return subprocess.check_output([str(c) for c in cmd], cwd=cwd, text=True).strip()


def sha256(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def locked_version(crate):
    lock = (ROOT / "Cargo.lock").read_text()
    m = re.search(r'\[\[package\]\]\nname = "%s"\nversion = "([^"]+)"' % re.escape(crate), lock)
    return m.group(1) if m else None


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--out", type=Path, required=True, help="output directory (new or empty)")
    args = parser.parse_args()
    out = args.out.resolve()
    if out.exists() and (not out.is_dir() or any(out.iterdir())):
        parser.error(f"{out} must be a new or empty directory")

    # trunk rejects NO_COLOR=1 ("invalid value for --no-color").
    env = dict(os.environ)
    env.pop("NO_COLOR", None)
    env["RUSTFLAGS"] = RUSTFLAGS

    if shutil.which("trunk") is None:
        sys.exit(f"trunk not found; install {TRUNK_VERSION}: cargo install trunk --version {TRUNK_VERSION} --locked")
    trunk = output("trunk", "--version")
    if trunk.split()[-1] != TRUNK_VERSION:
        sys.exit(f"found {trunk}, need trunk {TRUNK_VERSION}: cargo install trunk --version {TRUNK_VERSION} --locked")

    # Run from the repo root so rustup resolves the pinned toolchain.
    run("rustup", "target", "add", "wasm32-unknown-unknown")
    run("make", "bootstrap")
    run("trunk", "build", "--release", "--locked", "--public-url", "./", "--dist", out, cwd=FRONTEND, env=env)

    for name in NOT_SHIPPED:
        path = out / name
        if path.is_dir():
            shutil.rmtree(path)
    # Ship the project licence, bundled-font notices and dependency licence
    # texts alongside the binary. Source and locked build instructions are
    # linked by exact revision in both the manifest and the notices.
    revision = output("git", "rev-parse", "HEAD")
    metadata = json.loads(output("cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", "wasm32-unknown-unknown"))
    notices = ["PSoXide web player\nGPL-2.0-or-later\n",
               f"Source: https://github.com/EBonura/PSoXide-emulator/tree/{revision}\n",
               "Build: python3 tools/build-web-player.py --out player\n",
               "Locked SDK sources: see components.lock.json and `make bootstrap` in that source tree.\n",
               (ROOT / "LICENSE").read_text()]
    for pkg in sorted(metadata["packages"], key=lambda p: p["name"]):
        base = Path(pkg["manifest_path"]).parent
        if pkg.get("source") is None:
            continue
        notices.append(f"\n===== {pkg['name']} {pkg['version']} ({pkg.get('license') or 'see source'}) =====\n")
        notices.append(f"Source: {pkg.get('repository') or pkg.get('source')}\n")
        candidates = {p for p in base.iterdir() if p.is_file() and p.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT"))}
        if pkg.get("license_file"):
            candidates.add(base / pkg["license_file"])
        for path in sorted(candidates):
            notices.append(path.name + "\n" + path.read_text(errors="replace"))
    fonts = FRONTEND / "assets/fonts"
    for path in sorted(fonts.iterdir()):
        if path.is_file() and path.suffix != ".ttf":
            notices.append(f"\n===== Bundled font notice: {path.name} =====\n" + path.read_text(errors="replace"))
    (out / "THIRD-PARTY-NOTICES.txt").write_text("\n".join(notices))
    files = sorted(p for p in out.rglob("*") if p.is_file())
    names = [p.relative_to(out).as_posix() for p in files]
    if "index.html" not in names or not any(n.endswith("_bg.wasm") for n in names):
        sys.exit("trunk output is missing index.html or the wasm")
    stray = [n for n in names if n.lower().endswith((".exe", ".bin", ".cue", ".flac", ".gz")) or "manifest" in n]
    if stray:
        sys.exit(f"unexpected program or disc files in the player bundle: {stray}")

    wasm_opt = re.search(r'^wasm_opt\s*=\s*"([^"]+)"', (FRONTEND / "Trunk.toml").read_text(), re.M)
    dirty = output("git", "status", "--porcelain", "--untracked-files=no")
    record = {
        "schema": 1,
        "emulator_revision": revision,
        "source_url": f"https://github.com/EBonura/PSoXide-emulator/tree/{revision}",
        "emulator_dirty": bool(dirty),
        "components_lock_sha256": sha256(ROOT / "components.lock.json"),
        "rustc": output("rustc", "-V"),
        "trunk": trunk,
        "wasm_bindgen": locked_version("wasm-bindgen"),
        "wasm_opt": wasm_opt.group(1) if wasm_opt else None,
        "rustflags": RUSTFLAGS,
        "files": {n: {"bytes": p.stat().st_size, "sha256": sha256(p)} for n, p in zip(names, files)},
        "total_bytes": sum(p.stat().st_size for p in files),
    }
    (out / RECORD).write_text(json.dumps(record, indent=2) + "\n")
    if dirty:
        print("warning: the emulator checkout has uncommitted changes (recorded as emulator_dirty)", file=sys.stderr)
    print(f"Player bundle: {out} ({len(files)} files, {record['total_bytes']} bytes)")


if __name__ == "__main__":
    main()
