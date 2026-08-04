//! The fan-out door: one observation, broadcast to whoever is listening.
//!
//! [`observe_stamped`] reports to both consumers and judges neither: the host
//! decides what the rail draws, and `audit { }` decides what the trail keeps.
//! Every evaluator door that settles a fact routes through it.  Three sites
//! stand outside, each reaching only one consumer: `capability::enforce` and
//! `Shell::audit_deputy_prefixes` have no [`Mooring`], and exarch's host
//! doors have no trail.
//!
//! `within`, `grant`, `guard`, `try`, and `audit` are all collection
//! boundaries, not observations: none of them constructs one.  `try`/`audit`
//! force collection on regardless of the surrounding state via
//! [`forced_subtree`], which marks the trail's length before `body` runs and
//! reads back everything pushed after that mark.
//!
//! With nobody listening the recorders are no-ops, so the dispatcher can call
//! them unconditionally; [`forced_subtree`] is the exception, since `try` and
//! `audit` force collection whether or not audit is on.

use crate::types::{
    AuditIo, BodyResult, Break, BuiltinEntry, CallSite, CapturePolicy, CommandOrigin, Control,
    Decision, Escape, Map, Mooring, Observation, Observed, Raw, STDERR_CAP_BYTES, Settled, Shell,
    Value, epoch_us, split,
};

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
/// open trail (`Mooring::surface` and `Audit::push` are each already a no-op
/// when their consumer is absent).  Core does not judge what is worth
/// hearing — the host filters the rail, and `audit { }` filters the trail.
pub(crate) fn observe_stamped(shell: &mut Shell, mooring: &Mooring, obs: Observation) {
    mooring.surface(&obs.to_value());
    shell.local.audit.push(obs);
}

/// An instantaneous door: stamped now, at the current dispatch site.
pub(crate) fn observe(shell: &mut Shell, mooring: &Mooring, what: Observed) {
    let obs = Observation::instant(shell.call_site(), shell.mobile.context.principal(), what);
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
/// host on the other end of the sink.
fn listening(shell: &Shell, mooring: &Mooring) -> bool {
    shell.local.audit.active() || mooring.has_surface()
}

fn cap_stderr(buf: &mut Vec<u8>) {
    if buf.len() > STDERR_CAP_BYTES {
        buf.truncate(STDERR_CAP_BYTES);
    }
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
    mut stderr: Vec<u8>,
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
    let (status, value, error) = match result {
        Ok(v) => (shell.mobile.control.last_status, v.clone(), None),
        Err(Break::Error(e)) => (e.exit_code(), Value::Unit, Some(e.message.clone())),
        Err(_) => return,
    };
    if shell.local.audit.captures_bytes() {
        cap_stderr(&mut stderr);
    }
    let mut argv = Vec::with_capacity(args.len() + 1);
    argv.push(cmd.to_string());
    argv.extend(args.iter().map(std::string::ToString::to_string));
    let obs = Observation::spanning(
        start.site,
        start.time,
        epoch_us(),
        shell.mobile.context.principal(),
        Observed::Command {
            argv,
            status,
            origin,
            io: AuditIo { stdout, stderr },
            error,
            value,
        },
    );
    observe_stamped(shell, mooring, obs);
}

/// Wrap a command body in the audit lifecycle: stamp the start, tee its
/// stdout and stderr through the capture, finalize the observation.
pub(crate) fn frame_call<F>(
    cmd: &str,
    args: &[Value],
    origin: CommandOrigin,
    mooring: &Mooring,
    shell: &mut Shell,
    body: F,
) -> Raw<Value>
where
    F: FnOnce(&mut Shell, &Frame) -> Raw<Value>,
{
    let start = start(shell, mooring);
    let (result, stdout, stderr) = super::with_audit_capture(shell, |shell| {
        body(shell, &Frame(()))
    })
    .map_err(|e| Control::Break(Break::Error(shell.err(format!("audit capture: {e}"), 1))))?;
    finish_command(
        shell,
        mooring,
        start,
        cmd,
        origin,
        args,
        &outcome_for_audit(&result),
        stdout,
        stderr,
    );
    result
}

/// Run a native body inside a fresh audit frame — the only way to reach
/// [`BuiltinEntry::call_body`].
pub(crate) fn run_native(
    entry: &BuiltinEntry,
    args: &[Value],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    frame_call(
        &entry.name,
        args,
        CommandOrigin::Builtin,
        mooring,
        shell,
        |shell, frame| {
            entry
                .call_body(frame, args, mooring, shell)
                .map_err(Control::from)
        },
    )
}

impl BuiltinEntry {
    /// Framed public surface for hosts and tests: the body runs under its
    /// own audit frame.
    ///
    /// # Errors
    /// Propagates a `Break` raised by the body.
    pub fn run(&self, args: &[Value], mooring: &Mooring, shell: &mut Shell) -> Settled<Value> {
        super::absorb_tail(run_native(self, args, mooring, shell), mooring, shell)
    }
}

/// A tail call does not survive a command boundary, so it records as `Unit`.
fn outcome_for_audit(raw: &Raw<Value>) -> Settled<Value> {
    match raw {
        Ok(v) => Ok(v.clone()),
        Err(Control::Tail(_)) => Ok(Value::Unit),
        Err(Control::Break(b)) => Err(b.clone()),
    }
}

/// Record a denied capability check.  The trail wants one only when some
/// enclosing grants layer asked for `audit: true`, but the rail always does,
/// so the gate sits on the push rather than on the observation.
pub(crate) fn record_capability(shell: &mut Shell, mooring: &Mooring, resource: &str, fields: Map) {
    let obs = Observation::instant(
        shell.call_site(),
        shell.mobile.context.principal(),
        Observed::Capability {
            resource: resource.to_string(),
            decision: Decision::Denied,
            fields,
        },
    );
    mooring.surface(&obs.to_value());
    if shell.should_audit_capabilities() {
        shell.local.audit.push(obs);
    }
}

/// Force collection on for `body` and return the observations it produced;
/// used by `try` (to name the failing command) and `audit` (to return the
/// subtree).  The observations stay in the trail, flat among whatever else it
/// collects.
pub(crate) fn forced_subtree(
    shell: &mut Shell,
    capture: CapturePolicy,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> Result<(BodyResult, Vec<Observation>), Escape> {
    let (mark, settled) = with_capture_policy(shell, capture, |shell| {
        let mark = shell.local.audit.force_open();
        (mark, body(shell))
    });
    let body_result = split(settled)?;
    Ok((body_result, shell.local.audit.since(mark)))
}

/// Install `policy` for `f`, restoring the previous one after.  Capture is
/// monotonic: an inner `try`'s `Off` must not silence an outer `audit`'s
/// `Bytes`, hence the merge rather than a plain swap.
fn with_capture_policy<R>(
    shell: &mut Shell,
    policy: CapturePolicy,
    f: impl FnOnce(&mut Shell) -> R,
) -> R {
    let saved = shell.local.audit.capture_policy();
    let merged = match (saved, policy) {
        (CapturePolicy::Bytes, _) | (_, CapturePolicy::Bytes) => CapturePolicy::Bytes,
        _ => CapturePolicy::Off,
    };
    shell.local.audit.set_capture(merged);
    let r = f(shell);
    shell.local.audit.set_capture(saved);
    r
}
