//! Audit recording — the single lexical scope helper plus the command
//! lifecycle.
//!
//! Two public surfaces:
//!
//! - [`with_scope`] wraps a structural scope arm (`grant`, `within`,
//!   `guard`) so its body's audit children become the wrapping node's
//!   children.  [`record_scope`] is the lower-level form that returns
//!   the constructed node so `try` and `audit` can override fields
//!   before pushing.
//! - [`start`] / [`finish_command`] is the command lifecycle for every
//!   non-scope node (builtins, externals, uutils stages).  Capability
//!   checks go through [`record_capability`].
//!
//! All functions are no-ops when `shell.local.audit.active()` is `false`, so
//! the dispatcher path can call them unconditionally.

use crate::types::*;

/// Stamp captured at the start of a command, paired with
/// [`finish_command`] which reads it back as the node's `start` plus
/// the source position the node should name.  Carrying both in one
/// value keeps the dispatch site's bookkeeping to a single local.
#[derive(Clone, Debug, Default)]
pub(crate) struct AuditStart {
    pub site: CallSite,
    pub time: i64,
}

/// Snapshot the call site and wall clock to start one command's audit
/// record.  When audit is inactive the dispatcher's matching
/// [`finish_command`] is a no-op, so we skip the `script` clone and the
/// `epoch_us` syscall — the empty `AuditStart` we return is never
/// inspected.  Audit state cannot flip from inactive to active mid-
/// command (every scope that turns it on also restores on exit), so the
/// short-circuit is sound.
pub(crate) fn start(shell: &Shell) -> AuditStart {
    if !shell.local.audit.active() {
        return AuditStart::default();
    }
    AuditStart {
        site: shell.turn.loc.audit_site(),
        time: epoch_us(),
    }
}

/// Cap stderr at `STDERR_CAP_BYTES` in place.  Applied by the common
/// node-finalisation path whenever bytes are being captured (§10.3).
fn cap_stderr(buf: &mut Vec<u8>) {
    if buf.len() > STDERR_CAP_BYTES {
        buf.truncate(STDERR_CAP_BYTES);
    }
}

/// Record one command node.  Pushes a single node into the active
/// audit trail.  No-op when audit is inactive.  Internal builtins
/// (names starting with `_`) are skipped — their wrapping scope (when
/// any) already names the user-visible event.
///
/// `result` carries the command's outcome: on `Ok`, `value` becomes
/// the node's `value`; on `Break::Error`, the error's message is
/// used as `stderr` when the caller didn't capture stderr.  Exit /
/// Stopped propagate without recording.
pub(crate) fn finish_command(
    shell: &mut Shell,
    start: AuditStart,
    cmd: &str,
    args: &[Value],
    result: &Settled<Value>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) {
    if !shell.local.audit.active() {
        return;
    }
    if cmd.starts_with('_') {
        return;
    }
    let (status, value, mut node_stderr) = match result {
        Ok(v) => (shell.mobile.control.last_status, v.clone(), stderr),
        Err(Break::Error(e)) => {
            let s = if stderr.is_empty() {
                e.message.clone().into_bytes()
            } else {
                stderr
            };
            (e.exit_code(), Value::Unit, s)
        }
        Err(_) => return,
    };
    if shell.local.audit.captures_bytes() {
        cap_stderr(&mut node_stderr);
    }
    let arg_strs: Vec<String> = args.iter().map(|v| v.to_string()).collect();
    let node = ExecNode::command(
        cmd,
        arg_strs,
        status,
        start.site,
        AuditIo {
            stdout,
            stderr: node_stderr,
        },
        value,
        Vec::new(),
        AuditTime {
            start: start.time,
            end: epoch_us(),
        },
        shell.mobile.context.principal(),
    );
    shell.local.audit.push(node);
}

/// Wrap a command body in the audit lifecycle: stamp the start, route
/// captured stdout/stderr through the audit tee, finalize the node
/// from the body's outcome.  No-op overhead when audit is inactive —
/// [`start`] returns an empty stamp and [`finish_command`] short-
/// circuits, so the framing is free in the common case.
pub(crate) fn frame_call<F>(cmd: &str, args: &[Value], shell: &mut Shell, body: F) -> Raw<Value>
where
    F: FnOnce(&mut Shell) -> Raw<Value>,
{
    let start = start(shell);
    let (result, stdout, stderr) = super::with_audit_capture(shell, body)
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

/// `Tail` calls don't survive a command boundary — record them as a
/// `Value::Unit` outcome.  `Break::Error` carries through verbatim;
/// `Break::Escape` (Exit / Stopped) propagates without an audit node.
fn outcome_for_audit(raw: &Raw<Value>) -> Settled<Value> {
    match raw {
        Ok(v) => Ok(v.clone()),
        Err(Control::Tail(_)) => Ok(Value::Unit),
        Err(Control::Break(b)) => Err(b.clone()),
    }
}

/// Record one capability-check node.  No-op when capability auditing
/// is not currently requested (§11.4).  `fields` is spliced into the
/// node's `value` map next to `resource` and `decision`.
pub(crate) fn record_capability(shell: &mut Shell, resource: &str, decision: &str, fields: Map) {
    if !shell.should_audit_capabilities() {
        return;
    }
    let site = shell.turn.loc.audit_site();
    let node = ExecNode::capability_check(
        resource,
        decision,
        site,
        shell.mobile.context.principal(),
        fields,
    );
    shell.local.audit.push(node);
}

/// What [`record_scope`] returns to its caller on the non-escape path.
///
/// `record_scope` returns `Err(Escape)` directly for `Exit` / `Stopped`,
/// so a constructed `ScopeRecord` always has both a body and a node —
/// there is no "node missing because the body escaped" state to handle.
pub(crate) struct ScopeRecord {
    /// The body's non-escape outcome: either a value or a recoverable
    /// runtime error.  Caller chooses how to surface it.
    pub body: BodyResult,
    /// The wrapping scope node, always constructed.
    pub node: ExecNode,
}

/// Run `body` in a forced audit subtree and build the wrapping scope
/// node.  Used directly by `try` (to override `value` with the
/// `ok | err` variant) and `audit` (to return the node as a value),
/// and indirectly by [`with_scope`] for `grant` / `within` / `guard`.
///
/// `capture` is the byte-capture policy installed while the body
/// runs; the previous policy is restored on return.  Bytes captured
/// at the dispatcher level by `with_audit_capture` flow into the
/// children, so this policy is what they observe.
pub(crate) fn record_scope(
    shell: &mut Shell,
    cmd: &str,
    capture: CapturePolicy,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> Result<ScopeRecord, Escape> {
    let start = start(shell);
    let principal = shell.mobile.context.principal();
    let (fragment, settled) =
        with_capture_policy(shell, capture, |shell| shell.audit_forced_child(body));
    // Split escape paths off into `Err(Escape)`.  `split` is total: a
    // `Settled<Value>` cannot encode a tail call by construction (the
    // private `Tail` lives only in `Control`, absorbed by the
    // trampoline before any `Settled` is built), so every arm of the
    // match is reachable.
    let body_result = crate::types::split(settled)?;
    let (status, value, mut stderr) = match &body_result {
        BodyResult::Value(v) => (shell.mobile.control.last_status, v.clone(), Vec::new()),
        BodyResult::Error(e) => (e.exit_code(), Value::Unit, e.message.clone().into_bytes()),
    };
    if shell.local.audit.captures_bytes() {
        cap_stderr(&mut stderr);
    }
    // Scope nodes carry no serialised `args`: the structural IR
    // node *is* the record of what the scope received.
    let node = ExecNode::command(
        cmd,
        Vec::new(),
        status,
        start.site,
        AuditIo {
            stdout: Vec::new(),
            stderr,
        },
        value,
        fragment.into_nodes(),
        AuditTime {
            start: start.time,
            end: epoch_us(),
        },
        principal,
    );
    Ok(ScopeRecord {
        body: body_result,
        node,
    })
}

/// The lexical scope helper used by `grant`, `within`, and `guard`.
/// Records one wrapping node into the parent audit trail when audit
/// is active; otherwise runs `body` unchanged.  The body's result
/// (including `Break::Error`, `Exit`, `Stopped`) propagates
/// upward; only the scope node is the difference from a bare call.
pub(crate) fn with_scope(
    shell: &mut Shell,
    cmd: &str,
    body: impl FnOnce(&mut Shell) -> Settled<Value>,
) -> Settled<Value> {
    if !shell.local.audit.active() {
        return body(shell);
    }
    let capture = shell.local.audit.capture_policy();
    let record = record_scope(shell, cmd, capture, body).map_err(Break::Escape)?;
    shell.local.audit.push(record.node);
    match record.body {
        BodyResult::Value(v) => Ok(v),
        BodyResult::Error(e) => Err(Break::Error(e)),
    }
}

/// Run `f` with `policy` installed as the audit byte-capture policy.
/// Bytes capture is monotonic: an inner `try` with
/// `CapturePolicy::Off` does not override an outer `audit`'s
/// `Bytes`.  The previous policy is restored after `f` returns.
pub(crate) fn with_capture_policy<R>(
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
