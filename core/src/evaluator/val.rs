//! Value-layer evaluator for the CBPV IR — literals, variables, thunks,
//! collection literals, none of them effectful. Closing a value is pure: it
//! reads `Env` and nothing else, so it can never observe a `cd`.

use crate::diagnostic;
use crate::ir::{Val, ValListElem, ValMapEntry};
use crate::types::{Closure, Env, Error, List, Value};

/// Renders one interpolation piece for `eval_interpolation` in `comp.rs`.
pub(crate) fn interpolate_piece(v: &Value) -> Result<String, Error> {
    match v {
        Value::Unit | Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) => {
            Ok(v.to_string())
        }
        Value::Bytes(_) => Err(Error::new("cannot interpolate Bytes in string", 1)
            .with_hint("render with str (lossy UTF-8), or decode with from-string")),
        _ => Err(Error::new(
            format!("cannot interpolate {} in string", v.type_name()),
            1,
        )
        .with_hint("use str to convert, or index into the value")),
    }
}

/// Closes a value term: `Variable` resolves through `env` alone, so a miss is
/// an undefined variable; `Thunk(M)` closes to `Value::Thunk(Closure { comp:
/// M, env })` — a computation closure held as data (§1.1 of the CEK plan).
pub(crate) fn close(val: &Val, env: &Env) -> Result<Value, Error> {
    match val {
        Val::Unit => Ok(Value::Unit),
        Val::Int(n) => Ok(Value::Int(*n)),
        Val::Float(f) => Ok(Value::Float(*f)),
        Val::Bool(b) => Ok(Value::Bool(*b)),
        Val::String(s) => Ok(Value::String(s.clone())),
        Val::Variable(name) => env.get(name).cloned().ok_or_else(|| {
            let hint = match name.as_str() {
                "STATUS" => {
                    "there is no status register: a failure raises an error \
                     record that carries its own status — catch it with `try` \
                     and read `$err[status]` from the handler's argument"
                }
                _ => "check spelling, or ensure the variable is defined before this line",
            };
            Error::new(format!("undefined variable: ${name}"), 1).with_hint(hint)
        }),
        Val::Thunk(comp) => Ok(Value::Thunk(Closure {
            comp: comp.clone(),
            env: env.clone(),
        })),
        Val::List(elems) => eval_list(elems, env),
        Val::Map(entries) => eval_map(entries, env),
        Val::Variant { label, payload } => {
            let payload = match payload {
                Some(p) => Some(Box::new(close(p, env)?)),
                None => None,
            };
            Ok(Value::Variant {
                label: label.clone(),
                payload,
            })
        }
    }
}

/// Evaluates a list literal, splicing `...spread` elements inline. The
/// cons and snoc shapes reuse the spread's persistent spine instead of
/// rebuilding it, and still evaluate left to right.
fn eval_list(elems: &[ValListElem], env: &Env) -> Result<Value, Error> {
    use ValListElem::{Single, Spread};

    if let [Single(sx), Spread(sxs)] = elems {
        let x = close(&sx.item, env)?;
        let xs = close(&sxs.item, env)?;
        let Value::List(mut v) = xs else {
            return Err(spread_type_err(&xs));
        };
        v.push_front(x);
        return Ok(Value::List(v));
    }

    if let [Spread(sxs), Single(sx)] = elems {
        let xs = close(&sxs.item, env)?;
        let x = close(&sx.item, env)?;
        let Value::List(mut v) = xs else {
            return Err(spread_type_err(&xs));
        };
        v.push_back(x);
        return Ok(Value::List(v));
    }

    let mut items: List = List::new();
    for elem in elems {
        match elem {
            Single(v) => items.push_back(close(&v.item, env)?),
            Spread(v) => match close(&v.item, env)? {
                Value::List(inner) => items.append(inner),
                val => return Err(spread_type_err(&val)),
            },
        }
    }
    Ok(Value::List(items))
}

/// Shared with argument-spread checking in `call.rs`.
pub(crate) fn spread_type_err(val: &Value) -> Error {
    Error::new(
        format!("spread requires a List, got {}", val.type_name()),
        1,
    )
    .with_hint("spread (...) expands a list")
}

/// Evaluates a map literal. Explicit entries win over spreads: `seen`
/// gates the spread pass, because `Value::map` collects into an ordered
/// map where a later insert would otherwise overwrite the earlier.
fn eval_map(entries: &[ValMapEntry], env: &Env) -> Result<Value, Error> {
    let mut pairs: Vec<(String, Value)> = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new();
    for entry in entries {
        if let ValMapEntry::Entry(key_val, v) = entry {
            let key_value = close(key_val, env)?;
            let key = match &key_value {
                Value::String(s) => s.clone(),
                _ => {
                    return Err(Error::new(
                        format!(
                            "map key must be a String, got {} '{key_value}'",
                            key_value.type_name()
                        ),
                        1,
                    )
                    .with_hint("use str to convert"));
                }
            };
            if !seen.insert(key.clone()) {
                diagnostic::shell_warning(&format!("duplicate key '{key}'"));
            }
            pairs.push((key, close(&v.item, env)?));
        }
    }
    for entry in entries {
        if let ValMapEntry::Spread(v) = entry {
            match close(&v.item, env)? {
                Value::Map(inner) => {
                    for (k, v) in inner {
                        if seen.insert(k.clone()) {
                            pairs.push((k, v));
                        }
                    }
                }
                val => {
                    return Err(Error::new(
                        format!("spread requires a Map, got {}", val.type_name()),
                        1,
                    )
                    .with_hint("spread (...) in a map expands key-value pairs"));
                }
            }
        }
    }
    Ok(Value::map(pairs))
}
