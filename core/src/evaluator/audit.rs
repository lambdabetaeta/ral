//! Audit recording: `with_scope` wraps a structural scope arm (`grant`,
//! `within`, `guard`) so its body's nodes become the wrapper's children.
//!
//! `frame_call` brackets every other node — builtins, externals, stages —
//! in the `start` / `finish_command` lifecycle.
//!
//! With `shell.local.audit.active()` false the recorders are no-ops, so the
//! dispatcher can call them unconditionally; `record_scope` is the exception,
//! since `try` and `audit` force a subtree whether or not audit is on.

use crate::source::Span;
use crate::types::{
    AuditIo, AuditTime, BodyResult, Break, BuiltinEntry, CallSite, CapturePolicy, Control, Escape,
    ExecNode, Map, Mooring, NodeOutcome, Raw, STDERR_CAP_BYTES, Settled, Shell, Value, epoch_us,
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

/// Assemble one scope or combinator node.  Shared by [`record_scope`] and
/// `iterate_audited` in `builtins::collections`, so the two cannot drift on
/// the node shape.  Such a node carries no `args`: the structural IR is
/// already the record of what the scope received; and no I/O: a scope does
/// not write to file descriptors, its children do.
pub(crate) fn scope_node(
    cmd: &str,
    start: &AuditStart,
    principal: String,
    outcome: NodeOutcome,
    children: Vec<ExecNode>,
) -> ExecNode {
    let NodeOutcome {
        status,
        value,
        error,
    } = outcome;
    ExecNode::command(
        cmd,
        Vec::new(),
        status,
        start.site.clone(),
        AuditIo::default(),
        error,
        value,
        children,
        AuditTime {
            start: start.time,
            end: epoch_us(),
        },
        principal,
    )
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

/// [`record_scope`]'s non-escape result.  `Exit` and `Stopped` leave as
/// `Err(Escape)` instead, so a `ScopeRecord` always has both halves — there is
/// no "node missing because the body escaped" state for callers to handle.
pub(crate) struct ScopeRecord {
    pub body: BodyResult,
    pub node: ExecNode,
}

/// Run `body` in a forced audit subtree and build the wrapping scope node,
/// handing it back unpushed: `try` overwrites its `value` with the
/// `ok | err` variant and `audit` returns it as a value.  [`with_scope`] is
/// the plain form for `grant` / `within` / `guard`.
///
/// `capture` is installed for the body and restored on return; it is the
/// policy the children observe as they are recorded.  `span` is the scope's
/// own position, not the dispatch register a command node reads — a scope
/// names where it sits, not whatever command preceded it.
pub(crate) fn record_scope(
    shell: &mut Shell,
    cmd: &str,
    capture: CapturePolicy,
    span: Option<Span>,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> Result<ScopeRecord, Escape> {
    let start = if shell.local.audit.active() {
        AuditStart {
            site: shell.site_of(span),
            time: epoch_us(),
        }
    } else {
        AuditStart::default()
    };
    let principal = shell.mobile.context.principal();
    let (fragment, settled) =
        with_capture_policy(shell, capture, |shell| shell.audit_forced_child(body));
    let body_result = crate::types::split(settled)?;
    let outcome = match &body_result {
        BodyResult::Value(v) => NodeOutcome::of_value(shell.mobile.control.last_status, v.clone()),
        BodyResult::Error(e) => NodeOutcome::of_error(e),
    };
    let node = scope_node(cmd, &start, principal, outcome, fragment.into_nodes());
    Ok(ScopeRecord {
        body: body_result,
        node,
    })
}

/// Push one wrapping node for `grant`, `within`, or `guard`.  The body's
/// result propagates untouched, errors and escapes included; the node is the
/// only difference from calling `body` directly.
pub(crate) fn with_scope(
    shell: &mut Shell,
    cmd: &str,
    span: Option<Span>,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> Settled<Value> {
    if !shell.local.audit.active() {
        return body(shell);
    }
    let capture = shell.local.audit.capture_policy();
    let record = record_scope(shell, cmd, capture, span, body).map_err(Break::Escape)?;
    shell.local.audit.push(record.node);
    match record.body {
        BodyResult::Value(v) => Ok(v),
        BodyResult::Error(e) => Err(Break::Error(e)),
    }
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
