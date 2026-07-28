//! Arithmetic, comparison, negation, and subscripting: the leaves `comp.rs`
//! dispatches `CompKind::Binary`, `CompKind::Not`, and `CompKind::Index` to.

use super::val::eval_val;
use crate::ir::Val;
use crate::syntax::ast::{ArithOp, BinaryOp, BinaryOpKind, CompareOp, EqOp};
use crate::types::{Break, Error, Settled, Shell, Value};

// ── Indexing ─────────────────────────────────────────────────────────────

/// Index a `List` by non-negative `Int`, or a `Map` by `String` key.
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

/// Negation of a `Bool`, and only a `Bool`: nothing else is truthy here.
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

/// Arithmetic, comparison, or equality; both operands evaluate, left first.
pub(crate) fn eval_binary(op: BinaryOp, lhs: &Val, rhs: &Val, shell: &mut Shell) -> Settled<Value> {
    let l = eval_val(lhs, shell).map_err(Break::from)?;
    let r = eval_val(rhs, shell).map_err(Break::from)?;
    binop(&l, op, &r, shell)
}

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

/// `==`/`!=` share `values_equal` with the `equal` builtin, so comparing a pair
/// of closures errors on both surfaces instead of answering a non-reflexive
/// `false`.
fn binop(l: &Value, op: BinaryOp, r: &Value, shell: &Shell) -> Settled<Value> {
    match op.kind() {
        BinaryOpKind::Eq(EqOp::Eq) => Ok(Value::Bool(crate::builtins::util::values_equal(l, r)?)),
        BinaryOpKind::Eq(EqOp::Ne) => Ok(Value::Bool(!crate::builtins::util::values_equal(l, r)?)),
        BinaryOpKind::Compare(c) => compare(l, c, r),
        BinaryOpKind::Arith(a) => Ok(arithmetic(l, a, r, shell)?),
    }
}

/// Ordering is `builtins::util::value_ordering`, shared with `lt`, `gt`, and
/// `sort-list`, which owns the comparison taxonomy.
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

/// Int·Int stays in `i64` and is overflow-checked; one Float operand promotes
/// both, and there `%` is refused rather than given IEEE remainder semantics.
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
        // Both operands cleared `require_numeric`, so `as_float` cannot fail.
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
