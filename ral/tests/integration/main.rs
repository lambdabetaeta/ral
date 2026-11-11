//! ral's integration tests: what a program does (`corpus`), what the `ral`
//! process does (`cli`), and what the OS enforces of a grant (`assurance`).

#![allow(clippy::disallowed_methods, reason = "test code")]
#![allow(
    clippy::disallowed_macros,
    reason = "[test] libtest captures print macros"
)]

mod assurance;
mod cli;
mod common;
mod corpus;

/// A re-exec of this binary in a role never reaches libtest.
#[ctor::ctor(unsafe)]
fn before_main() {
    if let Some(code) = ral_core::invocation::serve_process_before_main(&[]) {
        std::process::exit(i32::from(code));
    }
}
