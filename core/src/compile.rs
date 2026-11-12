//! Source text to a checked [`Toplevel`]: parse, elaborate, typecheck.

use crate::diagnostic::{Label, Rejection, Report};
use crate::elaborator::{Loader, elaborate};
use crate::error::Error;
use crate::fact::{
    CompileFailure, CompilePhase, IoKind, IoLeaf, IoOperation, Reason, TypeErrorCount,
};
use crate::ir::{Module, Name, Toplevel};
use crate::source::{FileId, Source, SourceOrigin, Span};
use crate::syntax::parser::{ParseError, parse_with};
use crate::typecheck::{SessionSchemes, TypeError, Unit, typecheck};
use std::num::NonZeroU8;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Why [`compile_and_typecheck`] produced no toplevel, kept structured until
/// it is rejected.
#[derive(Debug, Clone)]
pub enum CompileError {
    Parse(ParseError),
    Types(Vec<TypeError>),
    Use { at: Span, refusal: UseRefusal },
}

/// Why a written module path could not be checked.
#[derive(Debug, Clone)]
pub enum UseRefusal {
    Read {
        path: PathBuf,
        error: Box<Error>,
    },
    Compile(Box<Rejection>),
    Cycle {
        chain: Vec<SourceOrigin>,
    },
    Depth {
        chain: Vec<SourceOrigin>,
        limit: usize,
    },
    Unavailable {
        reason: String,
    },
}

impl std::fmt::Display for UseRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let chain = |origins: &[SourceOrigin]| {
            origins
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(" → ")
        };
        match self {
            Self::Read { error, .. } => error.fmt(f),
            Self::Compile(rejection) => {
                write!(f, "module {} could not be checked", rejection.source.name())
            }
            Self::Cycle { chain: origins } => write!(f, "module load cycle: {}", chain(origins)),
            Self::Depth {
                chain: origins,
                limit,
            } => write!(f, "module load depth exceeds {limit}: {}", chain(origins)),
            Self::Unavailable { reason } => write!(f, "module loading is unavailable: {reason}"),
        }
    }
}

impl UseRefusal {
    fn report(&self, at: Span) -> Report {
        let (code, label, hints) = match self {
            Self::Read { error, .. } => {
                let mut hints = error
                    .hints()
                    .iter()
                    .map(ToString::to_string)
                    .chain(error.reason.advice())
                    .collect::<Vec<_>>();
                if let Reason::Io(io) = &error.reason
                    && let IoLeaf::Os(os) = io.cause.leaf.as_ref()
                    && os.raw.is_some()
                    && let Some(detail) = &os.detail
                    && !error.describe().contains(detail)
                {
                    hints.push(detail.clone());
                }
                let label = match &error.reason {
                    Reason::Denied(_) => "reading this module was denied",
                    Reason::Io(io)
                        if io.operation == IoOperation::Read
                            && matches!(io.cause.leaf.as_ref(), IoLeaf::Os(os)
                                if os.kind == IoKind::InvalidData) =>
                    {
                        "this module is not valid UTF-8"
                    }
                    _ => "this module could not be read",
                };
                ("U0001", label, hints)
            }
            Self::Compile(_) => ("U0002", "this module could not be checked", Vec::new()),
            Self::Cycle { .. } => ("U0003", "this import closes a module cycle", Vec::new()),
            Self::Depth { .. } => (
                "U0004",
                "this import exceeds the module depth limit",
                Vec::new(),
            ),
            Self::Unavailable { .. } => ("U0005", "module loading is unavailable here", Vec::new()),
        };
        Report {
            code: Some(code),
            message: self.to_string(),
            at: Some(Label {
                span: Some(at),
                text: label.into(),
            }),
            also: None,
            hint: (!hints.is_empty()).then(|| hints.join("\n")),
        }
    }
}

/// A loader for compilation contexts that cannot read modules.
pub(crate) fn no_modules(_: Span, _: &str, _: Option<&Path>) -> Result<Arc<Module>, UseRefusal> {
    Err(UseRefusal::Unavailable {
        reason: "this compilation context has no module loader".into(),
    })
}

/// One plain message: the parse error, or the type errors a line each.
impl std::fmt::Display for CompileError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Parse(e) => write!(f, "{e}"),
            Self::Use { refusal, .. } => write!(f, "{refusal}"),
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
                Self::Use { .. } => CompilePhase::Use,
                Self::Types(errors) => CompilePhase::Types(TypeErrorCount {
                    count: errors.len().into(),
                }),
            },
        }
    }

    /// The failure drawable against `source`, with the status a run that
    /// failed so exits on: 2 for a parse failure, 1 for a type failure.
    pub fn reject(self, source: Source) -> Rejection {
        let (reports, status, nested) = match self {
            Self::Parse(e) => (
                vec![e.report(&source)],
                const { NonZeroU8::new(2).unwrap() },
                None,
            ),
            Self::Types(errs) => (
                errs.iter().map(TypeError::report).collect(),
                NonZeroU8::MIN,
                None,
            ),
            Self::Use { at, refusal } => {
                let report = refusal.report(at);
                let nested = match refusal {
                    UseRefusal::Compile(rejection) => Some(rejection),
                    _ => None,
                };
                let status = nested
                    .as_ref()
                    .map_or(NonZeroU8::MIN, |rejection| rejection.status);
                (vec![report], status, nested)
            }
        };
        Rejection {
            source,
            reports,
            status,
            nested,
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
/// the checker takes the types, to seed inference. Closed units pass Σ's
/// schemes; session callers pass their live bindings.
///
/// `file` stamps every span, so pass the id `source` is registered under in
/// the session's `SourceDb` — otherwise the spans carry the `FileId::DUMMY`
/// placeholder and diagnostics render with no source context. `script` is the
/// file that same source was read from, `None` for a virtual source; the
/// elaborator bakes it, shown, into every `$SCRIPT` in the body:
/// self-location is lexical, fixed at elaboration, never read at eval time.
///
/// `unit` selects a script, host options table, or closed module. `loader`
/// checks written module paths at their `use` sites.
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
    unit: Unit,
    loader: Loader<'_>,
) -> Result<Toplevel, CompileError> {
    on_compile_stack(|| {
        let ast = parse_with(source, file).map_err(CompileError::Parse)?;
        let comp = elaborate(
            &ast,
            schemes.bindings.iter().map(|(n, _)| Name::from(n.as_str())),
            script,
            loader,
        )?;
        typecheck(&comp, schemes, unit).map_err(|mut errors| {
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
    fn unavailable_use_keeps_the_written_path_span() {
        let text = "use 'library.ral'";
        let error = compile_and_typecheck(
            text,
            core_schemes(),
            FileId::DUMMY,
            None,
            Unit::Script,
            &mut no_modules,
        )
        .unwrap_err();
        let CompileError::Use {
            at,
            refusal: UseRefusal::Unavailable { .. },
        } = &error
        else {
            panic!("expected an unavailable use: {error:?}");
        };
        assert!(text[at.start as usize..at.end as usize].contains("library.ral"));
        let rejection = error.reject(Source::from_text(SourceOrigin::named("importer"), text));
        assert!(rejection.nested.is_none());
        assert!(rejection.render().contains("no module loader"));
        assert_eq!(rejection.reports[0].code, Some("U0005"));
        assert!(
            rejection.reports[0]
                .at
                .as_ref()
                .unwrap()
                .text
                .contains("unavailable")
        );
    }

    #[test]
    fn use_rejections_retain_nested_source_and_status() {
        let nested_source = Source::from_text(SourceOrigin::named("library.ral"), ")");
        let nested = compile_and_typecheck(
            nested_source.as_str(),
            core_schemes(),
            FileId::DUMMY,
            None,
            Unit::Module,
            &mut no_modules,
        )
        .unwrap_err()
        .reject(nested_source.clone());
        let status = nested.status;
        let text = "use 'library.ral'";
        let error = compile_and_typecheck(
            text,
            core_schemes(),
            FileId::DUMMY,
            None,
            Unit::Script,
            &mut |_, _, _| Err(UseRefusal::Compile(Box::new(nested.clone()))),
        )
        .unwrap_err();
        let rejection = error.reject(Source::from_text(SourceOrigin::named("importer"), text));
        assert_eq!(rejection.status, status);
        assert_eq!(rejection.nested.as_ref().unwrap().source, nested_source);
    }

    #[test]
    fn use_reports_distinguish_refusals_and_keep_read_advice_and_os_details() {
        use crate::fact::{IoLeaf, IoOperation, IoSubject};

        let path = PathBuf::from("library.ral");
        let os = std::io::Error::from_raw_os_error(13);
        let error = Error::io(
            IoOperation::Open,
            IoSubject::Path(crate::text::quote_native(path.as_os_str())),
            &os,
        )
        .with_hint("first stored hint")
        .with_hint("second stored hint");
        let Reason::Io(io) = &error.reason else {
            panic!("structured I/O refusal")
        };
        let IoLeaf::Os(cause) = io.cause.leaf.as_ref() else {
            panic!("OS detail retained")
        };
        assert_eq!(cause.raw, os.raw_os_error());
        let refusal = UseRefusal::Read {
            path,
            error: Box::new(error),
        };
        let at = Span::new(FileId::DUMMY, 4, 17);
        let report = refusal.report(at);
        assert_eq!(report.code, Some("U0001"));
        assert_eq!(report.at.as_ref().unwrap().span, Some(at));
        assert!(report.at.as_ref().unwrap().text.contains("read"));
        assert!(!report.at.as_ref().unwrap().text.contains("checked"));
        assert!(
            report.message.contains(&os.to_string())
                || report
                    .hint
                    .as_ref()
                    .is_some_and(|hint| hint.contains(&os.to_string()))
        );
        assert!(!report.message.contains("module: could not"));
        for hint in ["first stored hint", "second stored hint"] {
            assert!(report.hint.as_ref().unwrap().contains(hint));
        }
        let nested = compile_and_typecheck(
            ")",
            core_schemes(),
            FileId::DUMMY,
            None,
            Unit::Module,
            &mut no_modules,
        )
        .unwrap_err()
        .reject(Source::from_text(SourceOrigin::named("nested"), ")"));
        for (refusal, code, label) in [
            (UseRefusal::Compile(Box::new(nested)), "U0002", "checked"),
            (UseRefusal::Cycle { chain: Vec::new() }, "U0003", "cycle"),
            (
                UseRefusal::Depth {
                    chain: Vec::new(),
                    limit: 100,
                },
                "U0004",
                "depth",
            ),
            (
                no_modules(at, "library.ral", None).unwrap_err(),
                "U0005",
                "unavailable",
            ),
        ] {
            let report = refusal.report(at);
            assert_eq!(report.code, Some(code));
            assert!(report.at.unwrap().text.contains(label));
        }
    }

    #[test]
    fn kind_only_read_errors_do_not_add_generic_advice() {
        let os = std::io::Error::from(std::io::ErrorKind::NotFound);
        let error = Error::io(IoOperation::Open, crate::fact::IoSubject::Unspecified, &os);
        let Reason::Io(io) = &error.reason else {
            panic!("structured I/O refusal")
        };
        let IoLeaf::Os(cause) = io.cause.leaf.as_ref() else {
            panic!("OS metadata retained")
        };
        assert_eq!(cause.raw, None);
        assert_eq!(cause.detail.as_deref(), Some(os.to_string().as_str()));
        let at = Span::point(FileId::DUMMY, 0);
        let refusal = |error| UseRefusal::Read {
            path: PathBuf::from("missing.ral"),
            error: Box::new(error),
        };
        assert!(refusal(error.clone()).report(at).hint.is_none());
        let report = refusal(error.with_hint("explicit read advice")).report(at);
        assert_eq!(report.hint.as_deref(), Some("explicit read advice"));
    }

    #[test]
    fn module_exports_instantiate_polymorphically_and_hide_private_names() {
        let top = compile_and_typecheck(
            "let id = { |x| return $x }; let _private = 1",
            core_schemes(),
            FileId::DUMMY,
            None,
            Unit::Module,
            &mut no_modules,
        )
        .unwrap();
        let module = std::sync::Arc::new(crate::ir::Module {
            top,
            origin: SourceOrigin::named("library.ral"),
        });
        compile_and_typecheck(
            "let m = use 'library.ral'; let n = $m[id] 1; let s = $m[id] 'a'",
            core_schemes(),
            FileId::DUMMY,
            None,
            Unit::Script,
            &mut |_, _, _| Ok(std::sync::Arc::clone(&module)),
        )
        .expect("each use of id gets its own instance");
        let error = compile_and_typecheck(
            "let m = use 'library.ral'; return $m[_private]",
            core_schemes(),
            FileId::DUMMY,
            None,
            Unit::Script,
            &mut |_, _, _| Ok(std::sync::Arc::clone(&module)),
        )
        .expect_err("private names are absent from the module's type");
        assert!(matches!(error, CompileError::Types(_)));
    }

    #[test]
    fn empty_host_sources_anchor_missing_options_at_eof() {
        for text in ["", " \t\n", "# café", "# comment\n \t"] {
            let source = Source::from_text(SourceOrigin::named("empty-rc"), text);
            let file = SourceDb::default().register(source.clone());
            let Err(CompileError::Types(errors)) = compile_and_typecheck(
                text,
                core_schemes(),
                file,
                None,
                Unit::Host(declared(Form::Rc)),
                &mut no_modules,
            ) else {
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
