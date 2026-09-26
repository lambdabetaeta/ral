//! Value-layer evaluator for the CBPV IR — literals, variables, thunks,
//! collection literals, none of them effectful. Closing a value is pure: it
//! reads `Env` and nothing else, so it can never observe a `cd`. A thunk
//! value is `⟨M, ρ|occ(M)⟩`: it keeps only the bindings its body mentions.

use std::sync::Arc;

use crate::ir::Val;
use crate::types::{Closure, Env, Error, List, Value};

/// Renders one interpolation piece for `machine::eval_rules`'s
/// `CompKind::Interpolation` rule.
pub(crate) fn interpolate_piece(v: &Value) -> Result<String, Error> {
    match v {
        Value::Unit | Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) => {
            Ok(v.to_string())
        }
        Value::Bytes(_) => Err(Error::new("cannot interpolate Bytes in string", 1)
            .with_hint("render with str (lossy UTF-8), or decode with from-string")),
        _ => Err(
            Error::new(format!("cannot interpolate {} in string", v.type_name()), 1)
                .with_hint("use str to convert, or index into the value"),
        ),
    }
}

/// Closes a value term: `Variable` resolves through `env` alone, so a miss is
/// an undefined variable; `Thunk(M)` closes to the thunk value
/// `⟨M, env|occ(M)⟩`, through [`Closure::new`].
pub(crate) fn close(val: &Val, env: &Env) -> Result<Value, Error> {
    match val {
        Val::Unit => Ok(Value::Unit),
        Val::Int(n) => Ok(Value::Int(*n)),
        Val::Float(f) => Ok(Value::Float(*f)),
        Val::Bool(b) => Ok(Value::Bool(*b)),
        Val::String(s) => Ok(Value::string(s.clone())),
        Val::Variable(name) => env.get(name).cloned().ok_or_else(|| {
            let hint = match name.as_ref() {
                "STATUS" => {
                    "there is no status register: a failure raises an error \
                     record that carries its own status — catch it with `try` \
                     and read `$err[status]` from the handler's argument"
                }
                _ => "check spelling, or ensure the variable is defined before this line",
            };
            Error::new(format!("undefined variable: ${name}"), 1).with_hint(hint)
        }),
        Val::Thunk(comp) => Ok(Value::Thunk(Closure::new(Arc::clone(comp), env))),
        Val::List(elems) => {
            let mut items: List = List::new();
            for elem in elems {
                items.push_back(close(&elem.item, env)?);
            }
            Ok(Value::List(items))
        }
        // Distinct-keyed, so no key check on the way in.
        Val::Record(entries) | Val::Map(entries) => {
            let mut pairs = Vec::with_capacity(entries.len());
            for (key, value) in entries {
                pairs.push((key.to_string(), close(&value.item, env)?));
            }
            Ok(Value::map(pairs))
        }
        Val::Variant { label, payload } => {
            let payload = match payload {
                Some(p) => Some(Box::new(close(p, env)?)),
                None => None,
            };
            Ok(Value::Variant {
                label: label.to_string(),
                payload,
            })
        }
    }
}

/// Shared with argument-spread checking in `machine::close_args`, and with
/// [`super::assemble`]'s list rule.
pub(crate) fn spread_type_err(val: &Value) -> Error {
    Error::new(
        format!("spread requires a List, got {}", val.type_name()),
        1,
    )
    .with_hint("spread (...) expands a list")
}
