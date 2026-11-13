//! ral's structural equality and exact numeric order.

use crate::data::Finite;
use crate::fact::{CompareFailure, EqualityOrOrder, EvalFailure, Reason, ValueShape};
use crate::ir::{VClosRef, ValClos, View};
use crate::shell::runtime::{Error, Settled};
use std::cmp::Ordering;

fn cmp_int_float(i: i64, f: Finite) -> Ordering {
    let Some(whole) = f.to_i64() else {
        return if f.get() > 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    };
    // `f - trunc(f)` is never `-0.0`.
    i.cmp(&whole)
        .then_with(|| 0.0_f64.total_cmp(&(f.get() - f.get().trunc())))
}
fn all(pairs: impl Iterator<Item = Settled<bool>>) -> Settled<bool> {
    for eq in pairs {
        if !eq? {
            return Ok(false);
        }
    }
    Ok(true)
}
fn comparison_error(
    left: ValueShape,
    right: ValueShape,
    operation: &str,
    relation: EqualityOrOrder,
) -> Error {
    Error::new(Reason::Evaluation(Box::new(EvalFailure::Compare(
        CompareFailure {
            operation: operation.into(),
            relation,
            left,
            right,
        },
    ))))
}
impl VClosRef<'_> {
    fn number_order(self, other: Self) -> Option<Ordering> {
        match (self.view(), other.view()) {
            (View::Int(a), View::Int(b)) => Some(a.cmp(&b)),
            (View::Int(a), View::Float(b)) => Some(cmp_int_float(a, b)),
            (View::Float(a), View::Int(b)) => Some(cmp_int_float(b, a).reverse()),
            (View::Float(a), View::Float(b)) => Some(a.cmp(&b)),
            _ => None,
        }
    }
    /// # Errors
    /// Either side is a block, function or handle.
    pub(crate) fn equals(self, other: Self) -> Settled<bool> {
        let (left_labels, left) = self.peel_variants();
        let (right_labels, right) = other.peel_variants();
        if left_labels.iter().zip(&right_labels).any(|(a, b)| a != b) {
            return Ok(false);
        }
        let shapes = match left_labels.len().cmp(&right_labels.len()) {
            Ordering::Less if left.opacity().is_some() => Some((left.shape(), ValueShape::Variant)),
            Ordering::Greater if right.opacity().is_some() => {
                Some((ValueShape::Variant, right.shape()))
            }
            Ordering::Less | Ordering::Greater => return Ok(false),
            Ordering::Equal => None,
        };
        if let Some((left, right)) = shapes {
            return Err(
                comparison_error(left, right, "equality", EqualityOrOrder::Equality).into(),
            );
        }
        // An absent tag payload short-circuits before inspecting the other payload.
        if !left_labels.is_empty() && left.is_unit() != right.is_unit() {
            return Ok(false);
        }
        Ok(match (left.view(), right.view()) {
            (View::Unit, View::Unit) => true,
            (View::Bool(a), View::Bool(b)) => a == b,
            (View::Int(_) | View::Float(_), View::Int(_) | View::Float(_)) => {
                left.number_order(right) == Some(Ordering::Equal)
            }
            (View::String(a), View::String(b)) => a == b,
            (View::Bytes(a), View::Bytes(b)) => a == b,
            (View::List(a), View::List(b)) => {
                a.len() == b.len() && all(a.iter().zip(b.iter()).map(|(a, b)| a.equals(b)))?
            }
            (View::Map(a), View::Map(b)) => {
                a.len() == b.len()
                    && all(a
                        .iter()
                        .zip(b.iter())
                        .map(|((ka, a), (kb, b))| Ok(ka == kb && a.equals(b)?)))?
            }
            _ if left.opacity().is_some() || right.opacity().is_some() => {
                return Err(comparison_error(
                    left.shape(),
                    right.shape(),
                    "equality",
                    EqualityOrOrder::Equality,
                )
                .into());
            }
            _ => false,
        })
    }
    /// # Errors
    /// Anything but numbers or two strings.
    pub(crate) fn compare(self, other: Self, op: &str) -> Settled<Ordering> {
        if let Some(order) = self.number_order(other) {
            return Ok(order);
        }
        match (self.view(), other.view()) {
            (View::String(a), View::String(b)) => Ok(a.cmp(b)),
            _ => Err(
                comparison_error(self.shape(), other.shape(), op, EqualityOrOrder::Order).into(),
            ),
        }
    }
}
impl ValClos {
    pub(crate) fn equals(&self, other: &Self) -> Settled<bool> {
        VClosRef::from(self).equals(other.into())
    }
    pub(crate) fn compare(&self, other: &Self, op: &str) -> Settled<Ordering> {
        VClosRef::from(self).compare(other.into(), op)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::{Env, block_over};
    fn float(f: f64) -> ValClos {
        ValClos::float(Finite::new(f).expect("finite"))
    }
    const TWO_53: i64 = 9_007_199_254_740_992;
    fn cmp(a: &ValClos, b: &ValClos) -> Ordering {
        a.compare(b, "test").expect("numbers order")
    }
    #[test]
    fn mixed_int_float_order_is_exact_past_2_53() {
        let (lo, hi, f) = (
            ValClos::int(TWO_53),
            ValClos::int(TWO_53 + 1),
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
        assert_eq!(
            cmp(&ValClos::int(i64::MAX), &float(9_223_372_036_854_775_808.0)),
            Ordering::Less
        );
        assert_eq!(
            cmp(
                &ValClos::int(i64::MIN),
                &float(-9_223_372_036_854_775_808.0)
            ),
            Ordering::Equal
        );
        assert_eq!(cmp(&ValClos::int(0), &float(0.5)), Ordering::Less);
        assert_eq!(cmp(&ValClos::int(-1), &float(-0.5)), Ordering::Less);
        assert!(ValClos::int(3).equals(&float(3.0)).unwrap());
    }
    #[test]
    fn a_suspension_refuses_even_nested_but_a_first_false_stops_the_walk() {
        let block = block_over(&Env::EMPTY);
        let item = |n| ValClos::list(vec![ValClos::int(n), block.clone()]);
        assert!(item(1).equals(&item(1)).is_err());
        assert!(!item(1).equals(&item(2)).unwrap());
        assert!(block.equals(&ValClos::int(1)).is_err());
    }
}
