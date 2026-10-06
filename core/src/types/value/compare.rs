//! ral's equality and order: one pair of `Value` methods, behind `==`, `<`,
//! `equal`, `lt`, `gt`, `sort-list` and the rest.

use super::Value;
use crate::first_order::Finite;
use crate::types::{Settled, sig_hint};
use std::cmp::Ordering;

/// `i` against `f`, exactly: no rounding of `i` into an `f64`.
fn cmp_int_float(i: i64, f: Finite) -> Ordering {
    let Some(whole) = f.to_i64() else {
        return if f.get() > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    };
    // `f - trunc(f)` is never `-0.0`, so `total_cmp` agrees with `==`.
    i.cmp(&whole)
        .then_with(|| 0.0_f64.total_cmp(&(f.get() - f.get().trunc())))
}

/// Conjunction that stops at the first `false` or `Err`, in traversal order.
fn all(pairs: impl Iterator<Item = Settled<bool>>) -> Settled<bool> {
    for eq in pairs {
        if !eq? {
            return Ok(false);
        }
    }
    Ok(true)
}

impl Value {
    /// Numbers by magnitude, `None` for a non-number.
    fn number_order(&self, other: &Self) -> Option<Ordering> {
        match (self, other) {
            (Self::Int(a), Self::Int(b)) => Some(a.cmp(b)),
            (Self::Int(a), Self::Float(b)) => Some(cmp_int_float(*a, *b)),
            (Self::Float(a), Self::Int(b)) => Some(cmp_int_float(*b, *a).reverse()),
            (Self::Float(a), Self::Float(b)) => Some(a.cmp(b)),
            _ => None,
        }
    }

    /// ral's `==`: structural, and a suspension refuses rather than answering
    /// a non-reflexive `false`.
    ///
    /// # Errors
    /// Either side is a block, function or handle.
    pub(crate) fn equals(&self, other: &Self) -> Settled<bool> {
        Ok(match (self, other) {
            (Self::Unit, Self::Unit) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(_) | Self::Float(_), Self::Int(_) | Self::Float(_)) => {
                self.number_order(other) == Some(Ordering::Equal)
            }
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Bytes(a), Self::Bytes(b)) => a == b,
            (Self::List(a), Self::List(b)) => {
                a.len() == b.len() && all(a.iter().zip(b.iter()).map(|(a, b)| a.equals(&b)))?
            }
            // Both sides iterate sorted by key, so a pointwise zip decides it.
            (Self::Map(a), Self::Map(b)) => {
                a.len() == b.len()
                    && all(a
                        .iter()
                        .zip(b.iter())
                        .map(|((ka, a), (kb, b))| Ok(ka == kb && a.equals(&b)?)))?
            }
            (
                Self::Variant {
                    label: la,
                    payload: pa,
                },
                Self::Variant {
                    label: lb,
                    payload: pb,
                },
            ) => {
                la == lb
                    && match (pa, pb) {
                        (Some(a), Some(b)) => a.equals(b)?,
                        (None, None) => true,
                        _ => false,
                    }
            }
            (a, b) if a.opacity().is_some() || b.opacity().is_some() => {
                return Err(sig_hint(
                    format!(
                        "equal: cannot compare {} with {}",
                        a.type_name(),
                        b.type_name()
                    ),
                    "equality is defined on scalars, strings, bytes, lists, maps, and variants",
                ));
            }
            _ => false,
        })
    }

    /// ral's order: numbers by magnitude, exactly across Int and Float, and
    /// strings lexicographically.  `op` names the asker in the error.
    ///
    /// # Errors
    /// Anything else.
    pub(crate) fn compare(&self, other: &Self, op: &str) -> Settled<Ordering> {
        if let Some(order) = self.number_order(other) {
            return Ok(order);
        }
        match (self, other) {
            (Self::String(a), Self::String(b)) => Ok(a.cmp(b)),
            _ => Err(sig_hint(
                format!(
                    "{op}: cannot compare {} with {}",
                    self.type_name(),
                    other.type_name()
                ),
                "ordering is defined on numbers and strings",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Env, block_over};

    fn float(f: f64) -> Value {
        Value::Float(Finite::new(f).expect("finite"))
    }

    const TWO_53: i64 = 9_007_199_254_740_992;

    fn cmp(a: &Value, b: &Value) -> Ordering {
        a.compare(b, "test").expect("numbers order")
    }

    /// B7: promotion to `f64` made `2^53+1 == 2^53.0 == 2^53 < 2^53+1`.
    #[test]
    fn mixed_int_float_order_is_exact_past_2_53() {
        let (lo, hi, f) = (
            Value::Int(TWO_53),
            Value::Int(TWO_53 + 1),
            float(9_007_199_254_740_992.0),
        );
        assert_eq!(cmp(&lo, &f), Ordering::Equal);
        assert_eq!(cmp(&lo, &hi), Ordering::Less);
        assert_eq!(cmp(&f, &hi), Ordering::Less);
        assert_eq!(cmp(&hi, &f), Ordering::Greater);
        assert!(!hi.equals(&f).unwrap());
    }

    #[test]
    fn the_ends_of_the_integer_range_meet_their_floats() {
        let top = float(9_223_372_036_854_775_808.0);
        assert_eq!(cmp(&Value::Int(i64::MAX), &top), Ordering::Less);
        let bottom = float(-9_223_372_036_854_775_808.0);
        assert_eq!(cmp(&Value::Int(i64::MIN), &bottom), Ordering::Equal);
        assert_eq!(cmp(&Value::Int(0), &float(0.5)), Ordering::Less);
        assert_eq!(cmp(&Value::Int(-1), &float(-0.5)), Ordering::Less);
        assert!(Value::Int(3).equals(&float(3.0)).unwrap());
    }

    #[test]
    fn a_suspension_refuses_even_nested_but_a_first_false_stops_the_walk() {
        let block = block_over(&Env::new());
        let item = |n: i64| Value::list(vec![Value::Int(n), block.clone()]);
        assert!(item(1).equals(&item(1)).is_err());
        assert!(!item(1).equals(&item(2)).unwrap());
        assert!(block.equals(&Value::Int(1)).is_err());
    }
}
