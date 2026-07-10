//! Runtime for the `case` sum eliminator.
//!
//! Typechecking guarantees that the scrutinee is a variant whose row is
//! covered by the handler table, so this code's missing-handler branch is
//! an internal error rather than a user-facing failure.
//!
//! ## Tail-call optimisation
//!
//! When the eliminator is handed [`Tail::Yes`] — its enclosing
//! computation granted it the tail position — the matched handler is
//! returned as a [`TailCall`] signal rather than being applied via a
//! fresh trampoline frame.  The surrounding trampoline loop catches the
//! signal and continues in O(1) host stack frames — the same direct-emit
//! path [`eval_app`](super::call) takes for a tail-positioned
//! application.

use super::apply;
use super::val::eval_val;
use crate::ir::Val;
use crate::syntax::tag::tag_row_label;
use crate::types::{Raw, Shell, Tail, TailCall, Value};

/// Evaluate `case scrutinee table`: force the matching handler thunk on
/// the variant's payload (or `Unit` for nullary tags).
///
/// The selected arm inherits the case's tail position.
pub(crate) fn eval_case(scrutinee: &Val, table: &Val, tail: Tail, shell: &mut Shell) -> Raw<Value> {
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

    apply(handler, vec![payload_val], shell).map_err(Into::into)
}
