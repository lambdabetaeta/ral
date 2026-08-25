//! Numeric rounding builtins.
//!
//! All four take a Float only, since an integer is already rounded.  `round`
//! stays in Float even at zero places: `round 3.7 0` is `4.0`.

use crate::types::{Settled, Value, fmt_float, sig, sig_hint};

use super::util::f64_to_i64;

/// The largest `places` for which `10^places` is still finite in `f64`.
const MAX_PLACES: i64 = 308;

/// Both arms are defensive: a Float is finite by construction, and
/// `round`'s and `float_to_int`'s schemes in the typechecker admit a Float
/// only.
fn finite_float(name: &str, val: &Value) -> Settled<f64> {
    match val {
        Value::Float(f) if f.is_finite() => Ok(*f),
        Value::Float(f) => Err(sig(format!(
            "{name}: {} is not a finite number",
            fmt_float(*f)
        ))),
        other => Err(sig_hint(
            format!("{name}: expected Float, got {}", other.type_name()),
            "e.g. round 3.7 0",
        )),
    }
}

fn to_int(name: &str, args: &[Value], op: fn(f64) -> f64) -> Settled<Value> {
    let x = finite_float(name, &args[0])?;
    Ok(Value::Int(f64_to_i64(name, op(x))?))
}

pub(super) fn builtin_round(args: &[Value]) -> Settled<Value> {
    let x = finite_float("round", &args[0])?;
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
    let r = (x * factor).round() / factor;
    if r.is_finite() {
        Ok(Value::Float(r))
    } else {
        Err(sig(format!(
            "round: rounding {} to {places} places is not representable as a Float",
            fmt_float(x)
        )))
    }
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
