//! First-order data, one Rust type at a time.
//!
//! A [`Datum`] encodes to an [`FOValue`] and decodes back strictly, naming
//! whatever arrived ill-shaped; every protocol payload is typed through it.

use crate::serial::FOValue;

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
            return Err(format!("field `{k} given twice — which one did you mean?"));
        }
    }
    Ok(())
}

/// Name the key the writer most likely meant, else list them all.
fn unknown_key(k: &str, keys: &[&str]) -> String {
    let nearest = keys
        .iter()
        .map(|key| (strsim::damerau_levenshtein(k, key), key))
        .min()
        .filter(|(d, _)| *d <= 2);
    if let Some((_, key)) = nearest {
        return format!("unknown field `{k} — did you mean `{key}?");
    }
    let known: Vec<String> = keys.iter().map(|k| format!("`{k}")).collect();
    format!("unknown field `{k} — expected {}", known.join(", "))
}

/// A [`Datum`] for a struct that is a record, one `field: "key"` per field;
/// its decode refuses a missing, duplicate or unknown key.
#[macro_export]
macro_rules! record {
    ($ty:ty { $($field:ident: $key:literal),* $(,)? }) => {
        impl $crate::serial::datum::Datum for $ty {
            fn encode(self) -> $crate::serial::FOValue {
                $crate::serial::FOValue::Map {
                    entries: vec![$((
                        $key.into(),
                        $crate::serial::datum::Datum::encode(self.$field),
                    )),*],
                }
            }

            fn decode(v: &$crate::serial::FOValue) -> Result<Self, String> {
                use $crate::serial::datum::field;
                $crate::serial::datum::exact_keys(v, &[$($key),*])?;
                Ok(Self { $($field: field(v, $key)?),* })
            }
        }
    };
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
