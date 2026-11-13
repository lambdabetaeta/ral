//! Fixed operations on values, and the three reads.

use super::assemble;
use crate::data::Finite;
use crate::fact::{
    Access, ArithmeticFailure, BinaryArithmeticOp, DivOrMod, EvalFailure, Expected,
    FloatBinaryOverflow, FloatOverflow, IndexBounds, IntegerBinaryOverflow, IntegerOverflow,
    InvariantFailure, KeyOrField, MissingEntry, Reason, Unreadable, Utf8Subject, ValueShape,
};
use crate::io;
use crate::ir::{ArithOp, BinaryOp, CompareOp, EqOp};
use crate::ir::{Env, Key, MapRef, Prim, VClosRef, Val, ValClos, View};
use crate::shell::runtime::{Break, Error, Settled, Signature};

// ── Reads ────────────────────────────────────────────────────────────────

/// `$r[ℓ]`: a record's field.
pub(crate) fn field(val: &ValClos, label: &str) -> Result<ValClos, Error> {
    match val.view() {
        View::Map(fields) => entry(fields, label, KeyOrField::Field),
        _ => Err(unreadable(val, Access::Field(label.into()))),
    }
}

/// `$xs[n]`: a list's element, counting from 0.
pub(crate) fn position(val: &ValClos, n: usize) -> Result<ValClos, Error> {
    let View::List(items) = val.view() else {
        return Err(unreadable(val, Access::Position(n.into())));
    };
    items.get(n).map(VClosRef::into_owned).ok_or_else(|| {
        evaluation(EvalFailure::IndexBounds(IndexBounds {
            index: n.into(),
            length: items.len().into(),
        }))
    })
}

/// `$m[k]`: a map's value at a computed key.
pub(crate) fn lookup(val: &ValClos, key: &ValClos) -> Result<ValClos, Error> {
    let View::String(key) = key.view() else {
        return Err(key.expected("lookup", Expected::Shape(ValueShape::String)));
    };
    match val.view() {
        View::Map(m) => entry(m, key, KeyOrField::Key),
        _ => Err(unreadable(val, Access::Key(key.to_string()))),
    }
}

fn entry(map: MapRef<'_>, key: &str, kind: KeyOrField) -> Result<ValClos, Error> {
    map.get(key).map(VClosRef::into_owned).ok_or_else(|| {
        evaluation(EvalFailure::MissingEntry(MissingEntry {
            kind,
            key: key.into(),
            available: map.keys().map(str::to_owned).collect(),
        }))
    })
}

fn unreadable(val: &ValClos, access: Access) -> Error {
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
) -> Settled<ValClos> {
    let sig = &shell.sig;
    Ok(match prim {
        Prim::Binary(op, lhs, rhs) => eval_binary(*op, lhs, rhs, &env, sig)?,
        Prim::Negate(value) => eval_negate(value, &env, sig)?,
        Prim::Not(value) => eval_not(value, &env, sig)?,
        Prim::Index { target, key } => {
            let value = ValClos::close(target, &env, sig)?;
            match &key.item {
                Key::Label(label) => field(&value, label),
                Key::Position(n) => position(&value, *n),
                Key::Data(key) => lookup(&value, &ValClos::close(key, &env, sig)?),
            }?
        }
        Prim::Update { base, fields } => assemble::eval_update(base, fields, &env, sig)?,
        Prim::List(elems) => assemble::eval_list(elems, &env, sig)?,
        Prim::Map(entries) => assemble::eval_map(entries, &env, shell)?,
        Prim::Render(value) => ValClos::string(render(&ValClos::close(value, &env, sig)?)?),
        Prim::Concat(parts) => {
            let mut text = String::new();
            for value in parts {
                text.push_str(ValClos::close(value, &env, sig)?.as_str("concat")?);
            }
            ValClos::string(text)
        }
        Prim::Decode(value) => {
            let value = ValClos::close(value, &env, sig)?;
            // Release the capture's scope before taking over its buffer.
            drop(env);
            let View::Bytes(bytes) = value.view() else {
                return Err(Error::new(Reason::Invariant(Box::new(
                    InvariantFailure::CaptureNotBytes(value.shape()),
                )))
                .into());
            };
            let bytes = bytes.clone();
            drop(value);
            let mut bytes = bytes.into_vec();
            bytes.truncate(io::without_terminator(&bytes).len());
            ValClos::string(crate::shell::runtime::decode_utf8_strict(
                bytes,
                "capture",
                Utf8Subject::CapturedOutput,
            )?)
        }
    })
}

fn render(value: &ValClos) -> Result<String, Error> {
    if let Some(text) = value.scalar_text() {
        return Ok(text);
    }
    Err(evaluation(EvalFailure::Interpolate(value.shape())))
}

/// Negation of a `Bool`, and only a `Bool`: nothing else is truthy here.
fn eval_not(val: &Val, env: &Env, sig: &Signature) -> Result<ValClos, Error> {
    let value = ValClos::close(val, env, sig)?;
    match value.view() {
        View::Bool(b) => Ok(ValClos::bool(!b)),
        _ => Err(value.expected("not", Expected::Shape(ValueShape::Bool))),
    }
}

/// `-v` on a number, `Int` overflow-checked as [`arithmetic`] is.
fn eval_negate(val: &Val, env: &Env, sig: &Signature) -> Result<ValClos, Error> {
    let value = ValClos::close(val, env, sig)?;
    match value.view() {
        View::Int(n) => n.checked_neg().map(ValClos::int).ok_or_else(|| {
            arithmetic_error(ArithmeticFailure::IntegerOverflow(IntegerOverflow::Negate(
                n,
            )))
        }),
        View::Float(f) => Ok(ValClos::float(-f)),
        _ => Err(not_numeric(&value)),
    }
}

/// Arithmetic, comparison, or equality; both operands evaluate, left first.
fn eval_binary(op: BinaryOp, lhs: &Val, rhs: &Val, env: &Env, sig: &Signature) -> Settled<ValClos> {
    let l = ValClos::close(lhs, env, sig).map_err(Break::from)?;
    let r = ValClos::close(rhs, env, sig).map_err(Break::from)?;
    binop(&l, op, &r)
}

/// The one wording for a non-numeric operand, shared by `-v` and by
/// `arithmetic`'s two-operand check.
fn not_numeric(val: &ValClos) -> Error {
    val.expected(
        "arithmetic",
        Expected::AnyOf(vec![ValueShape::Int, ValueShape::Float]),
    )
}

/// `==`/`!=` use [`ValClos::equals`]; closures refuse comparison.
fn binop(l: &ValClos, op: BinaryOp, r: &ValClos) -> Settled<ValClos> {
    match op {
        BinaryOp::Eq(EqOp::Eq) => Ok(ValClos::bool(l.equals(r)?)),
        BinaryOp::Eq(EqOp::Ne) => Ok(ValClos::bool(!l.equals(r)?)),
        BinaryOp::Compare(c) => compare(l, c, r),
        BinaryOp::Arith(a) => Ok(arithmetic(l, a, r)?),
    }
}

/// Ordering is [`ValClos::compare`], shared with `sort-list`.
fn compare(l: &ValClos, op: CompareOp, r: &ValClos) -> Settled<ValClos> {
    Ok(ValClos::bool(op.holds(l.compare(r, "comparison")?)))
}

/// Int·Int stays in `i64` and is overflow-checked; one Float operand promotes
/// both, and there `%` is refused rather than given IEEE remainder semantics.
fn arithmetic(l: &ValClos, op: ArithOp, r: &ValClos) -> Result<ValClos, Error> {
    let div_zero = || arithmetic_error(ArithmeticFailure::ZeroDivisor(DivOrMod::Div));
    match (l.view(), r.view()) {
        (View::Int(a), View::Int(b)) => match (op, b) {
            (ArithOp::Div, 0) => Err(div_zero()),
            (ArithOp::Mod, 0) => Err(arithmetic_error(ArithmeticFailure::ZeroDivisor(
                DivOrMod::Mod,
            ))),
            _ => op.int(a, b).map(ValClos::int).ok_or_else(|| {
                arithmetic_error(ArithmeticFailure::IntegerOverflow(IntegerOverflow::Binary(
                    IntegerBinaryOverflow {
                        operator: arithmetic_op(op),
                        left: a,
                        right: b,
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
                Finite::new(v).map(ValClos::float).ok_or_else(|| {
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
        let map = ValClos::map(vec![("a".into(), ValClos::int(1))]);
        for (result, kind) in [
            (field(&map, "b"), KeyOrField::Field),
            (lookup(&map, &ValClos::string("b")), KeyOrField::Key),
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
        let reason = eval_reason(position(&ValClos::list(vec![]), index).unwrap_err());
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
            (field(&ValClos::UNIT, "a"), Access::Field("a".into())),
            (
                lookup(&ValClos::UNIT, &ValClos::string("a")),
                Access::Key("a".into()),
            ),
            (
                position(&ValClos::UNIT, index),
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
            lookup(&map, &ValClos::int(1)).unwrap_err().reason,
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
                eval_reason(arithmetic(&ValClos::int(a), operator, &ValClos::int(b)).unwrap_err()),
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
                eval_reason(arithmetic(&ValClos::int(1), operator, &ValClos::int(0)).unwrap_err()),
                EvalFailure::Arithmetic(ArithmeticFailure::ZeroDivisor(kind))
            );
        }
        let large = Finite::new(f64::MAX).unwrap();
        assert_eq!(
            eval_reason(
                arithmetic(&ValClos::float(large), ArithOp::Mul, &ValClos::int(2)).unwrap_err()
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
                arithmetic(&ValClos::float(large), ArithOp::Mod, &ValClos::int(2)).unwrap_err()
            ),
            EvalFailure::Arithmetic(ArithmeticFailure::ModuloNeedsIntegers)
        );
    }

    #[test]
    fn interpolation_keeps_only_the_runtime_shape() {
        for value in [
            ValClos::UNIT,
            ValClos::bytes(vec![]),
            ValClos::list(vec![]),
            ValClos::map(vec![]),
            ValClos::variant("private-tag", None),
            crate::ir::block_over(&Env::new()),
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
            arithmetic(&ValClos::bool(true), ArithOp::Add, &ValClos::int(1)),
            arithmetic(&ValClos::int(1), ArithOp::Add, &ValClos::bool(true)),
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
            &ValClos::int(1),
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
