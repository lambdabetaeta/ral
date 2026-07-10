//! Numeric rounding builtins.
//!
//! `round <x> <places>` rounds a Float to `places` decimal places (halves
//! away from zero) and always returns a Float — `round 3.7 0` is `4.0`, not
//! `4`; reach for `int` when you want the whole number.  `floor`, `ceil`, and
//! `trunc` take one Float and return the Int in their direction.
//!
//! All four accept a Float only — an Int is rejected at the type level, since
//! an integer is already rounded.  A non-finite value (NaN, ±∞) is refused,
//! and an `Int`-producing result that leaves the `i64` range is refused
//! rather than silently corrupted: `NaN as i64` is `0` and an out-of-range
//! cast saturates, both of which would misreport the input.

use crate::types::{Settled, Value, sig, sig_hint};

use super::util::{check_arity, f64_to_i64};

/// The most decimal places `f64` can carry before `10^places` overflows to
/// infinity; beyond ~15 significant digits the extra places are noise.
const MAX_PLACES: i64 = 308;

/// Read a finite Float argument.  The type checker already constrains the
/// argument to `Float`, so the non-Float arm is defensive; the finiteness
/// check is the real gate — NaN and ±∞ have no meaningful rounding.
fn finite_float(name: &str, val: &Value) -> Settled<f64> {
    match val {
        Value::Float(f) if f.is_finite() => Ok(*f),
        Value::Float(f) => Err(sig(format!("{name}: {f} is not a finite number"))),
        other => Err(sig_hint(
            format!("{name}: expected Float, got {}", other.type_name()),
            "e.g. round 3.7 0",
        )),
    }
}

fn to_int(name: &str, args: &[Value], op: fn(f64) -> f64) -> Settled<Value> {
    check_arity(args, 1, name)?;
    let x = finite_float(name, &args[0])?;
    Ok(Value::Int(f64_to_i64(name, op(x))?))
}

pub(super) fn builtin_round(args: &[Value]) -> Settled<Value> {
    check_arity(args, 2, "round")?;
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
            "round: rounding {x} to {places} places is not representable as a Float"
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
