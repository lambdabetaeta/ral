//! Value-layer evaluator for the CBPV IR.
//!
//! Values are side-effect-free: literals, variables, thunk closures,
//! list and map constructors, and tilde-expansion. [`eval_val`] is
//! the dispatch entry; collection literals (`eval_list`, `eval_map`)
//! and string-piece rendering (`interpolate_piece`) live alongside
//! because they are pure value transformations.

use crate::diagnostic;
use crate::ir::*;
use crate::path::tilde::expand_tilde_path;
use crate::types::*;

/// Renders one piece of a string interpolation as text. Only scalar
/// types (`Unit`, `String`, `Int`, `Float`, `Bool`) are interpolable;
/// structured values produce a diagnostic error.
pub(crate) fn interpolate_piece(v: &Value, shell: &Shell) -> Result<String, Error> {
    match v {
        Value::Unit => Ok(String::new()),
        Value::String(_) | Value::Int(_) | Value::Float(_) | Value::Bool(_) => Ok(v.to_string()),
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

/// Evaluates a value term of the CBPV IR.
///
/// Values are side-effect-free: literals, variables, thunk closures,
/// list and map constructors, and tilde-expansion.  [`Val::Variable`]
/// runs value-name lookup: env → pseudo-vars (`$env`, `$args`,
/// `$script`, `$nproc`) → explicit builtin value form.  A name that
/// misses all three is reported as an undefined variable — value
/// position has no handler or external command arm.
pub(crate) fn eval_val(val: &Val, shell: &mut Shell) -> Result<Value, Error> {
    match val {
        Val::Unit => Ok(Value::Unit),
        Val::Int(n) => Ok(Value::Int(*n)),
        Val::Float(f) => Ok(Value::Float(*f)),
        Val::Bool(b) => Ok(Value::Bool(*b)),
        Val::String(s) => Ok(Value::String(s.clone())),
        Val::Variable(name) => shell.lookup_value_name(name).ok_or_else(|| {
            shell.err_hint(
                format!("undefined variable: ${name}"),
                "check spelling, or ensure the variable is defined before this line",
                1,
            )
        }),
        // `Val::Thunk(comp)` is the IR producer of a callable value.
        // Project to `Value::Lambda` when the body is a `Lam`
        // computation (a function literal `{ |x| ... }`), and to
        // `Value::Block` otherwise (a nullary block literal
        // `{ ... }`). The split lets every downstream consumer —
        // `apply`, `step_force`, typecheck — observe the body shape
        // without re-introspecting the IR. Curried lambdas carry
        // their inner `Lam`-chain in the body field; the elaborator
        // flattens nested lambdas, so `body` is either the lambda's
        // tail or a nested `Lam`.
        Val::Thunk(body) => match &body.item {
            crate::ir::CompKind::Lam {
                param,
                body: lam_body,
            } => Ok(Value::Lambda {
                param: param.clone(),
                body: lam_body.clone(),
                captured: shell.snapshot(),
            }),
            _ => Ok(Value::Block {
                body: body.clone(),
                captured: shell.snapshot(),
            }),
        },
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
        Val::TildePath(path) => Ok(Value::String(expand_tilde_path(
            path.user.as_deref(),
            path.suffix.as_deref(),
            &shell.mobile.context.home(),
        ))),
    }
}

/// Evaluates a list literal, expanding `...spread` elements inline.
///
/// Cons (`[x, ...xs]`) becomes `push_front`, snoc (`[...xs, x]`)
/// becomes `push_back` — both O(1) amortised on the persistent
/// `List` spine. Aliasing is safe by construction: mutating a clone
/// path-copies the affected nodes, leaving any other handle to the
/// same list unchanged.
fn eval_list(elems: &[ValListElem], shell: &mut Shell) -> Result<Value, Error> {
    use ValListElem::{Single, Spread};

    // Cons: [x, ...xs] — single first (left-to-right), then spread.
    if let [Single(sx), Spread(sxs)] = elems {
        let x = eval_val(sx, shell)?;
        let xs = eval_val(sxs, shell)?;
        let Value::List(mut v) = xs else {
            return Err(spread_type_err(shell, &xs));
        };
        v.push_front(x);
        return Ok(Value::List(v));
    }

    // Snoc: [...xs, x] — spread first, then single.
    if let [Spread(sxs), Single(sx)] = elems {
        let xs = eval_val(sxs, shell)?;
        let x = eval_val(sx, shell)?;
        let Value::List(mut v) = xs else {
            return Err(spread_type_err(shell, &xs));
        };
        v.push_back(x);
        return Ok(Value::List(v));
    }

    // General case: any element count, any mix of Single/Spread.
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

pub(crate) fn spread_type_err(shell: &Shell, val: &Value) -> Error {
    shell.err_hint(
        format!("spread requires a List, got {}", val.type_name()),
        "spread (...) expands a list",
        1,
    )
}

/// Evaluates a map literal. Explicit entries are processed first
/// and take priority; spread entries fill in keys not already
/// present. Duplicate explicit keys emit a diagnostic warning.
fn eval_map(entries: &[ValMapEntry], shell: &mut Shell) -> Result<Value, Error> {
    let mut pairs: Vec<(String, Value)> = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new();
    // Explicit entries first so they win over spreads.
    for entry in entries {
        if let ValMapEntry::Entry(key_val, v) = entry {
            let key_value = eval_val(key_val, shell)?;
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
                diagnostic::shell_warning(&format!(
                    "duplicate key '{key}' (line {})",
                    shell.turn.loc.line
                ));
            }
            pairs.push((key, eval_val(v, shell)?));
        }
    }
    // Spreads fill in keys not already present.
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
