//! Primitive operators and value indexing.
//!
//! [`eval_primop`] evaluates `PrimOp` nodes (arithmetic, comparison, boolean
//! negation) produced by the elaborator's expression desugaring.
//! [`index_value`] implements `Comp::Index` — subscripting into lists by
//! integer position and into maps by string key.

use super::eval_val;
use crate::ast::ExprOp;
use crate::ir::Val;
use crate::types::*;

// ── Indexing ─────────────────────────────────────────────────────────────

/// Index into a composite value.
///
/// Lists are indexed by non-negative `Int`; maps by `String` key.
/// Out-of-bounds or missing-key errors carry a hint listing valid indices
/// or available keys.
pub(crate) fn index_value(val: &Value, key: &Value, shell: &Shell) -> Result<Value, EvalSignal> {
    match val {
        Value::List(items) => {
            let idx: usize = key
                .as_int()
                .and_then(|i| usize::try_from(i).ok())
                .ok_or_else(|| {
                    shell.err_hint(
                        format!(
                            "list index must be a non-negative Int, got {} '{key}'",
                            key.type_name()
                        ),
                        "list indices are zero-based integers",
                        1,
                    )
                })?;
            items.get(idx).cloned().ok_or_else(|| {
                shell.err_hint(
                    format!(
                        "index {idx} out of bounds for list of length {}",
                        items.len()
                    ),
                    if items.is_empty() {
                        "the list is empty".into()
                    } else {
                        format!("valid indices: 0..{}", items.len() - 1)
                    },
                    1,
                )
            })
        }
        Value::Map(pairs) => {
            let key_str = match key {
                Value::String(s) => s.as_str(),
                _ => {
                    return Err(shell.err_hint(
                        format!("map key must be a String, got {} '{key}'", key.type_name()),
                        "use str to convert",
                        1,
                    ));
                }
            };
            pairs
                .iter()
                .find(|(k, _)| k == key_str)
                .map(|(_, v)| v.clone())
                .ok_or_else(|| {
                    let ks: Vec<&str> = pairs.iter().map(|(k, _)| k.as_str()).collect();
                    let hint = if ks.is_empty() {
                        "the map is empty".into()
                    } else {
                        format!("available: {}", ks.join(", "))
                    };
                    shell.err_hint(format!("key '{key_str}' not found"), hint, 1)
                })
        }
        _ => Err(shell.err_hint(
            format!("cannot index into {}", val.type_name()),
            "indexing requires a List or Map",
            1,
        )),
    }
}

// ── Primitive ops ────────────────────────────────────────────────────────

/// Dispatch a `Comp::PrimOp`. `Not` is unary; all other `ExprOp`s are binary.
pub(crate) fn eval_primop(op: ExprOp, args: &[Val], shell: &mut Shell) -> Result<Value, EvalSignal> {
    match op {
        ExprOp::Not => {
            debug_assert_eq!(args.len(), 1, "Not takes one operand");
            match eval_val(&args[0], shell)? {
                Value::Bool(b) => Ok(Value::Bool(!b)),
                other => Err(shell.err_hint(
                    format!("not: expected Bool, got {} '{}'", other.type_name(), other),
                    "use a comparison or explicit Bool",
                    1,
                )),
            }
        }
        _ => {
            debug_assert_eq!(args.len(), 2, "binary operator takes two operands");
            let l = eval_val(&args[0], shell)?;
            let r = eval_val(&args[1], shell)?;
            binop(&l, op, &r, shell)
        }
    }
}

/// Ensure `val` is `Int` or `Float`; error otherwise.
fn require_numeric(val: &Value, shell: &Shell) -> Result<Value, EvalSignal> {
    match val {
        Value::Int(_) | Value::Float(_) => Ok(val.clone()),
        _ => Err(shell.err_hint(
            format!(
                "expected Int or Float in arithmetic, got {} '{}'",
                val.type_name(),
                val
            ),
            "use int or float to convert",
            1,
        )),
    }
}

/// Evaluate a binary operation.
///
/// `Eq`/`Ne` work on any value pair (structural equality).  Relational
/// comparisons (`Lt`, `Gt`, `Le`, `Ge`) require numeric operands and
/// promote to `Float` when mixed.  Arithmetic follows the same promotion
/// rule; integer operations check for overflow and division by zero.
/// `Mod` is integer-only.
fn binop(l: &Value, op: ExprOp, r: &Value, shell: &Shell) -> Result<Value, EvalSignal> {
    let div_zero = || shell.err("division by zero", 1);
    let mod_zero = || shell.err("modulo by zero", 1);
    match op {
        ExprOp::Eq => return Ok(Value::Bool(l == r)),
        ExprOp::Ne => return Ok(Value::Bool(l != r)),
        _ => {}
    }
    match op {
        ExprOp::Lt | ExprOp::Gt | ExprOp::Le | ExprOp::Ge => {
            let a = require_numeric(l, shell)?.as_float().unwrap();
            let b = require_numeric(r, shell)?.as_float().unwrap();
            return Ok(Value::Bool(match op {
                ExprOp::Lt => a < b,
                ExprOp::Gt => a > b,
                ExprOp::Le => a <= b,
                ExprOp::Ge => a >= b,
                _ => unreachable!(),
            }));
        }
        _ => {}
    }
    let lv = require_numeric(l, shell)?;
    let rv = require_numeric(r, shell)?;
    match (&lv, &rv) {
        (Value::Int(a), Value::Int(b)) => {
            let overflow = || shell.err(format!("integer overflow: {a} and {b} exceed i64 range"), 1);
            Ok(match op {
                ExprOp::Add => a.checked_add(*b).map(Value::Int).ok_or_else(overflow)?,
                ExprOp::Sub => a.checked_sub(*b).map(Value::Int).ok_or_else(overflow)?,
                ExprOp::Mul => a.checked_mul(*b).map(Value::Int).ok_or_else(overflow)?,
                ExprOp::Div => {
                    if *b == 0 {
                        return Err(div_zero());
                    }
                    Value::Int(a / b)
                }
                ExprOp::Mod => {
                    if *b == 0 {
                        return Err(mod_zero());
                    }
                    Value::Int(a % b)
                }
                _ => unreachable!("non-arithmetic op slipped through"),
            })
        }
        _ => {
            let a = lv.as_float().unwrap();
            let b = rv.as_float().unwrap();
            Ok(match op {
                ExprOp::Add => Value::Float(a + b),
                ExprOp::Sub => Value::Float(a - b),
                ExprOp::Mul => Value::Float(a * b),
                ExprOp::Div => {
                    if b == 0.0 {
                        return Err(div_zero());
                    }
                    Value::Float(a / b)
                }
                ExprOp::Mod => {
                    return Err(shell.err_hint("% requires Int operands", "use int to convert", 1));
                }
                _ => unreachable!("non-arithmetic op slipped through"),
            })
        }
    }
}
