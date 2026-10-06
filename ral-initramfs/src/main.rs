//! The initramfs's `/init`, as a program.
//!
//! [`ral_initramfs::run`] does not return on success — it hands off to
//! `ral-daemon` — so everything reaching this function is a refusal, worth
//! one clear sentence on the console the host is reading.

#![allow(
    clippy::disallowed_macros,
    reason = "[silent:guest-main] the initramfs binary has no shell to print through"
)]
use std::process::ExitCode;

fn main() -> ExitCode {
    match ral_initramfs::run() {
        Err(refusal) => {
            eprintln!("ral-initramfs: {refusal}");
            ExitCode::FAILURE
        }
    }
}
