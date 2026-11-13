//! Pure collection assembly.

use crate::fact::{EvalFailure, Expected, Reason, ValueShape};
use crate::ir::{Env, MapEntryV, Name, Val, ValClos, ValListElem, View};
use crate::shell::runtime::{Break, Error, Settled, Signature};
use crate::source::Spanned;
use std::collections::BTreeMap;

/// Evaluated left to right; a spread shares its persistent node ([`crate::ir::ListRef::concat`]).
pub(super) fn eval_list(
    elems: &[ValListElem],
    env: &Env,
    sig: &Signature,
) -> Result<ValClos, Error> {
    use ValListElem::{Single, Spread};

    let close = |elem: &Spanned<Val>| ValClos::close(&elem.item, env, sig);
    elems
        .iter()
        .try_fold(ValClos::list(Vec::new()), |acc, elem| {
            let View::List(items) = acc.view() else {
                unreachable!("the accumulator is a list")
            };
            Ok(match elem {
                Single(x) => items.snoc(close(x)?),
                Spread(xs) => items.concat(close(xs)?.as_list("spread")?),
            })
        })
}

pub(super) fn eval_update(
    base: &Spanned<Val>,
    fields: &[(Name, Spanned<Val>)],
    env: &Env,
    sig: &Signature,
) -> Result<ValClos, Error> {
    let mut result = BTreeMap::new();
    for (key, value) in fields {
        if result.contains_key(key.as_ref()) {
            return Err(duplicate_key(key));
        }
        result.insert(key.to_string(), ValClos::close(&value.item, env, sig)?);
    }
    let base = ValClos::close(&base.item, env, sig)?;
    for (key, value) in base.as_map("spread")?.iter() {
        result
            .entry(key.to_string())
            .or_insert_with(|| value.into_owned());
    }
    Ok(ValClos::map(result))
}

pub(super) fn eval_map(
    entries: &[MapEntryV],
    env: &Env,
    shell: &crate::shell::runtime::Shell,
) -> Settled<ValClos> {
    let sig = &shell.sig;
    let mut explicit = std::collections::HashSet::new();
    let mut result = BTreeMap::new();
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
                for (key, value) in map.iter() {
                    result
                        .entry(key.to_string())
                        .or_insert_with(|| value.into_owned());
                }
            }
        }
    }
    Ok(ValClos::map(result))
}

fn duplicate_key(key: &str) -> Error {
    Error::new(Reason::Evaluation(Box::new(EvalFailure::DuplicateMapKey(
        key.into(),
    ))))
}
