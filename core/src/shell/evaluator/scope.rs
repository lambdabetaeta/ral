//! Vocabulary shared by the scope IR nodes — `within`, `grant`, `try`,
//! `guard`, `audit` — whose machine arms live in `evaluator::machine`:
//! parsing and installing `within`'s options.

use crate::environ::EnvVars;
use crate::fact::{Expected, InvalidCause, InvalidFailure, Reason, ShapeFailure, ValueShape};
use crate::shell::runtime::{Error, FrameHandle, HandlerEntry, Map, Settled, Shell, Value};

use crate::path::{CwdPath, Reading};

/// Parsed `within [...]` options; each key becomes a `Shell::with_*` scope.
pub(crate) struct WithinScope {
    env_overrides: Option<EnvVars>,
    cwd: Option<CwdPath>,
    handlers: Option<(Vec<HandlerEntry>, Option<Value>)>,
}

impl WithinScope {
    /// `shell` resolves and permission-checks `dir:`. Handlers are statically checked.
    pub(crate) fn parse(opts: &Map, shell: &mut Shell) -> Settled<Self> {
        let mut env_overrides = None;
        let mut cwd = None;
        let mut catch_all: Option<Value> = None;
        // `handlers` and `handler` are independent: naming either installs a
        // frame, naming neither leaves the stack alone.
        let mut saw_handlers = false;
        let mut entries = Vec::new();

        for (k, v) in opts {
            match k {
                "env" => env_overrides = Some(parse_env(&v)?),
                "dir" => cwd = Some(parse_dir(&v, shell)?),
                "handler" => {
                    catch_all = Some(v.into_owned());
                    saw_handlers = true;
                }
                "handlers" => {
                    entries = v
                        .as_map("within")?
                        .iter()
                        .map(|(name, thunk)| {
                            HandlerEntry::ral_per_name(name.to_string(), thunk.into_owned())
                        })
                        .collect();
                    saw_handlers = true;
                }
                _ => unreachable!(
                    "within's options are syntax; the checker admits only declared keys"
                ),
            }
        }

        Ok(Self {
            env_overrides,
            cwd,
            handlers: saw_handlers.then_some((entries, catch_all)),
        })
    }

    /// Install the parsed keys into the store — env, then cwd, then
    /// handlers — and return the token that undoes them, in reverse, once
    /// the body has run.
    pub(crate) fn enter(self, shell: &mut Shell) -> WithinUndo {
        let Self {
            env_overrides,
            cwd,
            handlers,
        } = self;
        let saved_env = env_overrides.map(|overrides| {
            let saved = shell.context.env_overrides.clone();
            shell.context.overlay_env(&overrides);
            saved
        });
        let saved_cwd = cwd.map(|dir| shell.enter_cwd(dir));
        let handlers =
            handlers.map(|(entries, catch_all)| shell.context.handlers.push(entries, catch_all));
        WithinUndo {
            saved_env,
            saved_cwd,
            handlers,
        }
    }
}

/// `env:` — a map of scalar overrides.
fn parse_env(v: &Value) -> Settled<EnvVars> {
    let mut env = EnvVars::new();
    let Value::Map(values) = v else {
        return Err(Error::new(Reason::Invalid(Box::new(
            InvalidFailure::new(
                "within".into(),
                InvalidCause::Shape(ShapeFailure {
                    expected: Expected::Shape(ValueShape::Map),
                    found: v.found(),
                }),
            )
            .field("env".into()),
        )))
        .into());
    };
    for (name, value) in values {
        let text = value.scalar_text().ok_or_else(|| {
            Error::new(Reason::Invalid(Box::new(
                InvalidFailure::new(
                    "within".into(),
                    InvalidCause::Shape(ShapeFailure {
                        expected: Expected::AnyOf(vec![
                            ValueShape::String,
                            ValueShape::Int,
                            ValueShape::Float,
                            ValueShape::Bool,
                        ]),
                        found: value.found(),
                    }),
                )
                .field(name.to_string())
                .field("env".into()),
            )))
        })?;
        env.insert(name.to_string(), text).map_err(|reserved| {
            Error::new(Reason::Invalid(Box::new(
                InvalidFailure::new(
                    "within".into(),
                    InvalidCause::ReservedEnvironment(reserved.into()),
                )
                .field(name.to_string())
                .field("env".into()),
            )))
        })?;
    }
    Ok(env)
}

/// `dir:` — resolved, permission-checked, and required to exist as a
/// directory, and to be one a path can be read against ([`CwdPath`]), before
/// the body runs rather than at the first command.  The cwd is where the
/// directory is.
fn parse_dir(v: &Value, shell: &mut Shell) -> Settled<CwdPath> {
    let path = v.to_string();
    if path.is_empty() {
        return Err(Error::new(Reason::Invalid(Box::new(
            InvalidFailure::new("within".into(), InvalidCause::EmptyPath).field("dir".into()),
        )))
        .into());
    }
    let rp = shell.resolve(&path);
    let dir = shell.locate_dir::<Reading>(&rp)?;
    Ok(
        CwdPath::new(dir.real().as_path().to_path_buf()).map_err(|e| {
            Error::new(Reason::Invalid(Box::new(
                InvalidFailure::new("within".into(), InvalidCause::Path((&e).into()))
                    .field("dir".into()),
            )))
        })?,
    )
}

/// The undo token `WithinScope::enter` returns: what to put back, and in
/// what order, once `within`'s body has run.  Frames hold this, never a
/// `Context` clone.
pub(crate) struct WithinUndo {
    saved_env: Option<EnvVars>,
    /// The whole cell, so a `cd` in the body is undone with the entry.
    saved_cwd: Option<CwdPath>,
    handlers: Option<FrameHandle>,
}

impl WithinUndo {
    /// Undo in the reverse of install order: handlers, then cwd, then env.
    pub(crate) fn apply(self, shell: &mut Shell) {
        if let Some(handle) = self.handlers {
            shell.context.handlers.remove_by_handle(handle);
        }
        if let Some(saved) = self.saved_cwd {
            shell.restore_cwd(saved);
        }
        if let Some(saved) = self.saved_env {
            shell.context.env_overrides = saved;
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::path::shown;
    use crate::protocol::Run;
    use crate::shell::run::RunReport;
    use crate::shell::runtime::Mooring;
    use crate::shell::runtime::{BuiltinBody, BuiltinEntry};
    use crate::source::SourceOrigin;

    /// Every command observation's own `argv[0]`, in order, from an
    /// `audit { … }` report's flat `trail`.
    fn command_argv0s(report: &Value) -> Vec<String> {
        let map = report.as_map("test").expect("audit returns a map");
        let trail_field = map.get("trail");
        let Some(Value::List(trail)) = trail_field.as_deref() else {
            panic!("an audit report must have a list `trail` field");
        };
        trail
            .iter()
            .filter_map(|obs| {
                let obs = obs.as_map("test").ok()?;
                let what = obs.get("what")?;
                let Value::Variant { label, payload } = what.as_ref() else {
                    return None;
                };
                let fact = payload.as_deref()?.as_map("test").ok()?;
                match (label.as_ref(), fact.get("argv").as_deref()) {
                    ("command", Some(Value::List(argv))) => match argv.iter().next().as_deref() {
                        Some(Value::String(s)) => Some(s.to_string()),
                        _ => None,
                    },
                    _ => None,
                }
            })
            .collect()
    }

    /// `try` names the failing command from the error's own stamp — the
    /// innermost dispatch's shown name — and collects nothing to do it, so a
    /// stage launched under or after it inherits no policy.
    /// `Audit::active_policy` is exactly what `pipeline::thread` reads.
    #[test]
    fn try_names_the_failing_command_without_a_trail() {
        let mut shell = crate::test_helper::core_shell();
        let cmd = shell
            .run(Run::captured(
                r#"try { sh -c "exit 1"; return 'unreached' } { |e| return $e[frame] }"#,
                SourceOrigin::named("<test>"),
            ))
            .expect_ran()
            .into_settled()
            .expect("the handler must run");
        assert_eq!(cmd.to_string(), "sh");
        assert_eq!(
            shell.local.audit.active_policy(),
            None,
            "`try` must open no trail"
        );
    }

    /// Stands in for any Rust panic a delimited body can raise mid-eval.
    fn panic_now(_args: &[Value], _mooring: &Mooring, _shell: &mut Shell) -> Settled<Value> {
        panic!("scope test: deliberate mid-body panic")
    }

    fn panic_now_scheme(_: &mut crate::ty::Binders) -> crate::ty::Ty {
        use crate::typecheck::builtins::{pure, thunk};
        thunk(pure(crate::ty::Ty::unit()))
    }

    static PANIC_BUILTINS_ARR: [BuiltinEntry; 1] = [BuiltinEntry::new(
        std::borrow::Cow::Borrowed("core-panic-now"),
        panic_now_scheme,
        "test-only: panic the evaluator mid-run.",
        BuiltinBody::Static(panic_now),
    )];
    static PANIC_BUILTINS: &[BuiltinEntry] = &PANIC_BUILTINS_ARR;

    /// Law 2 holds under a panic too: `Frame::abandon` closes the scope under
    /// `catch_unwind`, before resuming the unwind — so a body that panics
    /// leaves the trail exactly as closed as one that returns normally.
    #[test]
    fn a_panicking_audit_body_still_closes_the_trail() {
        let mut shell = crate::HostSurface {
            statics: vec![PANIC_BUILTINS],
            ..crate::HostSurface::default()
        }
        .shell(
            crate::io::terminal::TerminalState::default(),
            crate::test_helper::process_cwd(),
        )
        .expect("a test host resolves its baseline");
        match shell.run(Run::captured(
            "audit { core-panic-now }",
            SourceOrigin::named("<test>"),
        )) {
            RunReport::Ran {
                ending: crate::shell::runtime::End::Panic(_),
                captured: None,
                ..
            } => {}
            RunReport::Static { .. } | RunReport::Ran { .. } => {
                panic!("a panicking body must report a panic ending")
            }
            RunReport::Checked { .. } => panic!("a source run must not report check-only success"),
        }
        assert_eq!(
            shell.local.audit.active_policy(),
            None,
            "a panic through `audit`'s body must not leave the trail open"
        );
    }

    /// A panic mid-run rolls back to the checkpoint `Shell::enter` takes at
    /// the top of the run: `env` and `context` (cwd included) read exactly as
    /// they did before the run started, even though the panicking phrase's
    /// own `let` and `cd` ran first.  Only a panic restores this way — an
    /// ordinary error leaves whatever `let`s landed before it (`docs/SPEC.md` §5.6).
    #[test]
    fn panic_mid_run_restores_the_pre_run_checkpoint() {
        let mut shell = crate::HostSurface {
            statics: vec![PANIC_BUILTINS],
            ..crate::HostSurface::default()
        }
        .shell(
            crate::io::terminal::TerminalState::default(),
            crate::test_helper::process_cwd(),
        )
        .expect("a test host resolves its baseline");
        let before_cwd = shell.cwd();
        let tmp = shown(std::env::temp_dir());

        match shell.run(Run::captured(
            format!("let checkpoint_leak = 1\ncd '{tmp}'\ncore-panic-now"),
            SourceOrigin::named("<test>"),
        )) {
            RunReport::Ran {
                ending: crate::shell::runtime::End::Panic(_),
                captured: None,
                ..
            } => {}
            RunReport::Static { .. } | RunReport::Ran { .. } => {
                panic!("a panicking body must report a panic ending")
            }
            RunReport::Checked { .. } => panic!("a source run must not report check-only success"),
        }

        assert!(
            shell.env.get("checkpoint_leak").is_none(),
            "a panic must roll back a let bound before it"
        );
        assert_eq!(
            shell.cwd(),
            before_cwd,
            "a panic must roll back a cd made before it"
        );
    }

    /// The flat merge: an `audit` around a `try` finds the commands the
    /// `try`'s body ran in its own trail, because a `try` interposes no
    /// scope at all.
    #[test]
    #[cfg(unix)]
    fn nested_delimiters_flat_merge() {
        let mut shell = crate::test_helper::core_shell();
        let report = shell.run(Run::captured(
            "audit { /bin/echo one; try { /bin/echo two } { |_e| return () }; /bin/echo three }",
            SourceOrigin::named("<test>"),
        )).expect_ran().into_settled().expect("audit body must succeed");
        assert_eq!(
            command_argv0s(&report),
            ["/bin/echo", "/bin/echo", "/bin/echo"]
        );
    }

    /// A builtin application is not an observation: `echo` is a base frame
    /// and `length` a native, and neither leaves a `` `command `` behind.
    /// Only the external does.
    #[test]
    fn builtins_leave_no_command_observation() {
        let mut shell = crate::test_helper::core_shell();
        let report = shell
            .run(Run::captured(
                "audit { echo one; length [1, 2]; /bin/echo two }",
                SourceOrigin::named("<test>"),
            ))
            .expect_ran()
            .into_settled()
            .expect("audit body must succeed");
        assert_eq!(command_argv0s(&report), ["/bin/echo"]);
    }

    /// `audit { }`'s report envelope: a body that returned gives `` `ok ``,
    /// over a flat `trail` naming the one command it ran.
    #[test]
    #[cfg(unix)]
    fn audit_reports_ok_over_its_trail() {
        let mut shell = crate::test_helper::core_shell();
        let report = shell
            .run(Run::captured(
                "audit { /bin/echo hi }",
                SourceOrigin::named("<test>"),
            ))
            .expect_ran()
            .into_settled()
            .expect("audit { echo hi } must succeed");
        let map = report.as_map("test").expect("audit returns a map");
        match map.get("outcome").as_deref() {
            Some(Value::Variant { label, .. }) => assert_eq!(label.as_ref(), "ok"),
            other => panic!("a returning body must report `ok, got {other:?}"),
        }
        assert_eq!(command_argv0s(&report), ["/bin/echo"]);
    }

    #[test]
    fn within_runs_a_handler_that_captures_a_session_function() {
        let mut shell = crate::test_helper::core_shell();
        let src = "let h = { |n| return $[$n + 1] }\n\
                   let blk = { within [handlers: [:, g: { |args| h 1; echo hi }]] { g } }\n\
                   !$blk";
        let out = crate::shell::evaluator::run_source(src, &mut shell)
            .expect("a handler in a captured block must retain its session function");
        assert_eq!(out, Value::Unit);
    }

    #[test]
    fn an_empty_handler_map_still_installs_a_scoped_frame() {
        let mut shell = crate::test_helper::core_shell();
        let absent = WithinScope::parse(&Map::new(), &mut shell).expect("no options");
        assert!(absent.handlers.is_none());
        let opts = Map::from_iter([("handlers".into(), Value::map(vec![]))]);
        let present = WithinScope::parse(&opts, &mut shell).expect("an empty handler map");
        assert!(matches!(present.handlers, Some((entries, None)) if entries.is_empty()));
    }

    /// A key the table declares but the parser lacks an arm for would reach
    /// the parser's `unreachable!`, so walking the table is what turns "new
    /// key, no arm" into a test failure.
    #[test]
    fn every_declared_within_key_is_handled_by_parse() {
        use crate::typecheck::table::{Form, declared};
        let mut shell = crate::test_helper::core_shell();
        for key in declared(Form::Within).keys {
            let opts = Map::from_iter([(key.label.to_string(), Value::Unit)]);
            let _ = WithinScope::parse(&opts, &mut shell);
        }
    }
    #[test]
    fn within_input_failures_keep_field_paths_and_reserved_names() {
        use crate::fact::{DataStep, ReservedVar};
        let failure = |result: Settled<EnvVars>| {
            let Err(crate::shell::runtime::Break::Error(Error {
                reason: Reason::Invalid(f),
                ..
            })) = result
            else {
                panic!("invalid environment")
            };
            *f
        };
        assert_eq!(
            failure(parse_env(&Value::Int(1))).path,
            vec![DataStep::Field("env".into())]
        );
        let scalar = failure(parse_env(&Value::map(vec![(
            "X".into(),
            Value::list(vec![]),
        )])));
        assert_eq!(scalar.operation, "within");
        assert_eq!(
            scalar.path,
            vec![DataStep::Field("env".into()), DataStep::Field("X".into())]
        );
        assert!(matches!(scalar.cause, InvalidCause::Shape(_)));
        let reserved = failure(parse_env(&Value::map(vec![(
            "PWD".into(),
            Value::string("/tmp"),
        )])));
        assert_eq!(
            reserved.cause,
            InvalidCause::ReservedEnvironment(ReservedVar::Pwd)
        );
        let mut shell = crate::test_helper::core_shell();
        let Err(crate::shell::runtime::Break::Error(error)) =
            parse_dir(&Value::string(""), &mut shell)
        else {
            panic!("empty directory")
        };
        assert!(
            matches!(error.reason, Reason::Invalid(f) if f.path == [DataStep::Field("dir".into())] && f.cause == InvalidCause::EmptyPath)
        );
    }
}
