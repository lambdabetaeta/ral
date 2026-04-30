//! Core library for the ral shell.
//!
//! Provides the complete pipeline from source text to execution:
//! lexing, parsing, elaboration, type checking, and evaluation.
//! Ancillary modules handle ANSI output, diagnostics, path resolution,
//! sandboxing, signal handling, and platform compatibility.

#[macro_use]
pub mod debug;
pub mod ansi;
pub mod ast;
pub mod builtins;
pub mod classify;
pub(crate) mod capability;
pub mod compat;
pub mod diagnostic;
pub mod elaborator;
pub mod evaluator;
pub mod exit_hints;
pub(crate) mod group;
pub mod io;
pub mod ir;
pub mod lexer;
pub mod parser;
pub mod path;
pub(crate) mod prelude_manifest;
pub mod sandbox;
#[cfg(unix)]
pub(crate) mod serial;
pub mod signal;
pub mod source;
pub mod span;
pub mod ty;
pub mod typecheck;
pub mod types;
pub mod util;

pub use ast::Ast;
pub use diagnostic::SourceLoc;
pub use elaborator::elaborate;
pub use evaluator::{call_value_pub, eval_comp, evaluate};
pub use ir::{Comp, Val};
pub use parser::{ParseError, parse};
pub use typecheck::{Scheme, TypeError, bake_prelude_schemes, typecheck};
pub use types::{AliasEntry, Shell, Error, EvalSignal, Value};

/// The two ahead-of-time phases — parse and elaborate — that every entry
/// point (script, `-c`, REPL line, rc file, plugin module) performs before
/// typecheck and eval.  Bundled so each call site says "compile this
/// source" rather than re-spelling the ladder.
pub fn compile(source: &str) -> Result<Comp, ParseError> {
    parse(source).map(|ast| elaborate(&ast, Default::default()))
}
