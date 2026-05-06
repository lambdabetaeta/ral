//! Free-standing utilities with no evaluator dependency.
//!
//! Tiny helpers that don't fit elsewhere.  The tilde-path types
//! and expansion live in [`crate::path::tilde`].

use crate::types::Value;

/// Parse a bare word into the most specific [`Value`] type.
///
/// Tries, in order: booleans (`true`/`false`), `unit`, integers,
/// floats (must contain `.`), and falls back to a string.
pub fn parse_literal(s: &str) -> Value {
    match s {
        "true" => Value::Bool(true),
        "false" => Value::Bool(false),
        "unit" => Value::Unit,
        _ if s.parse::<i64>().is_ok() => Value::Int(s.parse().unwrap()),
        _ if s.contains('.') && s.parse::<f64>().is_ok() => Value::Float(s.parse().unwrap()),
        _ => Value::String(s.to_string()),
    }
}
