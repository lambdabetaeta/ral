//! Destructuring bind: match a runtime `Value` against a compiled
//! `IrPattern`, staging its bindings and folding them into an [`Env`].
//! All-or-nothing: [`stage_pattern`] collects every binding first, so a
//! pattern that fails partway — `let [[p],[q,r]] = [[1],[2]]` — leaves the
//! environment it was given untouched.

use super::comp::eval_comp;
use crate::ir::IrPattern;
use crate::typecheck::Scheme;
use crate::types::{Binding, Control, Env, Mooring, Raw, Settled, Shell, Tail, Value};

/// Refuse every name a `let` pattern binds that would shadow a PATH command.
/// `run_phrases`'s `Define` arm alone calls this, under `Mode::Session`: a
/// nested `Bind`'s pattern is a local lexical name, not a command name, so
/// the trampoline binds one unchecked.
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

/// Refuse a binding that shadows a command on `PATH`: ral keeps the value
/// and command namespaces disjoint.  The caller — `run_phrases`'s `Define`
/// arm, under `Mode::Session` alone — is the only one that ever reaches
/// this, so every call here is already at session scope; block, lambda and
/// prelude bindings never enter the command namespace, so they go unchecked
/// by never calling in.
pub(crate) fn check_path_shadow(name: &str, shell: &Shell) -> Raw<()> {
    if let Some(path) = shell.locate_command(name) {
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

/// Every name `pattern` binds, in pattern order — what `run_phrases`'s
/// `Define` arm walks to report the names its RHS bound.
pub(crate) fn pattern_names(pattern: &IrPattern, out: &mut Vec<String>) {
    match pattern {
        IrPattern::Wildcard => {}
        IrPattern::Name(name) => out.push(name.clone()),
        IrPattern::List { elems, rest } => {
            for elem in elems {
                pattern_names(elem, out);
            }
            if let Some(name) = rest {
                out.push(name.clone());
            }
        }
        IrPattern::Map(entries) => {
            for entry in entries {
                pattern_names(&entry.pattern, out);
            }
        }
    }
}

/// Destructure `value` against `pattern`, attaching to each bound name the
/// scheme `schemes` lists for it (empty for a scheme-less bind), and fold the
/// result into `env`.
///
/// # Errors
/// The shape mismatches [`stage_pattern`] reports; never a `Tail`, so the
/// caller need not thread [`Raw`] any further than here.
pub(crate) fn bind_pattern(
    pattern: &IrPattern,
    value: &Value,
    schemes: &[(String, Scheme)],
    mut env: Env,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Env> {
    let mut staged = Vec::new();
    stage_pattern(pattern, value, schemes, mooring, shell, &mut staged).map_err(
        |control| match control {
            Control::Break(b) => b,
            Control::Tail(_) => unreachable!("pattern staging never applies a tail call"),
        },
    )?;
    for (name, binding) in staged {
        env.bind(name, binding);
    }
    Ok(env)
}

fn scheme_for<'a>(schemes: &'a [(String, Scheme)], name: &str) -> Option<&'a Scheme> {
    schemes.iter().find(|(n, _)| n == name).map(|(_, s)| s)
}

/// Recursive worker for [`bind_pattern`]: pushes each binding onto `staged`
/// rather than installing it, so a caller's environment stays untouched until
/// the whole pattern has matched.  Map-pattern defaults are the one thing it
/// does evaluate.
fn stage_pattern(
    pattern: &IrPattern,
    value: &Value,
    schemes: &[(String, Scheme)],
    mooring: &Mooring,
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
                    scheme: scheme_for(schemes, name).cloned(),
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
            // `Ty::List` carries no length, so a too-short list typechecks;
            // catch it here rather than silently skip element patterns.
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
            // Without a `...rest` tail, a longer list would lose its extras in silence.
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
                stage_pattern(pat, &items[i], schemes, mooring, shell, staged)?;
            }
            if let Some(name) = rest {
                // `imbl::Vector` splits in O(log n) by sharing structure: no element clones.
                let mut whole = items.clone();
                let tail = whole.split_off(elems.len());
                staged.push((
                    name.clone(),
                    Binding {
                        value: Value::List(tail),
                        scheme: scheme_for(schemes, name).cloned(),
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
                    // A default's value is bound, never returned, so it is
                    // never in tail position.
                    (None, Some(default_comp)) => {
                        eval_comp(default_comp, mooring, shell, Tail::No)?
                    }
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
                stage_pattern(&entry.pattern, &val, schemes, mooring, shell, staged)?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Break, Mooring};

    fn list_pat(elems: &[&str], rest: Option<&str>) -> IrPattern {
        IrPattern::List {
            elems: elems
                .iter()
                .map(|n| IrPattern::Name(n.to_string()))
                .collect(),
            rest: rest.map(str::to_string),
        }
    }

    /// The typechecker cannot catch this: `Ty::List` carries no length.
    #[test]
    fn rest_pattern_errors_when_list_shorter_than_elems() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let pat = list_pat(&["a", "b"], Some("rest"));
        let value = Value::list(vec![Value::String("x".into())]);
        let result = bind_pattern(
            &pat,
            &value,
            &[],
            shell.mobile.scope.clone(),
            &Mooring::adrift(),
            &mut shell,
        );
        match result {
            Err(Break::Error(e)) => {
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

    #[test]
    fn list_pattern_errors_when_list_longer_than_elems() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let pat = list_pat(&["a", "b"], None);
        let value = Value::list(vec![
            Value::String("x".into()),
            Value::String("y".into()),
            Value::String("z".into()),
        ]);
        let result = bind_pattern(
            &pat,
            &value,
            &[],
            shell.mobile.scope.clone(),
            &Mooring::adrift(),
            &mut shell,
        );
        match result {
            Err(Break::Error(e)) => {
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

    #[test]
    fn rest_pattern_binds_tail_when_list_long_enough() {
        let mut shell = Shell::new(crate::io::TerminalState::default());
        let pat = list_pat(&["a", "b"], Some("rest"));
        let value = Value::list(vec![
            Value::String("x".into()),
            Value::String("y".into()),
            Value::String("z".into()),
        ]);
        let env = bind_pattern(
            &pat,
            &value,
            &[],
            shell.mobile.scope.clone(),
            &Mooring::adrift(),
            &mut shell,
        )
        .expect("binds");
        assert_eq!(env.get("a"), Some(&Value::String("x".into())));
        assert_eq!(env.get("b"), Some(&Value::String("y".into())));
        assert_eq!(
            env.get("rest"),
            Some(&Value::list(vec![Value::String("z".into())])),
        );
    }
}
