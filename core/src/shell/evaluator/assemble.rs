//! Pure collection assembly.

use crate::fact::{EvalFailure, Expected, Reason, ValueShape};
use crate::ir::{MapEntryV, Name, Val, ValListElem};
use crate::shell::runtime::{Break, Env, Error, List, Map, Settled, Signature, ValClos, View};
use crate::source::Spanned;

pub(super) fn spread_type_err(val: &ValClos) -> Error {
    val.expected("spread", Expected::Shape(ValueShape::List))
}

/// Cons and snoc reuse the spread's persistent spine, evaluating left to right.
pub(super) fn eval_list(
    elems: &[ValListElem],
    env: &Env,
    sig: &Signature,
) -> Result<ValClos, Error> {
    use ValListElem::{Single, Spread};

    if let [Single(sx), Spread(sxs)] = elems {
        let x = ValClos::close(&sx.item, env, sig)?;
        let xs = ValClos::close(&sxs.item, env, sig)?;
        let View::List(v) = xs.view() else {
            return Err(spread_type_err(&xs));
        };
        let mut v = v.to_list();
        v.push_front(x);
        return Ok(v.into());
    }

    if let [Spread(sxs), Single(sx)] = elems {
        let xs = ValClos::close(&sxs.item, env, sig)?;
        let x = ValClos::close(&sx.item, env, sig)?;
        let View::List(v) = xs.view() else {
            return Err(spread_type_err(&xs));
        };
        let mut v = v.to_list();
        v.push_back(x);
        return Ok(v.into());
    }

    let mut items = List::new();
    for elem in elems {
        match elem {
            Single(v) => items.push_back(ValClos::close(&v.item, env, sig)?),
            Spread(v) => {
                let val = ValClos::close(&v.item, env, sig)?;
                match val.view() {
                    View::List(inner) => items.append(inner.to_list()),
                    _ => return Err(spread_type_err(&val)),
                }
            }
        }
    }
    Ok(items.into())
}

pub(super) fn eval_update(
    base: &Spanned<Val>,
    fields: &[(Name, Spanned<Val>)],
    env: &Env,
    sig: &Signature,
) -> Result<ValClos, Error> {
    let mut pairs = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (key, value) in fields {
        if !seen.insert(key.to_string()) {
            return Err(duplicate_key(key));
        }
        pairs.push((key.to_string(), ValClos::close(&value.item, env, sig)?));
    }
    let base = ValClos::close(&base.item, env, sig)?;
    for (key, value) in base.as_map("spread")? {
        if seen.insert(key.to_string()) {
            pairs.push((key.to_string(), value.into_owned()));
        }
    }
    Ok(ValClos::map(pairs))
}

pub(super) fn eval_map(
    entries: &[MapEntryV],
    env: &Env,
    shell: &crate::shell::runtime::Shell,
) -> Settled<ValClos> {
    let sig = &shell.sig;
    let mut explicit = std::collections::HashSet::new();
    let mut result = Map::new();
    for entry in entries {
        match entry {
            MapEntryV::Entry(key, value) => {
                let formed = ValClos::close(&key.item, env, sig)
                    .map_err(|error| Break::from(error).locate("map", key.span, shell))?;
                let View::String(label) = formed.view() else {
                    return Err(Break::from(
                        formed.expected("map", Expected::Shape(ValueShape::String)),
                    )
                    .locate("map", key.span, shell));
                };
                let label = label.to_string();
                if !explicit.insert(label.clone()) {
                    return Err(Break::from(duplicate_key(&label)).locate("map", key.span, shell));
                }
                let value = ValClos::close(&value.item, env, sig)
                    .map_err(|error| Break::from(error).locate("map", value.span, shell))?;
                result.insert(label, value);
            }
            MapEntryV::Spread(value) => {
                let formed = ValClos::close(&value.item, env, sig)
                    .map_err(|error| Break::from(error).locate("map", value.span, shell))?;
                let map = formed
                    .as_map("spread")
                    .map_err(|error| Break::from(error).locate("map", value.span, shell))?;
                for (key, value) in map {
                    if !result.contains_key(key) {
                        result.insert(key.to_string(), value.into_owned());
                    }
                }
            }
        }
    }
    Ok(result.into())
}

fn duplicate_key(key: &str) -> Error {
    Error::new(Reason::Evaluation(Box::new(EvalFailure::DuplicateMapKey(
        key.into(),
    ))))
}
