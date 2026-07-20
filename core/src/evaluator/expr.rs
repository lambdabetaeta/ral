//! Primitive operators and value indexing.
//!
//! [`eval_not`] and [`eval_binary`] evaluate the primitive operator nodes
//! (arithmetic, comparison, boolean negation) produced by the elaborator's
//! expression desugaring.
//! [`index_value`] implements `CompKind::Index` — subscripting into lists by
//! integer position and into maps by string key.

use super::val::eval_val;
use crate::ir::Val;
use crate::syntax::ast::{ArithOp, BinaryOp, BinaryOpKind, CompareOp, EqOp};
use crate::types::{Break, Error, Settled, Shell, Value};

// ── Indexing ─────────────────────────────────────────────────────────────

/// Index into a composite value.
///
/// Lists are indexed by non-negative `Int`; maps by `String` key.
/// Out-of-bounds or missing-key errors carry a hint listing valid indices
/// or available keys.
pub(crate) fn index_value(val: &Value, key: &Value, shell: &Shell) -> Result<Value, Error> {
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
        Value::Map(m) => {
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
            m.get(key_str).cloned().ok_or_else(|| {
                let ks: Vec<&str> = m.keys().map(String::as_str).collect();
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

/// Evaluate a `CompKind::Not(v)` — logical negation on a `Bool` value.
pub(crate) fn eval_not(val: &Val, shell: &mut Shell) -> Result<Value, Error> {
    match eval_val(val, shell)? {
        Value::Bool(b) => Ok(Value::Bool(!b)),
        other => Err(shell.err_hint(
            format!("not: expected Bool, got {} '{}'", other.type_name(), other),
            "use a comparison or explicit Bool",
            1,
        )),
    }
}

/// Evaluate a `CompKind::Binary(op, lhs, rhs)` — arithmetic, comparison,
/// or structural equality on two values.  Arity is encoded in the IR
/// variant, so no runtime arity guard is needed.
pub(crate) fn eval_binary(op: BinaryOp, lhs: &Val, rhs: &Val, shell: &mut Shell) -> Settled<Value> {
    let l = eval_val(lhs, shell).map_err(Break::from)?;
    let r = eval_val(rhs, shell).map_err(Break::from)?;
    binop(&l, op, &r, shell)
}

/// Ensure `val` is `Int` or `Float`; error otherwise.
fn require_numeric(val: &Value, shell: &Shell) -> Result<(), Error> {
    match val {
        Value::Int(_) | Value::Float(_) => Ok(()),
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

/// Evaluate a binary operation, dispatching on the operator category.
///
/// - `Eq`/`Ne`: structural equality on any value pair.
/// - `Lt`/`Gt`/`Le`/`Ge`: ordering via [`value_ordering`](crate::builtins::util::value_ordering)
///   — Int·Int as `i64`, mixed Int/Float as f64, String·String lexicographic.
/// - `Add`/`Sub`/`Mul`/`Div`/`Mod`: arithmetic on numerics.  Integer
///   ops check overflow; division/modulo guard against zero; `%` is
///   integer-only.
fn binop(l: &Value, op: BinaryOp, r: &Value, shell: &Shell) -> Settled<Value> {
    match op.kind() {
        // Route through `values_equal` — the same definition `equal`
        // uses — so `==`/`!=` and `equal` cannot disagree: a
        // computation pair (closures, blocks, handles) raises the same
        // "cannot compare" error on both surfaces instead of `==`
        // silently answering `false` for a non-reflexive `f == f`.
        BinaryOpKind::Eq(EqOp::Eq) => Ok(Value::Bool(crate::builtins::util::values_equal(l, r)?)),
        BinaryOpKind::Eq(EqOp::Ne) => Ok(Value::Bool(!crate::builtins::util::values_equal(l, r)?)),
        BinaryOpKind::Compare(c) => compare(l, c, r),
        BinaryOpKind::Arith(a) => Ok(arithmetic(l, a, r, shell)?),
    }
}

/// Ordering on the four [`CompareOp`] variants via
/// [`value_ordering`](crate::builtins::util::value_ordering), which owns
/// the comparison taxonomy.
fn compare(l: &Value, op: CompareOp, r: &Value) -> Settled<Value> {
    let want: fn(std::cmp::Ordering) -> bool = match op {
        CompareOp::Lt => std::cmp::Ordering::is_lt,
        CompareOp::Gt => std::cmp::Ordering::is_gt,
        CompareOp::Le => std::cmp::Ordering::is_le,
        CompareOp::Ge => std::cmp::Ordering::is_ge,
    };
    let order = crate::builtins::util::value_ordering(l, r, "comparison")?;
    Ok(Value::Bool(want(order)))
}

/// Arithmetic on the five [`ArithOp`] variants.  `Mod` rejects float
/// operands; `Div` / `Mod` reject a zero divisor; integer ops are
/// overflow-checked.
fn arithmetic(l: &Value, op: ArithOp, r: &Value, shell: &Shell) -> Result<Value, Error> {
    let div_zero = || shell.err("division by zero", 1);
    let mod_zero = || shell.err("modulo by zero", 1);
    require_numeric(l, shell)?;
    require_numeric(r, shell)?;
    if let (Value::Int(a), Value::Int(b)) = (l, r) {
        let overflow = || shell.err(format!("integer overflow: {a} and {b} exceed i64 range"), 1);
        Ok(match op {
            ArithOp::Add => a.checked_add(*b).map(Value::Int).ok_or_else(overflow)?,
            ArithOp::Sub => a.checked_sub(*b).map(Value::Int).ok_or_else(overflow)?,
            ArithOp::Mul => a.checked_mul(*b).map(Value::Int).ok_or_else(overflow)?,
            ArithOp::Div if *b == 0 => return Err(div_zero()),
            ArithOp::Div => a.checked_div(*b).map(Value::Int).ok_or_else(overflow)?,
            ArithOp::Mod if *b == 0 => return Err(mod_zero()),
            ArithOp::Mod => a.checked_rem(*b).map(Value::Int).ok_or_else(overflow)?,
        })
    } else {
        let a = l.as_float().unwrap();
        let b = r.as_float().unwrap();
        Ok(match op {
            ArithOp::Add => Value::Float(a + b),
            ArithOp::Sub => Value::Float(a - b),
            ArithOp::Mul => Value::Float(a * b),
            ArithOp::Div if b == 0.0 => return Err(div_zero()),
            ArithOp::Div => Value::Float(a / b),
            ArithOp::Mod => {
                return Err(shell.err_hint("% requires Int operands", "use int to convert", 1));
            }
        })
    }
}
