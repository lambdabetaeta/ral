//! The fan-out door: one observation, broadcast to whoever is listening.
//!
//! [`observe_stamped`] reports to both consumers and judges neither's
//! interest: the host decides what the rail draws, and `audit { }` decides
//! what the trail keeps.  It judges only whether anything happened at all —
//! a redirect onto the discard device is not a write.
//! Every evaluator door that settles a fact routes through it.  Two sites
//! stand outside, each reaching only one consumer: `capability::enforce` and
//! `Shell::audit_deputy_prefixes` have no [`Mooring`].
//!
//! A builtin application is not a fact.  The pure fragment performs no
//! effect, and a builtin that reaches the world does so through a door — a
//! redirect, `atomic_write`, `spawn_child` — which observes it there, typed as
//! the effect it is.  Only an external's dispatch is itself a door
//! ([`call_external`]); a native runs in a frame that stamps nothing and tees
//! nothing ([`call_native`]).
//!
//! `within`, `grant`, `guard`, `try`, and `audit` are all collection
//! boundaries, not observations: none of them constructs one.  Only `audit`
//! collects — `evaluator::machine`'s `Audit` arm opens a trail scope before
//! its body runs and closes it after, draining and closing the trail if the
//! body is the one that opened it, reading a suffix and leaving it open
//! otherwise (`Audit::open` / `Audit::close`, `core/src/types/audit.rs`).
//! The opener owns closing on every exit, panics included.  `try` names the
//! failure it catches from the error itself, so it forces nothing on.
//!
//! With nobody listening the recorders are no-ops, so the dispatcher can call
//! them unconditionally.

use crate::types::{
    AuditIo, Break, BuiltinEntry, CallSite, CapturePolicy, CommandOrigin, Decision, Mooring,
    Observation, Observed, Settled, Shell, Value, epoch_us,
};
use std::collections::BTreeMap;

/// Proof that a native body is running inside [`call_native`]'s dynamic
/// extent — mintable only in this module, so [`BuiltinEntry::call_body`]
/// cannot be reached unframed.
pub(crate) struct Frame(());

/// Where a command was called from and when it began — the two halves of an
/// observation's stamp, paired so the dispatch site carries one local.
#[derive(Clone, Debug, Default)]
pub(crate) struct AuditStart {
    pub site: Option<CallSite>,
    pub(crate) time: i64,
}

/// Report one observation to everyone listening: the surface sink and the
/// open trail (`Audit::push` is already a no-op with no trail open; the
/// surface is asked first because projecting to a [`Value`] costs more than
/// the question).  Core does not judge what is worth hearing — the host
/// filters the rail, and `audit { }` filters the trail.
///
/// It does judge what *happened*.  A redirect onto the [discard
/// device](crate::path::LexicalPath::is_discard) left the world as it found
/// it, so there is nothing to report: no card, no rail barrier, and no line
/// in an agent's trail claiming it wrote a file.  The same predicate
/// `capability::check_fs_op` asks, of the same resolver — what is not an
/// access is not a mutation either — asked here, at the one door every seam
/// passes through, rather than at each seam that mints the fact.
pub(crate) fn observe_stamped(shell: &mut Shell, mooring: &Mooring, obs: Observation) {
    if let Observed::Write { path, .. } = &obs.what
        && shell.resolve(path).is_discard()
    {
        return;
    }
    if mooring.has_surface() {
        mooring.surface_data(&obs.to_surface());
    }
    shell.local.audit.push(obs);
}

/// An instantaneous door: stamped now, at the current dispatch site.
pub(crate) fn observe(shell: &mut Shell, mooring: &Mooring, what: Observed) {
    if !listening(shell, mooring) {
        return;
    }
    let obs = Observation::instant(shell.call_site(), shell.context.principal(), what);
    observe_stamped(shell, mooring, obs);
}

/// Open one command's audit stamp.  With nobody listening — no trail open and
/// no sink installed — the stamp is empty and costs neither the `script`
/// clone nor the `epoch_us` syscall; should the command's own body then open
/// a trail, the observation carries that empty stamp rather than a late one.
pub(crate) fn start(shell: &Shell, mooring: &Mooring) -> AuditStart {
    if !listening(shell, mooring) {
        return AuditStart::default();
    }
    AuditStart {
        site: shell.call_site(),
        time: epoch_us(),
    }
}

/// Whether an observation would reach anyone: a trail collecting it, or a
/// host on the other end of the sink.  Doors whose *facts* cost something to
/// gather — the write door's before/after snapshots — ask this before
/// gathering them.
pub(crate) fn listening(shell: &Shell, mooring: &Mooring) -> bool {
    shell.local.audit.active() || mooring.has_surface()
}

/// The one place an [`Observed::Command`] is built, so every door that mints
/// one spells its argv the same way: the shown name, then its arguments.
pub(crate) fn command_fact(
    shown: &str,
    args: impl IntoIterator<Item = String>,
    status: i32,
    origin: CommandOrigin,
    io: AuditIo,
    error: Option<String>,
) -> Observed {
    Observed::Command {
        argv: std::iter::once(shown.to_string()).chain(args).collect(),
        status,
        origin,
        io,
        error,
    }
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
    if !listening(shell, mooring) {
        return;
    }
    let (status, error) = match result {
        Ok(_) => (0, None),
        Err(Break::Error(e)) => (e.exit_code(), Some(e.message.clone())),
        Err(_) => return,
    };
    let obs = Observation::spanning(
        start.site,
        start.time,
        epoch_us(),
        shell.context.principal(),
        command_fact(
            shown,
            Value::render_argv(args),
            status,
            CommandOrigin::External,
            io,
            error,
        ),
    );
    observe_stamped(shell, mooring, obs);
}

/// Stamp `cmd` on an error that has no command yet, so the innermost dispatch
/// wins — the rule `stamp` uses for a span.  Not an observation, and it
/// happens whether or not anyone is listening: `try`'s record needs it with
/// no trail open.  An `_`-prefixed internal name defers to the public wrapper
/// that called it.
fn name_failure(cmd: &str, result: &mut Settled<Value>) {
    if let Err(Break::Error(e)) = result
        && e.command.is_none()
        && !cmd.starts_with('_')
    {
        e.command = Some(cmd.into());
    }
}

/// Run a native body in its call frame, which only names a failure: a
/// builtin application is not an observation (see the module doc).
pub(crate) fn call_native<F>(cmd: &str, shell: &mut Shell, body: F) -> Settled<Value>
where
    F: FnOnce(&mut Shell, &Frame) -> Settled<Value>,
{
    let mut result = body(shell, &Frame(()));
    name_failure(cmd, &mut result);
    result
}

/// An external's door: stamp the start, tee its stdout and stderr through
/// the capture, name a failure, and settle the one [`Observed::Command`].
pub(crate) fn call_external<F>(
    shown: &str,
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
    body: F,
) -> Settled<Value>
where
    F: FnOnce(&mut Shell) -> Settled<Value>,
{
    let start = start(shell, mooring);
    let (mut result, stdout, stderr) = super::with_audit_capture(shell, body);
    name_failure(shown, &mut result);
    let io = AuditIo { stdout, stderr };
    finish_command(shell, mooring, start, shown, args, &result, io);
    result
}

/// Run a native body inside a fresh call frame — the only way to reach
/// [`BuiltinEntry::call_body`].
pub(crate) fn run_native(
    entry: &BuiltinEntry,
    args: &[Value],
    site: Option<&std::sync::Arc<crate::types::Site>>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    call_native(&entry.name, shell, |shell, frame| {
        entry.call_body(frame, args, site, mooring, shell)
    })
}

impl BuiltinEntry {
    /// Framed public surface for hosts and tests: the body runs under its
    /// own call frame.
    ///
    /// # Errors
    /// Propagates a `Break` raised by the body.
    pub fn run(&self, args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        run_native(self, args, None, mooring, shell)
    }
}

/// Record a denied capability check.  Both consumers want one, but each on
/// its own terms, so neither the projection nor the push is unconditional:
/// the rail hears a refusal whenever a host is listening, the trail whenever
/// one is open.
pub(crate) fn record_capability(
    shell: &mut Shell,
    mooring: &Mooring,
    resource: &str,
    fields: BTreeMap<String, String>,
) {
    let obs = Observation::instant(
        shell.call_site(),
        shell.context.principal(),
        Observed::Capability {
            resource: resource.to_string(),
            decision: Decision::Denied,
            fields,
        },
    );
    if mooring.has_surface() {
        mooring.surface_data(&obs.to_surface());
    }
    if shell.local.audit.active() {
        shell.local.audit.push(obs);
    }
}

/// Capture is monotonic: a nested request for `Off` must not silence an
/// enclosing `audit`'s `Bytes`, hence a merge rather than a plain swap.  The
/// run door composes the same way when a dispatch's own `Run.trail` opens
/// onto a session already under `--audit`.
pub(crate) fn merge_capture(saved: CapturePolicy, requested: CapturePolicy) -> CapturePolicy {
    match (saved, requested) {
        (CapturePolicy::Bytes, _) | (_, CapturePolicy::Bytes) => CapturePolicy::Bytes,
        _ => CapturePolicy::Off,
    }
}
