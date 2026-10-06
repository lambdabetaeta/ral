//! Arithmetic, comparison, negation, and subscripting: the leaves
//! `machine::eval_rules` dispatches `CompKind::Binary`, `CompKind::Negate`,
//! `CompKind::Not`, and `CompKind::Index` to.

use super::val::form;
use crate::first_order::Finite;
use crate::ir::Val;
use crate::ir::{ArithOp, BinaryOp, CompareOp, EqOp};
use crate::types::{Break, Env, Error, Settled, Signature, Value};

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
                    Error::new(format!(
                        "list index must be a non-negative Int, got {} '{key}'",
                        key.type_name()
                    ))
                    .with_hint("list indices are zero-based integers")
                })?;
            items
                .get(idx)
                .map(std::borrow::Cow::into_owned)
                .ok_or_else(|| {
                    Error::new(format!(
                        "index {idx} out of bounds for list of length {}",
                        items.len()
                    ))
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
                    return Err(Error::new(format!(
                        "map key must be a String, got {} '{key}'",
                        key.type_name()
                    ))
                    .with_hint("use str to convert"));
                }
            };
            m.get(key_str)
                .map(std::borrow::Cow::into_owned)
                .ok_or_else(|| {
                    let ks: Vec<&str> = m.keys().collect();
                    let hint = if ks.is_empty() {
                        "the map is empty".to_string()
                    } else {
                        format!("available: {}", ks.join(", "))
                    };
                    Error::new(format!("key '{key_str}' not found")).with_hint(hint)
                })
        }
        _ => Err(Error::new(format!("cannot index into {}", val.type_name()))
            .with_hint("indexing requires a List or Map")),
    }
}

// ── Primitive ops ────────────────────────────────────────────────────────

/// Negation of a `Bool`, and only a `Bool`: nothing else is truthy here.
pub(crate) fn eval_not(val: &Val, env: &Env, sig: &Signature) -> Result<Value, Error> {
    match form(val, env, sig)? {
        Value::Bool(b) => Ok(Value::Bool(!b)),
        other => Err(Error::new(format!(
            "not: expected Bool, got {} '{}'",
            other.type_name(),
            other
        ))
        .with_hint("use a comparison or explicit Bool")),
    }
}

/// `-v` on a number, `Int` overflow-checked as [`arithmetic`] is.
pub(crate) fn eval_negate(val: &Val, env: &Env, sig: &Signature) -> Result<Value, Error> {
    match form(val, env, sig)? {
        Value::Int(n) => n
            .checked_neg()
            .map(Value::Int)
            .ok_or_else(|| Error::new(format!("integer overflow: -{n} exceeds i64 range"))),
        Value::Float(f) => Ok(Value::Float(-f)),
        other => Err(not_numeric(&other)),
    }
}

/// Arithmetic, comparison, or equality; both operands evaluate, left first.
pub(crate) fn eval_binary(
    op: BinaryOp,
    lhs: &Val,
    rhs: &Val,
    env: &Env,
    sig: &Signature,
) -> Settled<Value> {
    let l = form(lhs, env, sig).map_err(Break::from)?;
    let r = form(rhs, env, sig).map_err(Break::from)?;
    binop(&l, op, &r)
}

/// The one wording for a non-numeric operand, shared by `-v` and by
/// `arithmetic`'s two-operand check.
fn not_numeric(val: &Value) -> Error {
    Error::new(format!(
        "expected Int or Float in arithmetic, got {} '{}'",
        val.type_name(),
        val
    ))
    .with_hint("use int or float to convert")
}

/// `==`/`!=` are [`Value::equals`], as `equal` is, so comparing a pair of
/// closures errors on both surfaces instead of answering a non-reflexive
/// `false`.
fn binop(l: &Value, op: BinaryOp, r: &Value) -> Settled<Value> {
    match op {
        BinaryOp::Eq(EqOp::Eq) => Ok(Value::Bool(l.equals(r)?)),
        BinaryOp::Eq(EqOp::Ne) => Ok(Value::Bool(!l.equals(r)?)),
        BinaryOp::Compare(c) => compare(l, c, r),
        BinaryOp::Arith(a) => Ok(arithmetic(l, a, r)?),
    }
}

/// Ordering is [`Value::compare`], shared with `sort-list`.
fn compare(l: &Value, op: CompareOp, r: &Value) -> Settled<Value> {
    Ok(Value::Bool(op.holds(l.compare(r, "comparison")?)))
}

/// Int·Int stays in `i64` and is overflow-checked; one Float operand promotes
/// both, and there `%` is refused rather than given IEEE remainder semantics.
fn arithmetic(l: &Value, op: ArithOp, r: &Value) -> Result<Value, Error> {
    let div_zero = || Error::new("division by zero");
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => match (op, *b) {
            (ArithOp::Div, 0) => Err(div_zero()),
            (ArithOp::Mod, 0) => Err(Error::new("modulo by zero")),
            _ => op.int(*a, *b).map(Value::Int).ok_or_else(|| {
                Error::new(format!("integer overflow: {a} and {b} exceed i64 range"))
            }),
        },
        _ => match (l.as_float(), r.as_float()) {
            (Some(a), Some(b)) => {
                if op == ArithOp::Div && b == 0.0 {
                    return Err(div_zero());
                }
                let v = op.float(a, b).ok_or_else(|| {
                    Error::new("% requires Int operands").with_hint("use int to convert")
                })?;
                // Finite operands with a nonzero divisor can still overflow to ±∞.
                Finite::new(v).map(Value::Float).ok_or_else(|| {
                    Error::new(format!("float overflow: {a} and {b} exceed f64 range"))
                })
            }
            (None, _) => Err(not_numeric(l)),
            (_, None) => Err(not_numeric(r)),
        },
    }
}
