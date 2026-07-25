//! Composes command resolution with execution of the chosen arm:
//! lexical value, builtin command binding, handler, or external command.
//!
//! [`run_call`] is the runtime entry, and the only thing `call::invoke`
//! reaches in here.  Resolution is env → builtins → handlers → external;
//! the `^name` form skips env and builtins; path heads skip all three.

use crate::ir::{CommandName, CommandWord};
use crate::types::{
    Break, BuiltinEntry, HandlerArity, HandlerEntry, HandlerFrame, Map, Raw, Settled, Shell, Value,
};

use super::command::{self, CommandIdentity, EvalRedirectV};
use crate::evaluator::audit;
use crate::evaluator::redirect::with_redirects;

// ── Resolution ─────────────────────────────────────────────────────────

/// The verdict of command-name resolution: one arm per place a name
/// can live.  Consumed by the dispatcher ([`run_call`]) and by pipeline
/// staging ([`crate::runtime::pipeline::resolve`]).
pub(crate) enum Resolution {
    /// The lexical environment binds this name.
    Env(Value),
    /// The builtin command table matched this name.
    Builtin(BuiltinEntry),
    /// The handler stack matched this name.  `depth` is the count of
    /// frames from the top to (and including) the matched frame.  The
    /// entry is boxed: its arm scheme makes [`HandlerEntry`] the widest
    /// payload by far, and indirection keeps the enum's other arms cheap.
    Handler {
        entry: Box<HandlerEntry>,
        depth: usize,
    },
    /// Neither env nor the handler stack matched.  Carries the
    /// PATH-resolved [`CommandIdentity`] for the external arm.
    External(CommandIdentity),
}

/// Resolve a bare command name: env → builtins → handlers → external.
/// Pure: no admission, no audit, no mutation.
pub(crate) fn resolve(name: &str, shell: &Shell) -> Resolution {
    if let Some(value) = shell.mobile.scope.get(name) {
        return Resolution::Env(value.clone());
    }
    if let Some(entry) = shell.lookup_builtin(name) {
        return Resolution::Builtin(entry);
    }
    resolve_handler_then_external(name, shell)
}

/// Resolve a [`CommandWord`] to its [`Resolution`].
///
/// [`CommandWord::Name`] delegates to bare command lookup.  `^name`
/// skips env and builtins, but still consults handlers before
/// external commands.  Path-bearing heads skip env, builtins, and
/// handlers.
pub(crate) fn resolve_command_word(head: &CommandWord, shell: &Shell) -> Resolution {
    let name = head.name();
    match name {
        CommandName::Path(_) | CommandName::TildePath(_) => Resolution::External(
            CommandIdentity::resolve(name.clone(), &shell.mobile.context),
        ),
        CommandName::Bare(s) => match head {
            CommandWord::Name(_) => resolve(s, shell),
            CommandWord::External(_) => resolve_handler_then_external(s, shell),
        },
    }
}

/// Walk user handlers, then fall through to external.
fn resolve_handler_then_external(name: &str, shell: &Shell) -> Resolution {
    if let Some((entry, depth)) = shell.lookup_handler(name) {
        return Resolution::Handler {
            entry: Box::new(entry),
            depth,
        };
    }
    Resolution::External(CommandIdentity::resolve(
        CommandName::Bare(name.to_string()),
        &shell.mobile.context,
    ))
}

/// Resolution composed with head admission.  `Err` is the grant's
/// "denied" verdict — the head is refused before any arguments
/// evaluate.  Env, Builtin, and Handler arms pass through
/// unconditionally; grant admission is an external-command property.
///
/// The `Resolution::Env` arm additionally renews the binding-lease ledger
/// (`decisions/260629_agent-binding-reaping`) for the resolved name — a
/// dispatch-time touch, not a lookup-time one: command dispatch is already
/// heavyweight (grant resolution, audit), so this costs nothing on the
/// pure-lookup path `Env::get` stays on. See [`run_call`] for why an
/// `Env` arm reaches dispatch at all.
pub(crate) fn classify_command(head: &CommandWord, shell: &mut Shell) -> Settled<Resolution> {
    let r = resolve_command_word(head, shell);
    if let Resolution::Env(_) = &r
        && let Some(name) = head.name().bare()
    {
        shell.local.bindings.renew_one(name);
    }
    if let Resolution::External(id) = &r
        && !crate::capability::admits_head(&shell.mobile.context, id)
    {
        return Err(refuse_head(id, shell));
    }
    Ok(r)
}

/// Denial error for a head the active grant refuses, plus the
/// matching `exec/denied` audit event.
fn refuse_head(id: &CommandIdentity, shell: &mut Shell) -> Break {
    let (msg, hint) = match shell.locate_command(&id.shown) {
        Some(p) => (
            format!(
                "command '{}' denied by active grant ({})",
                id.shown,
                p.display()
            ),
            "add the command to the grant exec map to allow it",
        ),
        None => (
            format!("command '{}' not found on PATH", id.shown),
            "install the command, or add it to the grant exec map if it lives elsewhere",
        ),
    };
    let mut fields = Map::new();
    fields.insert("name".into(), Value::String(id.shown.clone()));
    audit::record_capability(shell, "exec", "denied", fields);
    shell.err_hint(msg, hint, 1).into()
}

// ── Runners ─────────────────────────────────────────────────────────────

/// Runtime entry: resolve `head` against the active grant, then run
/// the chosen arm.
///
/// Source-location bookkeeping is skipped for `_`-prefixed names —
/// the host-registered `_ed-*` REPL surface — so audit readers see
/// the user-visible caller rather than the editor builtin.
///
/// The [`Resolution::Env`] arm is defensive: the elaborator already
/// routes env-bound names through CBPV [`CompKind::App`] via the
/// `is_bound` check, so a name reaching this point should normally
/// resolve to [`Resolution::Handler`] or [`Resolution::External`].
/// When a runtime mechanism nonetheless installs a binding the
/// elaborator could not see, the value is applied under the
/// caller's redirects.
pub(crate) fn run_call(
    head: &CommandWord,
    args: &[Value],
    redirects: &[EvalRedirectV],
    shell: &mut Shell,
) -> Raw<Value> {
    if !matches!(head.name().bare(), Some(name) if name.starts_with('_')) {
        shell.run.loc.record_call_site_here();
    }

    match classify_command(head, shell)? {
        Resolution::Env(value) => with_redirects(redirects, shell, |shell| {
            crate::evaluator::apply(value, args.to_vec(), shell).map_err(Into::into)
        }),
        Resolution::Builtin(entry) => run_builtin(&entry, args, redirects, shell),
        Resolution::Handler { entry, depth } => with_redirects(redirects, shell, |shell| {
            run_handler(&entry, depth, args, shell)
        }),
        Resolution::External(id) => run_external(id, args, redirects, shell),
    }
}

/// Run a builtin command binding under the host-call envelope.
fn run_builtin(
    entry: &BuiltinEntry,
    args: &[Value],
    redirects: &[EvalRedirectV],
    shell: &mut Shell,
) -> Raw<Value> {
    let body = entry.body.clone();
    run_host_thunk(&entry.name, args, redirects, shell, move |a, s| {
        body.call(a, s)
    })
}

/// RAII guard for handler self-masking: lifts the matched frame off the
/// stack on [`Self::strip`] and re-inserts it at its original position on
/// `Drop`, panic or otherwise.
///
/// The frame is the one piece of dynamic context whose loss is permanent
/// and user-visible — a stripped alias frame dropped on an unwind is a
/// silently deleted user alias, with no save elsewhere to rebuild from.
/// A straight-line restore is skipped when the body unwinds; the guard
/// restores it at the source instead.
struct MaskedHandler<'a> {
    shell: &'a mut Shell,
    frame: Option<HandlerFrame>,
}

impl<'a> MaskedHandler<'a> {
    fn strip(shell: &'a mut Shell, depth: usize) -> Self {
        let frame = shell.mobile.context.handlers.strip_matched(depth);
        Self {
            shell,
            frame: Some(frame),
        }
    }
}

impl Drop for MaskedHandler<'_> {
    fn drop(&mut self) {
        if let Some(frame) = self.frame.take() {
            self.shell.mobile.context.handlers.restore_matched(frame);
        }
    }
}

/// Run a user handler entry.  The matched frame is lifted from the
/// stack for the dynamic extent of the body so a same-name call from
/// inside reaches the next outer match.
fn run_handler(
    entry: &HandlerEntry,
    depth: usize,
    args: &[Value],
    shell: &mut Shell,
) -> Raw<Value> {
    let thunk = entry.thunk.clone();
    let call_args = match entry.arity {
        HandlerArity::CatchAll => vec![
            Value::String(entry.name.clone().into_owned()),
            Value::list(args.to_vec()),
        ],
        HandlerArity::Unary => vec![Value::list(args.to_vec())],
    };
    let masked = MaskedHandler::strip(shell, depth);
    let result = crate::evaluator::apply(thunk, call_args, masked.shell);
    drop(masked);
    result.map_err(Into::into)
}

fn run_host_thunk(
    name: &str,
    args: &[Value],
    redirects: &[EvalRedirectV],
    shell: &mut Shell,
    f: impl FnOnce(&[Value], &mut Shell) -> Settled<Value>,
) -> Raw<Value> {
    audit::frame_call(name, args, shell, |shell| {
        with_redirects(redirects, shell, |shell| f(args, shell).map_err(Into::into))
    })
}

/// Runs an external command through the OS layer.
fn run_external(
    id: CommandIdentity,
    args: &[Value],
    redirects: &[EvalRedirectV],
    shell: &mut Shell,
) -> Raw<Value> {
    let stdin_guard = command::install_stdin_redirect(redirects, shell)?;
    let shown = id.shown.clone();
    let result = audit::frame_call(&shown, args, shell, move |shell| {
        command::run(&id, args, redirects, shell)
    });
    stdin_guard.restore(shell);
    result
}
