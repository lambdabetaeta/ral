//! First-order data, one Rust type at a time.
//!
//! A [`Datum`] encodes to an [`FOValue`] and decodes back strictly, naming
//! whatever arrived ill-shaped; every protocol payload is typed through it.

use crate::data::{FOValue, Finite};
use std::collections::BTreeMap;
use std::num::NonZeroU8;
use std::sync::Arc;

#[doc(hidden)]
pub use strum;

pub trait Datum: Sized {
    fn encode(self) -> FOValue;

    /// # Errors
    /// A sentence naming the shape expected and the one that arrived.
    fn decode(v: &FOValue) -> Result<Self, String>;
}

fn expected(what: &str, v: &FOValue) -> String {
    format!("expected {what}, got {}", v.shape())
}

/// A tagged value; `Unit` is represented by an absent payload.
pub fn tag(label: &str, payload: Option<FOValue>) -> FOValue {
    FOValue::Variant {
        label: label.into(),
        payload: payload
            .filter(|p| !matches!(p, FOValue::Unit))
            .map(Box::new),
    }
}

/// A variant's label and payload, a `Unit` payload read as absent; `None` for anything else.
pub fn untag(v: &FOValue) -> Option<(&str, Option<&FOValue>)> {
    match v {
        FOValue::Variant { label, payload } => Some((
            label,
            payload.as_deref().filter(|p| !matches!(p, FOValue::Unit)),
        )),
        _ => None,
    }
}

/// One record field, decoded.
///
/// # Errors
/// The field's absence, or its own decode error under its name.
pub fn field<T: Datum>(v: &FOValue, key: &str) -> Result<T, String> {
    let f = v
        .field(key)
        .ok_or_else(|| format!("no `{key} field in {}", v.shape()))?;
    T::decode(f).map_err(|why| format!("`{key}: {why}"))
}

/// Refuse a record whose keys are not exactly `keys`, each once.
///
/// # Errors
/// The first duplicate or unknown key, or a value that is not a record.
pub fn exact_keys(v: &FOValue, keys: &[&str]) -> Result<(), String> {
    let FOValue::Map { entries } = v else {
        return Err(expected("a record", v));
    };
    let mut seen = std::collections::HashSet::new();
    for (k, _) in entries {
        if !keys.contains(&k.as_str()) {
            return Err(unknown_key(k, keys));
        }
        if !seen.insert(k.as_str()) {
            return Err(format!("field `{k}` given twice: which one did you mean?"));
        }
    }
    Ok(())
}

/// Name the key the writer most likely meant, else list them all.
fn unknown_key(k: &str, keys: &[&str]) -> String {
    if let Some(key) = crate::text::near_names(k, keys.iter().copied(), 1).first() {
        return format!("unknown field `{k}`: did you mean `{key}`?");
    }
    let known: Vec<String> = keys.iter().map(|k| format!("`{k}`")).collect();
    format!("unknown field `{k}`: expected {}", known.join(", "))
}

/// A [`Datum`] for a struct that is a record, one `field: "key"` per field.
///
/// Its decode refuses a missing, duplicate or unknown key, and its encoding is
/// canonical, keys sorted.  With `typed`, also its [`Typed`](crate::ty::Typed)
/// record type, field types read off the fields themselves.
#[macro_export]
macro_rules! record {
    (typed $ty:ty { $($field:ident: $key:literal),* $(,)? }) => {
        $crate::record!($ty { $($field: $key),* });
        impl $crate::ty::Typed for $ty {
            fn ty() -> $crate::ty::Ty {
                $crate::memoized_ty!($crate::ty::closed_record(&[$((
                    $key,
                    $crate::ty::field_ty(|s: &$ty| &s.$field),
                )),*]))
            }
        }
    };
    ($ty:ty { $($field:ident: $key:literal),* $(,)? }) => {
        impl $crate::data::datum::Datum for $ty {
            fn encode(self) -> $crate::data::FOValue {
                let mut entries = vec![$((
                    $key.to_string(),
                    $crate::data::datum::Datum::encode(self.$field),
                )),*];
                entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                $crate::data::FOValue::Map { entries }
            }

            fn decode(v: &$crate::data::FOValue) -> Result<Self, String> {
                use $crate::data::datum::field;
                $crate::data::datum::exact_keys(v, &[$($key),*])?;
                Ok(Self { $($field: field(v, $key)?),* })
            }
        }
    };
}

/// A [`Datum`] for an enum of bare tags, each variant's label its kebab-case
/// name.
///
/// The enum derives `IntoStaticStr` and `VariantArray` with
/// `#[strum(serialize_all = "kebab-case")]`.  With `typed`, also its closed
/// variant type.
#[macro_export]
macro_rules! label {
    (typed $ty:ty) => {
        $crate::label!($ty);
        impl $crate::ty::Typed for $ty {
            fn ty() -> $crate::ty::Ty {
                $crate::memoized_ty!($crate::ty::closed_variant(
                    &<$ty as $crate::data::datum::strum::VariantArray>::VARIANTS
                        .iter()
                        .map(|v| (<&str>::from(*v), $crate::ty::Ty::unit()))
                        .collect::<Vec<_>>(),
                ))
            }
        }
    };
    ($ty:ty) => {
        impl $crate::data::datum::Datum for $ty {
            fn encode(self) -> $crate::data::FOValue {
                $crate::data::datum::tag(<&str>::from(self), None)
            }

            fn decode(v: &$crate::data::FOValue) -> Result<Self, String> {
                let all = <$ty as $crate::data::datum::strum::VariantArray>::VARIANTS;
                $crate::data::datum::labelled(v, all, |x| <&str>::from(*x))
            }
        }
    };
}

/// A [`Datum`] for an enum whose variants are tags, each arm `Name: "label"`
/// or `Name(Payload): "label"`.  With `typed`, also its closed variant type.
#[macro_export]
macro_rules! variant {
    (@bind $t:ty, $x:ident) => { $x };
    (@ty) => { $crate::ty::Ty::unit() };
    (@ty $t:ty) => { <$t as $crate::ty::Typed>::ty() };
    (@enc) => { None };
    (@enc $x:ident $t:ty) => { Some($crate::data::datum::Datum::encode($x)) };
    (@dec $label:literal, $arm:path, $p:ident) => {
        match $p {
            None => Ok($arm),
            Some(x) => Err(format!("`{} takes no payload, got {}", $label, x.shape())),
        }
    };
    (@dec $label:literal, $arm:path, $p:ident, $t:ty) => {
        <$t as $crate::data::datum::Datum>::decode(
            $p.unwrap_or(&$crate::data::FOValue::Unit),
        )
        .map($arm)
        .map_err(|why| format!("`{}: {why}", $label))
    };
    (typed $ty:ty { $($arm:ident $(($payload:ty))?: $label:literal),* $(,)? }) => {
        $crate::variant!($ty { $($arm $(($payload))?: $label),* });
        impl $crate::ty::Typed for $ty {
            fn ty() -> $crate::ty::Ty {
                $crate::memoized_ty!($crate::ty::closed_variant(&[$((
                    $label,
                    $crate::variant!(@ty $($payload)?),
                )),*]))
            }
        }
    };
    ($ty:ty { $($arm:ident $(($payload:ty))?: $label:literal),* $(,)? }) => {
        impl $crate::data::datum::Datum for $ty {
            fn encode(self) -> $crate::data::FOValue {
                match self {
                    $(Self::$arm $(($crate::variant!(@bind $payload, payload)))? => {
                        $crate::data::datum::tag(
                            $label,
                            $crate::variant!(@enc $(payload $payload)?),
                        )
                    })*
                }
            }

            fn decode(v: &$crate::data::FOValue) -> Result<Self, String> {
                match $crate::data::datum::untag(v) {
                    $(Some(($label, payload)) => {
                        $crate::variant!(@dec $label, Self::$arm, payload $(, $payload)?)
                    })*
                    Some((other, _)) => Err(format!(
                        "unknown tag `{other}: expected {}",
                        [$(concat!("`", $label)),*].join(", ")
                    )),
                    None => Err(format!("expected a tag, got {}", v.shape())),
                }
            }
        }
    };
}

/// The variant of `all` whose label `v` is, for [`label!`].
///
/// # Errors
/// A sentence naming the labels expected and the shape that arrived.
pub fn labelled<T: Copy>(
    v: &FOValue,
    all: &[T],
    label: impl Fn(&T) -> &'static str,
) -> Result<T, String> {
    match untag(v) {
        Some((l, None)) => all.iter().find(|x| label(x) == l).copied(),
        _ => None,
    }
    .ok_or_else(|| {
        let known: Vec<_> = all.iter().map(|x| format!("`{}", label(x))).collect();
        format!("expected one of {}, got {}", known.join(", "), v.shape())
    })
}

impl Datum for () {
    fn encode(self) -> FOValue {
        FOValue::Unit
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match v {
            FOValue::Unit => Ok(()),
            _ => Err(expected("()", v)),
        }
    }
}

impl Datum for u8 {
    fn encode(self) -> FOValue {
        FOValue::Int { value: self.into() }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        let n = v.as_int().ok_or_else(|| expected("an Int", v))?;
        Self::try_from(n).map_err(|_| format!("{n} is not a byte (0..=255)"))
    }
}

impl Datum for NonZeroU8 {
    fn encode(self) -> FOValue {
        FOValue::Int {
            value: self.get().into(),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        let n = v.as_int().ok_or_else(|| expected("an Int", v))?;
        u8::try_from(n)
            .ok()
            .and_then(Self::new)
            .ok_or_else(|| format!("{n} is not a failure status (1..=255)"))
    }
}

impl Datum for FOValue {
    fn encode(self) -> Self {
        self
    }

    fn decode(v: &Self) -> Result<Self, String> {
        Ok(v.clone())
    }
}

/// Serde through a type's first-order encoding.
pub mod as_datum {
    use super::{Datum, FOValue};
    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    /// # Errors
    /// The serializer's error.
    pub fn serialize<T: Datum + Clone, S: Serializer>(v: &T, s: S) -> Result<S::Ok, S::Error> {
        v.clone().encode().serialize(s)
    }

    /// # Errors
    /// Invalid first-order data, or the type's decode error.
    pub fn deserialize<'de, T: Datum, D: Deserializer<'de>>(d: D) -> Result<T, D::Error> {
        T::decode(&FOValue::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

impl Datum for String {
    fn encode(self) -> FOValue {
        FOValue::String { value: self }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        v.as_str()
            .map(str::to_string)
            .ok_or_else(|| expected("a Str", v))
    }
}

impl Datum for bool {
    fn encode(self) -> FOValue {
        FOValue::Bool { value: self }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        v.as_bool().ok_or_else(|| expected("a Bool", v))
    }
}

impl Datum for i32 {
    fn encode(self) -> FOValue {
        FOValue::Int { value: self.into() }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        v.as_int()
            .and_then(|n| Self::try_from(n).ok())
            .ok_or_else(|| expected("an exit status", v))
    }
}

/// Saturating at `i64::MAX`: a count that large is already a lie.
impl Datum for u64 {
    fn encode(self) -> FOValue {
        FOValue::Int {
            value: i64::try_from(self).unwrap_or(i64::MAX),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        v.as_int()
            .and_then(|n| Self::try_from(n).ok())
            .ok_or_else(|| expected("a non-negative Int", v))
    }
}

impl Datum for usize {
    fn encode(self) -> FOValue {
        u64::try_from(self).unwrap_or(u64::MAX).encode()
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        Self::try_from(u64::decode(v)?).map_err(|_| expected("an Int this host can index", v))
    }
}

impl<T: Datum> Datum for Option<T> {
    fn encode(self) -> FOValue {
        match self {
            Some(x) => tag("some", Some(x.encode())),
            None => tag("none", None),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match untag(v) {
            Some(("none", None)) => Ok(None),
            Some(("some", payload)) => T::decode(payload.unwrap_or(&FOValue::Unit)).map(Some),
            _ => Err(expected("`none or `some", v)),
        }
    }
}

impl<T: Datum> Datum for Box<T> {
    fn encode(self) -> FOValue {
        (*self).encode()
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        T::decode(v).map(Self::new)
    }
}

impl Datum for i64 {
    fn encode(self) -> FOValue {
        FOValue::Int { value: self }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        v.as_int().ok_or_else(|| expected("an Int", v))
    }
}

impl Datum for Finite {
    fn encode(self) -> FOValue {
        FOValue::Float { value: self }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match v {
            FOValue::Float { value } => Ok(*value),
            _ => Err(expected("a Float", v)),
        }
    }
}

impl Datum for u32 {
    fn encode(self) -> FOValue {
        FOValue::Int { value: self.into() }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        v.as_int()
            .and_then(|n| Self::try_from(n).ok())
            .ok_or_else(|| expected("a non-negative Int below 2^32", v))
    }
}

impl Datum for Arc<str> {
    fn encode(self) -> FOValue {
        FOValue::String {
            value: self.to_string(),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        v.as_str()
            .map(Self::from)
            .ok_or_else(|| expected("a Str", v))
    }
}

/// A string-keyed map, its keys already in order.
impl<T: Datum> Datum for BTreeMap<String, T> {
    fn encode(self) -> FOValue {
        FOValue::Map {
            entries: self.into_iter().map(|(k, v)| (k, v.encode())).collect(),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        let FOValue::Map { entries } = v else {
            return Err(expected("a record", v));
        };
        entries
            .iter()
            .map(|(k, x)| {
                Ok((
                    k.clone(),
                    T::decode(x).map_err(|why| format!("`{k}: {why}"))?,
                ))
            })
            .collect()
    }
}

impl<T: Datum> Datum for Vec<T> {
    fn encode(self) -> FOValue {
        FOValue::List {
            items: self.into_iter().map(Datum::encode).collect(),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        v.as_list()
            .ok_or_else(|| expected("a list", v))?
            .iter()
            .enumerate()
            .map(|(i, x)| T::decode(x).map_err(|why| format!("element {i}: {why}")))
            .collect()
    }
}

/// A two-element list.
impl<A: Datum, B: Datum> Datum for (A, B) {
    fn encode(self) -> FOValue {
        FOValue::List {
            items: vec![self.0.encode(), self.1.encode()],
        }
    }

    fn decode(v: &FOValue) -> Result<Self, String> {
        match v.as_list() {
            Some([a, b]) => Ok((A::decode(a)?, B::decode(b)?)),
            _ => Err(expected("a list of 2 elements", v)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::{Ty, Typed};

    #[test]
    fn a_unit_payload_is_absent_at_both_doors() {
        let raw = |payload: Option<FOValue>| FOValue::Variant {
            label: "ok".into(),
            payload: payload.map(Box::new),
        };
        assert_eq!(tag("ok", Some(FOValue::Unit)), raw(None));
        assert_eq!(tag("ok", Some(1_i64.encode())), raw(Some(1_i64.encode())));
        for absent in [None, Some(FOValue::Unit)] {
            assert_eq!(untag(&raw(absent)), Some(("ok", None)));
        }
        let one = raw(Some(1_i64.encode()));
        assert_eq!(untag(&one), Some(("ok", Some(&1_i64.encode()))));
    }

    #[test]
    fn an_absent_payload_decodes_as_unit_only_for_unit() {
        #[derive(Debug, PartialEq, Eq)]
        enum Carrier {
            Carried(()),
            Number(i64),
        }
        variant!(Carrier {
            Carried(()): "carried",
            Number(i64): "number",
        });
        assert_eq!(
            Carrier::decode(&tag("carried", None)),
            Ok(Carrier::Carried(()))
        );
        let why = Carrier::decode(&tag("number", None)).unwrap_err();
        assert!(why.starts_with("`number:"), "{why}");
        assert_eq!(Option::<()>::decode(&tag("some", None)), Ok(Some(())));
        assert!(Option::<i64>::decode(&tag("some", None)).is_err());
    }

    #[test]
    fn an_optional_unit_round_trips() {
        for value in [None, Some(())] {
            assert_eq!(Option::<()>::decode(&value.encode()), Ok(value));
        }
        assert_eq!(Option::<i64>::decode(&Some(1_i64).encode()), Ok(Some(1)));
    }

    #[test]
    fn a_byte_decodes_only_a_byte() {
        assert_eq!(u8::decode(&255_i64.encode()), Ok(255));
        assert_eq!(u8::decode(&0_i64.encode()), Ok(0));
        for n in [-1_i64, 256] {
            assert_eq!(
                u8::decode(&n.encode()),
                Err(format!("{n} is not a byte (0..=255)"))
            );
        }
        assert!(u8::decode(&FOValue::Unit).is_err());
        assert_eq!(u8::ty(), Ty::int());
    }
}
