//! Pattern matching and destructuring bind.
//!
//! `assign_pattern` destructs a runtime `Value` against a compiled
//! [`IrPattern`], installing bindings into `shell`.  A `Name` pattern
//! installs the bind's scheme next to the value; destructured
//! components install scheme-less.  Mismatches raise a runtime error
//! with a located `expected … got …` message and a hint about the
//! required shape.  Map-pattern defaults are already elaborated IR
//! ([`Arc<Comp>`]); the evaluator simply runs them when a key is
//! absent — no parser syntax is touched at runtime.

use super::comp::eval_comp;
use crate::ir::IrPattern;
use crate::typecheck::Scheme;
use crate::types::*;

pub(crate) fn assign_pattern(
    pattern: &IrPattern,
    value: &Value,
    scheme: Option<&Scheme>,
    shell: &mut Shell,
) -> Raw<()> {
    match pattern {
        IrPattern::Wildcard => Ok(()),
        IrPattern::Name(name) => {
            if crate::syntax::ast::WordLiteral::classify(name).is_some() {
                return Err(shell
                    .err(format!("cannot assign to literal '{name}'"), 1)
                    .into());
            }
            shell.mobile.scope.set_binding(
                name.clone(),
                Binding {
                    value: value.clone(),
                    scheme: scheme.cloned(),
                },
            );
            Ok(())
        }
        IrPattern::List { elems, rest } => {
            let Value::List(items) = value else {
                return Err(shell
                    .err_hint(
                        format!("expected List, got {}", value.type_name()),
                        "right-hand side must be a list",
                        1,
                    )
                    .into());
            };
            // Every element pattern must bind; only the `rest` tail may
            // be empty.  `Ty::List` carries no length, so a list shorter
            // than `elems` typechecks — guard at runtime rather than
            // silently skip element patterns past `items.len()`.
            if elems.len() > items.len() {
                let hint = if rest.is_none() {
                    "use [..., ...rest] to capture remaining elements"
                } else {
                    "the list has too few elements for the named bindings"
                };
                return Err(shell
                    .err_hint(
                        format!("need {} values, got {}", elems.len(), items.len()),
                        hint,
                        1,
                    )
                    .into());
            }
            for (i, pat) in elems.iter().enumerate() {
                assign_pattern(pat, &items[i], None, shell)?;
            }
            if let Some(name) = rest {
                // `List::split_off` returns a tail that structurally shares
                // with the source — O(log n), no element clones.
                let mut whole = items.clone();
                let tail = whole.split_off(elems.len());
                shell.mobile.scope.set(name.clone(), Value::List(tail));
            }
            Ok(())
        }
        IrPattern::Map(entries) => {
            let Value::Map(m) = value else {
                return Err(shell
                    .err_hint(
                        format!("expected Map, got {}", value.type_name()),
                        "right-hand side must be a map",
                        1,
                    )
                    .into());
            };
            for entry in entries {
                let key_label = entry.key.row_label();
                let val = match (m.get(&key_label), &entry.default) {
                    (Some(v), _) => v.clone(),
                    // A pattern default fills in a missing field; its
                    // value is bound, never the enclosing body's result,
                    // so it runs under a non-trivial continuation
                    // ([`Tail::No`]) by construction (review finding E4).
                    (None, Some(default_comp)) => eval_comp(default_comp, shell, Tail::No)?,
                    (None, None) => {
                        let ks: Vec<&str> = m.keys().map(|k| k.as_str()).collect();
                        return Err(shell
                            .err_hint(
                                format!("key '{key_label}' not found"),
                                format!("available: {}", ks.join(", ")),
                                1,
                            )
                            .into());
                    }
                };
                assign_pattern(&entry.pattern, &val, None, shell)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list_pat(elems: &[&str], rest: Option<&str>) -> IrPattern {
        IrPattern::List {
            elems: elems
                .iter()
                .map(|n| IrPattern::Name(n.to_string()))
                .collect(),
            rest: rest.map(str::to_string),
        }
    }

    /// `Ty::List` carries no length, so `[a, b, ...rest] = [x]` typechecks.
    /// At runtime the list is too short to bind every element pattern: the
    /// evaluator must error rather than silently skip `b`.
    #[test]
    fn rest_pattern_errors_when_list_shorter_than_elems() {
        let mut shell = Shell::new(Default::default());
        let pat = list_pat(&["a", "b"], Some("rest"));
        let value = Value::list(vec![Value::String("x".into())]);
        let result = assign_pattern(&pat, &value, None, &mut shell);
        match result {
            Err(Control::Break(Break::Error(e))) => {
                assert!(
                    e.message.contains("need 2 values, got 1"),
                    "expected length error, got {:?}",
                    e.message,
                );
            }
            other => panic!("expected length error, got {other:?}"),
        }
        assert!(shell.mobile.scope.get("a").is_none());
        assert!(shell.mobile.scope.get("b").is_none());
    }

    /// When the list covers every element pattern, the tail binds to `rest`.
    #[test]
    fn rest_pattern_binds_tail_when_list_long_enough() {
        let mut shell = Shell::new(Default::default());
        let pat = list_pat(&["a", "b"], Some("rest"));
        let value = Value::list(vec![
            Value::String("x".into()),
            Value::String("y".into()),
            Value::String("z".into()),
        ]);
        assign_pattern(&pat, &value, None, &mut shell).expect("binds");
        assert_eq!(
            shell.mobile.scope.get("a"),
            Some(&Value::String("x".into()))
        );
        assert_eq!(
            shell.mobile.scope.get("b"),
            Some(&Value::String("y".into()))
        );
        assert_eq!(
            shell.mobile.scope.get("rest"),
            Some(&Value::list(vec![Value::String("z".into())])),
        );
    }
}
