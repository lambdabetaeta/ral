//! Core library for the ral shell: source text to execution — lexing,
//! parsing, elaboration, type checking, evaluation — plus the ancillary
//! machinery for ANSI output, diagnostics, path resolution, sandboxing,
//! signals, and platform shims.
//!
//! A host process (the `ral` REPL, `exarch`, a test binary) embeds the
//! language through [`boot`], which decodes the prelude the host's build
//! script baked and loads it into a fresh [`Shell`].
#![cfg_attr(
    test,
    allow(
        clippy::disallowed_macros,
        reason = "[test] libtest captures print macros"
    )
)]

pub mod ansi;
pub mod boot;
pub mod builtins;
pub mod capability;
pub mod carrier;
pub mod compile;
pub mod diagnostic;
pub mod elaborator;
pub mod engine;
pub mod evaluator;
pub mod fact;
pub mod first_order;
pub(crate) mod frame;
pub mod guard;
pub mod host;
pub mod invocation;
pub mod io;
pub mod ir;
pub mod load;
pub mod path;
/// Names exported by `prelude.ral`, harvested from its top-level `let`
/// bindings by `build.rs`.
pub(crate) mod prelude_manifest {
    include!(concat!(env!("OUT_DIR"), "/prelude_manifest.rs"));
}
pub mod process;
pub mod protocol;
mod role;
pub mod run;
pub(crate) mod runtime;
pub mod sandbox;
pub mod seed;
pub mod source;
pub mod sync;
pub mod syntax;
pub mod terminal;
#[cfg(feature = "test-util")]
pub mod test_access;
#[cfg(test)]
pub(crate) mod test_env;
#[cfg(any(test, feature = "test-util"))]
pub mod test_helper;
pub mod text;
pub mod ty;
pub mod typecheck;
pub mod types;
pub mod uutils;

// The host surface. A host imports a run from here; it does not reach the
// evaluator or syntax layers through the crate root.
pub use boot::HostSurface;
pub use guard::SpawnGrant;
pub use io::{Captured, RunIo, RunStdin};
pub use process::RequestedTerminalAccess;
pub use role::{Invocation, Role, classify};
pub use ty::Scheme;
pub use typecheck::{SessionSchemes, TypeError, bake_prelude, typecheck};
pub use types::{
    Break, DefaultPolicy, Error, Escape, EventSink, HookName, HookSig, Map, RegisterError, Settled,
    Shell, SurfaceSink, Value,
};

/// Pre-`main` dispatch for the lib's own unit-test binary, so a test that
/// re-execs `current_exe()` (here, *this* binary) does not land a role's flag
/// in libtest's argv parser.
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_lib_test_binary() {
    if let Some(code) = invocation::serve_process(&[]) {
        #[allow(
            clippy::disallowed_methods,
            reason = "a re-exec stage has finished and dropped whatever shell it booted; the detach-birth fixture's survivor is meant to outlive this exit, which is what it is testing"
        )]
        std::process::exit(i32::from(code));
    }
}
