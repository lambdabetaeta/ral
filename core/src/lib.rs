//! Core library for the ral shell.
//!
//! Provides the complete pipeline from source text to execution:
//! lexing, parsing, elaboration, type checking, and evaluation.
//! Ancillary modules handle ANSI output, diagnostics, path resolution,
//! sandboxing, signal handling, and platform compatibility.
//!
//! # Hosting a `Shell`
//!
//! Embedding the language in a host process — the interactive `ral`
//! REPL, `exarch`, a test binary — goes through [`boot`].  The prelude
//! is baked ahead of time into a [`postcard`] blob (the annotated IR plus
//! the typed [`typecheck::Scheme`] list) by the host's build script and
//! embedded at compile time; [`boot::boot_shell`] then constructs, seeds,
//! and loads it into a fresh [`Shell`].  The single encode site
//! ([`boot::bake_prelude_to_out_dir`]) and the single decode site
//! ([`boot::BakedPrelude`]) live next to one rerun-if-changed list, so
//! the schema-evolution hazard — postcard carries no schema, and a field
//! added to the IR or scheme vocabulary silently invalidates an old bake
//! — is contained in one file rather than spread across each host.

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
/// Build-generated set of names exported by `prelude.ral`.
///
/// The `build.rs` script scans `src/prelude.ral` for top-level `let`
/// bindings and emits a `PRELUDE_EXPORTS` constant array into
/// `$OUT_DIR/prelude_manifest.rs`.
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
// Unix-only: every consumer of its env-mutation helpers is a
// `#[cfg(unix)]` test (Windows path/env assertions live elsewhere).
#[cfg(all(test, unix))]
pub(crate) mod test_env;
pub mod test_helper;
pub mod text;
pub mod transport;
pub mod typecheck;
pub mod types;
#[cfg(unix)]
pub(crate) mod wire;

// The crate-root host surface: the run seam, the host-embedding re-exec
// helpers, the typed-compile API, and ordinary value / rendering / diagnostic
// types.  A host imports a run from here; it does not reach the evaluator or
// syntax layers through the crate root.
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

// Compile-pipeline internals: reachable inside the crate (and used by
// `compile` / `compile_and_typecheck` below), but no longer crate-root
// re-exports.  A host that needs raw parse / elaborate / evaluate reaches the
// owning module explicitly — `ral_core::syntax::parser::parse`,
// `ral_core::elaborator::elaborate`, `ral_core::evaluator::evaluate`,
// `ral_core::ir::Comp` — which reads as deliberately stepping past the
// run-door seam rather than as part of it.
pub(crate) use elaborator::elaborate;
pub(crate) use evaluator::evaluate;
pub(crate) use ir::Comp;
pub(crate) use syntax::parser::{ParseError, parse, parse_with};

/// The two ahead-of-time phases — parse and elaborate — that every entry
/// point (script, `-c`, REPL line, rc file, plugin module) performs before
/// typecheck and eval.
///
/// Bundled so each call site says "compile this
/// source" rather than re-spelling the ladder.
///
/// # Errors
/// Returns `Err` if `source` fails to parse; elaboration is infallible.
pub fn compile(source: &str) -> Result<Comp, ParseError> {
    parse(source).map(|ast| elaborate(&ast, std::collections::HashSet::default()))
}

/// Outcome of [`compile_and_typecheck`]: either a compiled program, a parse
/// error, or one or more type errors.
///
/// Callers render diagnostics in the
/// shape that suits their UI (compact REPL, ariadne-rendered text for
/// tool calls, etc.) — this enum carries the structured errors so the
/// rendering choice stays at the call site.
pub enum CompileOutcome {
    /// Source compiled and typechecked cleanly.  Carries the annotated
    /// comp — the checker's scheme verdict rides inside it.
    Compiled(Comp),
    /// Parse failed before elaboration started.
    Parse(ParseError),
    /// Typecheck flagged at least one error after elaboration.
    Types(Vec<TypeError>),
}

impl CompileOutcome {
    /// Collapse to the compiled comp or a single rendered error message —
    /// the shape the `source`/`use`/plugin loaders want, which report a
    /// failed load as one fatal error rather than per-error ariadne
    /// output, so the structured errors flatten to newline-joined text.
    ///
    /// # Errors
    /// Returns `Err` (a rendered message) if the outcome is a parse error, or
    /// the newline-joined type errors if it is a typecheck failure.
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
/// `schemes` is one name→scheme map read off the live scope: the
/// elaborator consumes the names (which names are free variable
/// references rather than command heads — the REPL's live shell env;
/// non-REPL callers pass an empty map), the checker consumes the schemes
/// (each session binding's type, seeding the run's inference).
///
/// This is the shared ahead-of-time pipeline used by every entry point
/// that takes source text and turns it into something the evaluator can
/// run.  Signal-clearing, location bookkeeping, and post-eval rendering
/// are caller concerns and stay at each call site.
///
/// `file` is the [`FileId`](source::FileId) every span in the compiled
/// program is stamped with. A production caller passes the id its source
/// will be (or already is) registered under in the session's `SourceDb`,
/// so the program's spans carry the run's real file identity rather than
/// the [`FileId::DUMMY`](source::FileId::DUMMY) placeholder [`parse`] falls
/// back to.
pub fn compile_and_typecheck(
    source: &str,
    schemes: SessionSchemes,
    file: source::FileId,
) -> CompileOutcome {
    let ast = match parse_with(source, file) {
        Ok(a) => a,
        Err(e) => return CompileOutcome::Parse(e),
    };
    let comp = elaborate(
        &ast,
        schemes.bindings.iter().map(|(n, _)| n.clone()).collect(),
    );
    match typecheck(&comp, schemes) {
        Ok(annotated) => CompileOutcome::Compiled(annotated),
        Err(errs) => CompileOutcome::Types(errs),
    }
}

/// Pre-`main` dispatch for the lib's own unit-test binary.
///
/// Serves the shared re-exec stages (see
/// [`test_helper::run_pre_main_reexec_stages`]) so a lib unit test that
/// spawns a pipeline stage or a per-command sandbox re-exec of
/// `current_exe()` — here, *this* test binary — does not land the hidden
/// tail flags in libtest's argv parser.
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_lib_test_binary() {
    if let Some(code) = test_helper::run_pre_main_reexec_stages() {
        std::process::exit(i32::from(code));
    }
}
