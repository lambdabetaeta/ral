//! Map / value predicates: `keys`, `has`, `is-empty`, `equal`, `lt`, `gt`.
//! The filesystem predicates (`exists`, `is-file`, …) live in [`super::fs`]
//! beside the other path-resolving builtins.
//!
//! Each comparison records its boolean outcome in `shell.last_status` so
//! that pipeline `?` chaining and `if` see a familiar exit-code-shaped
//! signal alongside the returned `Bool`.

use crate::types::{Value, Settled, as_map_ref, Shell, Break, Error};

use super::util::{check_arity, order_cmp, values_equal};

pub(super) fn builtin_keys(args: &[Value]) -> Settled<Value> {
    check_arity(args, 1, "keys")?;
    let m = as_map_ref(&args[0], "keys")?;
    Ok(Value::list(
        m.keys().map(|k| Value::String(k.clone())).collect(),
    ))
}

pub(super) fn builtin_has(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 2, "has")?;
    let m = as_map_ref(&args[0], "has")?;
    let found = m.contains_key(&args[1].to_string());
    shell.set_status_from_bool(found);
    Ok(Value::Bool(found))
}

pub(super) fn builtin_is_empty(args: &[Value], shell: &mut Shell) -> Settled<Value> {
    check_arity(args, 1, "is-empty")?;
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
    check_arity(args, 2, "equal")?;
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
