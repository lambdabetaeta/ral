//! Numeric rounding builtins.
//!
//! All four take a Float only, since an integer is already rounded.  `round`
//! stays in Float even at zero places: `round 3.7 0` is `4.0`.

use crate::first_order::Finite;
use crate::types::{Settled, Value, sig, sig_hint};

/// The largest `places` for which `10^places` is still finite in `f64`.
const MAX_PLACES: i64 = 308;

fn float(name: &str, val: &Value) -> Settled<Finite> {
    match val {
        Value::Float(f) => Ok(*f),
        other => Err(sig_hint(
            format!("{name}: expected Float, got {}", other.type_name()),
            "e.g. round 3.7 0",
        )),
    }
}

fn to_int(name: &str, args: &[Value], op: fn(f64) -> f64) -> Settled<Value> {
    let x = float(name, &args[0])?;
    Finite::new(op(x.get()))
        .and_then(Finite::to_i64)
        .map(Value::Int)
        .ok_or_else(|| sig(format!("{name}: {x} is outside the integer range")))
}

pub(super) fn builtin_round(args: &[Value]) -> Settled<Value> {
    let x = float("round", &args[0])?;
    let places = match &args[1] {
        Value::Int(n) => *n,
        other => {
            return Err(sig_hint(
                format!("round: places must be an Int, got {}", other.type_name()),
                "e.g. round 3.14159 2",
            ));
        }
    };
    if !(0..=MAX_PLACES).contains(&places) {
        return Err(sig_hint(
            format!("round: places must be between 0 and {MAX_PLACES}, got {places}"),
            "0 rounds to a whole number, 2 to hundredths",
        ));
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "places is range-checked to 0..=MAX_PLACES just above"
    )]
    let factor = 10f64.powi(places as i32);
    Finite::new((x.get() * factor).round() / factor)
        .map(Value::Float)
        .ok_or_else(|| {
            sig(format!(
                "round: rounding {x} to {places} places is not representable as a Float"
            ))
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
