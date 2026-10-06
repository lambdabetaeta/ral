//! A float that is neither NaN nor infinite, by type.

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::cmp::Ordering;
use std::fmt;

/// An `f64` that is finite.  IEEE `==` is an equivalence on such values, so
/// equality is total (and keeps `0.0 == -0.0`); there is no `Hash`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Finite(f64);

impl Eq for Finite {}

impl PartialOrd for Finite {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// From `partial_cmp`, never `total_cmp`, which would order `-0.0` below `0.0`.
impl Ord for Finite {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.partial_cmp(&other.0).unwrap_or(Ordering::Equal)
    }
}

impl Finite {
    #[must_use]
    pub const fn new(f: f64) -> Option<Self> {
        if f.is_finite() { Some(Self(f)) } else { None }
    }

    #[must_use]
    pub const fn get(self) -> f64 {
        self.0
    }

    /// The integer part, if it fits an `i64` (which `as` would saturate).
    #[must_use]
    pub fn to_i64(self) -> Option<i64> {
        // `i64::MAX` is not itself an `f64`; it rounds up to exactly this.
        const BOUND: f64 = 9_223_372_036_854_775_808.0;
        (-BOUND..BOUND).contains(&self.0).then(|| {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "range-checked into [-2^63, 2^63) just above; the cast is exact"
            )]
            let n = self.0 as i64;
            n
        })
    }
}

impl From<i64> for Finite {
    fn from(n: i64) -> Self {
        #[allow(
            clippy::cast_precision_loss,
            reason = "Int→Float coercion; loss beyond 2^53 is intrinsic to an f64 mantissa"
        )]
        Self(n as f64)
    }
}

impl std::ops::Neg for Finite {
    type Output = Self;

    fn neg(self) -> Self {
        Self(-self.0)
    }
}

/// The one printed spelling of a `Float`: ryu's shortest round trip, with the
/// point restored to a bare exponent mantissa (`1e300` becomes `1.0e300`) so
/// the printer's image stays inside the numeral grammar.
impl fmt::Display for Finite {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut buf = ryu::Buffer::new();
        let text = buf.format_finite(self.0);
        match text.split_once('e') {
            Some((mantissa, exp)) if !mantissa.contains('.') => write!(f, "{mantissa}.0e{exp}"),
            _ => f.write_str(text),
        }
    }
}

/// Floats cross as their IEEE-754 bits: JSON has no number for NaN or ±∞,
/// and a decimal text can come back an ULP off.  A word that is no finite
/// float's bits is refused.
impl Serialize for Finite {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u64(self.0.to_bits())
    }
}

impl<'de> Deserialize<'de> for Finite {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let bits = u64::deserialize(d)?;
        Self::new(f64::from_bits(bits)).ok_or_else(|| {
            serde::de::Error::custom(format!("{bits:#018x} are the bits of a NaN or an infinity"))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finite(f: f64) -> Finite {
        Finite::new(f).expect("finite")
    }

    #[test]
    fn only_finite_floats_construct() {
        assert!(Finite::new(f64::NAN).is_none());
        assert!(Finite::new(f64::INFINITY).is_none());
        assert!(Finite::new(f64::NEG_INFINITY).is_none());
        assert!(Finite::new(-0.0).is_some());
    }

    /// B5, B6: bits, exactly; and no `u64` is a float unless it is a finite one.
    #[test]
    fn serde_round_trips_by_bits_and_refuses_the_rest() {
        for f in [-0.0, 1.5, f64::MIN_POSITIVE, f64::MAX, 0.1] {
            let json = serde_json::to_vec(&finite(f)).expect("serialise");
            let back: Finite = serde_json::from_slice(&json).expect("deserialise");
            assert_eq!(back.get().to_bits(), f.to_bits());
        }
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let json = serde_json::to_vec(&f.to_bits()).expect("serialise");
            assert!(serde_json::from_slice::<Finite>(&json).is_err());
        }
    }

    #[test]
    fn zero_has_one_sign_of_equality_and_order() {
        assert_eq!(finite(0.0), finite(-0.0));
        assert_eq!(finite(0.0).cmp(&finite(-0.0)), Ordering::Equal);
    }

    #[test]
    fn to_i64_refuses_what_as_would_saturate() {
        assert_eq!(finite(-3.9).to_i64(), Some(-3));
        assert_eq!(finite(9_223_372_036_854_775_808.0).to_i64(), None);
        assert_eq!(
            finite(-9_223_372_036_854_775_808.0).to_i64(),
            Some(i64::MIN)
        );
    }

    #[test]
    fn display_keeps_the_numeral_grammar() {
        assert_eq!(finite(3.0).to_string(), "3.0");
        assert_eq!(finite(1e300).to_string(), "1.0e300");
        assert_eq!(finite(f64::MAX).to_string(), "1.7976931348623157e308");
    }
}
