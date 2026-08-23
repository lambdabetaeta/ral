//! Evaluator arms for the scope IR nodes — `within`, `grant`, `try`,
//! `guard`, `audit` — and the vocabulary they share.
//!
//! The arms live here rather than inline in `eval_comp` so that match's
//! unoptimised debug frame stays small enough for deep recursion under the
//! default 2 MiB test-thread stack (`deeply_nested_calls`).

use crate::ir::Val;
use crate::types::{
    BodyResult, Break, CapturePolicy, HandlerEntry, HandlerRole, Map, Mooring, Observation,
    Observed, Raw, Settled, Shell, Value, as_map, sig, tree_value, validate_handler_arity,
};

use crate::evaluator::val::eval_val;
use crate::evaluator::{apply, audit};
use std::collections::HashMap;
use std::path::PathBuf;

/// `try`'s body result, flattened for the handler-call decision.
pub(crate) struct Outcome {
    pub ok: bool,
    pub status: i32,
    pub value: Value,
    pub message: String,
    pub cmd: String,
    pub line: usize,
    pub col: usize,
}

/// The `{cmd, status, message, line, col}` record `try` hands its handler and
/// `poll` its `` `err `` payload.  Bytes are absent by design; `audit` is the
/// forensic path.  Mirrors `typecheck::builtins::try_error_record`.
pub(crate) fn error_record(
    cmd: &str,
    status: i32,
    message: &str,
    line: usize,
    col: usize,
) -> Value {
    #[allow(
        clippy::cast_possible_wrap,
        reason = "line/col are source positions bounded by source size, far below i64::MAX"
    )]
    Value::map(vec![
        ("cmd".into(), Value::String(cmd.to_string())),
        ("status".into(), Value::Int(i64::from(status))),
        ("message".into(), Value::String(message.to_string())),
        ("line".into(), Value::Int(line as i64)),
        ("col".into(), Value::Int(col as i64)),
    ])
}

/// A failed body's position comes from the error's own span; an unspanned
/// error falls back to the run's call site.  Only a [`Observed::Command`]
/// names the failing command: a capability check's denial no longer
/// masquerades as one via the old status pun (D4).
pub(crate) fn classify(body: &BodyResult, children: &[Observation], shell: &Shell) -> Outcome {
    match body {
        BodyResult::Value(v) => Outcome {
            ok: true,
            status: 0,
            value: v.clone(),
            message: String::new(),
            cmd: String::new(),
            line: 0,
            col: 0,
        },
        BodyResult::Error(e) => {
            let failing = children.iter().rev().find_map(|obs| match &obs.what {
                Observed::Command { status, argv, .. } if *status != 0 => argv.first().cloned(),
                _ => None,
            });
            let site = e
                .span
                .map_or_else(|| shell.call_site(), |s| shell.site_of(Some(s)));
            Outcome {
                ok: false,
                status: e.exit_code(),
                value: Value::Unit,
                message: e.message.clone(),
                cmd: failing.unwrap_or_else(|| "<runtime>".into()),
                line: site.line,
                col: site.col,
            }
        }
    }
}

/// Parsed `within [...]` options; each key becomes a `Shell::with_*` scope.
pub(crate) struct WithinScope {
    env_overrides: Option<HashMap<String, String>>,
    cwd: Option<PathBuf>,
    handlers: Option<(Vec<HandlerEntry>, Option<Value>)>,
}

impl WithinScope {
    pub(crate) fn parse(opts: &Map, shell: &mut Shell) -> Settled<Self> {
        let mut env_overrides = None;
        let mut cwd = None;
        let mut entries: Vec<HandlerEntry> = Vec::new();
        let mut catch_all: Option<Value> = None;
        let mut saw_handlers = false;

        for (k, v) in opts {
            match k.as_str() {
                "env" => {
                    let overrides = as_map(v, "within env")?;
                    for k in overrides.keys() {
                        if matches!(k.as_str(), "PWD" | "OLDPWD") {
                            return Err(sig(format!(
                                "within env: `{k}` is derived from the shell's working \
                                 directory and cannot be set here; use `cd` to change the \
                                 directory"
                            )));
                        }
                    }
                    let map = overrides
                        .into_iter()
                        .map(|(ek, ev)| {
                            let sv = match ev {
                                Value::String(s) => s,
                                Value::Int(n) => n.to_string(),
                                Value::Float(n) => n.to_string(),
                                Value::Bool(b) => b.to_string(),
                                other => {
                                    return Err(sig(format!(
                                        "within env: value for '{ek}' must be a scalar (string, int, float, or bool), got {}",
                                        other.type_name()
                                    )));
                                }
                            };
                            Ok((ek, sv))
                        })
                        .collect::<Settled<HashMap<String, String>>>()?;
                    env_overrides = Some(map);
                }
                "dir" => {
                    let path = v.to_string();
                    if path.is_empty() {
                        return Err(sig("within dir: path cannot be empty"));
                    }
                    let rp = shell.resolve(&path);
                    shell.check_fs_read(&rp)?;
                    if !rp.as_path().is_dir() {
                        return Err(sig(format!("within dir: {path}: not a directory")));
                    }
                    cwd = Some(rp.into_inner());
                }
                "handlers" => {
                    let map = as_map(v, "within handlers")?;
                    entries = map
                        .into_iter()
                        .map(|(cmd, thunk_val)| {
                            HandlerEntry::vet(
                                cmd,
                                thunk_val,
                                shell.session_schemes(),
                                HandlerRole::Scoped,
                            )
                        })
                        .collect::<Settled<Vec<HandlerEntry>>>()?;
                    saw_handlers = true;
                }
                "handler" => {
                    validate_handler_arity(v, 2, "within handler: catch-all")?;
                    catch_all = Some(v.clone());
                    saw_handlers = true;
                }
                _ => return Err(sig(format!("within: unknown key '{k}'"))),
            }
        }

        let handlers = if saw_handlers {
            Some((entries, catch_all))
        } else {
            None
        };
        Ok(Self {
            env_overrides,
            cwd,
            handlers,
        })
    }

    /// Nest the parsed keys as `with_*` scopes around `body`: env outermost,
    /// then cwd, handlers innermost.
    pub(crate) fn enter<R>(self, shell: &mut Shell, body: impl FnOnce(&mut Shell) -> R) -> R {
        let Self {
            env_overrides,
            cwd,
            handlers,
        } = self;
        let wrapped = |shell: &mut Shell| match handlers {
            Some((entries, catch_all)) => shell.with_handlers(entries, catch_all, body),
            None => body(shell),
        };
        let wrapped = |shell: &mut Shell| match cwd {
            Some(path) => shell.with_cwd(path, wrapped),
            None => wrapped(shell),
        };
        match env_overrides {
            Some(o) => shell.with_env(o, wrapped),
            None => wrapped(shell),
        }
    }
}

// ── Lifted `eval_comp` arms ──────────────────────────────────────────────

pub(crate) fn eval_within(
    opts: &Val,
    body: &Val,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    let opts_val = eval_val(opts, shell)?;
    let opts_map = as_map(&opts_val, "within")?;
    let scope = WithinScope::parse(&opts_map, shell)?;
    let body = eval_val(body, shell)?;
    scope
        .enter(shell, |shell| apply(body, vec![], mooring, shell))
        .map_err(Into::into)
}

pub(crate) fn eval_grant(
    caps: &Val,
    body: &Val,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    let caps_val = eval_val(caps, shell)?;
    let home = shell.mobile.context.home();
    let cwd = shell.cwd();
    let ctx = crate::path::sigil::FreezeCtx {
        home: home.as_deref(),
        cwd: &cwd,
    };
    let caps =
        crate::capability::decode_capability_map(&caps_val, "grant", &ctx).map_err(Break::from)?;
    let body = eval_val(body, shell)?;
    shell
        .with_capabilities(caps, |shell| apply(body, vec![], mooring, shell))
        .map_err(Into::into)
}

pub(crate) fn eval_try(
    body: &Val,
    handler: &Val,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    // Pure control flow: bytes keep flowing through fd 1/2, hence
    // `CapturePolicy::Off`.  Children are still forced so the error record can
    // name the failing command; `Exit`/`Stopped` leave via `?` before `classify`.
    let body_val = eval_val(body, shell)?;
    let handler_val = eval_val(handler, shell)?;
    let (body_result, children) = audit::delimited(shell, CapturePolicy::Off, |s| {
        apply(body_val, vec![], mooring, s)
    })
    .map_err(Break::Escape)?;

    let outcome = classify(&body_result, &children, shell);

    let err_record = error_record(
        &outcome.cmd,
        outcome.status,
        &outcome.message,
        outcome.line,
        outcome.col,
    );

    // `try` recovers, so the failure's status must not leak past it.
    shell.mobile.control.last_status = 0;

    if outcome.ok {
        Ok(outcome.value)
    } else {
        apply(handler_val, vec![err_record], mooring, shell).map_err(Into::into)
    }
}

pub(crate) fn eval_guard(
    body: &Val,
    cleanup: &Val,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    let body_val = eval_val(body, shell)?;
    let cleanup_val = eval_val(cleanup, shell)?;
    let body_result = apply(body_val, vec![], mooring, shell);
    // One rule for both signals: any halt of the cleanup pre-empts the body's
    // outcome, error exactly as escape.  A cleanup that cannot fail the
    // computation is a cleanup whose failures are unreportable; whoever wants
    // log-and-continue writes `guard { … } { try { … } { |e| … } }`.
    apply(cleanup_val, vec![], mooring, shell)
        .and(body_result)
        .map_err(Into::into)
}

pub(crate) fn eval_audit(body: &Val, mooring: &Mooring, shell: &mut Shell) -> Raw<Value> {
    let body_val = eval_val(body, shell)?;
    let (body_result, children) = audit::delimited(shell, CapturePolicy::Bytes, |s| {
        apply(body_val, vec![], mooring, s)
    })
    .map_err(Break::Escape)?;
    let (status, value, error) = match &body_result {
        BodyResult::Value(v) => (shell.mobile.control.last_status, v.clone(), None),
        BodyResult::Error(e) => (e.exit_code(), Value::Unit, Some(e.message_with_hint())),
    };
    shell.mobile.control.last_status = status;
    Ok(tree_value(status, value, error, &children))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};
    use crate::transport::{Program, Run};
    use crate::types::{BuiltinBody, BuiltinEntry, Capabilities};

    fn capture_req(src: &str) -> RunRequest<'static> {
        RunRequest {
            run: Run {
                program: Program::Source(src.into()),
                script_name: "<test>".into(),
                caps: Capabilities::root(),
                wall: None,
                deferred_lease: None,
                worker_cap: None,
                io: RunIo::Capture,
                terminal: RequestedTerminalAccess::Denied,
                stdin: RunStdin::Empty,
                trail: None,
            },
            surface: None,
            deferred: None,
            desk: None,
            nursery: None,
            lifecycle: Box::new(()),
        }
    }

    fn run_source(shell: &mut Shell, src: &str) -> RunReport {
        let _slot_guard = crate::process::cancel::REQUEST_SERIAL.lock();
        shell.run(capture_req(src))
    }

    /// A command observation's own `argv[0]`, depth-first, for every
    /// command in an `audit { … }` tree's flat `children` list.
    fn command_argv0s(tree: &Value) -> Vec<String> {
        let map = as_map(tree, "test").expect("audit returns a map");
        let Some(Value::List(children)) = map.get("children") else {
            panic!("audit tree must have a list `children` field");
        };
        children
            .iter()
            .filter_map(|c| {
                let m = as_map(c, "test").ok()?;
                match m.get("argv") {
                    Some(Value::List(argv)) => match argv.iter().next() {
                        Some(Value::String(s)) => Some(s.clone()),
                        _ => None,
                    },
                    _ => None,
                }
            })
            .collect()
    }

    /// `try` forces collection on for its body via `delimited`, and closing
    /// is the opener's: once `try` returns, the trail is closed again, so a
    /// stage launched afterward inherits nothing.  `Audit::active_policy` is
    /// exactly what `pipeline::launch` and `child_eval` read to decide.
    #[test]
    fn try_closes_the_trail_it_opened() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        match run_source(&mut shell, r#"try { sh -c "exit 1" } { |_e| return unit }"#) {
            RunReport::Ran { .. } => {}
            RunReport::Static { .. } => panic!("well-formed source must run"),
        }
        assert_eq!(
            shell.local.audit.active_policy(),
            None,
            "a stage launched after `try` must inherit no policy"
        );
    }

    /// Stands in for any Rust panic a `try` body can raise mid-eval.
    fn panic_now(_args: &[Value], _mooring: &Mooring, _shell: &mut Shell) -> Settled<Value> {
        panic!("scope test: deliberate mid-`try` panic")
    }

    fn panic_now_scheme(_u: &mut crate::typecheck::Unifier) -> crate::typecheck::Scheme {
        use crate::typecheck::builtins::{mk_scheme, pure, thunk};
        mk_scheme(&[], &[], &[], thunk(pure(crate::typecheck::Ty::Unit)))
    }

    static PANIC_BUILTINS_ARR: [BuiltinEntry; 1] = [BuiltinEntry::new(
        std::borrow::Cow::Borrowed("core-panic-now"),
        crate::typecheck::builtins::BuiltinTypeRule::Scheme(panic_now_scheme),
        "test-only: panic the evaluator mid-run.",
        BuiltinBody::Static(panic_now),
    )];
    static PANIC_BUILTINS: &[BuiltinEntry] = &PANIC_BUILTINS_ARR;

    /// Law 2 holds under a panic too: `delimited` closes its scope under
    /// `catch_unwind`, before resuming the unwind — so a body that panics
    /// leaves the trail exactly as closed as one that returns normally.
    #[test]
    fn a_panicking_try_body_still_closes_the_trail() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.install_builtins(PANIC_BUILTINS);
        match run_source(&mut shell, "try { core-panic-now } { |_e| return unit }") {
            RunReport::Static { .. } => {}
            RunReport::Ran { .. } => panic!("a panicking body must report Static"),
        }
        assert_eq!(
            shell.local.audit.active_policy(),
            None,
            "a panic through `try`'s body must not leave the trail open"
        );
    }

    /// Nested delimiters see the flat merge: an outer `audit` around a
    /// `try` still finds the `try`'s own children in its tree, because
    /// `try`'s `close` only reads a suffix and leaves the outer trail
    /// intact.
    #[test]
    fn nested_delimiters_flat_merge() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let tree = match run_source(
            &mut shell,
            "audit { echo one; try { echo two } { |_e| return unit }; echo three }",
        ) {
            RunReport::Ran { ending, .. } => ending.into_result().expect("audit body must succeed"),
            RunReport::Static { .. } => panic!("well-formed source must run"),
        };
        assert_eq!(command_argv0s(&tree), ["echo", "echo", "echo"]);
    }

    /// `audit { }`'s own recorded shape is unchanged by the lifecycle
    /// rewrite: status, and a flat `children` list naming the one command
    /// its body ran.
    #[test]
    fn audit_shape_is_unchanged() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let tree = match run_source(&mut shell, "audit { echo hi }") {
            RunReport::Ran { ending, .. } => ending
                .into_result()
                .expect("audit { echo hi } must succeed"),
            RunReport::Static { .. } => panic!("well-formed source must run"),
        };
        let map = as_map(&tree, "test").expect("audit returns a map");
        assert_eq!(map.get("status"), Some(&Value::Int(0)));
        assert_eq!(command_argv0s(&tree), ["echo"]);
    }
}
