//! Core library for the ral shell: source text to execution — lexing,
//! parsing, elaboration, type checking, evaluation — plus the ancillary
//! machinery for ANSI output, diagnostics, path resolution, sandboxing,
//! signals, and platform shims.
//!
//! A host process (the `ral` REPL, `exarch`, a test binary) embeds the
//! language through [`boot`], which decodes the prelude the host's build
//! script baked and loads it into a fresh [`Shell`].

pub mod ansi;
pub mod boot;
pub mod builtins;
pub mod capability;
pub mod diagnostic;
pub mod elaborator;
pub(crate) mod engine_seed;
pub mod evaluator;
pub mod exit_hints;
pub mod host;
pub mod io;
pub mod ir;
pub mod path;
/// Names exported by `prelude.ral`, harvested from its top-level `let`
/// bindings by `build.rs`.
pub(crate) mod prelude_manifest {
    include!(concat!(env!("OUT_DIR"), "/prelude_manifest.rs"));
}
#[cfg(unix)]
pub mod engine;
#[cfg(unix)]
pub mod hatch;
pub mod process;
pub mod protocol;
pub mod run;
pub(crate) mod runtime;
pub mod sandbox;
pub mod serial;
pub mod source;
pub mod spawn_grant;
pub(crate) mod stream;
pub(crate) mod subprocess;
pub(crate) mod subprocess_codec;
pub mod sync;
pub mod syntax;
#[cfg(feature = "test-util")]
pub mod test_access;
#[cfg(test)]
pub(crate) mod test_env;
pub mod test_helper;
pub mod text;
pub mod typecheck;
pub mod types;
pub mod uutils;
// Public because `WireTransport::adopt` takes a `WireStream` in its own
// signature: a front-end handing over a booted guest's control plane has to
// be able to name what it is handing over.
pub mod wire;

// The host surface. A host imports a run from here; it does not reach the
// evaluator or syntax layers through the crate root.
pub use boot::HostSurface;
pub use run::{
    Captured, Ending, RequestedTerminalAccess, RunIo, RunLifecycle, RunReport, RunRequest,
    RunStdin, StaticDiagnostics,
};
pub use runtime::pipeline::helper::{try_run_bundled_tool, try_run_pipeline_anchor};
pub use spawn_grant::SpawnGrant;
pub use typecheck::{Scheme, SessionSchemes, TypeError, bake_prelude, typecheck};
pub use types::{
    Break, DefaultPolicy, Error, Escape, EventSink, HookName, HookSig, Map, RegisterError, Settled,
    Shell, SurfaceSink, Value,
};

// Compile-pipeline internals, deliberately not re-exported: a host wanting
// raw parse / elaborate / evaluate names the owning module, which reads as
// stepping past the run-door seam rather than as part of it.
pub(crate) use elaborator::elaborate;
pub(crate) use ir::Toplevel;
pub(crate) use syntax::parser::{ParseError, parse, parse_with};

/// Parse and elaborate: the two ahead-of-time phases every entry point runs
/// before typecheck and eval.
///
/// # Errors
/// Parse failure, or any `$SCRIPT` in `source` — this caller passes no name,
/// so there is no script identity to bake the reference against.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn compile(source: &str) -> Result<Toplevel, ParseError> {
    parse(source).and_then(|ast| elaborate(&ast, std::collections::HashSet::default(), ""))
}

/// Why [`compile_and_typecheck`] produced no toplevel, kept structured so
/// the rendering choice stays at the call site.
#[derive(Debug)]
pub enum CompileError {
    Parse(ParseError),
    Types(Vec<TypeError>),
}

/// One plain message: the parse error, or the type errors a line each.
impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "{e}"),
            Self::Types(errors) => {
                for (i, e) in errors.iter().enumerate() {
                    if i > 0 {
                        writeln!(f)?;
                    }
                    write!(f, "{e}")?;
                }
                Ok(())
            }
        }
    }
}

/// Parse, elaborate, and typecheck `source` against the live session.
///
/// `schemes` is one map off the live scope split two ways: the elaborator
/// takes the names, to tell free-variable references from command heads;
/// the checker takes the types, to seed inference. Non-REPL callers pass an
/// empty map.
///
/// `file` stamps every span, so pass the id `source` is registered under in
/// the session's `SourceDb` — otherwise the spans carry the `FileId::DUMMY`
/// placeholder and diagnostics render with no source context. `name` is that
/// same source's display name, which the elaborator bakes into every
/// `$SCRIPT` in the body: self-location is lexical, fixed at elaboration,
/// never read at eval time.
///
/// `contract` is the form's hold on the row `source`'s last phrase returns —
/// an rc file's top-level keys, a plugin manifest's fields.  The *inferred*
/// row is what is checked, so a key misspelled inside a spread is caught with
/// one written out; `None` for a program no form speaks about.
///
/// # Errors
/// The parse error, or every type error, as a [`CompileError`].
pub fn compile_and_typecheck(
    source: &str,
    schemes: SessionSchemes,
    file: source::FileId,
    name: &str,
    contract: Option<typecheck::ReturnContract>,
) -> Result<Toplevel, CompileError> {
    let ast = parse_with(source, file).map_err(CompileError::Parse)?;
    let comp = elaborate(
        &ast,
        schemes.bindings.iter().map(|(n, _)| n.clone()).collect(),
        name,
    )
    .map_err(CompileError::Parse)?;
    typecheck(&comp, schemes, contract).map_err(CompileError::Types)
}

/// Pre-`main` dispatch for the lib's own unit-test binary: serve the shared
/// re-exec stages, so a test that re-execs `current_exe()` — here, *this*
/// binary — does not land the hidden tail flags in libtest's argv parser.
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_lib_test_binary() {
    if let Some(code) = test_helper::run_pre_main_reexec_stages() {
        #[allow(
            clippy::disallowed_methods,
            reason = "a re-exec stage has finished and dropped whatever shell it booted; the detach-birth fixture's survivor is meant to outlive this exit, which is what it is testing"
        )]
        std::process::exit(i32::from(code));
    }
}
