//! Value-layer evaluator for the CBPV IR — literals, variables, thunks,
//! collection literals, none of them effectful. Forming a value is pure: it
//! reads `Env` and nothing else, so it can never observe a `cd`. A thunk
//! value is `⟨M, ρ|occ(M)⟩`: it keeps only the bindings its body mentions. A
//! list, record or map literal is the value closure `⟨V, ρ|occ(V)⟩` too,
//! unless one of its direct names is answered only by Σ, which the closure's
//! Σ-free inspection could never read back out — that literal is built
//! eagerly instead.

use std::sync::Arc;

use crate::ir::Val;
use crate::types::{Closure, Env, Error, List, Map, Signature, Value};

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

/// Forms a value term, one step: a constant is its value; `Variable`
/// resolves through ρ, then Σ ([`crate::types::lookup`]), so a miss is an
/// undefined variable — the only error `form` raises; `Thunk(M)` forms the
/// thunk value `⟨M, env|occ(M)⟩`, through [`Closure::new`]; a variant forms
/// its payload; a list, record or map literal forms the value closure
/// `⟨V, env|occ(V)⟩` when every name it directly mentions is bound there
/// ([`literal_names_bound`]), and is built eagerly otherwise.
pub(crate) fn form(val: &Val, env: &Env, sig: &Signature) -> Result<Value, Error> {
    match val {
        Val::Unit => Ok(Value::Unit),
        Val::Int(n) => Ok(Value::Int(*n)),
        Val::Float(f) => Ok(Value::Float(*f)),
        Val::Bool(b) => Ok(Value::Bool(*b)),
        Val::String(s) => Ok(Value::string(s.clone())),
        Val::Variable(name) => crate::types::lookup(name, env, sig)
            .cloned()
            .ok_or_else(|| {
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
        Val::Thunk(node) => Ok(Value::Thunk(Closure::new(
            Arc::clone(node.shape()),
            node.occ(),
            env,
        ))),
        Val::List(node) => {
            if node
                .shape()
                .iter()
                .all(|e| literal_names_bound(&e.item, env))
            {
                Ok(Value::List(List::literal(node, env)))
            } else {
                let mut items: List = List::new();
                for elem in node.shape() {
                    items.push_back(form(&elem.item, env, sig)?);
                }
                Ok(Value::List(items))
            }
        }
        // Distinct-keyed, so no key check on the way in.
        Val::Record(node) | Val::Map(node) => {
            if node
                .shape()
                .iter()
                .all(|(_, v)| literal_names_bound(&v.item, env))
            {
                Ok(Value::Map(Map::literal(node, env)))
            } else {
                let entries = node.shape();
                let mut pairs = Vec::with_capacity(entries.len());
                for (key, value) in entries {
                    pairs.push((key.to_string(), form(&value.item, env, sig)?));
                }
                Ok(Value::map(pairs))
            }
        }
        Val::Variant { label, payload } => {
            let payload = match payload {
                Some(p) => Some(Box::new(form(p, env, sig)?)),
                None => None,
            };
            Ok(Value::Variant {
                label: label.clone(),
                payload,
            })
        }
    }
}

/// Whether every direct name `val` mentions — a `Variable`, through variant
/// payloads and nested list/record/map literal nodes, but never inside a
/// thunk — is bound in `env`.
///
/// A `Literal` value closure inspects ρ with no Σ fallback, so a name only Σ
/// answers must never become one of its entries: this is the check
/// that keeps that true, over the same names `form` would otherwise capture.
/// A thunk's own names are exempt because forcing it goes through `form`
/// again, Σ included.
fn literal_names_bound(val: &Val, env: &Env) -> bool {
    match val {
        Val::Unit | Val::Int(_) | Val::Float(_) | Val::Bool(_) | Val::String(_) | Val::Thunk(_) => {
            true
        }
        Val::Variable(name) => env.get(name).is_some(),
        Val::Variant { payload, .. } => payload
            .as_deref()
            .is_none_or(|p| literal_names_bound(p, env)),
        Val::List(node) => node
            .shape()
            .iter()
            .all(|e| literal_names_bound(&e.item, env)),
        Val::Record(node) | Val::Map(node) => node
            .shape()
            .iter()
            .all(|(_, v)| literal_names_bound(&v.item, env)),
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
