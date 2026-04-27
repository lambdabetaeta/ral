//! Execution auditing — records exec nodes into the audit tree when active.
//!
//! All functions are no-ops when `shell.audit.tree` is `None`, so callers
//! need not guard against the common case of auditing being off.

use crate::types::*;

/// Capture a monotonic microsecond timestamp if auditing is active, else 0.
/// Paired with `record_exec`, which reads it back as the node's `start`.
pub(crate) fn start(shell: &Shell) -> i64 {
    if shell.audit.tree.is_some() {
        epoch_us()
    } else {
        0
    }
}

/// Scope-introducing builtins that are dispatched by their bare names but
/// record their own interior nodes via [`with_audited_scope`] (see
/// SPEC §10.3).  The dispatch-side leaf would duplicate that, so it is
/// suppressed here.  Underscore-prefixed scope builtins (`_audit`,
/// `_try`, `_guard`, `_each`, `_map`) are already covered by the
/// `cmd.starts_with('_')` rule below.
const SCOPE_BUILTINS: &[&str] = &["grant", "within"];

/// Build a leaf `ExecNode` at the current call site, stamped with start/end
/// timestamps and the `USER` principal.  The node has no children, value, or
/// captured I/O — the caller fills those in.
fn make_node(shell: &Shell, cmd: &str, args: &[Value], status: i32, start_us: i64) -> ExecNode {
    let arg_strs: Vec<String> = args.iter().map(|v| v.to_string()).collect();
    let mut node = ExecNode::leaf(
        cmd,
        arg_strs,
        status,
        &shell.location.call_site.script,
        shell.location.call_site.line,
        shell.location.call_site.col,
    );
    node.start = start_us;
    node.end = epoch_us();
    node.principal = shell
        .dynamic
        .env_vars
        .get("USER")
        .cloned()
        .unwrap_or_default();
    node
}

pub(crate) fn record_exec(shell: &mut Shell, cmd: &str, args: &[Value], value: &Value, start_us: i64) {
    if shell.audit.tree.is_none() || cmd.starts_with('_') || SCOPE_BUILTINS.contains(&cmd) {
        return;
    }
    let mut node = make_node(shell, cmd, args, shell.control.last_status, start_us);
    node.value = value.clone();
    if !shell.audit.captured_stdout.is_empty() {
        node.stdout = std::mem::take(&mut shell.audit.captured_stdout);
    }
    if !shell.audit.captured_stderr.is_empty() {
        node.stderr = std::mem::take(&mut shell.audit.captured_stderr);
    }
    if let Some(tree) = &mut shell.audit.tree {
        tree.push(node);
    }
}

/// Run `f` inside a fresh audit subtree, then record a single interior
/// `ExecNode` with `cmd`/`args`/captured-children at the parent level.
/// Pairs SPEC §10.3 scope nodes (`grant`, `within`, `guard`, …) with the
/// dispatch-side leaf they suppress.  When auditing is off the body
/// runs unchanged with no node created.
///
/// On `EvalSignal::Exit` the body's exit propagates without recording —
/// the process is on its way out and a half-built tree carries no
/// information.  This matches `_audit`'s existing behaviour.
pub(crate) fn with_audited_scope(
    shell: &mut Shell,
    cmd: &str,
    args: &[Value],
    f: impl FnOnce(&mut Shell) -> Result<Value, EvalSignal>,
) -> Result<Value, EvalSignal> {
    if shell.audit.tree.is_none() {
        return f(shell);
    }
    let start_us = epoch_us();
    let (children, result) = shell.with_audit_scope(f);

    if let Err(EvalSignal::Exit(_)) = &result {
        return result;
    }

    let mut node = make_node(shell, cmd, args, shell.control.last_status, start_us);
    node.children = children;
    match &result {
        Ok(v) => node.value = v.clone(),
        Err(EvalSignal::Error(e)) => {
            node.status = e.status;
            node.stderr = e.message.clone().into_bytes();
        }
        Err(_) => {}
    }
    if let Some(tree) = &mut shell.audit.tree {
        tree.push(node);
    }
    result
}

/// Record a capability-check "denied" node in the exec tree, if auditing is on.
/// Mirrors the audit side of `CommandHead::GrantDenied`.
pub(crate) fn record_deny(shell: &mut Shell, name: &str, args: &[Value]) {
    if !shell.should_audit_capabilities() {
        return;
    }
    let script = shell.location.call_site.script.clone();
    let line = shell.location.call_site.line;
    let col = shell.location.call_site.col;
    let mut node = ExecNode::capability_check("exec", "denied", &script, line, col);
    node.principal = shell
        .dynamic
        .env_vars
        .get("USER")
        .cloned()
        .unwrap_or_default();
    if let Value::Map(ref mut pairs) = node.value {
        let args_val: Vec<Value> = args.iter().map(|v| Value::String(v.to_string())).collect();
        pairs.push(("name".into(), Value::String(name.into())));
        pairs.push(("args".into(), Value::List(args_val)));
    }
    if let Some(tree) = &mut shell.audit.tree {
        tree.push(node);
    }
}
