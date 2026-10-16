// SPDX-License-Identifier: GPL-2.0-or-later
//! Compatibility name for the emulator binary.
//!
//! The desktop emulator package and binary are called `psoxide-emulator`. Scripts
//! written before the rename call `target/release/frontend` by path; this tiny
//! binary keeps those paths working. It runs the `psoxide-emulator` binary that
//! sits next to it, with the same arguments, standard streams, environment and
//! exit status. On Unix it replaces itself with that process, so the process id
//! is unchanged too.
//!
//! New scripts should call `psoxide-emulator` directly. This file is the only
//! thing to delete once nothing refers to the old name.

#[cfg(not(target_arch = "wasm32"))]
fn main() -> std::process::ExitCode {
    use std::process::{Command, ExitCode};

    let exe = match std::env::current_exe() {
        Ok(path) => path,
        Err(e) => {
            eprintln!("frontend: cannot locate itself: {e}");
            return ExitCode::from(2);
        }
    };
    let target = exe.with_file_name(format!("psoxide-emulator{}", std::env::consts::EXE_SUFFIX));
    let mut command = Command::new(&target);
    command.args(std::env::args_os().skip(1));

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec` only returns on failure.
        let e = command.exec();
        eprintln!(
            "frontend: this name is kept for old scripts and runs {}, which could not be started ({e}). \
             Build it with `cargo build --release -p psoxide-emulator`.",
            target.display()
        );
        ExitCode::from(127)
    }
    #[cfg(not(unix))]
    {
        match command.status() {
            Ok(status) => status
                .code()
                .map_or(ExitCode::FAILURE, |code| ExitCode::from(code as u8)),
            Err(e) => {
                eprintln!(
                    "frontend: this name is kept for old scripts and runs {}, which could not be started ({e}). \
                     Build it with `cargo build --release -p psoxide-emulator`.",
                    target.display()
                );
                ExitCode::from(127)
            }
        }
    }
}

// The browser build has no process to start. The stub only keeps a
// `--target wasm32-unknown-unknown` build of the whole package compiling; Trunk
// itself builds `psoxide-emulator` alone (`data-bin` in index.html).
#[cfg(target_arch = "wasm32")]
fn main() {}
