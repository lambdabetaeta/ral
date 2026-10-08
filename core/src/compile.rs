//! Source text to a checked [`Toplevel`]: parse, elaborate, typecheck.

use crate::diagnostic::Rejection;
use crate::elaborator::elaborate;
use crate::ir::{Name, Toplevel};
use crate::source::{FileId, Source};
use crate::syntax::parser::{ParseError, parse_with};
use crate::typecheck::{ReturnContract, SessionSchemes, TypeError, typecheck};
use crate::types::Error;

/// Why [`compile_and_typecheck`] produced no toplevel, kept structured until
/// it is rejected.
#[derive(Debug, Clone)]
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
                let lines: Vec<String> = errors.iter().map(|e| e.kind.render_message()).collect();
                f.write_str(&lines.join("\n"))
            }
        }
    }
}

impl CompileError {
    /// The failure drawable against `source`, with the status a run that
    /// failed so exits on: 2 for a parse failure, 1 for a type failure.
    pub fn reject(self, source: Source) -> Rejection {
        let (reports, status) = match self {
            Self::Parse(e) => (vec![e.report(&source)], 2),
            Self::Types(errs) => (errs.iter().map(TypeError::report).collect(), 1),
        };
        Rejection {
            source,
            reports,
            status,
        }
    }

    /// A loaded file's failure: the plain message for a handler to read, the
    /// whole rejection for a renderer to draw.
    pub fn into_error(self, source: Source) -> Error {
        let message = self.to_string();
        Error {
            rejection: Some(Box::new(self.reject(source))),
            ..Error::new(message)
        }
    }
}

/// The front end's stack. A term's depth sizes its walks and its syntax does
/// not bound it; address space, committed only as touched.
const COMPILE_STACK: usize = 64 << 20;

/// Run `f` on a thread with [`COMPILE_STACK`].  Every recursive walk of a
/// term, parse to typecheck and the printers, runs through here.
///
/// # Panics
/// If the thread cannot be spawned; a panic in `f` resumes here.
pub fn on_compile_stack<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|s| {
        std::thread::Builder::new()
            .name("compile".into())
            .stack_size(COMPILE_STACK)
            .spawn_scoped(s, f)
            .expect("spawn the compile thread")
            .join()
            .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
    })
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
    file: FileId,
    name: &str,
    contract: Option<ReturnContract>,
) -> Result<Toplevel, CompileError> {
    on_compile_stack(|| {
        let ast = parse_with(source, file).map_err(CompileError::Parse)?;
        let comp = elaborate(
            &ast,
            schemes.bindings.iter().map(|(n, _)| Name::from(n.as_str())),
            name,
        )
        .map_err(CompileError::Parse)?;
        typecheck(&comp, schemes, contract).map_err(CompileError::Types)
    })
}
