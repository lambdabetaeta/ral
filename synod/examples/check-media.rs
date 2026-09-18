//! Refuse to package a synod around guest media whose boot contract or engine
//! protocol has drifted from this host's.
//!
//! Packaging's business, so it runs from `beforeBuildCommand` rather than
//! `build.rs`, where a stale image was a compile error for the whole
//! workspace.  Absent media is not a failure: the bundler names the missing
//! resource itself.

#![allow(
    clippy::disallowed_methods,
    reason = "[silent:boot-contract-build] Packaging scaffolding reading the media's own manifest, not turn-time model data I/O."
)]

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    // Component-wise, not one `../vm-image/...` literal: the refusal quotes
    // this path back at a person to paste into a shell.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate sits inside the workspace")
        .join("vm-image")
        .join("out")
        .join("boot")
        .join("boot-manifest.txt");

    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return ExitCode::SUCCESS;
    };
    let path = manifest.display().to_string();
    for check in [ral_daemon::boot::check_media, ral_core::protocol::check_media] {
        if let Err(refusal) = check(&text, &path) {
            eprintln!("synod cannot be packaged with this guest media: {refusal}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
