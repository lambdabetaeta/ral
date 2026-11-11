//! Fixed operations on values, and the three reads.

use super::{assemble, val::form};
use crate::data::Finite;
use crate::fact::{
    Access, ArithmeticFailure, BinaryArithmeticOp, DivOrMod, EvalFailure, Expected,
    FloatBinaryOverflow, FloatOverflow, IndexBounds, IntegerBinaryOverflow, IntegerOverflow,
    InvariantFailure, KeyOrField, MissingEntry, Reason, Unreadable, Utf8Subject, ValueShape,
};
use crate::io;
use crate::ir::{ArithOp, BinaryOp, CompareOp, EqOp};
use crate::ir::{Key, Prim, Val};
use crate::shell::runtime::{Break, Env, Error, Map, Settled, Signature, Value};
use std::borrow::Cow;

// ── Reads ────────────────────────────────────────────────────────────────

/// `$r[ℓ]`: a record's field.
pub(crate) fn field(val: &Value, label: &str) -> Result<Value, Error> {
    match val {
        Value::Map(fields) => entry(fields, label, KeyOrField::Field),
        _ => Err(unreadable(val, Access::Field(label.into()))),
    }
}

/// `$xs[n]`: a list's element, counting from 0.
pub(crate) fn position(val: &Value, n: usize) -> Result<Value, Error> {
    let Value::List(items) = val else {
        return Err(unreadable(val, Access::Position(n.into())));
    };
    items.get(n).map(Cow::into_owned).ok_or_else(|| {
        evaluation(EvalFailure::IndexBounds(IndexBounds {
            index: n.into(),
            length: items.len().into(),
        }))
    })
}

/// `$m[k]`: a map's value at a computed key.
pub(crate) fn lookup(val: &Value, key: &Value) -> Result<Value, Error> {
    let Value::String(key) = key else {
        return Err(key.expected("lookup", Expected::Shape(ValueShape::String)));
    };
    match val {
        Value::Map(m) => entry(m, key, KeyOrField::Key),
        _ => Err(unreadable(val, Access::Key(key.to_string()))),
    }
}

fn entry(map: &Map, key: &str, kind: KeyOrField) -> Result<Value, Error> {
    map.get(key).map(Cow::into_owned).ok_or_else(|| {
        evaluation(EvalFailure::MissingEntry(MissingEntry {
            kind,
            key: key.into(),
            available: map.keys().map(str::to_owned).collect(),
        }))
    })
}

fn unreadable(val: &Value, access: Access) -> Error {
    evaluation(EvalFailure::Unreadable(Unreadable {
        access,
        found: val.found(),
    }))
}

fn evaluation(cause: EvalFailure) -> Error {
    Error::new(Reason::Evaluation(Box::new(cause)))
}

fn arithmetic_error(cause: ArithmeticFailure) -> Error {
    evaluation(EvalFailure::Arithmetic(cause))
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
                return Err(Error::new(Reason::Invariant(Box::new(
                    InvariantFailure::CaptureNotBytes(value.shape()),
                )))
                .into());
            };
            let mut bytes = bytes.into_vec();
            bytes.truncate(io::without_terminator(&bytes).len());
            Value::string(crate::shell::runtime::decode_utf8_strict(
                bytes,
                "capture",
                Utf8Subject::CapturedOutput,
            )?)
        }
    })
}

fn render(value: &Value) -> Result<String, Error> {
    if let Some(text) = value.scalar_text() {
        return Ok(text);
    }
    Err(evaluation(EvalFailure::Interpolate(value.shape())))
}

/// Negation of a `Bool`, and only a `Bool`: nothing else is truthy here.
fn eval_not(val: &Val, env: &Env, sig: &Signature) -> Result<Value, Error> {
    match form(val, env, sig)? {
        Value::Bool(b) => Ok(Value::Bool(!b)),
        other => Err(other.expected("not", Expected::Shape(ValueShape::Bool))),
    }
}

/// `-v` on a number, `Int` overflow-checked as [`arithmetic`] is.
fn eval_negate(val: &Val, env: &Env, sig: &Signature) -> Result<Value, Error> {
    match form(val, env, sig)? {
        Value::Int(n) => n.checked_neg().map(Value::Int).ok_or_else(|| {
            arithmetic_error(ArithmeticFailure::IntegerOverflow(IntegerOverflow::Negate(
                n,
            )))
        }),
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
    val.expected(
        "arithmetic",
        Expected::AnyOf(vec![ValueShape::Int, ValueShape::Float]),
    )
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
    let div_zero = || arithmetic_error(ArithmeticFailure::ZeroDivisor(DivOrMod::Div));
    match (l, r) {
        (Value::Int(a), Value::Int(b)) => match (op, *b) {
            (ArithOp::Div, 0) => Err(div_zero()),
            (ArithOp::Mod, 0) => Err(arithmetic_error(ArithmeticFailure::ZeroDivisor(
                DivOrMod::Mod,
            ))),
            _ => op.int(*a, *b).map(Value::Int).ok_or_else(|| {
                arithmetic_error(ArithmeticFailure::IntegerOverflow(IntegerOverflow::Binary(
                    IntegerBinaryOverflow {
                        operator: arithmetic_op(op),
                        left: *a,
                        right: *b,
                    },
                )))
            }),
        },
        _ => match (l.as_float(), r.as_float()) {
            (Some(a), Some(b)) => {
                if op == ArithOp::Div && b == 0.0 {
                    return Err(div_zero());
                }
                let v = op
                    .float(a, b)
                    .ok_or_else(|| arithmetic_error(ArithmeticFailure::ModuloNeedsIntegers))?;
                // Finite operands with a nonzero divisor can still overflow to ±∞.
                Finite::new(v).map(Value::Float).ok_or_else(|| {
                    arithmetic_error(ArithmeticFailure::FloatOverflow(FloatOverflow::Binary(
                        FloatBinaryOverflow {
                            operator: arithmetic_op(op),
                            left: Finite::new(a).expect("finite numeric operand"),
                            right: Finite::new(b).expect("finite numeric operand"),
                        },
                    )))
                })
            }
            (None, _) => Err(not_numeric(l)),
            (_, None) => Err(not_numeric(r)),
        },
    }
}

fn arithmetic_op(op: ArithOp) -> BinaryArithmeticOp {
    match op {
        ArithOp::Add => BinaryArithmeticOp::Add,
        ArithOp::Sub => BinaryArithmeticOp::Subtract,
        ArithOp::Mul => BinaryArithmeticOp::Multiply,
        ArithOp::Div => BinaryArithmeticOp::Divide,
        ArithOp::Mod => BinaryArithmeticOp::Modulo,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::datum::Datum;
    use crate::fact::{Count, FailureCode as _, Found};

    fn eval_reason(error: Error) -> EvalFailure {
        assert_eq!(error.failure_code().get(), 1);
        assert_eq!(error.hints(), []);
        let Reason::Evaluation(failure) = error.reason else {
            panic!("evaluation reason")
        };
        *failure
    }

    #[test]
    fn reads_preserve_access_missing_names_and_lossless_positions() {
        let map = Value::map(vec![("a".into(), Value::Int(1))]);
        for (result, kind) in [
            (field(&map, "b"), KeyOrField::Field),
            (lookup(&map, &Value::string("b")), KeyOrField::Key),
        ] {
            assert_eq!(
                eval_reason(result.unwrap_err()),
                EvalFailure::MissingEntry(MissingEntry {
                    kind,
                    key: "b".into(),
                    available: vec!["a".into()],
                })
            );
        }
        let index = usize::try_from(1u64 << 63).unwrap();
        let reason = eval_reason(position(&Value::list(vec![]), index).unwrap_err());
        assert_eq!(
            reason,
            EvalFailure::IndexBounds(IndexBounds {
                index: Count::from_u64(1u64 << 63),
                length: 0usize.into(),
            })
        );
        assert_eq!(
            EvalFailure::decode(&reason.clone().encode()).unwrap(),
            reason
        );
        for (result, access) in [
            (field(&Value::Unit, "a"), Access::Field("a".into())),
            (
                lookup(&Value::Unit, &Value::string("a")),
                Access::Key("a".into()),
            ),
            (
                position(&Value::Unit, index),
                Access::Position(index.into()),
            ),
        ] {
            assert_eq!(
                eval_reason(result.unwrap_err()),
                EvalFailure::Unreadable(Unreadable {
                    access,
                    found: Found::Unit
                })
            );
        }
        assert!(matches!(
            lookup(&map, &Value::Int(1)).unwrap_err().reason,
            Reason::Invalid(_)
        ));
    }

    #[test]
    fn arithmetic_retains_operators_operands_and_zero_divisor_kind() {
        for (operator, fact, a, b) in [
            (ArithOp::Add, BinaryArithmeticOp::Add, i64::MAX, 1),
            (ArithOp::Sub, BinaryArithmeticOp::Subtract, i64::MIN, 1),
            (ArithOp::Mul, BinaryArithmeticOp::Multiply, i64::MAX, 2),
            (ArithOp::Div, BinaryArithmeticOp::Divide, i64::MIN, -1),
            (ArithOp::Mod, BinaryArithmeticOp::Modulo, i64::MIN, -1),
        ] {
            assert_eq!(
                eval_reason(arithmetic(&Value::Int(a), operator, &Value::Int(b)).unwrap_err()),
                EvalFailure::Arithmetic(ArithmeticFailure::IntegerOverflow(
                    IntegerOverflow::Binary(IntegerBinaryOverflow {
                        operator: fact,
                        left: a,
                        right: b,
                    })
                ))
            );
        }
        for (operator, kind) in [(ArithOp::Div, DivOrMod::Div), (ArithOp::Mod, DivOrMod::Mod)] {
            assert_eq!(
                eval_reason(arithmetic(&Value::Int(1), operator, &Value::Int(0)).unwrap_err()),
                EvalFailure::Arithmetic(ArithmeticFailure::ZeroDivisor(kind))
            );
        }
        let large = Finite::new(f64::MAX).unwrap();
        assert_eq!(
            eval_reason(
                arithmetic(&Value::Float(large), ArithOp::Mul, &Value::Int(2)).unwrap_err()
            ),
            EvalFailure::Arithmetic(ArithmeticFailure::FloatOverflow(FloatOverflow::Binary(
                FloatBinaryOverflow {
                    operator: BinaryArithmeticOp::Multiply,
                    left: large,
                    right: Finite::from(2_i64),
                }
            )))
        );
        assert_eq!(
            eval_reason(
                arithmetic(&Value::Float(large), ArithOp::Mod, &Value::Int(2)).unwrap_err()
            ),
            EvalFailure::Arithmetic(ArithmeticFailure::ModuloNeedsIntegers)
        );
    }

    #[test]
    fn interpolation_keeps_only_the_runtime_shape() {
        for value in [
            Value::Unit,
            Value::bytes(vec![]),
            Value::list(vec![]),
            Value::map(vec![]),
            Value::variant("private-tag", None),
            crate::shell::runtime::block_over(&Env::new()),
        ] {
            assert_eq!(
                eval_reason(render(&value).unwrap_err()),
                EvalFailure::Interpolate(value.shape())
            );
        }
    }
    #[test]
    fn unary_numeric_shape_and_capture_backstops_are_typed() {
        let shell = crate::test_helper::core_shell();
        assert_eq!(
            eval_reason(eval_negate(&Val::Int(i64::MIN), &Env::new(), &shell.sig).unwrap_err()),
            EvalFailure::Arithmetic(ArithmeticFailure::IntegerOverflow(IntegerOverflow::Negate(
                i64::MIN
            )))
        );
        for result in [
            eval_not(&Val::Int(1), &Env::new(), &shell.sig),
            eval_negate(&Val::Bool(true), &Env::new(), &shell.sig),
            arithmetic(&Value::Bool(true), ArithOp::Add, &Value::Int(1)),
            arithmetic(&Value::Int(1), ArithOp::Add, &Value::Bool(true)),
        ] {
            assert!(matches!(result.unwrap_err().reason, Reason::Invalid(_)));
        }
        let Err(Break::Error(error)) = eval_prim(&Prim::Decode(Val::Int(1)), Env::new(), &shell)
        else {
            panic!("capture invariant")
        };
        assert_eq!(
            error.reason,
            Reason::Invariant(Box::new(InvariantFailure::CaptureNotBytes(ValueShape::Int)))
        );
        let Err(Break::Error(error)) = crate::shell::evaluator::apply(
            &Value::Int(1),
            vec![],
            &crate::shell::runtime::Mooring::adrift(),
            &mut crate::test_helper::core_shell(),
        ) else {
            panic!("call refusal")
        };
        assert_eq!(
            eval_reason(error),
            EvalFailure::Apply(crate::fact::ApplyFailure {
                operation: crate::fact::CallOrForce::Call,
                found: Found::Int(1)
            })
        );
    }
}
