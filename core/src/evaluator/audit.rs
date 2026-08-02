//! Audit recording: only two kinds of node are ever built — a real command
//! (builtin or external, via `frame_call`) and a capability check (via
//! `record_capability`).
//!
//! `within`, `grant`, `guard`, `try`, and `audit` are all collection
//! boundaries, not tree nodes: none of them constructs an `ExecNode`.
//! `try`/`audit` force collection on regardless of the surrounding state via
//! [`forced_subtree`], which never isolates their body's nodes from the
//! trail — it just marks where the trail was and reads back what got added.
//!
//! With `shell.local.audit.active()` false the recorders are no-ops, so the
//! dispatcher can call them unconditionally; [`forced_subtree`] is the
//! exception, since `try` and `audit` force collection whether or not audit
//! is on.

use crate::types::{
    AuditIo, AuditTime, BodyResult, Break, BuiltinEntry, CallSite, CapturePolicy, Control, Escape,
    ExecNode, Map, Mooring, NodeOutcome, Raw, STDERR_CAP_BYTES, Settled, Shell, Value, epoch_us,
    split,
};

/// Proof that a native body is running inside [`frame_call`]'s dynamic
/// extent — mintable only in this module, so [`BuiltinEntry::call_body`]
/// cannot be reached unframed.
pub(crate) struct Frame(());

/// Where a command was called from and when it began — the two halves of a
/// node's stamp, paired so the dispatch site carries one local.
#[derive(Clone, Debug, Default)]
pub(crate) struct AuditStart {
    pub site: CallSite,
    pub time: i64,
}

/// Open one command's audit record.  An inactive audit gets an empty stamp
/// and pays for neither the `script` clone nor the `epoch_us` syscall: the
/// matching `finish_command` will not read it, and audit cannot switch on
/// mid-command, since every scope that enables it also restores on exit.
pub(crate) fn start(shell: &Shell) -> AuditStart {
    if !shell.local.audit.active() {
        return AuditStart::default();
    }
    AuditStart {
        site: shell.call_site(),
        time: epoch_us(),
    }
}

fn cap_stderr(buf: &mut Vec<u8>) {
    if buf.len() > STDERR_CAP_BYTES {
        buf.truncate(STDERR_CAP_BYTES);
    }
}

fn finish_command(
    shell: &mut Shell,
    start: AuditStart,
    cmd: &str,
    args: &[Value],
    result: &Settled<Value>,
    stdout: Vec<u8>,
    mut stderr: Vec<u8>,
) {
    if !shell.local.audit.active() {
        return;
    }
    // Internal builtins go unrecorded: the prelude wrapper that called one is
    // the user-visible event, and it holds the dispatch register meanwhile.
    if cmd.starts_with('_') {
        return;
    }
    let outcome = match result {
        Ok(v) => NodeOutcome::of_value(shell.mobile.control.last_status, v.clone()),
        Err(Break::Error(e)) => NodeOutcome::of_error(e),
        Err(_) => return,
    };
    if shell.local.audit.captures_bytes() {
        cap_stderr(&mut stderr);
    }
    let arg_strs: Vec<String> = args.iter().map(std::string::ToString::to_string).collect();
    let node = ExecNode::command(
        cmd,
        arg_strs,
        outcome.status,
        start.site,
        AuditIo { stdout, stderr },
        outcome.error,
        outcome.value,
        Vec::new(),
        AuditTime {
            start: start.time,
            end: epoch_us(),
        },
        shell.mobile.context.principal(),
    );
    shell.local.audit.push(node);
}

/// Wrap a command body in the audit lifecycle: stamp the start, tee its
/// stdout and stderr through the capture, finalize the node.
pub(crate) fn frame_call<F>(cmd: &str, args: &[Value], shell: &mut Shell, body: F) -> Raw<Value>
where
    F: FnOnce(&mut Shell, &Frame) -> Raw<Value>,
{
    let start = start(shell);
    let (result, stdout, stderr) = super::with_audit_capture(shell, |shell| {
        body(shell, &Frame(()))
    })
    .map_err(|e| Control::Break(Break::Error(shell.err(format!("audit capture: {e}"), 1))))?;
    if shell.local.audit.active() {
        finish_command(
            shell,
            start,
            cmd,
            args,
            &outcome_for_audit(&result),
            stdout,
            stderr,
        );
    }
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
    frame_call(&entry.name, args, shell, |shell, frame| {
        entry
            .call_body(frame, args, mooring, shell)
            .map_err(Control::from)
    })
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

/// Record one capability-check node — only when a trail is collecting *and*
/// some enclosing grants layer asked for `audit: true`.  `fields` joins
/// `resource` and `decision` in the node's value map.
pub(crate) fn record_capability(shell: &mut Shell, resource: &str, decision: &str, fields: Map) {
    if !shell.should_audit_capabilities() {
        return;
    }
    let site = shell.call_site();
    let node = ExecNode::capability_check(
        resource,
        decision,
        site,
        shell.mobile.context.principal(),
        fields,
    );
    shell.local.audit.push(node);
}

/// Force collection on for `body` and return the nodes it produced; used by
/// `try` (to name the failing command) and `audit` (to return the subtree).
/// Nothing is moved out of the trail, so the nodes are already flat among
/// whatever the surrounding trail collects — no wrapping, no merge-back.
pub(crate) fn forced_subtree(
    shell: &mut Shell,
    capture: CapturePolicy,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> Result<(BodyResult, Vec<ExecNode>), Escape> {
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
