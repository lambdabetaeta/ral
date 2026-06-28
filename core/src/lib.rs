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
//! REPL, `exarch`, a test binary — goes through [`driver`].  The prelude
//! is baked ahead of time into a [`postcard`] blob (the annotated IR plus
//! the typed [`typecheck::Scheme`] list) by the host's build script and
//! embedded at compile time; [`driver::boot_shell`] then constructs, seeds,
//! and loads it into a fresh [`Shell`].  The single encode site
//! ([`driver::bake_prelude_to_out_dir`]) and the single decode site
//! ([`driver::BakedPrelude`]) live next to one rerun-if-changed list, so
//! the schema-evolution hazard — postcard carries no schema, and a field
//! added to the IR or scheme vocabulary silently invalidates an old bake
//! — is contained in one file rather than spread across each host.

pub mod ansi;
pub mod builtins;
pub mod capability;
pub(crate) mod child_eval;
pub mod diagnostic;
pub mod driver;
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
pub mod process;
pub(crate) mod runtime;
pub mod sandbox;
pub(crate) mod serial;
pub mod source;
pub mod stream;
pub(crate) mod subprocess;
pub(crate) mod subprocess_codec;
pub mod transport;
pub mod syntax;
#[cfg(test)]
pub(crate) mod test_env;
pub mod test_helper;
pub mod text;
pub mod turn;
pub mod typecheck;
pub mod types;

// The crate-root host surface: the turn seam, the host-embedding re-exec
// helpers, the typed-compile API, and ordinary value / rendering / diagnostic
// types.  A host imports a turn from here; it does not reach the evaluator or
// syntax layers through the crate root.
pub use driver::{Captured, RequestedTerminalAccess, TurnIo, TurnReport, TurnRequest, TurnStdin};
pub use runtime::pipeline::helper::{try_run_bundled_tool, try_run_pipeline_stage_helper};
pub use turn::{StaticDiagnostics, TurnLifecycle};
pub use typecheck::{Scheme, SessionSchemes, TypeError, bake_prelude, typecheck};
pub use types::{
    Break, DefaultPolicy, Error, Escape, EventSink, HookSig, Map, HookName, RegisterError,
    Settled, Shell, SurfaceSink, Value,
};

// Compile-pipeline internals: reachable inside the crate (and used by
// `compile` / `compile_and_typecheck` below), but no longer crate-root
// re-exports.  A host that needs raw parse / elaborate / evaluate reaches the
// owning module explicitly — `ral_core::syntax::parser::parse`,
// `ral_core::elaborator::elaborate`, `ral_core::evaluator::evaluate`,
// `ral_core::ir::Comp` — which reads as deliberately stepping past the
// turn-door seam rather than as part of it.  See
// decisions/260618_after-turn-api-simplifications.
pub(crate) use elaborator::elaborate;
pub(crate) use evaluator::evaluate;
pub(crate) use ir::Comp;
pub(crate) use syntax::parser::{ParseError, parse};

/// The two ahead-of-time phases — parse and elaborate — that every entry
/// point (script, `-c`, REPL line, rc file, plugin module) performs before
/// typecheck and eval.  Bundled so each call site says "compile this
/// source" rather than re-spelling the ladder.
pub fn compile(source: &str) -> Result<Comp, ParseError> {
    parse(source).map(|ast| elaborate(&ast, Default::default()))
}

/// Outcome of [`compile_and_typecheck`]: either a compiled program, a parse
/// error, or one or more type errors.  Callers render diagnostics in the
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
    pub fn into_comp_or_message(self) -> Result<Comp, String> {
        match self {
            CompileOutcome::Compiled(comp) => Ok(comp),
            CompileOutcome::Parse(e) => Err(e.to_string()),
            CompileOutcome::Types(errors) => Err(errors
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
/// (each session binding's type, seeding the turn's inference).
///
/// This is the shared ahead-of-time pipeline used by every entry point
/// that takes source text and turns it into something the evaluator can
/// run.  Signal-clearing, location bookkeeping, and post-eval rendering
/// are caller concerns and stay at each call site.
pub fn compile_and_typecheck(source: &str, schemes: SessionSchemes) -> CompileOutcome {
    let ast = match parse(source) {
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
/// A lib unit test that spawns a per-command sandbox re-exec (see
/// `sandbox::launch`) launches `current_exe()` — which here is *this*
/// test binary — with the `--sandbox-projection … --ral-sandbox-exec` /
/// `--ral-bundled-tool` tails.  Without this constructor those tails
/// would reach libtest's argv parser ("unknown argument") and the child
/// would never enter the OS sandbox.  Mirrors `core/tests/common/mod.rs`'s
/// constructor for the integration-test binaries, but ported into the
/// crate so the *lib* test binary serves the same pre-`main` stages:
///
///   1. `try_run_pipeline_stage_helper` — pipeline / capture re-execs.
///   2. `serve_sandbox_early_init` — `--sandbox-projection` enters the OS
///      sandbox, then `serve_sandbox_exec` (`--ral-sandbox-exec`) or
///      `try_run_bundled_tool` (`--ral-bundled-tool`) runs the target
///      confined.  A normal (non-re-exec) test invocation yields `None`
///      from both and falls through to libtest unchanged.
#[cfg(test)]
#[ctor::ctor(unsafe)]
fn init_lib_test_binary() {
    #[cfg(unix)]
    builtins::uutils::init_signal_dispositions();
    if let Some(code) = try_run_pipeline_stage_helper().or_else(sandbox::serve_sandbox_early_init) {
        std::process::exit(code as i32);
    }
}
