//! Vocabulary shared by the scope IR nodes — `within`, `grant`, `try`,
//! `guard`, `audit` — whose machine arms live in `evaluator::machine`:
//! parsing and installing `within`'s options, and classifying a delimited
//! body's result into the record `try` and `poll` hand their handler.

use crate::types::{
    Env, EnvVars, Error, FrameHandle, HandlerEntry, HandlerRole, Map, Observation, Observed,
    Settled, Shell, Value, as_map, sig, validate_handler_arity,
};

use std::collections::HashMap;
use std::path::PathBuf;

/// A failed `try`/`guard`/`audit` body, flattened for the error record.
pub(crate) struct Outcome {
    pub status: i32,
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
pub(crate) fn classify(e: &Error, children: &[Observation], shell: &Shell) -> Outcome {
    let failing = children.iter().rev().find_map(|obs| match &obs.what {
        Observed::Command { status, argv, .. } if *status != 0 => argv.first().cloned(),
        _ => None,
    });
    let site = e
        .span
        .map_or_else(|| shell.call_site(), |s| shell.site_of(Some(s)));
    Outcome {
        status: e.exit_code(),
        message: e.message.clone(),
        cmd: failing.unwrap_or_else(|| "<runtime>".into()),
        line: site.line,
        col: site.col,
    }
}

/// Parsed `within [...]` options; each key becomes a `Shell::with_*` scope.
pub(crate) struct WithinScope {
    env_overrides: Option<HashMap<String, String>>,
    cwd: Option<PathBuf>,
    handlers: Option<(Vec<HandlerEntry>, Option<Value>)>,
}

impl WithinScope {
    /// `shell` resolves and permission-checks `dir:`; handler validation
    /// reads its schemes off `env`, the lexical environment the `within`
    /// itself closes under, not the shell's mutable scope.
    pub(crate) fn parse(opts: &Map, env: &Env, shell: &mut Shell) -> Settled<Self> {
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
                                Value::Float(n) => crate::types::fmt_float(n),
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
                    let schemes = crate::typecheck::SessionSchemes {
                        bindings: env.binding_schemes(),
                        aliases: shell.context.handlers.alias_schemes(),
                        builtins: shell.session.builtins.clone(),
                    };
                    entries = map
                        .into_iter()
                        .map(|(cmd, thunk_val)| {
                            HandlerEntry::vet(cmd, thunk_val, schemes.clone(), HandlerRole::Scoped)
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
            shell.context.extend_env(overrides);
            saved
        });
        let saved_dir = cwd.map_or(SavedDir::Unset, |path| {
            SavedDir::Prior(shell.swap_cwd_override(path))
        });
        let handlers = handlers
            .map(|(entries, catch_all)| shell.context.handlers.push(entries, catch_all));
        WithinUndo {
            saved_env,
            saved_dir,
            handlers,
        }
    }
}

/// A `within [dir: …]` override, distinguishing "this scope left `dir`
/// alone" from "this scope installed `dir`, displacing `PathBuf` (or no
/// prior override)".
enum SavedDir {
    Unset,
    Prior(Option<PathBuf>),
}

/// The undo token `WithinScope::enter` returns: what to put back, and in
/// what order, once `within`'s body has run.  Frames hold this, never a
/// `Context` clone.
pub(crate) struct WithinUndo {
    saved_env: Option<EnvVars>,
    saved_dir: SavedDir,
    handlers: Option<FrameHandle>,
}

impl WithinUndo {
    /// Undo in the reverse of install order: handlers, then dir, then env.
    pub(crate) fn apply(self, shell: &mut Shell) {
        if let Some(handle) = self.handlers {
            shell.context.handlers.remove_by_handle(handle);
        }
        if let SavedDir::Prior(saved) = self.saved_dir {
            shell.restore_cwd_override(saved);
        }
        if let Some(saved) = self.saved_env {
            shell.context.env_overrides = saved;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{RequestedTerminalAccess, RunIo, RunReport, RunRequest, RunStdin};
    use crate::protocol::{Program, Run};
    use crate::types::Mooring;
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
            fork: None,
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
        match run_source(&mut shell, r#"try { sh -c "exit 1" } { |_e| return () }"#) {
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
        panic_now_scheme,
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
        match run_source(&mut shell, "try { core-panic-now } { |_e| return () }") {
            RunReport::Static { .. } => {}
            RunReport::Ran { .. } => panic!("a panicking body must report Static"),
        }
        assert_eq!(
            shell.local.audit.active_policy(),
            None,
            "a panic through `try`'s body must not leave the trail open"
        );
    }

    /// A panic mid-run rolls back to the checkpoint `Shell::enter` takes at
    /// the top of the run: `env`, `context` (cwd included), and
    /// `last_status` all read exactly as they did before the run started,
    /// even though the panicking phrase's own `let` and `cd` ran first.
    /// Only a panic restores this way — an ordinary error leaves whatever
    /// `let`s landed before it (S12).
    #[test]
    fn panic_mid_run_restores_the_pre_run_checkpoint() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        shell.install_builtins(PANIC_BUILTINS);
        shell.set_last_status(5);
        let before_cwd = shell.cwd();
        let tmp = std::env::temp_dir().display().to_string();

        match run_source(
            &mut shell,
            &format!("let checkpoint_leak = 1\ncd '{tmp}'\ncore-panic-now"),
        ) {
            RunReport::Static { .. } => {}
            RunReport::Ran { .. } => panic!("a panicking body must report Static"),
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
        assert_eq!(
            shell.last_status, 5,
            "a panic must roll back last_status"
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
            "audit { echo one; try { echo two } { |_e| return () }; echo three }",
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
