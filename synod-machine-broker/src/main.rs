//! A Windows `LocalSystem` service holds the privilege needed to start machines.

#[cfg(windows)]
mod scm;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod site;

#[cfg(windows)]
fn main() {
    scm::start();
}

#[cfg(not(windows))]
#[allow(
    clippy::disallowed_macros,
    reason = "[silent:broker-stub] a stub with no ral shell to print through"
)]
fn main() -> std::process::ExitCode {
    eprintln!(
        "synod machine broker: this service requires Windows; no machine broker runs on this platform."
    );
    std::process::ExitCode::FAILURE
}
