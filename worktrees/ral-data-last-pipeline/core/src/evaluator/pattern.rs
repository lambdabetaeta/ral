//! Pattern matching and destructuring bind.
//!
//! `assign_pattern` destructs a runtime `Value` against a compiled `Pattern`,
//! installing bindings into `shell`.  Mismatches carry `ErrorKind::PatternMismatch`
//! so `try_apply` (§16.4) can catch them without swallowing other errors.

use super::eval_comp;
use crate::ast::Pattern;
use crate::types::*;

pub(crate) fn assign_pattern(
    pattern: &Pattern,
    value: &Value,
    shell: &mut Shell,
) -> Result<(), EvalSignal> {
    match pattern {
        Pattern::Wildcard => Ok(()),
        Pattern::Name(name) => {
            if name == "true"
                || name == "false"
                || name.parse::<i64>().is_ok()
                || (name.contains('.') && name.parse::<f64>().is_ok())
            {
                return Err(shell.err(format!("cannot assign to literal '{name}'"), 1));
            }
            shell.set(name.clone(), value.clone());
            Ok(())
        }
        Pattern::List { elems, rest } => {
            let Value::List(items) = value else {
                return Err(shell.pm_err(
                    format!("expected List, got {}", value.type_name()),
                    "right-hand side must be a list",
                    1,
                ));
            };
            if rest.is_none() && elems.len() > items.len() {
                return Err(shell.pm_err(
                    format!("need {} values, got {}", elems.len(), items.len()),
                    "use [..., ...rest] to capture remaining elements",
                    1,
                ));
            }
            for (i, pat) in elems.iter().enumerate() {
                if i < items.len() {
                    assign_pattern(pat, &items[i], shell)?;
                }
            }
            if let Some(name) = rest {
                shell.set(
                    name.clone(),
                    Value::List(items.get(elems.len()..).unwrap_or(&[]).to_vec()),
                );
            }
            Ok(())
        }
        Pattern::Map(entries) => {
            let Value::Map(pairs) = value else {
                return Err(shell.pm_err(
                    format!("expected Map, got {}", value.type_name()),
                    "right-hand side must be a map",
                    1,
                ));
            };
            for (key, pat, default) in entries {
                let found = pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v);
                let val = match (found, default) {
                    (Some(v), _) => v.clone(),
                    (None, Some(def)) => {
                        let comp = crate::elaborator::elaborate(
                            std::slice::from_ref(def),
                            Default::default(),
                        );
                        eval_comp(&comp, shell)?
                    }
                    (None, None) => {
                        let ks: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
                        return Err(shell.pm_err(
                            format!("key '{key}' not found"),
                            format!("available: {}", ks.join(", ")),
                            1,
                        ));
                    }
                };
                assign_pattern(pat, &val, shell)?;
            }
            Ok(())
        }
    }
}
