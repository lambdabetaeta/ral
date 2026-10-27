//! Compatibility is checked by `beforeBuildCommand`, outside ordinary compilation.
//! Absent media is left to the bundler; present media requires a readable manifest.

#![allow(
    clippy::disallowed_macros,
    reason = "[silent:build-tool] the packaging script reads this result"
)]

use std::path::PathBuf;
use std::process::ExitCode;

use ral_core::path::shown;
use ral_vm_image_build_tools::manifest;

fn main() -> ExitCode {
    let mut boot = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    boot.pop();
    let boot = boot.join("out").join("boot");
    let result = manifest::read(&boot).and_then(|text| {
        let Some(text) = text else {
            return Ok(());
        };
        let path = shown(boot.join("boot-manifest.txt"));
        manifest::check(
            &text,
            &path,
            manifest::BOOT_KEY,
            guest_abi::boot::CONTRACT,
            manifest::BOOT_CONSEQUENCE,
        )?;
        manifest::check(
            &text,
            &path,
            manifest::PROTO_KEY,
            ral_core::protocol::PROTOCOL_VERSION,
            manifest::PROTO_CONSEQUENCE,
        )
    });
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(refusal) => {
            eprintln!("synod cannot be packaged with this guest media: {refusal}");
            ExitCode::FAILURE
        }
    }
}
