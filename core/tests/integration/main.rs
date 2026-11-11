//! core's integration tests: what the engine hands a host (`api`), and what
//! the OS enforces of a grant (`assurance`).

#![allow(clippy::disallowed_methods, reason = "test code")]

mod api;
mod assurance;
mod common;

/// A re-exec of this binary, as a role or as an assurance fixture, never
/// reaches libtest.
#[ctor::ctor(unsafe)]
fn before_main() {
    // Fixtures key on their own variables; only the order around `serve` matters.
    assurance::before_serve();
    if let Some(code) = ral_core::invocation::serve_process_before_main(&[]) {
        std::process::exit(i32::from(code));
    }
    assurance::after_serve();
}
