//! Map and value predicates: `keys`, `has`, `is-empty`, `equal`, `lt`, `gt`.
//! The filesystem probes (`exists`, `is-file`, …) live in [`super::fs`].
//!
//! Comparison belongs to [`super::util`], shared with the `$[…]` operators in
//! [`crate::evaluator::expr`], so the two cannot drift.

use crate::types::{Break, Error, Settled, Value, as_map_ref};

use super::util::{order_cmp, values_equal};

pub(super) fn builtin_keys(args: &[Value]) -> Settled<Value> {
    let m = as_map_ref(&args[0], "keys")?;
    Ok(Value::list(
        m.keys().map(|k| Value::String(k.clone())).collect(),
    ))
}

pub(super) fn builtin_has(args: &[Value]) -> Settled<Value> {
    let m = as_map_ref(&args[0], "has")?;
    let found = m.contains_key(&args[1].to_string());
    Ok(Value::Bool(found))
}

pub(super) fn builtin_is_empty(args: &[Value]) -> Settled<Value> {
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
    Ok(Value::Bool(r))
}

pub(super) fn builtin_equal(args: &[Value]) -> Settled<Value> {
    let r = values_equal(&args[0], &args[1])?;
    Ok(Value::Bool(r))
}

pub(super) fn builtin_lt(args: &[Value]) -> Settled<Value> {
    order_cmp(args, "lt", std::cmp::Ordering::is_lt)
}

pub(super) fn builtin_gt(args: &[Value]) -> Settled<Value> {
    order_cmp(args, "gt", std::cmp::Ordering::is_gt)
}
