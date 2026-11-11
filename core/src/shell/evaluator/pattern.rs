//! Destructuring: match a runtime `Value` against a compiled `Pattern`.
//! Pure and all-or-nothing: [`destructure`] collects every binding first, so a
//! pattern that fails partway — `let [[p],[q,r]] = [[1],[2]]` — binds nothing.

use crate::fact::{
    CountRule, EvalFailure, Expected, KeyOrField, MissingEntry, PatternLength, Reason, ValueShape,
};
use crate::ir::{Name, Pattern};
use crate::shell::runtime::{Binding, Env, Error, Settled, Value};

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
                return Err(value
                    .expected("pattern", Expected::Shape(ValueShape::List))
                    .into());
            };
            // `Ty::List` carries no length, so a too-short list typechecks;
            // catch it here rather than silently skip element patterns.
            if elems.len() > items.len() {
                return Err(length_failure(elems.len(), items.len(), rest.is_some()).into());
            }
            // Without a `...rest` tail, a longer list would lose its extras in silence.
            if rest.is_none() && items.len() > elems.len() {
                return Err(length_failure(elems.len(), items.len(), false).into());
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
        Pattern::Record(entries) => {
            let Value::Map(m) = value else {
                return Err(value
                    .expected("pattern", Expected::Shape(ValueShape::Map))
                    .into());
            };
            for entry in entries {
                let key_label = &entry.key;
                let Some(val) = m.get(key_label) else {
                    return Err(Error::new(Reason::Evaluation(Box::new(
                        EvalFailure::MissingEntry(MissingEntry {
                            kind: KeyOrField::Field,
                            key: key_label.clone(),
                            available: m.keys().map(str::to_owned).collect(),
                        }),
                    )))
                    .into());
                };
                stage(&entry.pattern, &val, staged)?;
            }
            Ok(())
        }
    }
}

fn length_failure(expected: usize, got: usize, rest: bool) -> Error {
    Error::new(Reason::Evaluation(Box::new(EvalFailure::PatternLength(
        PatternLength {
            expected: if rest {
                CountRule::AtLeast(expected.into())
            } else {
                CountRule::Exactly(expected.into())
            },
            got: got.into(),
        },
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::runtime::Break;

    fn list_pat(elems: &[&str], rest: Option<&str>) -> Pattern {
        Pattern::List {
            elems: elems.iter().map(|n| Pattern::Name((*n).into())).collect(),
            rest: rest.map(Into::into),
        }
    }

    fn failure(result: Settled<Vec<(Name, Value)>>) -> EvalFailure {
        match result {
            Err(Break::Error(Error {
                reason: Reason::Evaluation(e),
                ..
            })) => *e,
            other => panic!("expected pattern failure, got {other:?}"),
        }
    }

    /// The typechecker cannot catch this: `Ty::List` carries no length.
    #[test]
    fn rest_pattern_errors_when_list_shorter_than_elems() {
        let pat = list_pat(&["a", "b"], Some("rest"));
        let value = Value::list(vec![Value::string("x")]);
        assert_eq!(
            failure(destructure(&pat, &value)),
            EvalFailure::PatternLength(PatternLength {
                expected: CountRule::AtLeast(2usize.into()),
                got: 1usize.into()
            })
        );
    }

    #[test]
    fn list_pattern_errors_when_list_longer_than_elems() {
        let pat = list_pat(&["a", "b"], None);
        let value = Value::list(vec![
            Value::string("x"),
            Value::string("y"),
            Value::string("z"),
        ]);
        assert_eq!(
            failure(destructure(&pat, &value)),
            EvalFailure::PatternLength(PatternLength {
                expected: CountRule::Exactly(2usize.into()),
                got: 3usize.into()
            })
        );
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
    #[test]
    fn pattern_shape_and_missing_field_failures_preserve_their_distinctions() {
        for (pattern, shape) in [
            (list_pat(&["x"], None), ValueShape::List),
            (Pattern::Record(vec![]), ValueShape::Map),
        ] {
            let Err(Break::Error(error)) = destructure(&pattern, &Value::Unit) else {
                panic!("invalid pattern input")
            };
            assert_eq!(
                error.reason,
                Value::Unit
                    .expected("pattern", Expected::Shape(shape))
                    .reason
            );
        }
        let pattern = Pattern::Record(vec![crate::ir::FieldPattern {
            key: "wanted".into(),
            pattern: Pattern::Wildcard,
        }]);
        assert_eq!(
            failure(destructure(
                &pattern,
                &Value::map(vec![("available".into(), Value::Unit)])
            )),
            EvalFailure::MissingEntry(MissingEntry {
                kind: KeyOrField::Field,
                key: "wanted".into(),
                available: vec!["available".into()]
            })
        );
    }
}
