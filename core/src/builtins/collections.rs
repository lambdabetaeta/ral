//! List and collection combinators: `_each`, `_map`, `_filter`, `_sort-list`,
//! `_sort-list-by`, and `_fold`.
//!
//! These builtins provide the standard higher-order iteration primitives
//! over ral lists.  All of them participate in the audit tree when
//! auditing is active, recording their execution as interior nodes.

use crate::types::*;

use super::call_value;
use super::util::{as_list, check_arity, sig};

/// `_each <list> <fn>` -- call `fn` on each element for side effects.
/// Returns the result of the last application, or `Unit` for an empty list.
pub(super) fn builtin_each(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "each")?;
    shell.control.in_tail_position = false;
    let items = as_list(&args[0], "each")?;
    let func = &args[1];
    iterate_audited("for", shell, |shell| {
        let mut last = Value::Unit;
        for item in &items {
            match call_value(func, std::slice::from_ref(item), shell) {
                Ok(v) => last = v,
                Err(e) => return (last, Some(e)),
            }
        }
        (last, None)
    })
}

/// `_map <fn> <list>` -- apply `fn` to each element, return a new list.
pub(super) fn builtin_map(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "map")?;
    shell.control.in_tail_position = false;
    let func = &args[0];
    let items = as_list(&args[1], "map")?;
    iterate_audited("map", shell, |shell| {
        let mut out = Vec::with_capacity(items.len());
        for item in &items {
            match call_value(func, std::slice::from_ref(item), shell) {
                Ok(v) => out.push(v),
                Err(e) => return (Value::List(out), Some(e)),
            }
        }
        (Value::List(out), None)
    })
}

/// Run an iteration combinator, optionally inside an audit scope.
///
/// `body` returns its (possibly partial) value and an optional error: this
/// keeps the recorded audit-tree node faithful to whatever was accumulated
/// at the point of failure while propagating the error upwards.
fn iterate_audited(
    cmd: &str,
    shell: &mut Shell,
    body: impl FnOnce(&mut Shell) -> (Value, Option<EvalSignal>),
) -> Result<Value, EvalSignal> {
    let (value, err) = if shell.audit.tree.is_some() {
        let start = crate::types::epoch_us();
        let (children, (value, err)) = shell.with_audit_scope(body);
        let end = crate::types::epoch_us();
        let principal = shell
            .dynamic
            .env_vars()
            .get("USER")
            .cloned()
            .unwrap_or_default();
        let node = interior_node(
            cmd,
            &shell.location.call_site.script,
            shell.location.call_site.line,
            shell.location.call_site.col,
            shell.control.last_status,
            value.clone(),
            &err,
            children,
            start,
            end,
            principal,
        );
        if let Some(tree) = &mut shell.audit.tree {
            tree.push(node);
        }
        (value, err)
    } else {
        body(shell)
    };
    match err {
        Some(e) => Err(e),
        None => Ok(value),
    }
}

/// `_filter <fn> <list>` -- keep elements where `fn` returns `true`.
pub(super) fn builtin_filter(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "filter")?;
    shell.control.in_tail_position = false;
    let func = &args[0];
    let items = as_list(&args[1], "filter")?;
    let mut results = Vec::new();
    for item in &items {
        let result = call_value(func, std::slice::from_ref(item), shell)?;
        let keep = match &result {
            Value::Bool(b) => *b,
            _ => {
                return Err(sig(format!(
                    "filter: predicate must return Bool, got {} '{}'",
                    result.type_name(),
                    result
                )));
            }
        };
        if keep {
            results.push(item.clone());
        }
    }
    Ok(Value::List(results))
}

/// `_sort-list <list>` -- sort a list by the string representation of each element.
pub(super) fn builtin_sort(args: &[Value]) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "sort")?;
    let mut items = as_list(&args[0], "sort")?;
    items.sort_by_key(|a| a.to_string());
    Ok(Value::List(items))
}

/// `_sort-list-by <fn> <list>` -- sort by a key function.
/// Applies `fn` to each element to obtain a sort key, then sorts
/// lexicographically by the string representation of those keys.
pub(super) fn builtin_sort_by(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    if args.len() < 2 {
        return Err(sig("sort-by requires 2 arguments (f, items)"));
    }
    let func = &args[0];
    let mut items = as_list(&args[1], "sort-by")?;
    let mut keyed: Vec<(Value, Value)> = items
        .drain(..)
        .map(|item| {
            let key = call_value(func, std::slice::from_ref(&item), shell)?;
            Ok((key, item))
        })
        .collect::<Result<Vec<_>, EvalSignal>>()?;
    keyed.sort_by(|(ka, _), (kb, _)| ka.to_string().cmp(&kb.to_string()));
    Ok(Value::List(keyed.into_iter().map(|(_, v)| v).collect()))
}

/// `_fold <list> <init> <fn>` -- left fold over a list.
/// Calls `fn(acc, elem)` for each element, threading the accumulator.
pub(super) fn builtin_fold(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 3, "fold")?;
    shell.control.in_tail_position = false;
    let items = as_list(&args[0], "fold")?;
    let mut acc = args[1].clone();
    let func = &args[2];
    for item in &items {
        acc = call_value(func, &[acc, item.clone()], shell)?;
    }
    Ok(acc)
}

/// Build an audit-tree interior node for a collection combinator.
#[allow(clippy::too_many_arguments)]
fn interior_node(
    cmd: &str,
    script: &str,
    line: usize,
    col: usize,
    status: i32,
    value: Value,
    err: &Option<EvalSignal>,
    children: Vec<crate::types::ExecNode>,
    start: i64,
    end: i64,
    principal: String,
) -> crate::types::ExecNode {
    let stderr = match err {
        Some(EvalSignal::Error(e)) => e.message.clone(),
        _ => String::new(),
    };
    let status = match err {
        Some(EvalSignal::Error(e)) => e.status,
        _ => status,
    };
    crate::types::ExecNode {
        kind: crate::types::ExecNodeKind::Command,
        cmd: cmd.into(),
        args: Vec::new(),
        status,
        script: script.into(),
        line,
        col,
        stdout: Vec::new(),
        stderr: stderr.into_bytes(),
        value,
        children,
        start,
        end,
        principal,
    }
}
