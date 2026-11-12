//! Map predicates: `keys`, `has`.  Comparison is [`ValClos::equals`] and
//! [`ValClos::compare`]; `lt`, `gt` and `is-empty` are the prelude's.
//! The filesystem probes (`exists`, `is-file`, …) live in [`super::fs`].

use crate::shell::runtime::{Settled, ValClos};

pub(super) fn builtin_keys(args: &[ValClos]) -> Settled<ValClos> {
    let m = args[0].as_map("keys")?;
    Ok(ValClos::list(m.keys().map(ValClos::string).collect()))
}

pub(super) fn builtin_has(args: &[ValClos]) -> Settled<ValClos> {
    let m = args[0].as_map("has")?;
    let found = m.contains_key(args[1].as_str("has")?);
    Ok(ValClos::bool(found))
}
