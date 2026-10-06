//! First-order data, one Rust type at a time.
//!
//! A [`Datum`] encodes to an [`FOValue`] and decodes back strictly, naming
//! whatever arrived ill-shaped; every protocol payload is typed through it.

use crate::first_order::FOValue;
use std::collections::BTreeMap;
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

pub fn tag(label: &str, payload: Option<FOValue>) -> FOValue {
    FOValue::Variant {
        label: label.into(),
        payload: payload.map(Box::new),
    }
}

/// A variant's label and payload; `None` for anything else.
pub fn untag(v: &FOValue) -> Option<(&str, Option<&FOValue>)> {
    match v {
        FOValue::Variant { label, payload } => Some((label, payload.as_deref())),
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
                $crate::ty::closed_record(&[$((
                    $key,
                    $crate::ty::field_ty(|s: &Self| &s.$field),
                )),*])
            }
        }
    };
    ($ty:ty { $($field:ident: $key:literal),* $(,)? }) => {
        impl $crate::first_order::datum::Datum for $ty {
            fn encode(self) -> $crate::first_order::FOValue {
                let mut entries = vec![$((
                    $key.to_string(),
                    $crate::first_order::datum::Datum::encode(self.$field),
                )),*];
                entries.sort_by(|(a, _), (b, _)| a.cmp(b));
                $crate::first_order::FOValue::Map { entries }
            }

            fn decode(v: &$crate::first_order::FOValue) -> Result<Self, String> {
                use $crate::first_order::datum::field;
                $crate::first_order::datum::exact_keys(v, &[$($key),*])?;
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
                $crate::ty::closed_variant(
                    &<$ty as $crate::first_order::datum::strum::VariantArray>::VARIANTS
                        .iter()
                        .map(|v| (<&str>::from(*v), $crate::ty::Ty::Unit))
                        .collect::<Vec<_>>(),
                )
            }
        }
    };
    ($ty:ty) => {
        impl $crate::first_order::datum::Datum for $ty {
            fn encode(self) -> $crate::first_order::FOValue {
                $crate::first_order::datum::tag(<&str>::from(self), None)
            }

            fn decode(v: &$crate::first_order::FOValue) -> Result<Self, String> {
                let all = <$ty as $crate::first_order::datum::strum::VariantArray>::VARIANTS;
                $crate::first_order::datum::labelled(v, all, |x| <&str>::from(*x))
            }
        }
    };
}

/// A [`Datum`] for an enum whose variants are tags, each arm `Name: "label"`
/// or `Name(Payload): "label"`.  With `typed`, also its closed variant type.
#[macro_export]
macro_rules! variant {
    (@bind $t:ty, $x:ident) => { $x };
    (@ty) => { $crate::ty::Ty::Unit };
    (@ty $t:ty) => { <$t as $crate::ty::Typed>::ty() };
    (@enc) => { None };
    (@enc $x:ident $t:ty) => { Some($crate::first_order::datum::Datum::encode($x)) };
    (@dec $label:literal, $arm:path, $p:ident) => {
        match $p {
            None => Ok($arm),
            Some(x) => Err(format!("`{} takes no payload, got {}", $label, x.shape())),
        }
    };
    (@dec $label:literal, $arm:path, $p:ident, $t:ty) => {
        match $p {
            Some(x) => <$t as $crate::first_order::datum::Datum>::decode(x)
                .map($arm)
                .map_err(|why| format!("`{}: {why}", $label)),
            None => Err(format!("`{} needs a payload", $label)),
        }
    };
    (typed $ty:ty { $($arm:ident $(($payload:ty))?: $label:literal),* $(,)? }) => {
        $crate::variant!($ty { $($arm $(($payload))?: $label),* });
        impl $crate::ty::Typed for $ty {
            fn ty() -> $crate::ty::Ty {
                $crate::ty::closed_variant(&[$((
                    $label,
                    $crate::variant!(@ty $($payload)?),
                )),*])
            }
        }
    };
    ($ty:ty { $($arm:ident $(($payload:ty))?: $label:literal),* $(,)? }) => {
        impl $crate::first_order::datum::Datum for $ty {
            fn encode(self) -> $crate::first_order::FOValue {
                match self {
                    $(Self::$arm $(($crate::variant!(@bind $payload, payload)))? => {
                        $crate::first_order::datum::tag(
                            $label,
                            $crate::variant!(@enc $(payload $payload)?),
                        )
                    })*
                }
            }

            fn decode(v: &$crate::first_order::FOValue) -> Result<Self, String> {
                match $crate::first_order::datum::untag(v) {
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
            Some(("some", Some(x))) => T::decode(x).map(Some),
            _ => Err(expected("`none or `some", v)),
        }
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
