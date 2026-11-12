//! Source text to a checked [`Toplevel`]: parse, elaborate, typecheck.

use crate::diagnostic::Rejection;
use crate::elaborator::elaborate;
use crate::fact::{CompileFailure, CompilePhase, TypeErrorCount};
use crate::ir::{Name, Toplevel};
use crate::source::{FileId, Source, Span};
use crate::syntax::parser::{ParseError, parse_with};
use crate::typecheck::{SessionSchemes, Table, TypeError, typecheck};
use std::num::NonZeroU8;
use std::path::Path;

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
    /// Portable summary for a runtime load; rich source diagnostics stay on the report.
    pub fn failure(&self, source: impl Into<String>) -> CompileFailure {
        CompileFailure {
            source: source.into(),
            phase: match self {
                Self::Parse(_) => CompilePhase::Parse,
                Self::Types(errors) => CompilePhase::Types(TypeErrorCount {
                    count: errors.len().into(),
                }),
            },
        }
    }

    /// The failure drawable against `source`, with the status a run that
    /// failed so exits on: 2 for a parse failure, 1 for a type failure.
    pub fn reject(self, source: Source) -> Rejection {
        let (reports, status) = match self {
            Self::Parse(e) => (
                vec![e.report(&source)],
                const { NonZeroU8::new(2).unwrap() },
            ),
            Self::Types(errs) => (errs.iter().map(TypeError::report).collect(), NonZeroU8::MIN),
        };
        Rejection {
            source,
            reports,
            status,
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
/// placeholder and diagnostics render with no source context. `script` is the
/// file that same source was read from, `None` for a virtual source; the
/// elaborator bakes it, shown, into every `$SCRIPT` in the body:
/// self-location is lexical, fixed at elaboration, never read at eval time.
///
/// `options` is the table whose options this file's tail writes out (SPEC
/// §15.3); `None` for a program with no host table.
///
/// # Errors
/// The parse error, or every type error, as a [`CompileError`].
///
/// # Panics
/// If the compile thread cannot be spawned or the parser's source-length invariant fails.
pub fn compile_and_typecheck(
    source: &str,
    schemes: SessionSchemes,
    file: FileId,
    script: Option<&Path>,
    options: Option<&'static Table>,
) -> Result<Toplevel, CompileError> {
    on_compile_stack(|| {
        let ast = parse_with(source, file).map_err(CompileError::Parse)?;
        let comp = elaborate(
            &ast,
            schemes.bindings.iter().map(|(n, _)| Name::from(n.as_str())),
            script,
        )
        .map_err(CompileError::Parse)?;
        typecheck(&comp, schemes, options).map_err(|mut errors| {
            if comp.is_empty() {
                let eof = Span::point(
                    file,
                    u32::try_from(source.len()).expect("source length checked"),
                );
                for error in &mut errors {
                    error.pos.get_or_insert(eof);
                }
            }
            CompileError::Types(errors)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{SourceDb, SourceOrigin};
    use crate::test_helper::core_schemes;
    use crate::typecheck::TypeErrorKind;
    use crate::typecheck::table::{Form, declared};

    #[test]
    fn empty_host_sources_anchor_missing_options_at_eof() {
        for text in ["", " \t\n", "# café", "# comment\n \t"] {
            let source = Source::from_text(SourceOrigin::named("empty-rc"), text);
            let file = SourceDb::default().register(source.clone());
            let Err(CompileError::Types(errors)) =
                compile_and_typecheck(text, core_schemes(), file, None, Some(declared(Form::Rc)))
            else {
                panic!("expected missing rc options for {text:?}");
            };
            let [error] = &errors[..] else {
                panic!("expected one error for {text:?}: {errors:?}");
            };
            assert!(matches!(
                &error.kind,
                TypeErrorKind::OptionsMissing { form: "rc", missing, .. } if !missing.is_empty()
            ));
            assert_eq!(
                error.pos,
                Some(Span::point(file, u32::try_from(text.len()).unwrap()))
            );
            let report = error.report();
            assert!(
                report
                    .message
                    .starts_with("`rc` is missing required keys: ")
            );
            let rendered = report.render(&source);
            assert!(rendered.contains("[T0081]"), "{rendered}");
            assert!(rendered.contains("empty-rc:"), "{rendered}");
            assert!(
                rendered.contains(report.hint.as_deref().unwrap()),
                "{rendered}"
            );
            if text.is_empty() {
                assert!(rendered.contains("empty-rc:1:1:"), "{rendered}");
            }
        }
    }
}
