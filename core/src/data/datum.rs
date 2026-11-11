//! First-order data, one Rust type at a time.
//!
//! A [`Datum`] encodes to an [`FOValue`] and decodes back strictly, naming
//! whatever arrived ill-shaped; every protocol payload is typed through it.

use crate::data::{FOValue, Finite};
use crate::fact::{
    Count, CountRule, DecodeCause, DecodeFailure, DecodeIntegerRange, DecodeIntegerTarget,
    DecodeLength, Expected, Found, ShapeFailure, UnexpectedPayload, UnknownField, UnknownTag,
    ValueShape,
};
use std::collections::BTreeMap;
use std::num::NonZeroU8;
use std::sync::Arc;

#[doc(hidden)]
pub use strum;

pub trait Datum: Sized {
    fn encode(self) -> FOValue;

    /// # Errors
    /// The rejected shape, value or domain constraint, with its data path.
    fn decode(v: &FOValue) -> Result<Self, DecodeFailure>;
}

fn expected(shape: ValueShape, v: &FOValue) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
        expected: Expected::Shape(shape),
        found: Found::of(v),
    }))
}

fn integer_range(value: i64, target: DecodeIntegerTarget) -> DecodeFailure {
    DecodeFailure::new(DecodeCause::IntegerRange(DecodeIntegerRange {
        value,
        target,
    }))
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
pub fn field<T: Datum>(v: &FOValue, key: &str) -> Result<T, DecodeFailure> {
    let FOValue::Map { entries } = v else {
        return Err(expected(ValueShape::Map, v));
    };
    let mut values = entries.iter().filter(|(name, _)| name == key);
    let (_, value) = values
        .next()
        .ok_or_else(|| DecodeFailure::new(DecodeCause::MissingField(key.into())))?;
    if values.next().is_some() {
        return Err(DecodeFailure::new(DecodeCause::DuplicateField(key.into())));
    }
    T::decode(value).map_err(|why| why.field(key))
}

/// Refuse a record whose keys are not exactly `keys`, each once.
///
/// # Errors
/// The first duplicate or unknown key, or a value that is not a record.
pub fn exact_keys(v: &FOValue, keys: &[&str]) -> Result<(), DecodeFailure> {
    let FOValue::Map { entries } = v else {
        return Err(DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
            expected: Expected::Record(keys.iter().map(|key| (*key).into()).collect()),
            found: Found::of(v),
        })));
    };
    let mut seen = std::collections::HashSet::new();
    for (k, _) in entries {
        if !keys.contains(&k.as_str()) {
            return Err(DecodeFailure::new(DecodeCause::UnknownField(
                UnknownField {
                    field: k.clone(),
                    offered: keys.iter().map(|key| (*key).into()).collect(),
                },
            )));
        }
        if !seen.insert(k.as_str()) {
            return Err(DecodeFailure::new(DecodeCause::DuplicateField(k.clone())));
        }
    }
    Ok(())
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

            fn decode(v: &$crate::data::FOValue) -> Result<Self, $crate::fact::DecodeFailure> {
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

            fn decode(v: &$crate::data::FOValue) -> Result<Self, $crate::fact::DecodeFailure> {
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
            Some(x) => Err($crate::fact::DecodeFailure::new(
                $crate::fact::DecodeCause::UnexpectedPayload($crate::fact::UnexpectedPayload {
                    tag: $label.into(),
                    found: $crate::fact::Found::of(x),
                }),
            )),
        }
    };
    (@dec $label:literal, $arm:path, $p:ident, $t:ty) => {
        <$t as $crate::data::datum::Datum>::decode(
            $p.unwrap_or(&$crate::data::FOValue::Unit),
        )
        .map($arm)
        .map_err(|why| why.payload($label))
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

            fn decode(v: &$crate::data::FOValue) -> Result<Self, $crate::fact::DecodeFailure> {
                match $crate::data::datum::untag(v) {
                    $(Some(($label, payload)) => {
                        $crate::variant!(@dec $label, Self::$arm, payload $(, $payload)?)
                    })*
                    Some((other, _)) => Err($crate::fact::DecodeFailure::new(
                        $crate::fact::DecodeCause::UnknownTag($crate::fact::UnknownTag {
                            tag: other.into(),
                            offered: vec![$($label.into()),*],
                        }),
                    )),
                    None => Err($crate::fact::DecodeFailure::new(
                        $crate::fact::DecodeCause::Shape($crate::fact::ShapeFailure {
                            expected: $crate::fact::Expected::Tags(vec![$($label.into()),*]),
                            found: $crate::fact::Found::of(v),
                        }),
                    )),
                }
            }
        }
    };
}

/// The variant of `all` whose label `v` is, for [`label!`].
///
/// # Errors
/// An unknown tag, an unexpected payload, or a value that is not a tag.
pub fn labelled<T: Copy>(
    v: &FOValue,
    all: &[T],
    label: impl Fn(&T) -> &'static str,
) -> Result<T, DecodeFailure> {
    let offered = || all.iter().map(|x| label(x).to_string()).collect();
    let Some((tag, payload)) = untag(v) else {
        return Err(DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
            expected: Expected::Tags(offered()),
            found: Found::of(v),
        })));
    };
    let value = all
        .iter()
        .find(|x| label(x) == tag)
        .copied()
        .ok_or_else(|| {
            DecodeFailure::new(DecodeCause::UnknownTag(UnknownTag {
                tag: tag.into(),
                offered: offered(),
            }))
        })?;
    if let Some(payload) = payload {
        return Err(DecodeFailure::new(DecodeCause::UnexpectedPayload(
            UnexpectedPayload {
                tag: tag.into(),
                found: Found::of(payload),
            },
        )));
    }
    Ok(value)
}

impl Datum for () {
    fn encode(self) -> FOValue {
        FOValue::Unit
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        match v {
            FOValue::Unit => Ok(()),
            _ => Err(expected(ValueShape::Unit, v)),
        }
    }
}

impl Datum for u8 {
    fn encode(self) -> FOValue {
        FOValue::Int { value: self.into() }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let n = i64::decode(v)?;
        Self::try_from(n).map_err(|_| integer_range(n, DecodeIntegerTarget::Byte))
    }
}

impl Datum for NonZeroU8 {
    fn encode(self) -> FOValue {
        FOValue::Int {
            value: self.get().into(),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let n = i64::decode(v)?;
        u8::try_from(n)
            .ok()
            .and_then(Self::new)
            .ok_or_else(|| integer_range(n, DecodeIntegerTarget::FailureStatus))
    }
}

impl Datum for FOValue {
    fn encode(self) -> Self {
        self
    }

    fn decode(v: &Self) -> Result<Self, DecodeFailure> {
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        v.as_str()
            .map(str::to_string)
            .ok_or_else(|| expected(ValueShape::String, v))
    }
}

impl Datum for bool {
    fn encode(self) -> FOValue {
        FOValue::Bool { value: self }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        v.as_bool().ok_or_else(|| expected(ValueShape::Bool, v))
    }
}

impl Datum for i32 {
    fn encode(self) -> FOValue {
        FOValue::Int { value: self.into() }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let n = i64::decode(v)?;
        Self::try_from(n).map_err(|_| integer_range(n, DecodeIntegerTarget::I32))
    }
}

/// Legacy integer encoding saturates at `i64::MAX`; new failure counts use `Count`.
impl Datum for u64 {
    fn encode(self) -> FOValue {
        FOValue::Int {
            value: i64::try_from(self).unwrap_or(i64::MAX),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let n = i64::decode(v)?;
        Self::try_from(n).map_err(|_| integer_range(n, DecodeIntegerTarget::U64))
    }
}

impl Datum for usize {
    fn encode(self) -> FOValue {
        u64::try_from(self).unwrap_or(u64::MAX).encode()
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let n = i64::decode(v)?;
        Self::try_from(n).map_err(|_| integer_range(n, DecodeIntegerTarget::Usize))
    }
}

impl<T: Datum> Datum for Option<T> {
    fn encode(self) -> FOValue {
        match self {
            Some(x) => tag("some", Some(x.encode())),
            None => tag("none", None),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        match untag(v) {
            Some(("none", None)) => Ok(None),
            Some(("none", Some(payload))) => Err(DecodeFailure::new(
                DecodeCause::UnexpectedPayload(UnexpectedPayload {
                    tag: "none".into(),
                    found: Found::of(payload),
                }),
            )),
            Some(("some", payload)) => T::decode(payload.unwrap_or(&FOValue::Unit))
                .map(Some)
                .map_err(|why| why.payload("some")),
            Some((tag, _)) => Err(DecodeFailure::new(DecodeCause::UnknownTag(UnknownTag {
                tag: tag.into(),
                offered: vec!["none".into(), "some".into()],
            }))),
            None => Err(DecodeFailure::new(DecodeCause::Shape(ShapeFailure {
                expected: Expected::Tags(vec!["none".into(), "some".into()]),
                found: Found::of(v),
            }))),
        }
    }
}

impl<T: Datum> Datum for Box<T> {
    fn encode(self) -> FOValue {
        (*self).encode()
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        T::decode(v).map(Self::new)
    }
}

impl Datum for i64 {
    fn encode(self) -> FOValue {
        FOValue::Int { value: self }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        v.as_int().ok_or_else(|| expected(ValueShape::Int, v))
    }
}

impl Datum for Finite {
    fn encode(self) -> FOValue {
        FOValue::Float { value: self }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        match v {
            FOValue::Float { value } => Ok(*value),
            _ => Err(expected(ValueShape::Float, v)),
        }
    }
}

impl Datum for u32 {
    fn encode(self) -> FOValue {
        FOValue::Int { value: self.into() }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let n = i64::decode(v)?;
        Self::try_from(n).map_err(|_| integer_range(n, DecodeIntegerTarget::U32))
    }
}

impl Datum for Arc<str> {
    fn encode(self) -> FOValue {
        FOValue::String {
            value: self.to_string(),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        v.as_str()
            .map(Self::from)
            .ok_or_else(|| expected(ValueShape::String, v))
    }
}

/// A string-keyed map, its keys already in order.
impl<T: Datum> Datum for BTreeMap<String, T> {
    fn encode(self) -> FOValue {
        FOValue::Map {
            entries: self.into_iter().map(|(k, v)| (k, v.encode())).collect(),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let FOValue::Map { entries } = v else {
            return Err(expected(ValueShape::Map, v));
        };
        entries
            .iter()
            .map(|(k, x)| Ok((k.clone(), T::decode(x).map_err(|why| why.field(k))?)))
            .collect()
    }
}

impl<T: Datum> Datum for Vec<T> {
    fn encode(self) -> FOValue {
        FOValue::List {
            items: self.into_iter().map(Datum::encode).collect(),
        }
    }

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        v.as_list()
            .ok_or_else(|| expected(ValueShape::List, v))?
            .iter()
            .enumerate()
            .map(|(i, x)| T::decode(x).map_err(|why| why.index(Count::from(i))))
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

    fn decode(v: &FOValue) -> Result<Self, DecodeFailure> {
        let items = v.as_list().ok_or_else(|| expected(ValueShape::List, v))?;
        match items {
            [a, b] => Ok((
                A::decode(a).map_err(|why| why.index(Count::from_u64(0)))?,
                B::decode(b).map_err(|why| why.index(Count::from_u64(1)))?,
            )),
            _ => Err(DecodeFailure::new(DecodeCause::Length(DecodeLength {
                expected: CountRule::Exactly(Count::from_u64(2)),
                got: Count::from(items.len()),
            }))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::Typed;

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
        for payload in [None, Some(Box::new(FOValue::Unit))] {
            assert_eq!(
                Carrier::decode(&FOValue::Variant {
                    label: "carried".into(),
                    payload: payload.clone()
                }),
                Ok(Carrier::Carried(()))
            );
            assert_eq!(
                Option::<()>::decode(&FOValue::Variant {
                    label: "some".into(),
                    payload
                }),
                Ok(Some(()))
            );
        }
        let why = Carrier::decode(&tag("number", None)).unwrap_err();
        assert_eq!(
            why.path,
            vec![crate::fact::DataStep::Payload("number".into())]
        );
        assert!(matches!(
            *why.cause,
            DecodeCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::Int),
                found: Found::Unit,
            })
        ));
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
                Err(integer_range(n, DecodeIntegerTarget::Byte))
            );
        }
        assert!(u8::decode(&FOValue::Unit).is_err());
    }

    #[test]
    fn records_lists_and_payloads_keep_outermost_first_paths() {
        #[derive(Debug, PartialEq, Eq)]
        struct Envelope {
            values: Vec<Option<(i64, bool)>>,
        }
        record!(Envelope { values: "values" });
        let value = FOValue::Map {
            entries: vec![(
                "values".into(),
                FOValue::List {
                    items: vec![tag(
                        "some",
                        Some(FOValue::List {
                            items: vec![1_i64.encode(), "bad".to_string().encode()],
                        }),
                    )],
                },
            )],
        };
        let failure = Envelope::decode(&value).unwrap_err();
        assert_eq!(
            failure.path,
            vec![
                crate::fact::DataStep::Field("values".into()),
                crate::fact::DataStep::Index(Count::from_u64(0)),
                crate::fact::DataStep::Payload("some".into()),
                crate::fact::DataStep::Index(Count::from_u64(1)),
            ]
        );
        assert_eq!(
            *failure.cause,
            DecodeCause::Shape(ShapeFailure {
                expected: Expected::Shape(ValueShape::Bool),
                found: Found::Text(crate::fact::TextPreview::new("bad")),
            })
        );
    }

    #[test]
    fn missing_duplicate_and_unknown_fields_stay_at_the_record() {
        #[derive(Debug)]
        struct Record {
            value: bool,
        }
        record!(Record { value: "value" });
        let cases = [
            (vec![], DecodeCause::MissingField("value".into())),
            (
                vec![
                    ("value".into(), true.encode()),
                    ("value".into(), false.encode()),
                ],
                DecodeCause::DuplicateField("value".into()),
            ),
            (
                vec![("other".into(), true.encode())],
                DecodeCause::UnknownField(UnknownField {
                    field: "other".into(),
                    offered: vec!["value".into()],
                }),
            ),
        ];
        for (entries, cause) in cases {
            assert_eq!(
                Record::decode(&FOValue::Map { entries }).unwrap_err(),
                DecodeFailure::new(cause)
            );
        }
        let duplicate = FOValue::Map {
            entries: vec![
                ("value".into(), true.encode()),
                ("value".into(), false.encode()),
            ],
        };
        assert_eq!(
            field::<bool>(&duplicate, "value"),
            Err(DecodeFailure::new(DecodeCause::DuplicateField(
                "value".into()
            )))
        );
    }

    #[test]
    fn labels_distinguish_shape_unknown_tag_and_excess_payload() {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, strum::IntoStaticStr, strum::VariantArray)]
        #[strum(serialize_all = "kebab-case")]
        enum Label {
            Known,
        }
        label!(typed Label);
        assert!(matches!(
            *Label::decode(&true.encode()).unwrap_err().cause,
            DecodeCause::Shape(_)
        ));
        assert_eq!(
            Label::decode(&tag("other", None)),
            Err(DecodeFailure::new(DecodeCause::UnknownTag(UnknownTag {
                tag: "other".into(),
                offered: vec!["known".into()],
            })))
        );
        assert_eq!(
            Label::decode(&tag("known", Some(true.encode()))),
            Err(DecodeFailure::new(DecodeCause::UnexpectedPayload(
                UnexpectedPayload {
                    tag: "known".into(),
                    found: Found::Bool(true),
                }
            )))
        );
        assert_eq!(
            Label::decode(&FOValue::Variant {
                label: "known".into(),
                payload: Some(Box::new(FOValue::Unit))
            }),
            Ok(Label::Known)
        );
        let _ = Label::ty();
    }

    #[test]
    fn tuple_cardinality_and_integer_ranges_retain_the_operands() {
        assert_eq!(
            <(i64, bool)>::decode(&FOValue::List { items: vec![] }),
            Err(DecodeFailure::new(DecodeCause::Length(DecodeLength {
                expected: CountRule::Exactly(Count::from_u64(2)),
                got: Count::from_u64(0),
            })))
        );
        assert_eq!(
            i32::decode(&i64::MAX.encode()),
            Err(integer_range(i64::MAX, DecodeIntegerTarget::I32))
        );
        assert_eq!(
            u32::decode(&(-1_i64).encode()),
            Err(integer_range(-1, DecodeIntegerTarget::U32))
        );
        assert_eq!(
            u64::decode(&(-1_i64).encode()),
            Err(integer_range(-1, DecodeIntegerTarget::U64))
        );
        assert_eq!(
            usize::decode(&(-1_i64).encode()),
            Err(integer_range(-1, DecodeIntegerTarget::Usize))
        );
        assert_eq!(
            NonZeroU8::decode(&0_i64.encode()),
            Err(integer_range(0, DecodeIntegerTarget::FailureStatus))
        );
        assert!(matches!(
            *u32::decode(&true.encode()).unwrap_err().cause,
            DecodeCause::Shape(_)
        ));
    }

    #[test]
    fn string_keyed_maps_keep_the_key_path_and_replacement_policy() {
        let value = FOValue::Map {
            entries: vec![
                ("key".into(), 1_i64.encode()),
                ("key".into(), 2_i64.encode()),
            ],
        };
        assert_eq!(BTreeMap::<String, i64>::decode(&value).unwrap()["key"], 2);
        let failure = BTreeMap::<String, bool>::decode(&value).unwrap_err();
        assert_eq!(
            failure.path,
            vec![crate::fact::DataStep::Field("key".into())]
        );
    }
}
