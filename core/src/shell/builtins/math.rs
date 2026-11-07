//! Numeric rounding builtins.
//!
//! All four take a Float only, since an integer is already rounded.  `round`
//! stays in Float even at zero places: `round 3.7 0` is `4.0`.

use crate::data::Finite;
use crate::error::Error;
use crate::shell::runtime::{Settled, Value};

/// The largest `places` for which `10^places` is still finite in `f64`.
const MAX_PLACES: i64 = 308;

fn float(name: &str, val: &Value) -> Settled<Finite> {
    match val {
        Value::Float(f) => Ok(*f),
        other => Err(
            Error::new(format!("{name}: expected Float, got {}", other.type_name()))
                .with_hint("e.g. round 3.7 0")
                .into(),
        ),
    }
}

fn to_int(name: &str, args: &[Value], op: fn(f64) -> f64) -> Settled<Value> {
    let x = float(name, &args[0])?;
    Finite::new(op(x.get()))
        .and_then(Finite::to_i64)
        .map(Value::Int)
        .ok_or_else(|| Error::new(format!("{name}: {x} is outside the integer range")).into())
}

pub(super) fn builtin_round(args: &[Value]) -> Settled<Value> {
    let x = float("round", &args[0])?;
    let places = match &args[1] {
        Value::Int(n) => *n,
        other => {
            return Err(Error::new(format!(
                "round: places must be an Int, got {}",
                other.type_name()
            ))
            .with_hint("e.g. round 3.14159 2")
            .into());
        }
    };
    if !(0..=MAX_PLACES).contains(&places) {
        return Err(Error::new(format!(
            "round: places must be between 0 and {MAX_PLACES}, got {places}"
        ))
        .with_hint("0 rounds to a whole number, 2 to hundredths")
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
            Error::new(format!(
                "round: rounding {x} to {places} places is not representable as a Float"
            ))
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
