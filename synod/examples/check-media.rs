//! Refuse to package a synod around guest media whose boot contract or engine
//! protocol has drifted from this host's.
//!
//! Packaging's business, so it runs from `beforeBuildCommand` rather than
//! `build.rs`, where a stale image would be a compile error for the whole
//! workspace.  Absent media is not a failure: when none of the boot media
//! exists, the bundler names the missing resource itself.  Media *without* its
//! manifest is one — a kernel or initramfs from an older pipeline, which
//! nothing here could check — so it is refused rather than packaged unchecked.

#![allow(
    clippy::disallowed_methods,
    reason = "[silent:boot-contract-build] Packaging scaffolding reading the media's own manifest, not turn-time model data I/O."
)]

use std::path::PathBuf;
use std::process::ExitCode;

fn main() -> ExitCode {
    // Component-wise, not one `../vm-image/...` literal: the refusal quotes
    // this path back at a person to paste into a shell.
    let boot = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("this crate sits inside the workspace")
        .join("vm-image")
        .join("out")
        .join("boot");
    let manifest = boot.join("boot-manifest.txt");
    let path = manifest.display().to_string();

    let Ok(text) = std::fs::read_to_string(&manifest) else {
        if ["kernel", "initramfs.img"]
            .iter()
            .any(|media| boot.join(media).exists())
        {
            eprintln!(
                "synod cannot be packaged with this guest media: the boot media in {} has no \
                 manifest at {path}, so nothing can check it matches this synod. Rebuild the boot \
                 media from this checkout — `just guest-boot-wsl` or `just guest-boot amd64` \
                 for the Windows guest, `just guest-boot` for the Mac's — and package again.",
                boot.display()
            );
            return ExitCode::FAILURE;
        }
        return ExitCode::SUCCESS;
    };
    for check in [
        ral_daemon::boot::check_media,
        ral_core::protocol::check_media,
    ] {
        if let Err(refusal) = check(&text, &path) {
            eprintln!("synod cannot be packaged with this guest media: {refusal}");
            return ExitCode::FAILURE;
        }
    }
    ExitCode::SUCCESS
}
