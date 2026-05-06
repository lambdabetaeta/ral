//! Map / value predicates: `keys`, `has`, `is-empty`, `equal`, `lt`, `gt`.
//!
//! Each comparison records its boolean outcome in `shell.last_status` so
//! that pipeline `?` chaining and `if` see a familiar exit-code-shaped
//! signal alongside the returned `Bool`.

use crate::types::*;

use super::util::{check_arity, sig, str_cmp, values_equal};

pub(super) fn builtin_keys(args: &[Value]) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "keys")?;
    match &args[0] {
        Value::Map(pairs) => Ok(Value::List(
            pairs.iter().map(|(k, _)| Value::String(k.clone())).collect(),
        )),
        other => Err(sig(format!("keys expects a Map, got {}", other.type_name()))),
    }
}

pub(super) fn builtin_has(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "has")?;
    let key = args[1].to_string();
    let found = matches!(&args[0], Value::Map(pairs) if pairs.iter().any(|(k, _)| k == &key));
    shell.set_status_from_bool(found);
    Ok(Value::Bool(found))
}

pub(super) fn builtin_is_empty(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 1, "is-empty")?;
    let val = &args[0];
    let r = match val {
        Value::List(items) => items.is_empty(),
        Value::Map(pairs) => pairs.is_empty(),
        Value::Bytes(b) => b.is_empty(),
        Value::String(s) => s.is_empty(),
        _ => {
            return Err(EvalSignal::Error(
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

pub(super) fn builtin_equal(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    check_arity(args, 2, "equal")?;
    let r = values_equal(&args[0], &args[1]);
    shell.set_status_from_bool(r);
    Ok(Value::Bool(r))
}

pub(super) fn builtin_lt(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    str_cmp(args, shell, "lt", |a, b| a < b)
}

pub(super) fn builtin_gt(args: &[Value], shell: &mut Shell) -> Result<Value, EvalSignal> {
    str_cmp(args, shell, "gt", |a, b| a > b)
}
