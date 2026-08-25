//! Value-layer evaluator for the CBPV IR — literals, variables, thunks,
//! collection literals, none of them effectful. A thunk's body shape is
//! decided at construction, so consumers never re-inspect the IR.

use crate::diagnostic;
use crate::ir::{Comp, Val, ValListElem, ValMapEntry};
use crate::types::{Error, List, Shell, Value};

/// Renders one interpolation piece for `eval_interpolation` in `comp.rs`.
pub(crate) fn interpolate_piece(v: &Value, shell: &Shell) -> Result<String, Error> {
    match v {
        Value::Unit | Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) => {
            Ok(v.to_string())
        }
        Value::Bytes(_) => Err(shell.err_hint(
            "cannot interpolate Bytes in string",
            "render with str (lossy UTF-8), or decode with from-string",
            1,
        )),
        _ => Err(shell.err_hint(
            format!("cannot interpolate {} in string", v.type_name()),
            "use str to convert, or index into the value",
            1,
        )),
    }
}

/// Evaluates a value term. A `Val::Variable` resolves through
/// `Shell::lookup_value_name` alone — value position has no
/// external-command arm, so a miss is an undefined variable.
pub(crate) fn eval_val(val: &Val, shell: &mut Shell) -> Result<Value, Error> {
    match val {
        Val::Unit => Ok(Value::Unit),
        Val::Int(n) => Ok(Value::Int(*n)),
        Val::Float(f) => Ok(Value::Float(*f)),
        Val::Bool(b) => Ok(Value::Bool(*b)),
        Val::String(s) => Ok(Value::String(s.clone())),
        Val::Variable(name) => shell.lookup_value_name(name).ok_or_else(|| {
            let hint = match name.as_str() {
                "STATUS" => {
                    "there is no status register: a failure raises an error \
                     record that carries its own status — catch it with `try` \
                     and read `$err[status]` from the handler's argument"
                }
                _ => "check spelling, or ensure the variable is defined before this line",
            };
            shell.err_hint(format!("undefined variable: ${name}"), hint, 1)
        }),
        Val::Thunk(body) => Ok(close_lam(body, shell).unwrap_or_else(|| Value::Block {
            body: body.clone(),
            captured: shell.snapshot(),
        })),
        Val::List(elems) => eval_list(elems, shell),
        Val::Map(entries) => eval_map(entries, shell),
        Val::Variant { label, payload } => {
            let payload = match payload {
                Some(p) => Some(Box::new(eval_val(p, shell)?)),
                None => None,
            };
            Ok(Value::Variant {
                label: label.clone(),
                payload,
            })
        }
    }
}

/// Closes a `CompKind::Lam` into a [`Value::Lambda`], `None` otherwise.
/// The trampoline calls it on a curried lambda's flattened body, bypassing
/// `eval_comp`'s arm that rejects a bare lambda in computation position.
pub(crate) fn close_lam(comp: &Comp, shell: &Shell) -> Option<Value> {
    if let crate::ir::CompKind::Lam { param, body } = &comp.item {
        Some(Value::Lambda {
            param: param.clone(),
            body: body.clone(),
            captured: shell.snapshot(),
        })
    } else {
        None
    }
}

/// Evaluates a list literal, splicing `...spread` elements inline. The
/// cons and snoc shapes reuse the spread's persistent spine instead of
/// rebuilding it, and still evaluate left to right.
fn eval_list(elems: &[ValListElem], shell: &mut Shell) -> Result<Value, Error> {
    use ValListElem::{Single, Spread};

    if let [Single(sx), Spread(sxs)] = elems {
        let x = eval_val(sx, shell)?;
        let xs = eval_val(sxs, shell)?;
        let Value::List(mut v) = xs else {
            return Err(spread_type_err(shell, &xs));
        };
        v.push_front(x);
        return Ok(Value::List(v));
    }

    if let [Spread(sxs), Single(sx)] = elems {
        let xs = eval_val(sxs, shell)?;
        let x = eval_val(sx, shell)?;
        let Value::List(mut v) = xs else {
            return Err(spread_type_err(shell, &xs));
        };
        v.push_back(x);
        return Ok(Value::List(v));
    }

    let mut items: List = List::new();
    for elem in elems {
        match elem {
            Single(v) => items.push_back(eval_val(v, shell)?),
            Spread(v) => match eval_val(v, shell)? {
                Value::List(inner) => items.append(inner),
                val => return Err(spread_type_err(shell, &val)),
            },
        }
    }
    Ok(Value::List(items))
}

/// Shared with argument-spread checking in `call.rs`.
pub(crate) fn spread_type_err(shell: &Shell, val: &Value) -> Error {
    shell.err_hint(
        format!("spread requires a List, got {}", val.type_name()),
        "spread (...) expands a list",
        1,
    )
}

/// Evaluates a map literal. Explicit entries win over spreads: `seen`
/// gates the spread pass, because `Value::map` collects into an ordered
/// map where a later insert would otherwise overwrite the earlier.
fn eval_map(entries: &[ValMapEntry], shell: &mut Shell) -> Result<Value, Error> {
    let mut pairs: Vec<(String, Value)> = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new();
    for entry in entries {
        if let ValMapEntry::Entry(key_val, v) = entry {
            let key_value = eval_val(key_val, shell)?;
            let key = match &key_value {
                Value::String(s) => s.clone(),
                _ => {
                    return Err(shell.err_hint(
                        format!(
                            "map key must be a String, got {} '{key_value}'",
                            key_value.type_name()
                        ),
                        "use str to convert",
                        1,
                    ));
                }
            };
            if !seen.insert(key.clone()) {
                diagnostic::shell_warning(&format!("duplicate key '{key}'"));
            }
            pairs.push((key, eval_val(v, shell)?));
        }
    }
    for entry in entries {
        if let ValMapEntry::Spread(v) = entry {
            match eval_val(v, shell)? {
                Value::Map(inner) => {
                    for (k, v) in inner {
                        if seen.insert(k.clone()) {
                            pairs.push((k, v));
                        }
                    }
                }
                val => {
                    return Err(shell.err_hint(
                        format!("spread requires a Map, got {}", val.type_name()),
                        "spread (...) in a map expands key-value pairs",
                        1,
                    ));
                }
            }
        }
    }
    Ok(Value::map(pairs))
}
