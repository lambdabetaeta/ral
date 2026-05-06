//! Capture / dispatch primitives: `_try`, `try`, `_try-apply`, `guard`,
//! `_par`, `_audit`.
//!
//! `try`-shaped builtins reify the body's result as a record so the
//! caller can branch on success/failure without unwinding.  `guard`
//! interposes a cleanup thunk; `_par` is a parallel `map` over a
//! handle pool with optional concurrency limit.

use crate::diagnostic;
use crate::types::*;

use super::call_value;
use super::concurrency;
use super::util::{as_list, sig};

/// Result of running a body inside a capture boundary (`_try`, `_audit`).
///
/// Success vs failure is determined *solely* by `result`: `Ok` is success,
/// `Err(EvalSignal::Error)` is failure.  The POSIX `last_status` side-channel
/// is irrelevant here — it serves `?` chaining and `if`, not `try`.
///
/// All record fields (`ok`, `status`, `value`, `cmd`, …) are derived from
/// `result` on demand by `classify()`, so there are no redundant fields that
/// could diverge from the canonical `Result`.
struct CapturedEval {
    result: Result<Value, EvalSignal>,
    /// Bytes the body wrote to fd 1 during evaluation, captured via
    /// `with_capture`.  Surfaced as the `stdout` field of the `try` record
    /// so callers can inspect what a failing body emitted before failing.
    stdout: Vec<u8>,
    children: Vec<ExecNode>,
    start: i64,
    end: i64,
    principal: String,
    /// Call-site position, used as fallback when the error lacks location.
    call_line: usize,
    call_col: usize,
}

/// Derived fields from a `CapturedEval`.
struct Outcome {
    ok: bool,
    status: i32,
    value: Value,
    message: String,
    cmd: String,
    line: usize,
    col: usize,
}

impl CapturedEval {
    /// Derive success/failure and all record fields from `self.result`.
    fn classify(&self) -> Outcome {
        match &self.result {
            Ok(v) => Outcome {
                ok: true,
                status: 0,
                value: v.clone(),
                message: String::new(),
                cmd: String::new(),
                line: self.call_line,
                col: self.call_col,
            },
            Err(EvalSignal::Error(e)) => {
                let failing = self.children.iter().rev().find(|n| n.status != 0);
                Outcome {
                    ok: false,
                    status: e.status,
                    value: Value::Unit,
                    message: e.message.clone(),
                    cmd: failing
                        .map(|n| n.cmd.clone())
                        .unwrap_or_else(|| "<runtime>".into()),
                    line: e.loc.as_ref().map(|l| l.line).unwrap_or(self.call_line),
                    col: e.loc.as_ref().map(|l| l.col).unwrap_or(self.call_col),
                }
            }
            Err(EvalSignal::Exit(_) | EvalSignal::TailCall { .. }) => Outcome {
                ok: true,
                status: 0,
                value: Value::Unit,
                message: String::new(),
                cmd: String::new(),
                line: self.call_line,
                col: self.call_col,
            },
        }
    }

    fn try_record(&self) -> Value {
        let o = self.classify();
        Value::Map(vec![
            ("ok".into(), Value::Bool(o.ok)),
            ("value".into(), o.value),
            ("cmd".into(), Value::String(o.cmd)),
            ("status".into(), Value::Int(o.status as i64)),
            ("message".into(), Value::String(o.message)),
            ("stdout".into(), Value::Bytes(self.stdout.clone())),
            ("line".into(), Value::Int(o.line as i64)),
            ("col".into(), Value::Int(o.col as i64)),
        ])
    }

    fn audit_node(&self, cmd: &str) -> ExecNode {
        let o = self.classify();
        // For audit nodes the runtime-error message doubles as fd-2-shaped
        // bytes so the tree carries the failure text wherever a per-node
        // `stderr` is expected.  Successful or non-error signals leave it
        // empty.
        let stderr = match &self.result {
            Err(EvalSignal::Error(e)) => e.message.as_bytes().to_vec(),
            _ => Vec::new(),
        };
        ExecNode {
            kind: crate::types::ExecNodeKind::Command,
            cmd: cmd.into(),
            args: Vec::new(),
            status: o.status,
            script: String::new(),
            line: o.line,
            col: o.col,
            stdout: self.stdout.clone(),
            stderr,
            value: o.value,
            children: self.children.clone(),
            start: self.start,
            end: self.end,
            principal: self.principal.clone(),
        }
    }
}

/// Run `body` inside a capture boundary.
///
/// Collects the execution tree, timing metadata, and the body's stdout —
/// reusing `with_capture` (the same helper that gives `let x = hostname`
/// its bytes-to-string promotion).  `try` surfaces the captured stdout in
/// the `stdout` field of its record so a body that prints before failing
/// doesn't leak its output to the terminal.  All `try`/`audit` fields are
/// derived from `result` alone; `shell.control.last_status` is not consulted.
fn eval_captured(body: &Value, shell: &mut Shell) -> CapturedEval {
    let start = crate::types::epoch_us();
    let principal = shell
        .dynamic
        .env_vars()
        .get("USER")
        .cloned()
        .unwrap_or_default();
    let call_line = shell.location.line;
    let call_col = shell.location.col;
    let ((children, result), stdout) = crate::evaluator::with_full_capture(shell, |shell| {
        shell.with_audit_scope(|shell| call_value(body, &[], shell))
    });
    let end = crate::types::epoch_us();

    CapturedEval {
        result,
        stdout,
        children,
        start,
        end,
        principal,
        call_line,
        call_col,
    }
}
/// Shared core for `_try` and `try`: capture, debug-log, record audit node.
fn try_capture(body: &Value, shell: &mut Shell) -> (Outcome, Value) {
    let captured = eval_captured(body, shell);

    // Debug builds always echo; release builds opt in via RAL_DEBUG.
    // This surfaces errors that `try` is about to swallow so silent failures
    // inside handlers (plugin `ed-tui`, `$env[...]` probes) are visible.
    if (cfg!(debug_assertions) || std::env::var_os("RAL_DEBUG").is_some())
        && let Err(EvalSignal::Error(e)) = &captured.result
    {
        let loc = e
            .loc
            .as_ref()
            .map(|l| format!(" ({}:{})", l.line, l.col))
            .unwrap_or_default();
        eprintln!("ral: try caught error{loc}: {}", e.message);
    }

    let outcome = captured.classify();
    let record = captured.try_record();

    if let Some(parent_tree) = &mut shell.audit.tree {
        let mut node = captured.audit_node("try");
        node.script = shell.location.call_site.script.clone();
        node.line = shell.location.call_site.line;
        node.col = shell.location.call_site.col;
        node.value = record.clone();
        parent_tree.push(node);
    }

    (outcome, record)
}

/// `_try body` — returns the error record without dispatching.
pub(super) fn builtin_try(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.is_empty() {
        return Err(sig("try requires 1 argument (body)"));
    }
    let (_, record) = try_capture(&args[0], shell);
    shell.control.last_status = 0;
    Ok(record)
}

/// `try body handler` — on failure, calls handler with the error record.
pub(super) fn builtin_try_with(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.len() < 2 {
        return Err(sig("try requires 2 arguments (body, handler)"));
    }
    let (outcome, record) = try_capture(&args[0], shell);
    if outcome.ok {
        if let Value::Bool(b) = &outcome.value {
            shell.set_status_from_bool(*b);
        }
        Ok(outcome.value)
    } else {
        call_value(&args[1], &[record], shell)
    }
}

pub(super) fn builtin_try_apply(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.len() < 2 {
        return Err(sig("_try-apply requires 2 arguments (f, val)"));
    }
    let r = crate::evaluator::try_apply(&args[0], &args[1], shell)?;
    let (ok, value) = match r {
        Some(v) => (true, v),
        None => (false, Value::Unit),
    };
    Ok(Value::Map(vec![
        ("ok".into(), Value::Bool(ok)),
        ("value".into(), value),
    ]))
}

pub(super) fn builtin_guard(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.len() < 2 {
        return Err(sig("guard requires 2 arguments (body, cleanup)"));
    }
    crate::evaluator::audit::with_audited_scope(shell, "guard", args, |shell| {
        let body_result = call_value(&args[0], &[], shell);
        if let Err(e) = call_value(&args[1], &[], shell) {
            diagnostic::cmd_error("guard", &format!("cleanup failed: {e}"));
        }
        body_result
    })
}

pub(super) fn builtin_par(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.len() < 2 {
        return Err(sig("_par requires at least 2 arguments (f, items)"));
    }
    let func = &args[0];
    let items = as_list(&args[1], "_par")?;
    let limit = args.get(2).and_then(|v| v.as_int()).unwrap_or(0) as usize;

    if items.is_empty() {
        return Ok(Value::List(vec![]));
    }

    let mut handles: Vec<HandleInner> = Vec::with_capacity(items.len());
    let mut results: Vec<Value> = Vec::with_capacity(items.len());

    for (i, item) in items.iter().enumerate() {
        if limit > 0 && handles.len() >= limit {
            let idx = i - limit;
            results.push(concurrency::await_value(&handles[idx], shell)?);
        }

        let item_clone = item.clone();
        let func_clone = func.clone();
        handles.push(concurrency::spawn_child(
            shell.snapshot(),
            shell,
            concurrency::ChildIoMode::Buffered,
            "<par>",
            move |child_env| call_value(&func_clone, &[item_clone], child_env),
        )?);
    }

    let start = results.len();
    for h in &handles[start..] {
        results.push(concurrency::await_value(h, shell)?);
    }

    Ok(Value::List(results))
}

pub(super) fn builtin_audit(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.is_empty() {
        return Err(sig("_audit requires 1 argument (body)"));
    }
    let captured = eval_captured(&args[0], shell);

    if let Err(EvalSignal::Exit(_)) = &captured.result {
        return captured.result;
    }
    let status = captured.classify().status;
    let mut node = captured.audit_node("audit");
    node.script = shell.location.call_site.script.clone();
    node.line = shell.location.call_site.line;
    node.col = shell.location.call_site.col;

    if let Some(parent_tree) = &mut shell.audit.tree {
        parent_tree.push(node.clone());
    }

    shell.control.last_status = status;
    Ok(node.to_value())
}
