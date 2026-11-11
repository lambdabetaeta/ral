//! Numeric rounding builtins.
//!
//! All four take a Float only, since an integer is already rounded.  `round`
//! stays in Float even at zero places: `round 3.7 0` is `4.0`.

use crate::data::Finite;
use crate::error::Error;
use crate::fact::{
    ArithmeticFailure, EvalFailure, Expected, InvalidCause, InvalidFailure, Number, NumberRange,
    Reason, RoundUnrepresentable, ShapeFailure, ValueShape,
};
use crate::shell::runtime::invalid;
use crate::shell::runtime::{Settled, Value};

/// The largest `places` for which `10^places` is still finite in `f64`.
const MAX_PLACES: i64 = 308;

fn float(name: &str, val: &Value) -> Settled<Finite> {
    match val {
        Value::Float(f) => Ok(*f),
        other => Err(other
            .expected(name, Expected::Shape(ValueShape::Float))
            .into()),
    }
}

fn to_int(name: &str, args: &[Value], op: fn(f64) -> f64) -> Settled<Value> {
    let x = float(name, &args[0])?;
    Finite::new(op(x.get()))
        .and_then(Finite::to_i64)
        .map(Value::Int)
        .ok_or_else(|| invalid(name, InvalidCause::IntegerRange(x)).into())
}

pub(super) fn builtin_round(args: &[Value]) -> Settled<Value> {
    let x = float("round", &args[0])?;
    let places = match &args[1] {
        Value::Int(n) => *n,
        other => {
            return Err(Error::new(Reason::Invalid(Box::new(
                InvalidFailure::new(
                    "round".into(),
                    InvalidCause::Shape(ShapeFailure {
                        expected: Expected::Shape(ValueShape::Int),
                        found: other.found(),
                    }),
                )
                .index(1usize.into()),
            )))
            .into());
        }
    };
    if !(0..=MAX_PLACES).contains(&places) {
        return Err(Error::new(Reason::Invalid(Box::new(
            InvalidFailure::new(
                "round".into(),
                InvalidCause::Range(NumberRange {
                    value: Number::Int(places),
                    minimum: Number::Int(0),
                    maximum: Number::Int(MAX_PLACES),
                }),
            )
            .index(1usize.into()),
        )))
        .into());
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "places is range-checked to 0..=MAX_PLACES just above"
    )]
    let factor = 10f64.powi(places as i32);
    Finite::new((x.get() * factor).round() / factor)
        .map(Value::Float)
        .ok_or_else(|| {
            Error::new(Reason::Evaluation(Box::new(EvalFailure::Arithmetic(
                ArithmeticFailure::RoundUnrepresentable(RoundUnrepresentable { value: x, places }),
            ))))
            .into()
        })
}

pub(super) fn builtin_floor(args: &[Value]) -> Settled<Value> {
    to_int("floor", args, f64::floor)
}

pub(super) fn builtin_ceil(args: &[Value]) -> Settled<Value> {
    to_int("ceil", args, f64::ceil)
}

pub(super) fn builtin_trunc(args: &[Value]) -> Settled<Value> {
    to_int("trunc", args, f64::trunc)
}
