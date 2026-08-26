//! Command dispatch: resolve a head, then run the arm resolution names.
//!
//! Order is env → handlers → external; `^name` skips env, so it skips every
//! value builtin (a native is an env hit), but still consults handlers —
//! run frames and the base layer alike; a path head skips handlers too.
//! [`run_call`] is the entry from `evaluator::call`; pipeline staging and
//! `command::detach` reach `resolve_command_word` and `run_handler` directly.

use crate::ir::{CommandName, CommandWord};
use crate::source::Span;
use crate::types::{
    Break, BuiltinEntry, CommandOrigin, Control, Env, HandlerArity, HandlerEntry, HandlerFrame,
    HandlerLookup, Map, Mooring, Raw, Settled, Shell, Value,
};

use super::command::{self, CommandIdentity, EvalRedirectV};
use crate::evaluator::audit;
use crate::evaluator::redirect::with_redirects;

// ── Resolution ─────────────────────────────────────────────────────────

/// One arm per place a command name can live.
pub(crate) enum Resolution {
    Env(Value),
    /// `depth` counts handler frames from the top down to the matched one
    /// inclusive — what `strip_matched` takes.  Boxed: [`HandlerEntry`] would
    /// otherwise set the size of every arm.
    Handler {
        entry: Box<HandlerEntry>,
        depth: usize,
    },
    /// A base handler frame — a manifest row from the argv half.
    Base(BuiltinEntry),
    External(CommandIdentity),
}

/// Bare-name lookup: env → handlers → external.  A head like `f x` is a
/// lexical name, so `env` — not `shell.mobile.scope` — is what a bare name
/// resolves through.  No admission check and no audit — those belong to
/// [`classify_command`].
pub(crate) fn resolve(name: &str, env: &Env, shell: &Shell) -> Resolution {
    if let Some(value) = env.get(name) {
        return Resolution::Env(value.clone());
    }
    resolve_handler_then_external(name, shell)
}

/// Resolve a [`CommandWord`]: `^name` skips env but still consults handlers;
/// a path-bearing head skips handlers too.
pub(crate) fn resolve_command_word(head: &CommandWord, env: &Env, shell: &Shell) -> Resolution {
    let name = head.name();
    match name {
        CommandName::Path(_) | CommandName::TildePath(_) => Resolution::External(
            CommandIdentity::resolve(name.clone(), &shell.mobile.context),
        ),
        CommandName::Bare(s) => match head {
            CommandWord::Name(_) => resolve(s, env, shell),
            CommandWord::External(_) => resolve_handler_then_external(s, shell),
        },
    }
}

fn resolve_handler_then_external(name: &str, shell: &Shell) -> Resolution {
    match shell.lookup_handler(name) {
        Some(HandlerLookup::Frame(entry, depth)) => Resolution::Handler { entry, depth },
        Some(HandlerLookup::Base(entry)) => Resolution::Base(entry),
        None => Resolution::External(CommandIdentity::resolve(
            CommandName::Bare(name.to_string()),
            &shell.mobile.context,
        )),
    }
}

/// Resolution plus head admission: `Err` is the grant refusing the head before
/// any argument evaluates.  Grants govern exec alone, so the other arms pass
/// unconditionally.  An `Env` hit also renews that name's binding lease, a cost
/// dispatch can carry and the pure `Env::get` lookup cannot.
pub(crate) fn classify_command(
    head: &CommandWord,
    env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Resolution> {
    let r = resolve_command_word(head, env, shell);
    if let Resolution::Env(_) = &r
        && let Some(name) = head.name().bare()
    {
        shell.local.bindings.renew_one(name);
    }
    if let Resolution::External(id) = &r
        && !crate::capability::admits_head(&shell.mobile.context, id)
    {
        return Err(refuse_head(id, mooring, shell));
    }
    Ok(r)
}

/// Denial for a head the grant refuses.  One absent from PATH is reported as
/// missing instead, so nobody hunts for a grant entry to fix.
fn refuse_head(id: &CommandIdentity, mooring: &Mooring, shell: &mut Shell) -> Break {
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
    audit::record_capability(shell, mooring, "exec", fields);
    shell.err_hint(msg, hint, 1).into()
}

// ── Runners ─────────────────────────────────────────────────────────────

/// Runtime entry: admit `head`, then run the arm.
///
/// `span` becomes the run's call site, but not under `_`-prefixed names (the
/// host-registered `_ed-*` REPL surface), so the register keeps naming the
/// user's call rather than the wrapper's.  The [`Resolution::Env`] arm is
/// every native head's path — natives are invisible to the elaborator's
/// `is_bound`, so their heads stay commands — plus any binding installed
/// after elaboration.
pub(crate) fn run_call(
    head: &CommandWord,
    args: &[Value],
    redirects: &[EvalRedirectV],
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    if !matches!(head.name().bare(), Some(name) if name.starts_with('_')) {
        shell.local.audit.call_site = span;
    }
    let env = shell.mobile.scope.clone();

    match classify_command(head, &env, mooring, shell)? {
        // W2g: the machine's `Exec` rule runs this arm itself (§2.2).
        Resolution::Env(value) => with_redirects(redirects, mooring, shell, |shell| {
            crate::evaluator::apply(value, args.to_vec(), mooring, shell).map_err(Into::into)
        }),
        // W2g: `Unmask` is the frame; `run_handler`/`MaskedHandler` go.
        Resolution::Handler { entry, depth } => {
            with_redirects(redirects, mooring, shell, |shell| {
                run_handler(&entry, depth, args, mooring, shell).map_err(Into::into)
            })
        }
        Resolution::Base(entry) => run_base_frame(&entry, args, redirects, &env, mooring, shell)
            .map_err(Control::from),
        Resolution::External(id) => run_external(id, args, redirects, &env, mooring, shell)
            .map_err(Control::from),
    }
}

/// Run a base handler frame directly with the argv slice — no adapter, no
/// masking: a native body never self-forwards.
///
/// The values arrive unrendered, unlike a ral arm's ([`run_handler`]): a native
/// body renders what it writes and vets what it launches, and the exec
/// boundary's refusal is a judgement on the value's shape.  `env` is the
/// lexical environment at the call — the native sees it (§4).
pub(crate) fn run_base_frame(
    entry: &BuiltinEntry,
    args: &[Value],
    redirects: &[EvalRedirectV],
    env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    run_host_thunk(
        &entry.name,
        args,
        redirects,
        mooring,
        shell,
        |a, s, frame| entry.call_body(frame, a, env, mooring, s),
    )
}

// W2g: `Unmask` is the frame; this and `run_handler` go.
/// Lifts the matched frame off the handler stack on [`Self::strip`] and puts it
/// back at its position on `Drop`, unwinds included: the frame is held nowhere
/// else, so losing it to a panic silently deletes a user's alias.
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

// W2g: `Unmask` is the frame; this and `MaskedHandler` go.
/// Run a user handler.  The matched frame is masked for the body's dynamic
/// extent, so a same-name call from inside reaches the next outer match.
///
/// The arm receives the argv, and an argv is a list of strings: every element
/// arrives rendered, by the same total text conversion a base frame and the
/// exec boundary apply.  That is what makes an arm interchangeable with the
/// command it stands for — it consumes what an exec call would.
pub(crate) fn run_handler(
    entry: &HandlerEntry,
    depth: usize,
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let thunk = entry.thunk.clone();
    let argv = Value::list(
        Value::render_argv(args)
            .into_iter()
            .map(Value::String)
            .collect(),
    );
    let call_args = match entry.arity {
        HandlerArity::CatchAll => {
            vec![Value::String(entry.name.clone().into_owned()), argv]
        }
        HandlerArity::Unary => vec![argv],
    };
    let masked = MaskedHandler::strip(shell, depth);
    let result = crate::evaluator::apply(thunk, call_args, mooring, masked.shell);
    drop(masked);
    result
}

fn run_host_thunk(
    name: &str,
    args: &[Value],
    redirects: &[EvalRedirectV],
    mooring: &Mooring,
    shell: &mut Shell,
    f: impl FnOnce(&[Value], &mut Shell, &audit::Frame) -> Settled<Value>,
) -> Settled<Value> {
    audit::frame_call(
        name,
        args,
        CommandOrigin::Builtin,
        mooring,
        shell,
        |shell, frame| {
            with_redirects(redirects, mooring, shell, |shell| {
                f(args, shell, frame).map_err(Into::into)
            })
            .map_err(|c| match c {
                Control::Break(b) => b,
                // A host thunk's body is always `Settled` before it meets
                // `with_redirects`, so no tail call ever reaches here.
                Control::Tail(_) => unreachable!("a host thunk body never emits a tail call"),
            })
        },
    )
}

/// Run an external command.  No `with_redirects` frame, unlike the host arms:
/// the child's redirects are wired onto its own fds, and a `<file` stdin is
/// parked in `shell.io.stdin` for the spawn to collect.  `env` is unused —
/// an external command reads no lexical scope — but carried for the same
/// shape as [`run_base_frame`], since the `Exec` rule reaches both arms alike.
fn run_external(
    id: CommandIdentity,
    args: &[Value],
    redirects: &[EvalRedirectV],
    _env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    let stdin_guard = command::install_stdin_redirect(redirects, mooring, shell)?;
    let shown = id.shown.clone();
    let result = audit::frame_call(
        &shown,
        args,
        CommandOrigin::External,
        mooring,
        shell,
        move |shell, _| command::run(&id, args, redirects, mooring, shell),
    );
    stdin_guard.restore(shell);
    result
}
