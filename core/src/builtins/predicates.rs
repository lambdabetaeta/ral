//! Map predicates: `keys`, `has`.  Comparison is [`Value::equals`] and
//! [`Value::compare`]; `equal`, `lt`, `gt` and `is-empty` are the prelude's.
//! The filesystem probes (`exists`, `is-file`, …) live in [`super::fs`].

use crate::types::{Settled, Value};

pub(super) fn builtin_keys(args: &[Value]) -> Settled<Value> {
    let m = args[0].as_map_ref("keys")?;
    Ok(Value::list(m.keys().map(Value::string).collect()))
}

pub(super) fn builtin_has(args: &[Value]) -> Settled<Value> {
    let m = args[0].as_map_ref("has")?;
    let found = m.contains_key(args[1].as_str("has")?);
    Ok(Value::Bool(found))
}
