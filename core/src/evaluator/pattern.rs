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
use crate::types::{Shell, Raw, Value, Binding, Tail};

/// Refuse each name a `let` pattern binds that would shadow a PATH command.
/// Driven from `eval_bind` only: lambda parameters bind through
/// [`assign_pattern`] in the trampoline and are deliberately never checked —
/// a parameter is a local lexical name, not an entry in the command
/// namespace the user types into.
pub(crate) fn check_pattern_shadow(pattern: &IrPattern, shell: &Shell) -> Raw<()> {
    match pattern {
        IrPattern::Wildcard => Ok(()),
        IrPattern::Name(name) => check_path_shadow(name, shell),
        IrPattern::List { elems, rest } => {
            for elem in elems {
                check_pattern_shadow(elem, shell)?;
            }
            if let Some(name) = rest {
                check_path_shadow(name, shell)?;
            }
            Ok(())
        }
        IrPattern::Map(entries) => {
            for entry in entries {
                check_pattern_shadow(&entry.pattern, shell)?;
            }
            Ok(())
        }
    }
}

/// Refuse a session-scope binding that would shadow a command reachable on
/// `PATH`: ral keeps the value and command namespaces disjoint.  Bindings
/// below the session scope (block/lambda bodies, the prelude) never enter
/// the command namespace and are exempt ([`Env::at_session_scope`]).
pub(crate) fn check_path_shadow(name: &str, shell: &Shell) -> Raw<()> {
    if shell.mobile.scope.at_session_scope()
        && let Some(path) = shell.locate_command(name)
    {
        return Err(shell
            .err_hint(
                format!(
                    "cannot bind `{name}`: a command named `{name}` is reachable on PATH ({})",
                    path.display()
                ),
                "ral keeps value and command names disjoint; rename the binding",
                1,
            )
            .into());
    }
    Ok(())
}

/// Destructure `value` against `pattern`, installing bindings into `shell`.
///
/// Destructuring is transactional: every binding the pattern would
/// install is staged in a scratch buffer by [`stage_pattern`] and only
/// installed here, once the whole pattern has matched. A pattern that
/// fails partway through — `let [[p],[q,r]] = [[1],[2]]` binds `p` then
/// finds `[2]` too short for `[q,r]` — therefore leaves no partial
/// bindings visible, whether the caller is a REPL turn (which installs
/// its mobile on every outcome) or a nested destructure.
pub(crate) fn assign_pattern(
    pattern: &IrPattern,
    value: &Value,
    scheme: Option<&Scheme>,
    shell: &mut Shell,
) -> Raw<()> {
    let mut staged = Vec::new();
    stage_pattern(pattern, value, scheme, shell, &mut staged)?;
    for (name, binding) in staged {
        shell.install_scope_binding(name, binding);
    }
    Ok(())
}

/// Recursive worker for [`assign_pattern`]: matches `pattern` against
/// `value`, pushing each binding it would make onto `staged` rather than
/// installing it immediately. Only evaluates map-pattern defaults (which
/// may themselves have effects) — never installs a binding — so a
/// caller can discard `staged` on error without having touched `shell`'s
/// scope.
fn stage_pattern(
    pattern: &IrPattern,
    value: &Value,
    scheme: Option<&Scheme>,
    shell: &mut Shell,
    staged: &mut Vec<(String, Binding)>,
) -> Raw<()> {
    match pattern {
        IrPattern::Wildcard => Ok(()),
        IrPattern::Name(name) => {
            debug_assert!(
                crate::syntax::ast::WordLiteral::classify(name).is_none(),
                "parser guarantees a binding name is never a word literal",
            );
            staged.push((
                name.clone(),
                Binding {
                    value: value.clone(),
                    scheme: scheme.cloned(),
                },
            ));
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
            // Without a `...rest` tail the pattern must cover the list
            // exactly: a longer list would silently drop its extra
            // elements, so reject it rather than bind partially.
            if rest.is_none() && items.len() > elems.len() {
                return Err(shell
                    .err_hint(
                        format!("need {} values, got {}", elems.len(), items.len()),
                        "there are more elements; use [..., ...rest] to capture them",
                        1,
                    )
                    .into());
            }
            for (i, pat) in elems.iter().enumerate() {
                stage_pattern(pat, &items[i], None, shell, staged)?;
            }
            if let Some(name) = rest {
                // `List::split_off` returns a tail that structurally shares
                // with the source — O(log n), no element clones.
                let mut whole = items.clone();
                let tail = whole.split_off(elems.len());
                staged.push((
                    name.clone(),
                    Binding {
                        value: Value::List(tail),
                        scheme: None,
                    },
                ));
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
                    // ([`Tail::No`]) by construction.
                    (None, Some(default_comp)) => eval_comp(default_comp, shell, Tail::No)?,
                    (None, None) => {
                        let ks: Vec<&str> = m.keys().map(std::string::String::as_str).collect();
                        return Err(shell
                            .err_hint(
                                format!("key '{key_label}' not found"),
                                format!("available: {}", ks.join(", ")),
                                1,
                            )
                            .into());
                    }
                };
                stage_pattern(&entry.pattern, &val, None, shell, staged)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Break, Control};

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
        let mut shell = Shell::new(crate::io::TerminalState::default());
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

    /// Without a `...rest` tail the pattern must cover the list exactly:
    /// `[a, b] = [x, y, z]` would otherwise drop `z` silently, so it errors.
    #[test]
    fn list_pattern_errors_when_list_longer_than_elems() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let pat = list_pat(&["a", "b"], None);
        let value = Value::list(vec![
            Value::String("x".into()),
            Value::String("y".into()),
            Value::String("z".into()),
        ]);
        let result = assign_pattern(&pat, &value, None, &mut shell);
        match result {
            Err(Control::Break(Break::Error(e))) => {
                assert!(
                    e.message.contains("need 2 values, got 3"),
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
        let mut shell = Shell::new(crate::io::TerminalState::default());
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
