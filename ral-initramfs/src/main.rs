//! The initramfs's `/init`, as a program.
//!
//! [`ral_initramfs::run`] does not return on success — it hands off to
//! `ral-daemon` — so everything reaching this function is a refusal, worth
//! one clear sentence on the console the host is reading.

use std::process::ExitCode;

fn main() -> ExitCode {
    match ral_initramfs::run() {
        Err(refusal) => {
            eprintln!("ral-initramfs: {refusal}");
            ExitCode::FAILURE
        }
    }
}
