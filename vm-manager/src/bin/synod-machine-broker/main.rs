//! The machine broker, as a program — on the one platform that needs one.
//!
//! A broker exists because Hyper-V is privileged: starting a machine through
//! the Host Compute System wants rights no ordinary desktop user has, so on
//! Windows a `LocalSystem` service holds them and hands out machines over a named
//! pipe.  macOS asks for no such thing — Virtualization.framework starts a
//! machine in the calling process, given the entitlement synod is signed with —
//! so there is nothing there to broker and [`vm_manager::broker`] is not even
//! compiled.
//!
//! The program is still built on both, because a Cargo binary target belongs to
//! the package rather than to a platform: there is no `[[bin]]` gate, and
//! `required-features` is not platform-sensitive, so a target declared at all is
//! declared everywhere.  Hence this file — the entry point, present on every
//! platform — and `service.rs`, the Windows service it stands in front of.

#[cfg(windows)]
mod service;

#[cfg(windows)]
fn main() {
    service::start();
}

/// Off Windows there is no service to run, and saying so beats a program that
/// starts and then brokers nothing.
#[cfg(not(windows))]
fn main() {
    eprintln!(
        "synod machine broker: a Windows service, and this is not Windows. Here synod starts its \
         own machine through Virtualization.framework, so no broker holds the privilege for it."
    );
    std::process::exit(1);
}
