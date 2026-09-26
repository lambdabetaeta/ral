//! `CompKind::Assemble`: builds a list, record, or map some of whose parts
//! are spread or keyed at run time — the one rule that does O(data) work
//! over what would otherwise be value syntax.

use crate::diagnostic;
use crate::ir::{Assembly, MapPart, MapParts, ValListElem};
use crate::types::{Env, Error, List, Value};

use super::val::{close, spread_type_err};

pub(crate) fn eval(assembly: &Assembly, env: &Env) -> Result<Value, Error> {
    match assembly {
        Assembly::List(elems) => eval_list(elems, env),
        Assembly::Record(entries) => eval_map(entries, env),
        Assembly::Map(entries) => eval_map(entries, env),
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

/// Evaluates a record or map literal — one carrier, so one rule. Explicit
/// entries win over spreads: `seen` gates the spread pass, because
/// `Value::map` collects into an ordered map where a later insert would
/// otherwise overwrite the earlier.
fn eval_map<E: MapParts>(entries: &[E], env: &Env) -> Result<Value, Error> {
    let mut pairs: Vec<(String, Value)> = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new();
    for entry in entries {
        let (key, value) = match entry.part() {
            MapPart::Labelled(label, v) => (label.to_string(), v),
            MapPart::Computed(key_val, v) => {
                let key_value = close(key_val, env)?;
                let Value::String(key) = key_value else {
                    return Err(Error::new(
                        format!(
                            "map key must be a String, got {} '{key_value}'",
                            key_value.type_name()
                        ),
                        1,
                    )
                    .with_hint("use str to convert"));
                };
                (key.into_string(), v)
            }
            MapPart::Spread(_) => continue,
        };
        if !seen.insert(key.clone()) {
            diagnostic::shell_warning(&format!("duplicate key '{key}'"));
        }
        pairs.push((key, close(&value.item, env)?));
    }
    for entry in entries {
        if let MapPart::Spread(v) = entry.part() {
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
