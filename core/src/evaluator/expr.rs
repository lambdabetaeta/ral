//! Arithmetic, comparison, negation, and subscripting: the leaves `comp.rs`
//! dispatches `CompKind::Binary`, `CompKind::Negate`, `CompKind::Not`, and
//! `CompKind::Index` to.

use super::val::close;
use crate::ir::Val;
use crate::syntax::ast::{ArithOp, BinaryOp, BinaryOpKind, CompareOp, EqOp};
use crate::types::{Break, Env, Error, Settled, Value};

// ── Indexing ─────────────────────────────────────────────────────────────

/// Index a `List` by non-negative `Int`, or a `Map` by `String` key. Pure:
/// both operands are already-closed values, so no environment is needed.
pub(crate) fn index_value(val: &Value, key: &Value) -> Result<Value, Error> {
    match val {
        Value::List(items) => {
            let idx: usize = key
                .as_int()
                .and_then(|i| usize::try_from(i).ok())
                .ok_or_else(|| {
                    Error::new(
                        format!(
                            "list index must be a non-negative Int, got {} '{key}'",
                            key.type_name()
                        ),
                        1,
                    )
                    .with_hint("list indices are zero-based integers")
                })?;
            items.get(idx).cloned().ok_or_else(|| {
                Error::new(
                    format!(
                        "index {idx} out of bounds for list of length {}",
                        items.len()
                    ),
                    1,
                )
                .with_hint(if items.is_empty() {
                    "the list is empty".to_string()
                } else {
                    format!("valid indices: 0..{}", items.len() - 1)
                })
            })
        }
        Value::Map(m) => {
            let key_str = match key {
                Value::String(s) => s.as_str(),
                _ => {
                    return Err(Error::new(
                        format!("map key must be a String, got {} '{key}'", key.type_name()),
                        1,
                    )
                    .with_hint("use str to convert"));
                }
            };
            m.get(key_str).cloned().ok_or_else(|| {
                let ks: Vec<&str> = m.keys().map(String::as_str).collect();
                let hint = if ks.is_empty() {
                    "the map is empty".to_string()
                } else {
                    format!("available: {}", ks.join(", "))
                };
                Error::new(format!("key '{key_str}' not found"), 1).with_hint(hint)
            })
        }
        _ => Err(Error::new(format!("cannot index into {}", val.type_name()), 1)
            .with_hint("indexing requires a List or Map")),
    }
}

// ── Primitive ops ────────────────────────────────────────────────────────

/// Negation of a `Bool`, and only a `Bool`: nothing else is truthy here.
pub(crate) fn eval_not(val: &Val, env: &Env) -> Result<Value, Error> {
    match close(val, env)? {
        Value::Bool(b) => Ok(Value::Bool(!b)),
        other => Err(Error::new(
            format!("not: expected Bool, got {} '{}'", other.type_name(), other),
            1,
        )
        .with_hint("use a comparison or explicit Bool")),
    }
}

/// `-v` on a number, `Int` overflow-checked as [`arithmetic`] is.
pub(crate) fn eval_negate(val: &Val, env: &Env) -> Result<Value, Error> {
    let v = close(val, env)?;
    require_numeric(&v)?;
    match v {
        Value::Int(n) => n
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| Error::new(format!("integer overflow: -{n} exceeds i64 range"), 1)),
        // Cleared `require_numeric` and is not an `Int`, so `as_float` holds.
        other => Ok(Value::Float(-other.as_float().unwrap())),
    }
}

/// Arithmetic, comparison, or equality; both operands evaluate, left first.
pub(crate) fn eval_binary(op: BinaryOp, lhs: &Val, rhs: &Val, env: &Env) -> Settled<Value> {
    let l = close(lhs, env).map_err(Break::from)?;
    let r = close(rhs, env).map_err(Break::from)?;
    binop(&l, op, &r)
}

fn require_numeric(val: &Value) -> Result<(), Error> {
    match val {
        Value::Int(_) | Value::Float(_) => Ok(()),
        _ => Err(Error::new(
            format!(
                "expected Int or Float in arithmetic, got {} '{}'",
                val.type_name(),
                val
            ),
            1,
        )
        .with_hint("use int or float to convert")),
    }
}

/// `==`/`!=` share `values_equal` with the `equal` builtin, so comparing a pair
/// of closures errors on both surfaces instead of answering a non-reflexive
/// `false`.
fn binop(l: &Value, op: BinaryOp, r: &Value) -> Settled<Value> {
    match op.kind() {
        BinaryOpKind::Eq(EqOp::Eq) => Ok(Value::Bool(crate::builtins::util::values_equal(l, r)?)),
        BinaryOpKind::Eq(EqOp::Ne) => Ok(Value::Bool(!crate::builtins::util::values_equal(l, r)?)),
        BinaryOpKind::Compare(c) => compare(l, c, r),
        BinaryOpKind::Arith(a) => Ok(arithmetic(l, a, r)?),
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
fn arithmetic(l: &Value, op: ArithOp, r: &Value) -> Result<Value, Error> {
    let div_zero = || Error::new("division by zero", 1);
    let mod_zero = || Error::new("modulo by zero", 1);
    require_numeric(l)?;
    require_numeric(r)?;
    if let (Value::Int(a), Value::Int(b)) = (l, r) {
        let overflow =
            || Error::new(format!("integer overflow: {a} and {b} exceed i64 range"), 1);
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
        let v = match op {
            ArithOp::Add => a + b,
            ArithOp::Sub => a - b,
            ArithOp::Mul => a * b,
            ArithOp::Div if b == 0.0 => return Err(div_zero()),
            ArithOp::Div => a / b,
            ArithOp::Mod => {
                return Err(Error::new("% requires Int operands", 1).with_hint("use int to convert"));
            }
        };
        // Finite operands with a nonzero divisor can still overflow to ±∞,
        // and a Float is finite by construction.
        if v.is_finite() {
            Ok(Value::Float(v))
        } else {
            Err(Error::new(format!("float overflow: {a} and {b} exceed f64 range"), 1))
        }
    }
}
