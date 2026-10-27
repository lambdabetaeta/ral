//! Brokered boot uses the service's installed media and the caller's folder.
//! Usage: `broker-smoke [--read-only] <folder>`

#![allow(
    clippy::disallowed_macros,
    reason = "[silent:example] an example prints its result"
)]
#![allow(clippy::disallowed_methods, reason = "example program")]

#[cfg(windows)]
use ral_core::path::shown;
#[cfg(windows)]
use vm_manager::{Booted, Hypervisor, MachineSpec};

#[cfg(windows)]
fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let read_only = args.first().is_some_and(|first| first == "--read-only");
    if read_only {
        args.remove(0);
    }
    let Ok([folder]) = <[String; 1]>::try_from(args) else {
        eprintln!("usage: broker-smoke [--read-only] <folder>");
        std::process::exit(2);
    };

    let hypervisor = vm_manager::broker::client::Brokered;

    let grant = if read_only {
        "read-only"
    } else {
        "with changes allowed"
    };
    println!("asking the machine service for a machine over {folder}, {grant}...");
    let spec = MachineSpec {
        folder: folder.into(),
        read_only,
    };
    let Booted { machine, wires } = match hypervisor.boot(&spec) {
        Ok(booted) => booted,
        Err(err) => {
            eprintln!("boot failed: {}", err.worded(|path| shown(path)));
            std::process::exit(1);
        }
    };

    println!(
        "booted: the guest dialled the control plane and the net wire, and reached the granted \
         folder at /work"
    );
    drop(wires);

    match machine.shutdown() {
        Ok(()) => println!("stopped cleanly, and its session disk went with it"),
        Err(err) => {
            eprintln!("shutdown failed: {}", err.worded(|path| shown(path)));
            std::process::exit(1);
        }
    }
}

#[cfg(not(windows))]
fn main() -> std::process::ExitCode {
    eprintln!("broker-smoke requires Windows");
    std::process::ExitCode::FAILURE
}
