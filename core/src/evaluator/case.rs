//! Runtime for the `case` sum eliminator.
//!
//! Typechecking guarantees that the scrutinee is a variant whose row is
//! covered by the handler table, so this code's missing-handler branch is
//! an internal error rather than a user-facing failure.

use super::{eval_comp, trampoline};
use crate::ir::Comp;
use crate::types::{EvalSignal, Shell, Value};

/// Evaluate `case scrutinee table`: force the matching handler thunk on
/// the variant's payload (or `Unit` for nullary tags).
pub(crate) fn eval_case(
    scrutinee: &Comp,
    table: &Comp,
    shell: &mut Shell,
) -> Result<Value, EvalSignal> {
    let scrutinee_val = eval_comp(scrutinee, shell)?;
    let (label, payload) = match scrutinee_val {
        Value::Variant { label, payload } => (label, payload),
        other => {
            return Err(shell.err(
                format!(
                    "case: scrutinee must be a variant, got {} {}",
                    other.type_name(),
                    other
                ),
                1,
            ));
        }
    };

    let table_val = eval_comp(table, shell)?;
    let entries = match table_val {
        Value::Map(entries) => entries,
        other => {
            return Err(shell.err(
                format!(
                    "case: handler table must be a tag-keyed record, got {}",
                    other.type_name()
                ),
                1,
            ));
        }
    };

    let key = format!(".{label}");
    let handler = entries
        .into_iter()
        .find_map(|(k, v)| if k == key { Some(v) } else { None })
        .ok_or_else(|| {
            shell.err(
                format!("case: no handler for variant .{label} (typechecker bug)"),
                1,
            )
        })?;

    let payload_val = match payload {
        Some(p) => *p,
        None => Value::Unit,
    };
    trampoline(handler, vec![payload_val], shell)
}
