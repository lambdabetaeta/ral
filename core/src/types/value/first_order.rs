//! `Value` and the first-order vocabulary: one walk each way.
//!
//! Every conversion differs from its siblings only at the leaves that are not
//! data, so a leaf is a borrowed view handed to the caller, never a `&Value`.

use super::Value;
use crate::first_order::datum::Datum;
use crate::first_order::{FOValue, NoExt, NotData, Opaque};
use crate::types::{BuiltinEntry, Closure};
use std::convert::Infallible;

/// A `Value` that is not data.
pub(crate) enum Leaf<'a> {
    Thunk(&'a Closure),
    Native(&'a BuiltinEntry, &'a [Value]),
    Handle,
}

impl Leaf<'_> {
    pub(crate) fn kind(&self) -> Opaque {
        match self {
            Self::Thunk(c) if c.comp().arrow().is_none() => Opaque::Block,
            Self::Thunk(_) | Self::Native(..) => Opaque::Function,
            Self::Handle => Opaque::Handle,
        }
    }
}

impl Value {
    fn leaf(&self) -> Option<Leaf<'_>> {
        match self {
            Self::Thunk(c) => Some(Leaf::Thunk(c)),
            Self::Native { entry, applied } => Some(Leaf::Native(entry, applied)),
            Self::Handle(_) => Some(Leaf::Handle),
            _ => None,
        }
    }

    /// What this value is if it is not data.
    #[must_use]
    pub fn opacity(&self) -> Option<Opaque> {
        self.leaf().map(|l| l.kind())
    }
}

impl<X> FOValue<X> {
    /// Fold a value into data, handing each leaf that is not data to `leaf`.
    pub(crate) fn walk<E>(
        v: &Value,
        leaf: &mut impl FnMut(Leaf<'_>) -> Result<Self, E>,
    ) -> Result<Self, E> {
        Ok(match v {
            Value::Unit => Self::Unit,
            Value::Bool(v) => Self::Bool { value: *v },
            Value::Int(v) => Self::Int { value: *v },
            Value::Float(v) => Self::Float { value: *v },
            Value::String(v) => Self::String {
                value: v.to_string(),
            },
            Value::Bytes(v) => Self::Bytes { value: v.to_vec() },
            Value::List(items) => Self::List {
                items: items
                    .iter()
                    .map(|v| Self::walk(&v, leaf))
                    .collect::<Result<_, _>>()?,
            },
            Value::Map(items) => Self::Map {
                entries: items
                    .iter()
                    .map(|(k, v)| Ok((k.to_string(), Self::walk(&v, leaf)?)))
                    .collect::<Result<_, E>>()?,
            },
            Value::Variant { label, payload } => Self::Variant {
                label: label.to_string(),
                payload: payload
                    .as_deref()
                    .map(|p| Self::walk(p, leaf).map(Box::new))
                    .transpose()?,
            },
            Value::Thunk(c) => leaf(Leaf::Thunk(c))?,
            Value::Native { entry, applied } => leaf(Leaf::Native(entry, applied))?,
            Value::Handle(_) => leaf(Leaf::Handle)?,
        })
    }

    /// The inverse fold: `ext` rebuilds each extension.
    pub(crate) fn unwalk<E>(self, ext: &mut impl FnMut(X) -> Result<Value, E>) -> Result<Value, E> {
        Ok(match self {
            Self::Unit => Value::Unit,
            Self::Bool { value } => Value::Bool(value),
            Self::Int { value } => Value::Int(value),
            Self::Float { value } => Value::Float(value),
            Self::String { value } => Value::string(value),
            Self::Bytes { value } => Value::bytes(value),
            Self::List { items } => Value::list(
                items
                    .into_iter()
                    .map(|v| v.unwalk(ext))
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Self::Map { entries } => Value::Map(
                entries
                    .into_iter()
                    .map(|(k, v)| Ok((k, v.unwalk(ext)?)))
                    .collect::<Result<_, E>>()?,
            ),
            Self::Variant { label, payload } => Value::Variant {
                label: label.into(),
                payload: payload.map(|p| p.unwalk(ext).map(Box::new)).transpose()?,
            },
            Self::Ext(x) => ext(x)?,
        })
    }

    /// Visit every extension, through data alone.
    pub(crate) fn for_each_ext(&self, f: &mut impl FnMut(&X)) {
        match self {
            Self::Ext(x) => f(x),
            Self::List { items } => items.iter().for_each(|v| v.for_each_ext(f)),
            Self::Map { entries } => entries.iter().for_each(|(_, v)| v.for_each_ext(f)),
            Self::Variant { payload, .. } => payload.iter().for_each(|p| p.for_each_ext(f)),
            Self::Unit
            | Self::Bool { .. }
            | Self::Int { .. }
            | Self::Float { .. }
            | Self::String { .. }
            | Self::Bytes { .. } => {}
        }
    }
}

impl TryFrom<&Value> for FOValue {
    type Error = NotData;

    /// The first leaf that is not data, and whether it sat inside `v`.
    fn try_from(v: &Value) -> Result<Self, NotData> {
        Self::walk(v, &mut |l| Err(l.kind())).map_err(|leaf| NotData {
            leaf,
            nested: v.opacity().is_none(),
        })
    }
}

impl FOValue {
    /// [`FOValue::try_from`] made total: every leaf that is not data crosses
    /// as its `` `opaque `` placeholder, as the flat wire needs.
    pub(crate) fn scrubbed(v: &Value) -> Self {
        let Ok(fo) = Self::walk(v, &mut |l| Ok::<_, Infallible>(l.kind().placeholder()));
        fo
    }
}

impl Value {
    /// A [`Datum`] as the value ral code reads.
    pub fn from_datum(d: impl Datum) -> Self {
        Self::from(d.encode())
    }
}

impl From<FOValue> for Value {
    fn from(fo: FOValue) -> Self {
        let Ok(v) = fo.unwalk(&mut |x: NoExt| -> Result<Self, Infallible> { match x {} });
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Env, block_over};

    #[test]
    fn a_leaf_reports_whether_it_is_the_value_or_inside_it() {
        let block = block_over(&Env::new());
        let top = FOValue::try_from(&block).unwrap_err();
        assert_eq!((top.leaf, top.nested), (Opaque::Block, false));
        let inside = FOValue::try_from(&Value::list(vec![Value::Int(1), block])).unwrap_err();
        assert_eq!((inside.leaf, inside.nested), (Opaque::Block, true));
    }

    #[test]
    fn scrubbing_replaces_each_leaf_by_its_placeholder_and_data_round_trips() {
        let data = Value::map(vec![("a".into(), Value::variant("x", Some(Value::Int(1))))]);
        assert_eq!(Value::from(FOValue::scrubbed(&data)), data);
        let scrubbed = FOValue::scrubbed(&block_over(&Env::new()));
        assert_eq!(scrubbed, Opaque::Block.placeholder());
    }
}
