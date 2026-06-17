//! Error constructors and `Value` → `Map` coercions.
//!
//! These sit below both the builtins and the capability layer: a `sig`
//! is the one-line way to raise a runtime `Break::Error`, and the `as_map`
//! family walks a `Value` into a `Map` with a uniform "expects a Map"
//! error.  Keeping them in `types` lets every consumer reach them through
//! `use crate::types::*` without either layer importing the other's
//! helper module.

use super::*;

pub fn sig(message: impl Into<String>) -> Break {
    Break::Error(Error::new(message, 1))
}

pub(crate) fn sig_hint(message: impl Into<String>, hint: impl Into<String>) -> Break {
    Break::Error(Error::new(message, 1).with_hint(hint))
}

/// Borrow `val` as a `&Map` or fail with a `"{ctx} expects a Map, got …"`
/// sigil error.  The shared error path so the message wording stays in
/// one place; [`as_map`] is the owning variant for callers that need a
/// `Map` value.
pub(crate) fn as_map_ref<'a>(val: &'a Value, ctx: &str) -> Settled<&'a Map> {
    match val {
        Value::Map(m) => Ok(m),
        _ => Err(sig(format!("{ctx} expects a Map, got {}", val.type_name()))),
    }
}

pub fn as_map(val: &Value, ctx: &str) -> Settled<Map> {
    as_map_ref(val, ctx).cloned()
}
