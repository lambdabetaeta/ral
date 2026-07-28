//! Runtime for the `case` sum eliminator.
//!
//! Exhaustiveness is checked only when the handler table is a record
//! literal; an opaque table reaches the missing-handler branch here instead.

use super::apply;
use super::val::eval_val;
use crate::ir::Val;
use crate::syntax::tag::tag_row_label;
use crate::types::{Mooring, Raw, Shell, Tail, TailCall, Value};

/// Evaluate `case scrutinee table`: apply the matching handler to the
/// variant's payload, or to `Unit` for a nullary tag — handlers are unary.
///
/// A granted [`Tail::Yes`] passes to the handler as a [`TailCall`], the same
/// hand-off `eval_app` in `call.rs` makes for an application.
pub(crate) fn eval_case(
    scrutinee: &Val,
    table: &Val,
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    let scrutinee_val = eval_val(scrutinee, shell)?;
    let (label, payload) = match scrutinee_val {
        Value::Variant { label, payload } => (label, payload),
        other => {
            return Err(shell
                .err(
                    format!(
                        "case: scrutinee must be a variant, got {} {}",
                        other.type_name(),
                        other
                    ),
                    1,
                )
                .into());
        }
    };

    let table_val = eval_val(table, shell)?;
    let entries = match table_val {
        Value::Map(entries) => entries,
        other => {
            return Err(shell
                .err(
                    format!(
                        "case: handler table must be a tag-keyed record, got {}",
                        other.type_name()
                    ),
                    1,
                )
                .into());
        }
    };

    let key = tag_row_label(&label);
    let handler = entries.get(&key).cloned().ok_or_else(|| {
        shell.err(
            format!("case: no handler for variant `{label}` (typechecker bug)"),
            1,
        )
    })?;

    let payload_val = match payload {
        Some(p) => *p,
        None => Value::Unit,
    };

    if tail == Tail::Yes {
        return Err(TailCall {
            callee: handler,
            args: vec![payload_val],
        }
        .into());
    }

    apply(handler, vec![payload_val], mooring, shell).map_err(Into::into)
}
