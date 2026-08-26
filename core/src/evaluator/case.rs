//! Runtime for the `case` sum eliminator.
//!
//! Arms are syntax, so the alternatives are fixed before the program runs and
//! the checker has already proved they cover the scrutinee's row: this rule
//! selects a branch and runs it, exactly as `if` picks one of two.

use super::comp::{eval_comp, with_scope};
use super::pattern;
use super::val::close;
use crate::ir::{CaseArm, Val};
use crate::syntax::tag::tag_row_label;
use crate::types::{Mooring, Raw, Shell, Tail, Value};

/// Evaluate `case scrutinee [arms]`: bind the variant's payload — `Unit` for a
/// nullary tag — to the matching arm's pattern in a fresh scope, and run that
/// arm's body there.
///
/// The body is a branch, not a function the runtime applies: it inherits the
/// `case`'s own tail position, so a tail call in an arm still escapes to the
/// trampoline, and it inherits the ambient control state — the status at arm
/// entry is the one evaluating the scrutinee left, and every mutation the body
/// makes outlives the `case`, as an `if` branch's does.  Only the lexical scope
/// the pattern binds into is fresh.
pub(crate) fn eval_case(
    scrutinee: &Val,
    arms: &[CaseArm],
    tail: Tail,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Raw<Value> {
    let (label, payload) = match close(scrutinee, &shell.mobile.scope)? {
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

    // Unreachable from source: the checker closes the scrutinee's row to the
    // arms' label set. It remains for the variant that reaches here untyped —
    // one decoded at a boundary, or an IR re-entered from a live value — for
    // which silence would be the worse answer.
    let arm = arms
        .iter()
        .find(|arm| arm.tag.item == label)
        .ok_or_else(|| {
            let handled: Vec<String> = arms.iter().map(|a| tag_row_label(&a.tag.item)).collect();
            shell.err(
                format!(
                    "case: no arm for variant `{label}`; this case matches: {}",
                    handled.join(", ")
                ),
                1,
            )
        })?;

    let payload = payload.map_or(Value::Unit, |p| *p);
    with_scope(shell, |shell| {
        let env = pattern::bind_pattern(
            &arm.pattern,
            &payload,
            &[],
            shell.mobile.scope.clone(),
            mooring,
            shell,
        )?;
        shell.mobile.scope = env;
        eval_comp(arm.body.comp(), mooring, shell, tail)
    })
}
