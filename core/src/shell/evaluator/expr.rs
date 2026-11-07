//! Fixed operations on values, and the three reads.

use super::{assemble, val::form};
use crate::data::Finite;
use crate::io;
use crate::ir::{ArithOp, BinaryOp, CompareOp, EqOp};
use crate::ir::{Key, Prim, Val};
use crate::shell::runtime::{Break, Env, Error, Map, Settled, Signature, Value};
use std::borrow::Cow;

// ── Reads ────────────────────────────────────────────────────────────────

/// `$r[ℓ]`: a record's field.
pub(crate) fn field(val: &Value, label: &str) -> Result<Value, Error> {
    match val {
        Value::Map(fields) => entry(fields, label, "field", "record"),
        _ => Err(unreadable(
            val,
            &format!("the field `{label}`"),
            "a field is read from a record",
        )),
    }
}

/// `$xs[n]`: a list's element, counting from 0.
pub(crate) fn position(val: &Value, n: usize) -> Result<Value, Error> {
    let Value::List(items) = val else {
        return Err(unreadable(
            val,
            &format!("the position {n}"),
            "a position is read from a list",
        ));
    };
    items.get(n).map(Cow::into_owned).ok_or_else(|| {
        Error::new(format!(
            "index {n} out of bounds for list of length {}",
            items.len()
        ))
        .with_hint(if items.is_empty() {
            "the list is empty".to_string()
        } else {
            format!("valid indices: 0..{}", items.len() - 1)
        })
    })
}

/// `$m[k]`: a map's value at a computed key.
pub(crate) fn lookup(val: &Value, key: &Value) -> Result<Value, Error> {
    let Value::String(key) = key else {
        return Err(Error::new(format!(
            "map key must be a String, got {} '{key}'",
            key.type_name()
        ))
        .with_hint("use str to convert"));
    };
    match val {
        Value::Map(m) => entry(m, key, "key", "map"),
        _ => Err(unreadable(
            val,
            &format!("the key '{key}'"),
            "a computed key reads a map",
        )),
    }
}

fn entry(map: &Map, key: &str, what: &str, whole: &str) -> Result<Value, Error> {
    map.get(key).map(Cow::into_owned).ok_or_else(|| {
        let ks: Vec<&str> = map.keys().collect();
        let hint = if ks.is_empty() {
            format!("the {whole} is empty")
        } else {
            format!("available: {}", ks.join(", "))
        };
        Error::new(format!("{what} '{key}' not found")).with_hint(hint)
    })
}

fn unreadable(val: &Value, what: &str, rule: &str) -> Error {
    Error::new(format!(
        "cannot read {what} of a value of type {}",
        val.type_name()
    ))
    .with_hint(rule)
}

// ── Primitive ops ────────────────────────────────────────────────────────

pub(crate) fn eval_prim(
    prim: &Prim,
    env: Env,
    shell: &crate::shell::runtime::Shell,
) -> Settled<Value> {
    let sig = &shell.sig;
    Ok(match prim {
        Prim::Binary(op, lhs, rhs) => eval_binary(*op, lhs, rhs, &env, sig)?,
        Prim::Negate(value) => eval_negate(value, &env, sig)?,
        Prim::Not(value) => eval_not(value, &env, sig)?,
        Prim::Index { target, key } => {
            let value = form(target, &env, sig)?;
            match &key.item {
                Key::Label(label) => field(&value, label),
                Key::Position(n) => position(&value, *n),
                Key::Data(key) => lookup(&value, &form(key, &env, sig)?),
            }?
        }
        Prim::Update { base, fields } => assemble::eval_update(base, fields, &env, sig)?,
        Prim::List(elems) => assemble::eval_list(elems, &env, sig)?,
        Prim::Map(entries) => assemble::eval_map(entries, &env, shell)?,
        Prim::Render(value) => Value::string(render(&form(value, &env, sig)?)?),
        Prim::Concat(parts) => {
            let mut text = String::new();
            for value in parts {
                text.push_str(form(value, &env, sig)?.as_str("concat")?);
            }
            Value::string(text)
        }
        Prim::Decode(value) => {
            let value = form(value, &env, sig)?;
            // Release the capture's scope before taking over its buffer.
            drop(env);
            let Value::Bytes(bytes) = value else {
                return Err(Error::new(format!(
                    "the value boundary was handed {} where a captured byte payload belongs",
                    value.type_name()
                ))
                .with_hint(
                    "only the type checker writes this step, so this is a fault in ral \
                     rather than in your program: please report the source that produced it",
                )
                .into());
            };
            let mut bytes = bytes.into_vec();
            bytes.truncate(io::without_terminator(&bytes).len());
            Value::string(crate::shell::runtime::decode_utf8_strict(
                bytes,
                "captured output is not valid UTF-8 text",
                "to keep it as bytes, pipe it: `… | from-bytes`",
            )?)
        }
    })
}

fn render(value: &Value) -> Result<String, Error> {
    if let Some(text) = value.scalar_text() {
        return Ok(text);
    }
    match value {
        Value::Unit => Err(Error::new("cannot interpolate Unit in string")
            .with_hint("`()` is nothing to print; to print the text, write '()'")),
        Value::Bytes(_) => Err(Error::new("cannot interpolate Bytes in string")
            .with_hint("render with str (lossy UTF-8), or decode with from-string")),
        _ => Err(Error::new(format!(
            "cannot interpolate {} in string",
            value.type_name()
        ))
        .with_hint("use str to convert, or index into the value")),
    }
}

/// Negation of a `Bool`, and only a `Bool`: nothing else is truthy here.
fn eval_not(val: &Val, env: &Env, sig: &Signature) -> Result<Value, Error> {
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
fn eval_negate(val: &Val, env: &Env, sig: &Signature) -> Result<Value, Error> {
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
fn eval_binary(op: BinaryOp, lhs: &Val, rhs: &Val, env: &Env, sig: &Signature) -> Settled<Value> {
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

/// `==`/`!=` use [`Value::equals`]; closures refuse comparison.
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
