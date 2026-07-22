//! The guest's init, as a program.
//!
//! [`ral_daemon::serve`] does not return while the machine it is running
//! still exists, so everything reaching this function is a refusal — and a
//! refusal is worth one clear sentence on the console the host is reading,
//! not a panic.

use std::process::ExitCode;

fn main() -> ExitCode {
    match ral_daemon::serve() {
        Err(refusal) => {
            eprintln!("ral-daemon: {refusal}");
            ExitCode::FAILURE
        }
    }
}
