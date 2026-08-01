//! Map and value predicates: `keys`, `has`, `is-empty`, `equal`, `lt`, `gt`.
//! The filesystem probes (`exists`, `is-file`, …) live in [`super::fs`].
//!
//! Each Bool also lands in `$STATUS` (true → 0), so a bare predicate reads like
//! POSIX `test`.  Comparison belongs to [`super::util`], shared with the `$[…]`
//! operators in [`crate::evaluator::expr`], so the two cannot drift.

use crate::types::{Break, Error, Settled, Shell, Value, as_map_ref};

use super::util::{order_cmp, values_equal};

pub(super) fn builtin_keys(args: &[Value]) -> Settled<Value> {
    let m = as_map_ref(&args[0], "keys")?;
    Ok(Value::list(
        m.keys().map(|k| Value::String(k.clone())).collect(),
    ))
}

pub(super) fn builtin_has(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let m = as_map_ref(&args[0], "has")?;
    let found = m.contains_key(&args[1].to_string());
    shell.set_status_from_bool(found);
    Ok(Value::Bool(found))
}

pub(super) fn builtin_is_empty(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let val = &args[0];
    let r = match val {
        Value::List(items) => items.is_empty(),
        Value::Map(m) => m.is_empty(),
        Value::Bytes(b) => b.is_empty(),
        Value::String(s) => s.is_empty(),
        _ => {
            return Err(Break::Error(
                Error::new(
                    format!(
                        "is-empty expects List, Map, Bytes, or String, got {}",
                        val.type_name()
                    ),
                    1,
                )
                .with_hint("use file-empty to test whether a file or directory is empty"),
            ));
        }
    };
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

pub(super) fn builtin_equal(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    let r = values_equal(&args[0], &args[1])?;
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

pub(super) fn builtin_lt(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    order_cmp(args, shell, "lt", std::cmp::Ordering::is_lt)
}

pub(super) fn builtin_gt(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    order_cmp(args, shell, "gt", std::cmp::Ordering::is_gt)
}
