//! Value-layer evaluator for the CBPV IR — literals, variables, thunks,
//! collection literals, none of them effectful. Forming a value is pure: it
//! reads `Env` and nothing else, so it can never observe a `cd`. A thunk
//! value is `⟨M, ρ|occ(M)⟩`: it keeps only the bindings its body mentions. A
//! list, record or map literal is the value closure `⟨V, ρ|occ(V)⟩` too,
//! unless one of its direct names is answered only by Σ, which the closure's
//! Σ-free inspection could never read back out — that literal is built
//! eagerly instead.

use crate::fact::{EvalFailure, Expected, Reason, ValueShape};
use std::sync::Arc;

use crate::ir::Val;
use crate::shell::runtime::{Closure, Env, Error, List, Map, Signature, Value};

/// Forms a value term, one step: a constant is its value; `Variable`
/// resolves through ρ, then Σ ([`crate::shell::runtime::lookup`]), so a miss is an
/// undefined variable — the only error `form` raises; `Thunk(M)` forms the
/// thunk value `⟨M, env|occ(M)⟩`, through [`Closure::new`]; a variant forms
/// its payload; a list, record or map literal forms the value closure
/// `⟨V, env|occ(V)⟩` when every name it directly mentions is bound there
/// ([`literal_names_bound`]), and is built eagerly otherwise.
pub(crate) fn form(val: &Val, env: &Env, sig: &Signature) -> Result<Value, Error> {
    match val {
        Val::Protected { value, site } => Ok(site.wrap(form(value, env, sig)?)),
        Val::Unit => Ok(Value::Unit),
        Val::Int(n) => Ok(Value::Int(*n)),
        Val::Float(f) => Ok(Value::Float(*f)),
        Val::Bool(b) => Ok(Value::Bool(*b)),
        Val::String(s) => Ok(Value::string(s.clone())),
        Val::Variable(name) => crate::shell::runtime::lookup(name, env, sig)
            .cloned()
            .ok_or_else(|| {
                let hint = match name.as_ref() {
                    "STATUS" => None,
                    _ => crate::typecheck::hyphen_boundary(name, |p| {
                        crate::shell::runtime::lookup(p, env, sig).is_some()
                    })
                    .map(crate::typecheck::hyphen_boundary_hint),
                };
                let error = Error::new(Reason::Evaluation(Box::new(EvalFailure::MissingBinding(
                    name.to_string(),
                ))));
                match hint {
                    Some(hint) => error.with_hint(hint),
                    None => error,
                }
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
        Val::Variant { label, payload } => Ok(Value::variant(
            label.clone(),
            Some(form(payload, env, sig)?),
        )),
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
        Val::Protected { value, .. } => literal_names_bound(value, env),
        Val::Unit | Val::Int(_) | Val::Float(_) | Val::Bool(_) | Val::String(_) | Val::Thunk(_) => {
            true
        }
        Val::Variable(name) => env.get(name).is_some(),
        Val::Variant { payload, .. } => literal_names_bound(payload, env),
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

/// Shared with argv-spread checking in `machine::close_argv`, and with
/// [`super::assemble`]'s list rule.
pub(crate) fn spread_type_err(val: &Value) -> Error {
    val.expected("spread", Expected::Shape(ValueShape::List))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_bindings_and_spread_backstops_preserve_typed_causes() {
        let shell = crate::test_helper::core_shell();
        assert_eq!(
            form(&Val::Variable("absent".into()), &Env::new(), &shell.sig)
                .unwrap_err()
                .reason,
            Reason::Evaluation(Box::new(EvalFailure::MissingBinding("absent".into())))
        );
        assert!(matches!(
            spread_type_err(&Value::Int(1)).reason,
            Reason::Invalid(_)
        ));
        assert_eq!(
            form(&Val::Variable("STATUS".into()), &Env::new(), &shell.sig)
                .unwrap_err()
                .hints(),
            []
        );
    }
}
