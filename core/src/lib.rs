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
pub(crate) mod child_eval;
pub mod diagnostic;
pub mod elaborator;
pub mod evaluator;
pub mod exit_hints;
pub mod host;
pub mod io;
pub mod ir;
pub mod mode;
pub mod path;
/// Names exported by `prelude.ral`, harvested from its top-level `let`
/// bindings by `build.rs`.
pub(crate) mod prelude_manifest {
    include!(concat!(env!("OUT_DIR"), "/prelude_manifest.rs"));
}
#[cfg(unix)]
pub mod engine;
pub mod process;
pub mod run;
pub(crate) mod runtime;
pub mod sandbox;
pub mod serial;
pub mod source;
pub mod stream;
pub(crate) mod subprocess;
pub(crate) mod subprocess_codec;
pub mod syntax;
#[cfg(test)]
pub(crate) mod test_env;
pub mod test_helper;
pub mod text;
pub mod transport;
pub mod typecheck;
pub mod types;
// Public because `WireTransport::adopt` takes a `WireStream` in its own
// signature: a front-end handing over a booted guest's control plane has to
// be able to name what it is handing over.
pub mod wire;

// The host surface. A host imports a run from here; it does not reach the
// evaluator or syntax layers through the crate root.
pub use boot::HostSurface;
pub use run::{
    Captured, RequestedTerminalAccess, RunIo, RunLifecycle, RunReport, RunRequest, RunStdin,
    StaticDiagnostics,
};
pub use runtime::pipeline::helper::{try_run_bundled_tool, try_run_pipeline_stage_helper};
pub use typecheck::{Scheme, SessionSchemes, TypeError, bake_prelude, typecheck};
pub use types::{
    Break, DefaultPolicy, Error, Escape, EventSink, HookName, HookSig, Map, RegisterError, Settled,
    Shell, SurfaceSink, Value,
};

// Compile-pipeline internals, deliberately not re-exported: a host wanting
// raw parse / elaborate / evaluate names the owning module, which reads as
// stepping past the run-door seam rather than as part of it.
pub(crate) use elaborator::elaborate;
pub(crate) use evaluator::evaluate;
pub(crate) use ir::Comp;
pub(crate) use syntax::parser::{ParseError, parse, parse_with};

/// Parse and elaborate: the two ahead-of-time phases every entry point runs
/// before typecheck and eval.
///
/// # Errors
/// Parse failure, or any `$SCRIPT` in `source` — this caller passes no name,
/// so there is no script identity to bake the reference against.
pub fn compile(source: &str) -> Result<Comp, ParseError> {
    parse(source).and_then(|ast| elaborate(&ast, std::collections::HashSet::default(), ""))
}

/// Outcome of [`compile_and_typecheck`], carrying the errors structured so
/// the rendering choice stays at the call site.
pub enum CompileOutcome {
    /// The comp carries the checker's annotations.
    Compiled(Comp),
    Parse(ParseError),
    Types(Vec<TypeError>),
}

impl CompileOutcome {
    /// Collapse to the comp or one rendered message — the shape the
    /// `source` / `use` / plugin loaders want, reporting a failed load as a
    /// single fatal error rather than per-error ariadne output.
    ///
    /// # Errors
    /// The rendered parse error, or the newline-joined type errors.
    pub fn into_comp_or_message(self) -> Result<Comp, String> {
        match self {
            Self::Compiled(comp) => Ok(comp),
            Self::Parse(e) => Err(e.to_string()),
            Self::Types(errors) => Err(errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")),
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
pub fn compile_and_typecheck(
    source: &str,
    schemes: SessionSchemes,
    file: source::FileId,
    name: &str,
) -> CompileOutcome {
    let ast = match parse_with(source, file) {
        Ok(a) => a,
        Err(e) => return CompileOutcome::Parse(e),
    };
    let comp = match elaborate(
        &ast,
        schemes.bindings.iter().map(|(n, _)| n.clone()).collect(),
        name,
    ) {
        Ok(comp) => comp,
        Err(e) => return CompileOutcome::Parse(e),
    };
    match typecheck(&comp, schemes) {
        Ok(annotated) => CompileOutcome::Compiled(annotated),
        Err(errs) => CompileOutcome::Types(errs),
    }
}

/// Pre-`main` dispatch for the lib's own unit-test binary: serve the shared
/// re-exec stages, so a test that re-execs `current_exe()` — here, *this*
/// binary — does not land the hidden tail flags in libtest's argv parser.
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_lib_test_binary() {
    if let Some(code) = test_helper::run_pre_main_reexec_stages() {
        std::process::exit(i32::from(code));
    }
}
