//! Destructuring: match a runtime `Value` against a compiled `Pattern`.
//! Pure and all-or-nothing: [`destructure`] collects every binding first, so a
//! pattern that fails partway — `let [[p],[q,r]] = [[1],[2]]` — binds nothing.

use crate::ir::{Name, Pattern};
use crate::types::{Binding, Env, Error, Settled, Value};

/// The `(name, value)` pairs `pattern` binds in `value`, in pattern order.
///
/// # Errors
/// The shape mismatches [`stage`] reports.
pub(crate) fn destructure(pattern: &Pattern, value: &Value) -> Settled<Vec<(Name, Value)>> {
    let mut staged = Vec::new();
    stage(pattern, value, &mut staged)?;
    Ok(staged)
}

/// `env` extended by what `pattern` binds in `value`, each name scheme-less.
///
/// # Errors
/// The shape mismatches [`destructure`] reports.
pub(crate) fn bind(pattern: &Pattern, value: &Value, mut env: Env) -> Settled<Env> {
    env.extend(
        destructure(pattern, value)?
            .into_iter()
            .map(|(name, value)| {
                (
                    name,
                    Binding {
                        value,
                        scheme: None,
                    },
                )
            }),
    );
    Ok(env)
}

/// Recursive worker for [`destructure`]: pushes each binding onto `staged`.
fn stage(pattern: &Pattern, value: &Value, staged: &mut Vec<(Name, Value)>) -> Settled<()> {
    match pattern {
        Pattern::Wildcard => Ok(()),
        Pattern::Name(name) => {
            debug_assert!(
                crate::syntax::ast::WordLiteral::classify(name).is_none(),
                "parser guarantees a binding name is never a word literal",
            );
            staged.push((name.clone(), value.clone()));
            Ok(())
        }
        Pattern::List { elems, rest } => {
            let Value::List(items) = value else {
                return Err(
                    Error::new(format!("expected List, got {}", value.type_name()))
                        .with_hint("right-hand side must be a list")
                        .into(),
                );
            };
            // `Ty::List` carries no length, so a too-short list typechecks;
            // catch it here rather than silently skip element patterns.
            if elems.len() > items.len() {
                let hint = if rest.is_none() {
                    "use [..., ...rest] to capture remaining elements"
                } else {
                    "the list has too few elements for the named bindings"
                };
                return Err(Error::new(format!(
                    "need {} values, got {}",
                    elems.len(),
                    items.len()
                ))
                .with_hint(hint)
                .into());
            }
            // Without a `...rest` tail, a longer list would lose its extras in silence.
            if rest.is_none() && items.len() > elems.len() {
                return Err(Error::new(format!(
                    "need {} values, got {}",
                    elems.len(),
                    items.len()
                ))
                .with_hint("there are more elements; use [..., ...rest] to capture them")
                .into());
            }
            for (i, pat) in elems.iter().enumerate() {
                let item = items
                    .get(i)
                    .expect("checked above: elems.len() <= items.len()");
                stage(pat, &item, staged)?;
            }
            if let Some(name) = rest {
                // `imbl::Vector` splits in O(log n) by sharing structure: no element clones.
                let mut whole = items.clone();
                let tail = whole.split_off(elems.len());
                staged.push((name.clone(), Value::List(tail)));
            }
            Ok(())
        }
        Pattern::Map(entries) => {
            let Value::Map(m) = value else {
                return Err(
                    Error::new(format!("expected Map, got {}", value.type_name()))
                        .with_hint("right-hand side must be a map")
                        .into(),
                );
            };
            for entry in entries {
                let key_label = &entry.key;
                let Some(val) = m.get(key_label) else {
                    let ks: Vec<&str> = m.keys().collect();
                    return Err(Error::new(format!("key '{key_label}' not found"))
                        .with_hint(format!("available: {}", ks.join(", ")))
                        .into());
                };
                stage(&entry.pattern, &val, staged)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Break;

    fn list_pat(elems: &[&str], rest: Option<&str>) -> Pattern {
        Pattern::List {
            elems: elems.iter().map(|n| Pattern::Name((*n).into())).collect(),
            rest: rest.map(Into::into),
        }
    }

    fn message(result: Settled<Vec<(Name, Value)>>) -> String {
        match result {
            Err(Break::Error(e)) => e.message,
            other => panic!("expected length error, got {other:?}"),
        }
    }

    /// The typechecker cannot catch this: `Ty::List` carries no length.
    #[test]
    fn rest_pattern_errors_when_list_shorter_than_elems() {
        let pat = list_pat(&["a", "b"], Some("rest"));
        let value = Value::list(vec![Value::string("x")]);
        assert!(message(destructure(&pat, &value)).contains("need 2 values, got 1"));
    }

    #[test]
    fn list_pattern_errors_when_list_longer_than_elems() {
        let pat = list_pat(&["a", "b"], None);
        let value = Value::list(vec![
            Value::string("x"),
            Value::string("y"),
            Value::string("z"),
        ]);
        assert!(message(destructure(&pat, &value)).contains("need 2 values, got 3"));
    }

    #[test]
    fn rest_pattern_binds_tail_when_list_long_enough() {
        let pat = list_pat(&["a", "b"], Some("rest"));
        let value = Value::list(vec![
            Value::string("x"),
            Value::string("y"),
            Value::string("z"),
        ]);
        let env = bind(&pat, &value, Env::new()).expect("binds");
        assert_eq!(env.get("a"), Some(&Value::string("x")));
        assert_eq!(env.get("b"), Some(&Value::string("y")));
        assert_eq!(
            env.get("rest"),
            Some(&Value::list(vec![Value::string("z")])),
        );
    }
}
