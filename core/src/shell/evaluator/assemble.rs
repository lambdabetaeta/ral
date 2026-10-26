//! Atomic list and record assembly. Computed map operands use the CEK cursor.

use crate::ir::{ValListElem, ValRecordEntry};
use crate::shell::runtime::{Env, Error, List, Signature, Value};

use super::val::{form, spread_type_err};

/// Evaluates a list literal, splicing `...spread` elements inline. The
/// cons and snoc shapes reuse the spread's persistent spine instead of
/// rebuilding it, and still evaluate left to right.
pub(super) fn eval_list(elems: &[ValListElem], env: &Env, sig: &Signature) -> Result<Value, Error> {
    use ValListElem::{Single, Spread};

    if let [Single(sx), Spread(sxs)] = elems {
        let x = form(&sx.item, env, sig)?;
        let xs = form(&sxs.item, env, sig)?;
        let Value::List(mut v) = xs else {
            return Err(spread_type_err(&xs));
        };
        v.push_front(x);
        return Ok(Value::List(v));
    }

    if let [Spread(sxs), Single(sx)] = elems {
        let xs = form(&sxs.item, env, sig)?;
        let x = form(&sx.item, env, sig)?;
        let Value::List(mut v) = xs else {
            return Err(spread_type_err(&xs));
        };
        v.push_back(x);
        return Ok(Value::List(v));
    }

    let mut items: List = List::new();
    for elem in elems {
        match elem {
            Single(v) => items.push_back(form(&v.item, env, sig)?),
            Spread(v) => match form(&v.item, env, sig)? {
                Value::List(inner) => items.append(&inner),
                val => return Err(spread_type_err(&val)),
            },
        }
    }
    Ok(Value::List(items))
}

/// Explicit record fields override the spread's fields.
pub(super) fn eval_record(
    entries: &[ValRecordEntry],
    env: &Env,
    sig: &Signature,
) -> Result<Value, Error> {
    let mut pairs: Vec<(String, Value)> = Vec::new();
    let mut seen = std::collections::HashSet::<String>::new();
    for entry in entries {
        let ValRecordEntry::Field(key, value) = entry else {
            continue;
        };
        if !seen.insert(key.clone()) {
            return Err(Error::new(format!(
                "map literal: key '{key}' given twice; which did you mean?"
            )));
        }
        pairs.push((key.clone(), form(&value.item, env, sig)?));
    }
    for entry in entries {
        if let ValRecordEntry::Spread(v) = entry {
            match form(&v.item, env, sig)? {
                Value::Map(inner) => {
                    for (k, v) in &inner {
                        if seen.insert(k.to_string()) {
                            pairs.push((k.to_string(), v.into_owned()));
                        }
                    }
                }
                val => {
                    return Err(Error::new(format!(
                        "spread requires a Map, got {}",
                        val.type_name()
                    ))
                    .with_hint("spread (...) in a map expands key-value pairs"));
                }
            }
        }
    }
    Ok(Value::map(pairs))
}
