//! Error constructors and `Value` → collection coercions.

use super::{Break, Error, List, Map, Value};
use crate::source::Span;

pub fn sig(message: impl Into<String>) -> Break {
    Break::Error(Error::new(message, 1))
}

/// A bare signal error, positioned — for a fault the checker can already
/// place at a span, so the caret needs no help from `evaluator::machine`'s
/// generic stamp.
pub(crate) fn sig_at(message: impl Into<String>, span: Span) -> Break {
    Break::Error(Error::new(message, 1).at_span(span))
}

pub(crate) fn sig_hint(message: impl Into<String>, hint: impl Into<String>) -> Break {
    Break::Error(Error::new(message, 1).with_hint(hint))
}

/// Borrow `val` as a `&Map`, or fail with `"{ctx} expects a Map, got …"`.
/// A shape mismatch is never an exit, so the failure is a bare `Error` —
/// which the capability decoder folds straight into `PolicyError`.
pub(crate) fn as_map_ref<'a>(val: &'a Value, ctx: &str) -> Result<&'a Map, Error> {
    match val {
        Value::Map(m) => Ok(m),
        _ => Err(Error::new(
            format!("{ctx} expects a Map, got {}", val.type_name()),
            1,
        )),
    }
}

/// Owning variant of `as_map_ref`.
///
/// # Errors
/// Returns `Err` unless `val` is a `Value::Map`.
pub fn as_map(val: &Value, ctx: &str) -> Result<Map, Error> {
    as_map_ref(val, ctx).cloned()
}

/// Clone the elements of `val` into an owned `List`.
///
/// # Errors
/// Returns `Err` unless `val` is a `Value::List`.
pub fn as_list(val: &Value, ctx: &str) -> Result<List, Error> {
    match val {
        Value::List(items) => Ok(items.clone()),
        _ => Err(Error::new(
            format!("{ctx} expects a List, got {}", val.type_name()),
            1,
        )),
    }
}
