//! The one line tauri asks of every host crate: read `tauri.conf.json`,
//! embed the static `ui/` directory, and generate the context the binary
//! links against.  Synod's frontend is hand-written HTML/CSS/JS with no
//! bundler, so there is nothing here to compile first — the directory is
//! the build product.
//!
//! And one line it does not ask for: the guest media staged into that bundle
//! must speak the same boot contract as the host being built around it.  See
//! [`boot_contract`].

use std::path::PathBuf;

fn main() {
    boot_contract();
    tauri_build::build();
}

/// Refuse to build a synod around guest media it has outgrown.
///
/// `tauri.conf.json`'s resource map stages `../vm-image/out/boot/` into the
/// bundle verbatim, and nothing in that copying knows what it is copying: an
/// installer built on a Friday will happily carry an initramfs from the
/// Saturday before, whose `ral-daemon` has never heard of a `ral.` key this
/// host now writes — and the guest, quite correctly, refuses the whole command
/// line.  What the person holding that installer sees is a minute of silence
/// and "the guest did not dial the control plane", which names everything
/// except the cause.
///
/// So the media's manifest records the contract it was built for and
/// [`ral_daemon::boot::check_media`] compares it here, where the cost of being
/// wrong is one failed build rather than a shipped installer.  The path is the
/// resource map's own, spelled relative to this crate, so the file checked is
/// the file bundled.
///
/// No media at all is *not* a failure: a `cargo check`, a test run, and every
/// developer who has not spent an hour of podman on an image must still be
/// able to compile the crate.  The bundle is where absent media becomes an
/// error, and Tauri raises it there already, naming the missing resource.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:boot-contract-build] Build-time read of the guest media's own boot-manifest.txt, to compare the contract it records against this host's. Build scaffolding, not turn-time model data I/O — raises no surface card."
)]
fn boot_contract() {
    // Walked up to the workspace and down again, component by component,
    // rather than joined as one `../vm-image/...` literal: this path is quoted
    // back to a person in the refusal, and what they can paste into a shell is
    // their own platform's separators with no `..` left in the middle.
    let crate_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("cargo sets this"));
    let manifest = crate_dir
        .parent()
        .expect("this crate sits inside the workspace")
        .join("vm-image")
        .join("out")
        .join("boot")
        .join("boot-manifest.txt");
    // Named whether or not it exists: a manifest that *appears* — the first
    // build after `just guest-boot` — has to re-run this, and so does one
    // rewritten in place by a later media build.
    println!("cargo::rerun-if-changed={}", manifest.display());

    let Ok(text) = std::fs::read_to_string(&manifest) else {
        return;
    };
    // A refusal, not a panic: cargo prints a build script's stderr under its
    // own `--- stderr` heading, and the sentence is the whole of what the
    // reader needs — a panic would bury it under a location and a backtrace
    // note that point into this file rather than at the stale image.
    if let Err(refusal) = ral_daemon::boot::check_media(&text, &manifest.display().to_string()) {
        eprintln!("synod cannot be packaged with this guest media: {refusal}");
        std::process::exit(1);
    }
}
