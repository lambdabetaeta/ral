//! Check-time module linking and the host's session and closed-unit doors.

pub mod profile;

use crate::compile::{CompileError, UseRefusal, compile_and_typecheck};
use crate::error::Prov;
use crate::fact::{InvalidCause, InvalidFailure, IoOperation, IoSubject, Reason};
use crate::ir::{Module, Toplevel};
use crate::path::{Judged, LeafMode, Located, Reading, walk_lenient};
use crate::shell::evaluator::{Mode, Ran, run_phrases};
use crate::shell::runtime::{Break, Env, Error, Mooring, Settled, Shell, Value};
use crate::source::{FileId, Source, SourceFile, SourceOrigin, Span};
use crate::typecheck::{SessionSchemes, Table, Unit};
use std::path::Path;
use std::sync::Arc;

const MAX_SOURCE_DEPTH: usize = 100;

/// Check `source` against the session and run it there; its `let`s join the session.
///
/// # Errors
/// A compile refusal or the file's own failure.
pub fn extend_session(
    mooring: &Mooring,
    shell: &mut Shell,
    source: &str,
    origin: &SourceOrigin,
    options: Option<&'static Table>,
) -> Settled<Value> {
    let env = shell.env.clone();
    let top = check_source(source, origin, &env, shell, options)?;
    let ran = run_phrases(&top, env, Mode::Local, mooring, shell);
    shell.env = ran.env;
    ran.outcome
}

/// Check `source` against Σ and run it under Σ; its value is its tail.
///
/// # Errors
/// A compile refusal or the file's own failure.
pub fn evaluate_closed(
    mooring: &Mooring,
    shell: &mut Shell,
    source: &str,
    origin: &SourceOrigin,
    options: Option<&'static Table>,
) -> Settled<Value> {
    let top = check_source(source, origin, &Env::new(), shell, options)?;
    run_closed(&top, mooring, shell).outcome
}

fn run_closed(top: &Toplevel, mooring: &Mooring, shell: &mut Shell) -> Ran {
    run_phrases(top, Env::new(), Mode::Local, mooring, shell)
}

/// Run a linked module under Σ and return its public bindings as a record.
pub(crate) fn run_module(module: &Module, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
    let ran = run_closed(&module.top, mooring, shell);
    ran.outcome.map_err(|b| {
        b.map_error(|error| {
            if error.context().is_none() {
                error.with_context("use")
            } else {
                error
            }
        })
    })?;
    Ok(Value::map(
        crate::ir::public_exports(&module.top.phrases)
            .into_keys()
            .map(|name| {
                let value = ran
                    .env
                    .get(name)
                    .expect("a checked module's successful definition is bound")
                    .clone();
                (name.to_owned(), value)
            })
            .collect(),
    ))
}

/// Resolve, read, check against Σ and link the module `written` names,
/// below the files in `chain`.
pub(crate) fn load_module(
    at: Span,
    written: &str,
    script: Option<&Path>,
    chain: &[SourceOrigin],
    shell: &mut Shell,
) -> Result<Arc<Module>, UseRefusal> {
    let resolver = shell.context.resolver();
    let path = match script {
        Some(script) => resolver
            .resolve_in_script(written, script)
            .map_err(|error| UseRefusal::Read {
                path: written.into(),
                error: Box::new(Error::new(Reason::Invalid(Box::new(InvalidFailure::new(
                    "use".into(),
                    InvalidCause::Path((&error).into()),
                ))))),
            })?,
        None => resolver.resolve(written),
    };
    let previous = shell.local.audit.enter_site(Prov::Live {
        name: "use".into(),
        span: at,
    });
    let located = shell.locate::<Reading>(&path);
    shell.local.audit.restore_site(previous);
    let located = located.map_err(|failure| {
        let Break::Error(error) = failure else {
            unreachable!("path judgment neither evaluates nor polls cancellation")
        };
        UseRefusal::Read {
            path: path.as_path().to_path_buf(),
            error: Box::new(error),
        }
    })?;
    let file = SourceFile::new(located.real().as_path().to_path_buf()).map_err(|error| {
        UseRefusal::Read {
            path: path.as_path().to_path_buf(),
            error: Box::new(Error::new(Reason::Invalid(Box::new(InvalidFailure::new(
                "use".into(),
                InvalidCause::Path((&error).into()),
            ))))),
        }
    })?;
    let origin = SourceOrigin::File(file);
    let nested = chain
        .iter()
        .cloned()
        .chain([origin.clone()])
        .collect::<Vec<_>>();
    if chain.contains(&origin) {
        return Err(UseRefusal::Cycle { chain: nested });
    }
    if chain.len() >= MAX_SOURCE_DEPTH {
        return Err(UseRefusal::Depth {
            chain: nested,
            limit: MAX_SOURCE_DEPTH,
        });
    }
    let source = read_and_normalize(located, path.as_path())?;
    let text = Source::from_text(origin.clone(), &source);
    let file = shell.session.sources.register(text.clone());
    let schemes = shell.schemes_over(&Env::new());
    let top = compile_and_typecheck(
        &source,
        schemes,
        file,
        origin.file(),
        Unit::Module,
        &mut |at, written, script| load_module(at, written, script, &nested, shell),
    )
    .map_err(|error| UseRefusal::Compile(Box::new(error.reject(text))))?;
    Ok(Arc::new(Module { top, origin }))
}

fn read_and_normalize(
    located: Located<Judged<Reading>>,
    path: &Path,
) -> Result<String, UseRefusal> {
    let read = located.read();
    drop(located);
    let refusal = |operation, error: std::io::Error| UseRefusal::Read {
        path: path.to_path_buf(),
        error: Box::new(Error::io(
            operation,
            IoSubject::Path(crate::text::quote_native(path.as_os_str())),
            &error,
        )),
    };
    let reader = read.map_err(|error| refusal(IoOperation::Open, error))?;
    let source =
        std::io::read_to_string(reader).map_err(|error| refusal(IoOperation::Read, error))?;
    Ok(crate::source::normalize_source_text(source))
}

/// Check a registered root, preserving its written origin while using its
/// metadata-only real identity to detect module cycles. A vanished root is
/// still compilable: the caller already holds its source text.
pub(crate) fn compile_source(
    source: &str,
    origin: &SourceOrigin,
    file: FileId,
    schemes: SessionSchemes,
    unit: Unit,
    shell: &mut Shell,
) -> Result<Toplevel, CompileError> {
    let chain = origin
        .file()
        .map(|path| {
            let path = shell.context.resolver().anchor(path);
            walk_lenient(&path, LeafMode::Follow)
                .ok()
                .and_then(|trace| SourceFile::new(trace.real().as_path().to_path_buf()).ok())
                .map_or_else(|| origin.clone(), SourceOrigin::File)
        })
        .into_iter()
        .collect::<Vec<_>>();
    compile_and_typecheck(
        source,
        schemes,
        file,
        origin.file(),
        unit,
        &mut |at, written, script| load_module(at, written, script, &chain, shell),
    )
}

/// Register and check a host file against `env`, linking its written imports.
fn check_source(
    source: &str,
    origin: &SourceOrigin,
    env: &Env,
    shell: &mut Shell,
    options: Option<&'static Table>,
) -> Settled<Toplevel> {
    let text = Source::from_text(origin.clone(), source);
    let file = shell.session.sources.register(text.clone());
    let schemes = shell.schemes_over(env);
    compile_source(
        source,
        origin,
        file,
        schemes,
        options.map_or(Unit::Script, Unit::Host),
        shell,
    )
    .map_err(|error| {
        Error::new(Reason::Compile(Box::new(error.failure(origin.to_string()))))
            .with_rejection(error.reject(text))
            .into()
    })
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::fact::{CompilePhase, FailureCode as _};
    use crate::shell::runtime::Break;

    #[test]
    fn check_source_keeps_compile_summary_separate_from_rich_rejection() {
        for (source, parse) in [("let = ", true), ("$[1 + true]", false)] {
            let mut shell = crate::test_helper::core_shell();
            let origin = SourceOrigin::named("<load-summary>");
            let Err(Break::Error(error)) =
                check_source(source, &origin, &Env::new(), &mut shell, None)
            else {
                panic!("source must fail compilation");
            };
            let Reason::Compile(summary) = &error.reason else {
                panic!("compile summary")
            };
            assert_eq!(summary.source, "<load-summary>");
            let rejection = error.rejection().expect("rich report");
            assert_eq!(rejection.source.origin(), &origin);
            assert_eq!(rejection.source.as_str(), source);
            match &summary.phase {
                CompilePhase::Parse => assert!(parse),
                CompilePhase::Types(errors) => {
                    assert!(!parse);
                    assert_eq!(errors.count, rejection.reports.len().into());
                }
                CompilePhase::Use => panic!("fixture has no module load"),
            }
            assert_eq!(error.failure_code().get(), 1);
            assert!(
                rejection
                    .render()
                    .contains(if parse { "P0001" } else { "T0010" })
            );
            assert!(
                !error
                    .describe()
                    .contains(if parse { "P0001" } else { "T0010" })
            );
        }
    }

    fn import_site(shell: &mut Shell) -> Span {
        let file = shell.session.sources.register(Source::from_text(
            SourceOrigin::named("<importer>"),
            "use 'module.ral'",
        ));
        Span::new(file, 4, 16)
    }

    #[test]
    fn module_linking_normalizes_registers_and_never_rereads_the_source() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("module.ral");
        std::fs::write(&path, "\u{feff}let exported = 7\r\nlet _hidden = 8\r\n").unwrap();
        let mut shell = crate::test_helper::core_shell();
        let at = import_site(&mut shell);
        let previous = shell.local.audit.call_site().clone();
        let module = load_module(
            at,
            "module.ral",
            Some(&root.join("importer.ral")),
            &[],
            &mut shell,
        )
        .expect("link module");
        assert_eq!(shell.local.audit.call_site(), &previous);
        assert_eq!(module.origin.file(), Some(path.as_path()));
        let source = shell
            .session
            .sources
            .get(module.top.phrases[0].span.unwrap().file)
            .expect("module source registered");
        assert_eq!(source.as_str(), "let exported = 7\nlet _hidden = 8\n");
        std::fs::write(&path, "fail 'must not read this source'").unwrap();
        let value = run_module(&module, &Mooring::adrift(), &mut shell).expect("linked module");
        assert_eq!(value, Value::map(vec![("exported".into(), Value::Int(7))]));
        assert!(shell.env.get("exported").is_none());
    }

    #[test]
    fn module_read_refuses_invalid_utf8() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("module.ral");
        std::fs::write(&path, [0xff]).unwrap();
        let mut shell = crate::test_helper::core_shell();
        let at = import_site(&mut shell);
        let refusal = load_module(at, path.to_str().unwrap(), None, &[], &mut shell)
            .expect_err("module text must be UTF-8");
        assert!(matches!(&refusal, UseRefusal::Read { path: refused, error }
            if refused == &path && matches!(&error.reason, Reason::Io(io)
                if io.operation == IoOperation::Read
                    && matches!(io.cause.leaf.as_ref(), crate::fact::IoLeaf::Os(os)
                        if os.kind == crate::fact::IoKind::InvalidData))));
        let rejection = CompileError::Use { at, refusal }.reject(Source::from_text(
            SourceOrigin::named("<importer>"),
            "use 'module.ral'",
        ));
        let report = &rejection.reports[0];
        assert!(report.at.as_ref().unwrap().text.contains("UTF-8"));
        assert!(report.message.contains("UTF-8"));
    }

    #[test]
    fn missing_and_denied_modules_keep_structured_reasons_and_advice() {
        use crate::authority::policy::{Capabilities, FsPolicy};
        use crate::fact::{IoKind, IoLeaf};
        use crate::path::FrozenPath;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.ral");
        let mut shell = crate::test_helper::core_shell();
        let at = import_site(&mut shell);
        let missing = load_module(at, path.to_str().unwrap(), None, &[], &mut shell).unwrap_err();
        let UseRefusal::Read { error, .. } = &missing else {
            panic!("missing module is a read refusal")
        };
        assert!(matches!(&error.reason, Reason::Io(io)
            if matches!(io.cause.leaf.as_ref(), IoLeaf::Os(os) if os.kind == IoKind::NotFound)));
        shell.push_session_capabilities(crate::test_helper::layer(Capabilities {
            fs: Some(FsPolicy {
                deny_paths: vec![FrozenPath::freeze(&path).unwrap()],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        }));
        let denied = load_module(at, path.to_str().unwrap(), None, &[], &mut shell).unwrap_err();
        let UseRefusal::Read { error, .. } = &denied else {
            panic!("denied module is a read refusal")
        };
        assert!(matches!(error.reason, Reason::Denied(_)));
        let hints = error.reason.advice();
        let source = Source::from_text(SourceOrigin::named("<importer>"), "use 'module.ral'");
        let rejection = CompileError::Use {
            at,
            refusal: denied,
        }
        .reject(source);
        let report = &rejection.reports[0];
        assert_eq!(report.code, Some("U0001"));
        assert!(report.at.as_ref().unwrap().text.contains("denied"));
        for hint in hints {
            assert!(
                report
                    .hint
                    .as_ref()
                    .is_some_and(|help| help.contains(&hint))
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_directory_module_keeps_the_operating_system_read_failure() {
        use crate::fact::{IoKind, IoLeaf};

        let dir = tempfile::tempdir().unwrap();
        let mut shell = crate::test_helper::core_shell();
        let at = import_site(&mut shell);
        let refusal =
            load_module(at, dir.path().to_str().unwrap(), None, &[], &mut shell).unwrap_err();
        let UseRefusal::Read { error, .. } = &refusal else {
            panic!("a directory cannot be module source")
        };
        let Reason::Io(io) = &error.reason else {
            panic!("structured I/O failure")
        };
        let IoLeaf::Os(os) = io.cause.leaf.as_ref() else {
            panic!("operating system failure")
        };
        assert_eq!(os.kind, IoKind::IsADirectory);
        let detail = os.detail.clone().expect("original read detail");
        let hints = error.hints().to_vec();
        let rejection = CompileError::Use { at, refusal }.reject(Source::from_text(
            SourceOrigin::named("<importer>"),
            "use 'module.ral'",
        ));
        let report = &rejection.reports[0];
        assert!(report.message.contains(&detail));
        for hint in hints {
            assert!(
                report
                    .hint
                    .as_ref()
                    .is_some_and(|help| help.contains(&*hint))
            );
        }
        assert!(!report.message.contains("module: could not"));
    }

    #[test]
    fn root_cycles_use_real_identity_without_reading_the_root_again() {
        let dir = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(dir.path()).unwrap();
        let path = real.join("root.ral");
        std::fs::write(&path, [0xff]).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("root.ral", real.join("alias.ral")).unwrap();
        for spelling in ["root.ral", "./root.ral"]
            .into_iter()
            .chain(cfg!(unix).then_some("alias.ral"))
        {
            let origin = SourceOrigin::File(SourceFile::new(spelling).unwrap());
            let source = "use './root.ral'";
            for host_load in [false, true] {
                let mut shell = crate::test_helper::core_shell();
                shell.seed_cwd(crate::path::CwdPath::new(real.clone()).unwrap());
                let before = shell.sources().next_id();
                let rejection = if host_load {
                    let Err(Break::Error(error)) =
                        check_source(source, &origin, &Env::new(), &mut shell, None)
                    else {
                        panic!("host root self-import must fail")
                    };
                    error.rejection().unwrap().clone()
                } else {
                    let Err(diagnostics) =
                        crate::shell::run::compile_run(&mut shell, source, &origin)
                    else {
                        panic!("run root self-import must fail")
                    };
                    let crate::shell::run::StaticDiagnostics::Compile(rejection) = *diagnostics
                    else {
                        panic!("compile refusal")
                    };
                    rejection
                };
                assert_eq!(rejection.source.origin(), &origin);
                assert_eq!(rejection.reports[0].code, Some("U0003"));
                assert!(
                    rejection.nested.is_none(),
                    "the cycle is caught at the root import"
                );
                assert_eq!(shell.sources().next_id().0, before.0 + 1);
            }
        }
    }

    #[test]
    fn checking_already_read_root_text_does_not_request_root_read_authority() {
        use crate::authority::policy::{Capabilities, FsPolicy};
        use crate::path::FrozenPath;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("root.ral");
        std::fs::write(&path, [0xff]).unwrap();
        let origin = SourceOrigin::File(SourceFile::new(path.clone()).unwrap());
        let mut shell = crate::test_helper::core_shell();
        shell.push_session_capabilities(crate::test_helper::layer(Capabilities {
            fs: Some(FsPolicy {
                deny_paths: vec![FrozenPath::freeze(&path).unwrap()],
                ..FsPolicy::default()
            }),
            ..Capabilities::default()
        }));
        check_source("return 1", &origin, &Env::new(), &mut shell, None)
            .expect("root text is already held");
        crate::shell::run::compile_run(&mut shell, "return 1", &origin)
            .unwrap_or_else(|_| panic!("root text is already held"));
    }

    #[test]
    fn module_cycles_and_depth_are_check_time_chain_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let path = std::fs::canonicalize(dir.path())
            .unwrap()
            .join("module.ral");
        std::fs::write(&path, "let exported = 7").unwrap();
        let origin = SourceOrigin::File(SourceFile::new(path.clone()).unwrap());
        let mut shell = crate::test_helper::core_shell();
        let at = import_site(&mut shell);
        let cycle = load_module(
            at,
            path.to_str().unwrap(),
            None,
            std::slice::from_ref(&origin),
            &mut shell,
        )
        .expect_err("cycle");
        assert!(matches!(cycle, UseRefusal::Cycle { chain }
            if chain == [origin.clone(), origin.clone()]));
        let chain = (0..MAX_SOURCE_DEPTH)
            .map(|i| SourceOrigin::named(format!("<depth-{i}>")))
            .collect::<Vec<_>>();
        let depth = load_module(at, path.to_str().unwrap(), None, &chain, &mut shell)
            .expect_err("depth limit");
        assert!(matches!(depth, UseRefusal::Depth { chain: nested, limit }
            if limit == MAX_SOURCE_DEPTH && nested.len() == chain.len() + 1
                && nested[..chain.len()] == chain && nested.last() == Some(&origin)));
    }

    #[test]
    fn closed_evaluation_keeps_definitions_out_of_the_session() {
        let mut shell = crate::test_helper::core_shell();
        let value = evaluate_closed(
            &Mooring::adrift(),
            &mut shell,
            "let private = 7; return $private",
            &SourceOrigin::named("<closed>"),
            None,
        )
        .expect("closed unit");
        assert_eq!(value, Value::Int(7));
        assert!(shell.env.get("private").is_none());
    }

    #[test]
    fn session_extension_keeps_definitions_before_a_failure() {
        let mut shell = crate::test_helper::core_shell();
        let outcome = extend_session(
            &Mooring::adrift(),
            &mut shell,
            "let installed = 7; fail 'stop'",
            &SourceOrigin::named("<session-extension>"),
            None,
        );
        assert!(outcome.is_err());
        assert_eq!(shell.env.get("installed"), Some(&Value::Int(7)));
        assert!(shell.env.binding("installed").unwrap().scheme.is_some());
    }
}
