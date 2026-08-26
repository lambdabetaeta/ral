//! Command dispatch: resolve a head, then run the arm.
//!
//! Order is env → handlers → external; `^name` skips env, so it skips every
//! value builtin (a native is an env hit), but still consults handlers —
//! run frames and the base layer alike; a path head skips handlers too.
//! `evaluator::machine`'s `Exec` rule is the entry that classifies and runs
//! every arm; pipeline staging and `command::detach` reach
//! `resolve_command_word`/`classify_command` and `machine::apply_handler`
//! directly.

use crate::ir::{CommandName, CommandWord};
use crate::types::{
    Break, BuiltinEntry, CommandOrigin, Env, HandlerEntry, HandlerLookup, Map, Mooring, Settled,
    Shell, Value,
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
/// lexical name, so `env` — not `shell.env` — is what a bare name
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
        CommandName::Path(_) | CommandName::TildePath(_) => {
            Resolution::External(CommandIdentity::resolve(name.clone(), &shell.context))
        }
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
            &shell.context,
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
        && !crate::capability::admits_head(&shell.context, id)
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

/// Run a base handler frame directly with the argv slice — no adapter, no
/// masking: a native body never self-forwards.
///
/// The values arrive unrendered, unlike a ral arm's (`machine::apply_handler`):
/// a native body renders what it writes and vets what it launches, and the
/// exec boundary's refusal is a judgement on the value's shape.  `env` is the
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
        |shell, frame| with_redirects(redirects, mooring, shell, |shell| f(args, shell, frame)),
    )
}

/// Run an external command.  No `with_redirects` frame, unlike the host arms:
/// the child's redirects are wired onto its own fds, and a `<file` stdin is
/// parked in `shell.io.stdin` for the spawn to collect.  `env` is unused —
/// an external command reads no lexical scope — but carried for the same
/// shape as [`run_base_frame`], since the `Exec` rule reaches both arms alike.
pub(crate) fn run_external(
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
