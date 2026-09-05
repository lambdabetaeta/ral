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
    AuditIo, Break, BuiltinEntry, CallSite, CapturePolicy, CommandOrigin, Decision, Env, Mooring,
    Observation, Observed, Settled, Shell, Value, epoch_us,
};
use std::collections::BTreeMap;

/// Proof that a native body is running inside [`frame_call`]'s dynamic
/// extent — mintable only in this module, so [`BuiltinEntry::call_body`]
/// cannot be reached unframed.
pub(crate) struct Frame(());

/// Where a command was called from and when it began — the two halves of an
/// observation's stamp, paired so the dispatch site carries one local.
#[derive(Clone, Debug, Default)]
pub(crate) struct AuditStart {
    pub site: CallSite,
    pub time: i64,
}

/// Report one observation to everyone listening: the surface sink and the
/// open trail (`Audit::push` is already a no-op with no trail open; the
/// surface is asked first because projecting to a [`Value`] costs more than
/// the question).  Core does not judge what is worth hearing — the host
/// filters the rail, and `audit { }` filters the trail.
///
/// It does judge what *happened*.  A redirect onto the [discard
/// device](crate::path::ResolvedPath::is_discard) left the world as it found
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
        mooring.surface(&obs.to_value());
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

#[allow(clippy::too_many_arguments)]
fn finish_command(
    shell: &mut Shell,
    mooring: &Mooring,
    start: AuditStart,
    cmd: &str,
    origin: CommandOrigin,
    args: &[Value],
    result: &Settled<Value>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) {
    if !listening(shell, mooring) {
        return;
    }
    // Internal builtins go unrecorded: the prelude wrapper that called one is
    // the user-visible event, and it holds the dispatch register meanwhile.
    // Under one emission door this skip governs the rail too.
    if cmd.starts_with('_') {
        return;
    }
    let (status, error) = match result {
        Ok(_) => (0, None),
        Err(Break::Error(e)) => (e.exit_code(), Some(e.message.clone())),
        Err(_) => return,
    };
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(cmd.to_string());
    argv.extend(Value::render_argv(args));
    let obs = Observation::spanning(
        start.site,
        start.time,
        epoch_us(),
        shell.context.principal(),
        Observed::Command {
            argv,
            status,
            origin,
            io: AuditIo { stdout, stderr },
            error,
        },
    );
    observe_stamped(shell, mooring, obs);
}

/// Wrap a command body in the audit lifecycle: stamp the start, tee its
/// stdout and stderr through the capture, finalize the observation.
///
/// It also names the failure: a body that errored without a command yet takes
/// this dispatch's, so the innermost dispatch wins — the rule `stamp` uses for
/// a span.  That naming is not an observation, and happens whether or not
/// anyone is listening: `try`'s record needs it with no trail open.
pub(crate) fn frame_call<F>(
    cmd: &str,
    args: &[Value],
    origin: CommandOrigin,
    mooring: &Mooring,
    shell: &mut Shell,
    body: F,
) -> Settled<Value>
where
    F: FnOnce(&mut Shell, &Frame) -> Settled<Value>,
{
    let start = start(shell, mooring);
    let (mut result, stdout, stderr) =
        super::with_audit_capture(shell, |shell| body(shell, &Frame(())));
    if let Err(Break::Error(e)) = &mut result
        && e.command.is_none()
        && !cmd.starts_with('_')
    {
        e.command = Some(cmd.to_string());
    }
    finish_command(
        shell, mooring, start, cmd, origin, args, &result, stdout, stderr,
    );
    result
}

/// Run a native body inside a fresh audit frame — the only way to reach
/// [`BuiltinEntry::call_body`].  `env` is the lexical environment at the
/// call: the `Apply` frame's, or the `Exec` rule's.
pub(crate) fn run_native(
    entry: &BuiltinEntry,
    args: &[Value],
    env: &Env,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Value> {
    frame_call(
        &entry.name,
        args,
        CommandOrigin::Builtin,
        mooring,
        shell,
        |shell, frame| entry.call_body(frame, args, env, mooring, shell),
    )
}

impl BuiltinEntry {
    /// Framed public surface for hosts and tests: the body runs under its
    /// own audit frame.
    ///
    /// # Errors
    /// Propagates a `Break` raised by the body.
    pub fn run(&self, args: &[Value], env: &Env, mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        run_native(self, args, env, mooring, shell)
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
        mooring.surface(&obs.to_value());
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
