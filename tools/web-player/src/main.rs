//! Build the slim web player bundle that other sites embed with `?embed=1`.
//!
//! The output directory gets `index.html`, the JS glue and wasm, Trunk's
//! `snippets/`, the favicon, and a build record (`psoxide-player-build.json`:
//! emulator revision, tool versions, and a sha256 per file). No demo-disc
//! delivery and no bundled example programs: the host serves whatever it
//! embeds and names it with `?disc=`. The page loads from a relative base, so
//! the bundle can be served from any path (for example `/PSoXide/player/`).
//! See `docs/web-player.md` for the embed protocol.
//!
//! Toolchain, same on macOS and ubuntu-latest:
//!   - rustup. The nightly pinned in `rust-toolchain.toml` installs on first
//!     use; this tool adds its wasm32-unknown-unknown target.
//!   - trunk, exactly `TRUNK_VERSION` below
//!     (`cargo install trunk --version <TRUNK_VERSION> --locked`, or a release binary).
//!   - git and make.
//!   - Network on a cold run: `make bootstrap` builds the SDK's
//!     psoxide-components, which fetches the locked SDK sources from GitHub,
//!     and trunk downloads the wasm-bindgen CLI matching `Cargo.lock` and the
//!     wasm-opt pinned in `Trunk.toml` unless the ones on PATH already match.
//!
//! Usage: `cargo run --release --manifest-path tools/web-player/Cargo.toml -- --out DIR`
//! (DIR must be new or empty).

use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

const TRUNK_VERSION: &str = "0.21.14";
/// Same flags as the public itch.io build. `RUSTFLAGS` replaces the simd128
/// flag from `.cargo/config.toml`, so it is repeated here.
const RUSTFLAGS: &str = "-C target-feature=+simd128 -C link-arg=-zstack-size=16777216";
const RECORD: &str = "psoxide-player-build.json";
/// Copied into Trunk's output for the full web page; the player does not use it.
const NOT_SHIPPED: &[&str] = &["examples"];

type Result<T> = std::result::Result<T, String>;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("tools/web-player has a repo root two levels up")
        .to_path_buf()
}

/// Read a text file the way a text-mode reader does: lossy UTF-8 and
/// universal newlines.
fn read_text(path: &Path) -> Result<String> {
    let bytes = fs::read(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(String::from_utf8_lossy(&bytes)
        .replace("\r\n", "\n")
        .replace('\r', "\n"))
}

fn run(
    dir: &Path,
    envs: &[(&str, &str)],
    remove: &[&str],
    program: &str,
    args: &[&str],
) -> Result<()> {
    println!("+ {program} {}", args.join(" "));
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(dir);
    for key in remove {
        cmd.env_remove(key);
    }
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let status = cmd.status().map_err(|e| format!("spawn {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{program} {} failed with {status}", args.join(" ")))
    }
}

fn output(dir: &Path, program: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(program)
        .args(args)
        .current_dir(dir)
        .output()
        .map_err(|e| format!("spawn {program}: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "{program} {} failed with {}",
            args.join(" "),
            out.status
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn sha256(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| format!("read {}: {e}", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// Version of `crate_name` in `Cargo.lock`, if it is locked.
fn locked_version(root: &Path, crate_name: &str) -> Result<Option<String>> {
    let lock = read_text(&root.join("Cargo.lock"))?;
    let needle = format!("[[package]]\nname = \"{crate_name}\"\nversion = \"");
    Ok(lock.find(&needle).and_then(|at| {
        let rest = &lock[at + needle.len()..];
        rest.find('"').map(|end| rest[..end].to_string())
    }))
}

/// Every file under `dir`, as paths sorted component by component.
fn walk_files(dir: &Path, found: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))? {
        let path = entry.map_err(|e| e.to_string())?.path();
        if path.is_dir() {
            walk_files(&path, found)?;
        } else if path.is_file() {
            found.push(path);
        }
    }
    Ok(())
}

fn license_like(name: &str) -> bool {
    let upper = name.to_uppercase();
    ["LICENSE", "LICENCE", "COPYING", "NOTICE", "COPYRIGHT"]
        .iter()
        .any(|p| upper.starts_with(p))
}

fn wasm_opt_version(trunk_toml: &str) -> Option<String> {
    for line in trunk_toml.lines() {
        if let Some(rest) = line.strip_prefix("wasm_opt") {
            let rest = rest.trim_start();
            if let Some(rest) = rest.strip_prefix('=') {
                if let Some(rest) = rest.trim_start().strip_prefix('"') {
                    if let Some(end) = rest.find('"') {
                        if end > 0 {
                            return Some(rest[..end].to_string());
                        }
                    }
                }
            }
        }
    }
    None
}

/// Python-style JSON with two-space indent (no trailing newline).
fn to_pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).expect("serialising a json value")
}

fn build(out_arg: &Path) -> Result<()> {
    let root = root();
    let frontend = root.join("emu/crates/frontend");
    if out_arg.exists()
        && (!out_arg.is_dir()
            || fs::read_dir(out_arg)
                .map_err(|e| e.to_string())?
                .next()
                .is_some())
    {
        return Err(format!(
            "{} must be a new or empty directory",
            out_arg.display()
        ));
    }
    fs::create_dir_all(out_arg).map_err(|e| format!("create {}: {e}", out_arg.display()))?;
    let out = out_arg
        .canonicalize()
        .map_err(|e| format!("resolve {}: {e}", out_arg.display()))?;
    let out_str = out.to_str().ok_or("output path is not UTF-8")?;

    // trunk rejects NO_COLOR=1 ("invalid value for --no-color").
    let trunk_env = [("RUSTFLAGS", RUSTFLAGS)];
    let trunk = output(&root, "trunk", &["--version"]).map_err(|_| {
        format!("trunk not found; install {TRUNK_VERSION}: cargo install trunk --version {TRUNK_VERSION} --locked")
    })?;
    if trunk.split_whitespace().last() != Some(TRUNK_VERSION) {
        return Err(format!(
            "found {trunk}, need trunk {TRUNK_VERSION}: cargo install trunk --version {TRUNK_VERSION} --locked"
        ));
    }

    // Run from the repo root so rustup resolves the pinned toolchain.
    run(
        &root,
        &[],
        &[],
        "rustup",
        &["target", "add", "wasm32-unknown-unknown"],
    )?;
    run(&root, &[], &[], "make", &["bootstrap"])?;
    run(
        &frontend,
        &trunk_env,
        &["NO_COLOR"],
        "trunk",
        &[
            "build",
            "--release",
            "--locked",
            "--public-url",
            "./",
            "--dist",
            out_str,
        ],
    )?;

    for name in NOT_SHIPPED {
        let path = out.join(name);
        if path.is_dir() {
            fs::remove_dir_all(&path).map_err(|e| format!("remove {}: {e}", path.display()))?;
        }
    }

    // Ship the project licence, bundled-font notices and dependency licence
    // texts alongside the binary. Source and locked build instructions are
    // linked by exact revision in both the manifest and the notices.
    let revision = output(&root, "git", &["rev-parse", "HEAD"])?;
    let metadata: Value = serde_json::from_str(&output(
        &root,
        "cargo",
        &[
            "metadata",
            "--locked",
            "--format-version",
            "1",
            "--filter-platform",
            "wasm32-unknown-unknown",
        ],
    )?)
    .map_err(|e| format!("parse cargo metadata: {e}"))?;
    let mut notices = vec![
        "PSoXide web player\nGPL-2.0-or-later\n".to_string(),
        format!("Source: https://github.com/EBonura/PSoXide-emulator/tree/{revision}\n"),
        "Build: cargo run --release --manifest-path tools/web-player/Cargo.toml -- --out player\n"
            .to_string(),
        "Locked SDK sources: see components.lock.json and `make bootstrap` in that source tree.\n"
            .to_string(),
        read_text(&root.join("LICENSE"))?,
    ];
    let mut packages: Vec<&Value> = metadata["packages"]
        .as_array()
        .ok_or("metadata has no packages")?
        .iter()
        .collect();
    packages.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    for pkg in packages {
        let name = pkg["name"].as_str().unwrap_or_default();
        let version = pkg["version"].as_str().unwrap_or_default();
        let manifest = Path::new(pkg["manifest_path"].as_str().unwrap_or_default());
        let base = manifest.parent().ok_or("manifest has no parent")?;
        if pkg["source"].is_null() {
            continue;
        }
        let license = pkg["license"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("see source");
        notices.push(format!("\n===== {name} {version} ({license}) =====\n"));
        let source = pkg["repository"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| pkg["source"].as_str())
            .unwrap_or_default();
        notices.push(format!("Source: {source}\n"));
        let mut candidates = BTreeSet::new();
        for entry in fs::read_dir(base).map_err(|e| format!("read_dir {}: {e}", base.display()))? {
            let path = entry.map_err(|e| e.to_string())?.path();
            if path.is_file()
                && license_like(&path.file_name().unwrap_or_default().to_string_lossy())
            {
                candidates.insert(path);
            }
        }
        if let Some(file) = pkg["license_file"].as_str().filter(|s| !s.is_empty()) {
            candidates.insert(base.join(file));
        }
        for path in candidates {
            let file_name = path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned();
            notices.push(format!("{file_name}\n{}", read_text(&path)?));
        }
    }
    let fonts = frontend.join("assets/fonts");
    let mut font_files: Vec<PathBuf> = fs::read_dir(&fonts)
        .map_err(|e| format!("read_dir {}: {e}", fonts.display()))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_file() && p.extension().is_none_or(|ext| ext != "ttf"))
        .collect();
    font_files.sort();
    for path in font_files {
        let file_name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        notices.push(format!(
            "\n===== Bundled font notice: {file_name} =====\n{}",
            read_text(&path)?
        ));
    }
    fs::write(out.join("THIRD-PARTY-NOTICES.txt"), notices.join("\n"))
        .map_err(|e| e.to_string())?;

    let mut files = Vec::new();
    walk_files(&out, &mut files)?;
    files.sort();
    let names: Vec<String> = files
        .iter()
        .map(|p| {
            p.strip_prefix(&out)
                .expect("walked under out")
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/")
        })
        .collect();
    if !names.iter().any(|n| n == "index.html") || !names.iter().any(|n| n.ends_with("_bg.wasm")) {
        return Err("trunk output is missing index.html or the wasm".into());
    }
    let stray: Vec<&String> = names
        .iter()
        .filter(|n| {
            let lower = n.to_lowercase();
            [".exe", ".bin", ".cue", ".flac", ".gz"]
                .iter()
                .any(|s| lower.ends_with(s))
                || lower.contains("manifest")
        })
        .collect();
    if !stray.is_empty() {
        return Err(format!(
            "unexpected program or disc files in the player bundle: {stray:?}"
        ));
    }

    let wasm_opt = wasm_opt_version(&read_text(&frontend.join("Trunk.toml"))?);
    let dirty = output(
        &root,
        "git",
        &["status", "--porcelain", "--untracked-files=no"],
    )?;
    let mut file_map = Map::new();
    let mut total = 0u64;
    for (name, path) in names.iter().zip(&files) {
        let bytes = fs::metadata(path).map_err(|e| e.to_string())?.len();
        total += bytes;
        file_map.insert(
            name.clone(),
            json!({ "bytes": bytes, "sha256": sha256(path)? }),
        );
    }
    let record = json!({
        "schema": 1,
        "emulator_revision": revision,
        "source_url": format!("https://github.com/EBonura/PSoXide-emulator/tree/{revision}"),
        "emulator_dirty": !dirty.is_empty(),
        "components_lock_sha256": sha256(&root.join("components.lock.json"))?,
        "rustc": output(&root, "rustc", &["-V"])?,
        "trunk": trunk,
        "wasm_bindgen": locked_version(&root, "wasm-bindgen")?,
        "wasm_opt": wasm_opt,
        "rustflags": RUSTFLAGS,
        "files": file_map,
        "total_bytes": total,
    });
    fs::write(out.join(RECORD), to_pretty(&record) + "\n").map_err(|e| e.to_string())?;
    if !dirty.is_empty() {
        eprintln!(
            "warning: the emulator checkout has uncommitted changes (recorded as emulator_dirty)"
        );
    }
    println!(
        "Player bundle: {} ({} files, {total} bytes)",
        out.display(),
        files.len()
    );
    Ok(())
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut out = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--out" => out = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("Usage: web-player-bundle --out DIR   (DIR must be new or empty)");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("unexpected argument {other}");
                return ExitCode::from(2);
            }
        }
    }
    let Some(out) = out else {
        eprintln!("--out DIR is required");
        return ExitCode::from(2);
    };
    match build(&out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("{msg}");
            ExitCode::FAILURE
        }
    }
}
