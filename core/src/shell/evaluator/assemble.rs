//! Pure collection assembly.

use crate::fact::{EvalFailure, Expected, Reason, ValueShape};
use crate::ir::{MapEntryV, Name, Val, ValListElem};
use crate::shell::runtime::{Break, Env, Error, List, Map, Settled, Signature, Value};
use crate::source::Spanned;

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

pub(super) fn eval_update(
    base: &Spanned<Val>,
    fields: &[(Name, Spanned<Val>)],
    env: &Env,
    sig: &Signature,
) -> Result<Value, Error> {
    let mut pairs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (key, value) in fields {
        if !seen.insert(key.to_string()) {
            return Err(duplicate_key(key));
        }
        pairs.push((key.to_string(), form(&value.item, env, sig)?));
    }
    for (key, value) in &form_map(base, env, sig)? {
        if seen.insert(key.to_string()) {
            pairs.push((key.to_string(), value.into_owned()));
        }
    }
    Ok(Value::map(pairs))
}

pub(super) fn eval_map(
    entries: &[MapEntryV],
    env: &Env,
    shell: &crate::shell::runtime::Shell,
) -> Settled<Value> {
    let sig = &shell.sig;
    let mut explicit = std::collections::HashSet::new();
    let mut result = Map::new();
    for entry in entries {
        match entry {
            MapEntryV::Entry(key, value) => {
                let formed = form(&key.item, env, sig)
                    .map_err(|error| Break::from(error).locate("map", key.span, shell))?;
                let Value::String(label) = formed else {
                    return Err(Break::from(
                        formed.expected("map", Expected::Shape(ValueShape::String)),
                    )
                    .locate("map", key.span, shell));
                };
                let label = label.into_string();
                if !explicit.insert(label.clone()) {
                    return Err(Break::from(duplicate_key(&label)).locate("map", key.span, shell));
                }
                let value = form(&value.item, env, sig)
                    .map_err(|error| Break::from(error).locate("map", value.span, shell))?;
                result.insert(label, value);
            }
            MapEntryV::Spread(value) => {
                for (key, value) in &form_map(value, env, sig)
                    .map_err(|error| Break::from(error).locate("map", value.span, shell))?
                {
                    if !result.contains_key(key) {
                        result.insert(key.to_string(), value.into_owned());
                    }
                }
            }
        }
    }
    Ok(Value::Map(result))
}

fn form_map(value: &Spanned<Val>, env: &Env, sig: &Signature) -> Result<Map, Error> {
    match form(&value.item, env, sig)? {
        Value::Map(map) => Ok(map),
        value => Err(value.expected("spread", Expected::Shape(ValueShape::Map))),
    }
}

fn duplicate_key(key: &str) -> Error {
    Error::new(Reason::Evaluation(Box::new(EvalFailure::DuplicateMapKey(
        key.into(),
    ))))
}
