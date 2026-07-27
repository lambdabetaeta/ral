//! Error constructors and `Value` → collection coercions.
//!
//! These sit below both the builtins and the capability layer: a `sig`
//! is the one-line way to raise a runtime `Break::Error`, and the
//! `as_map`/`as_list` family walks a `Value` into a `Map` or `List` with
//! a uniform "expects a …" error.  Keeping them in `types` lets every
//! consumer reach them through `use crate::types::*` without either layer
//! importing the other's helper module.

use super::{Break, Error, List, Map, Value};

pub fn sig(message: impl Into<String>) -> Break {
    Break::Error(Error::new(message, 1))
}

pub(crate) fn sig_hint(message: impl Into<String>, hint: impl Into<String>) -> Break {
    Break::Error(Error::new(message, 1).with_hint(hint))
}

/// Borrow `val` as a `&Map` or fail with a `"{ctx} expects a Map, got …"`
/// error.  Every failure here is a shape mismatch, never a process exit,
/// so the return type carries `Error` rather than `Break`; a caller in a
/// `Settled`-returning function converts with `?`.  The shared error path
/// so the message wording stays in one place; [`as_map`] is the owning
/// variant for callers that need a `Map` value.
pub(crate) fn as_map_ref<'a>(val: &'a Value, ctx: &str) -> Result<&'a Map, Error> {
    match val {
        Value::Map(m) => Ok(m),
        _ => Err(Error::new(
            format!("{ctx} expects a Map, got {}", val.type_name()),
            1,
        )),
    }
}

/// Clone `val` into an owned `Map`, or fail with the shared
/// `"{ctx} expects a Map, got …"` error — the owning variant of
/// [`as_map_ref`].
///
/// # Errors
/// Returns `Err` if `val` is not a `Value::Map`.
pub fn as_map(val: &Value, ctx: &str) -> Result<Map, Error> {
    as_map_ref(val, ctx).cloned()
}

/// Clone the elements of `val` into an owned `List`, or fail with a
/// `"{ctx} expects a List, got …"` error.
///
/// # Errors
/// Returns `Err` if `val` is not a `Value::List`.
pub fn as_list(val: &Value, ctx: &str) -> Result<List, Error> {
    match val {
        Value::List(items) => Ok(items.clone()),
        _ => Err(Error::new(
            format!("{ctx} expects a List, got {}", val.type_name()),
            1,
        )),
    }
}
