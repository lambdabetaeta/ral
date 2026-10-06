//! Command dispatch: resolve a head, then run the arm.
//!
//! Order is env → handlers → external for a bare name; `^name` and a path
//! head are the external directly, consulting neither the env nor the stack.
//! `evaluator::machine`'s `Exec` rule is the entry that classifies and runs
//! every arm; pipeline staging reaches
//! `resolve_command_word`/`classify_command` directly.

use crate::capability::Program;
use crate::guard::{Denial, deny_head};
use crate::ir::{CommandName, CommandWord};
use crate::types::{
    AuditIo, AuditStart, Break, BuiltinEntry, CommandOrigin, Env, HandlerEntry, HandlerLookup,
    Mooring, Observation, Observed, Settled, Shell, Value, epoch_us, name_failure,
};

use super::capture::with_audit_capture;
use super::command::{self, Head};
use super::redirect::scope::with_redirects;
use crate::ir::Redirects;
use crate::source::Span;

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
    External(Head),
}

/// Bare-name lookup: env → handlers → external.  A head like `f x` is a
/// lexical name, so `env` — not `shell.env` — is what a bare name
/// resolves through.  No admission check and no audit — those belong to
/// [`classify_command`].
pub(crate) fn resolve(name: &str, env: &Env, shell: &Shell) -> Resolution {
    if let Some(value) = crate::types::lookup(name, env, &shell.sig) {
        return Resolution::Env(value.clone());
    }
    match shell.lookup_handler(name) {
        Some(HandlerLookup::Frame(entry, depth)) => Resolution::Handler { entry, depth },
        Some(HandlerLookup::Base(entry)) => Resolution::Base(entry),
        None => Resolution::External(Head::resolve(
            &CommandName::Bare(name.into()),
            &shell.context,
        )),
    }
}

/// Resolve a [`CommandWord`]: a bare name goes through [`resolve`]; `^name`
/// and a path head are the external directly, consulting neither the env nor
/// the handler stack.
pub(crate) fn resolve_command_word(head: &CommandWord, env: &Env, shell: &Shell) -> Resolution {
    match head {
        CommandWord::Name(CommandName::Bare(s)) => resolve(s, env, shell),
        CommandWord::External(name) | CommandWord::Name(name) => {
            Resolution::External(Head::resolve(name, &shell.context))
        }
    }
}

/// Resolution plus head admission: `Err` is the grant refusing the head before
/// any argument evaluates.  Grants govern exec alone, so the other arms pass
/// unconditionally.
pub(crate) fn classify_command(
    head: &CommandWord,
    env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Resolution> {
    let r = resolve_command_word(head, env, shell);
    if let Resolution::External(head) = &r
        && let Ok(program) = &head.program
        && !shell.context.grants.admits(program)
    {
        return Err(refuse_head(&head.shown, program, mooring, shell));
    }
    Ok(r)
}

/// Denial for a head whose program the grant refuses.
fn refuse_head(shown: &str, program: &Program, mooring: &Mooring, shell: &mut Shell) -> Break {
    let Denial { check, error } = deny_head(&shell.context.grants, shown, program);
    shell.record_check(Some(mooring), check);
    error.into()
}

// ── Runners ─────────────────────────────────────────────────────────────

/// Run a base handler frame directly with the argv slice — no adapter, no
/// masking: a native body never self-forwards.
///
/// The values arrive unrendered, unlike a ral arm's (`render_handler_args`):
/// a native body renders what it writes and vets what it launches, and the
/// exec boundary's refusal is a judgement on the value's shape.
pub(crate) fn run_base_frame(
    entry: &BuiltinEntry,
    args: &[Value],
    redirects: &Redirects<String>,
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    entry.framed(shell, |shell, frame| {
        with_redirects(redirects, span, mooring, shell, |shell| {
            entry.call_body(frame, args, None, mooring, shell)
        })
    })
}

/// An external's door, with its redirects installed inside it.
pub(crate) fn run_external(
    head: &Head,
    args: &[Value],
    redirects: &Redirects<String>,
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    call_external(&head.shown, args, mooring, shell, |shell| {
        with_redirects(redirects, span, mooring, shell, |shell| {
            command::run(head, args, mooring, shell)
        })
    })
}

/// An external's door: stamp the start, tee its stdout and stderr through
/// the capture, name a failure, and settle the one [`Observed::Command`].
fn call_external(
    shown: &str,
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> Settled<Value> {
    let start = shell.audit_start(mooring);
    let (mut result, stdout, stderr) = with_audit_capture(shell, body);
    name_failure(shown, &mut result);
    let io = AuditIo { stdout, stderr };
    finish_command(shell, mooring, start, shown, args, &result, io);
    result
}

fn finish_command(
    shell: &mut Shell,
    mooring: &Mooring,
    start: AuditStart,
    shown: &str,
    args: &[Value],
    result: &Settled<Value>,
    io: AuditIo,
) {
    if !shell.listening(mooring) {
        return;
    }
    let (status, error) = match result {
        Ok(_) => (0, None),
        Err(Break::Error(e)) => (e.code(), Some(e.message.clone())),
        Err(_) => return,
    };
    let obs = Observation::spanning(
        start.site,
        start.time,
        epoch_us(),
        shell.context.principal(),
        Observed::command(
            shown,
            Value::render_argv(args),
            status,
            CommandOrigin::External,
            io,
            error,
        ),
    );
    shell.observe_stamped(Some(mooring), obs);
}
