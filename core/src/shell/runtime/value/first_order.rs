//! Value closures and the first-order vocabulary.

use super::{ValClos, View};
use crate::data::datum::Datum;
use crate::data::{FOValue, NotData, Opaque};
use crate::fact::{NonData, OpaqueKind, ValueShape};
use std::convert::Infallible;

impl From<Opaque> for OpaqueKind {
    fn from(leaf: Opaque) -> Self {
        match leaf {
            Opaque::Block => Self::Block,
            Opaque::Function => Self::Function,
            Opaque::Handle => Self::Handle,
        }
    }
}
impl From<NotData> for NonData {
    fn from(error: NotData) -> Self {
        Self {
            leaf: error.leaf.into(),
            nested: error.nested,
        }
    }
}

impl ValClos {
    /// What is not data about this value, by [`ValClos::shape`].
    #[must_use]
    pub fn opacity(&self) -> Option<Opaque> {
        match self.shape() {
            ValueShape::Block => Some(Opaque::Block),
            ValueShape::Function => Some(Opaque::Function),
            ValueShape::Handle => Some(Opaque::Handle),
            _ => None,
        }
    }
}
impl FOValue {
    /// `leaf` answers for each opaque value met, which stands in its place.
    pub(crate) fn walk<E>(
        v: &ValClos,
        leaf: &mut impl FnMut(Opaque) -> Result<Self, E>,
    ) -> Result<Self, E> {
        let (labels, terminal) = v.peel_variants();
        let head = terminal.view();
        let is_unit = matches!(head, View::Unit);
        let mut encoded = match head {
            View::Unit => Self::Unit,
            View::Bool(value) => Self::Bool { value },
            View::Int(value) => Self::Int { value },
            View::Float(value) => Self::Float { value },
            View::String(value) => Self::String {
                value: value.to_string(),
            },
            View::Bytes(value) => Self::Bytes {
                value: value.to_vec(),
            },
            View::List(items) => Self::List {
                items: items
                    .iter()
                    .map(|value| Self::walk(&value, &mut *leaf))
                    .collect::<Result<_, E>>()?,
            },
            View::Map(entries) => Self::Map {
                entries: entries
                    .iter()
                    .map(|(name, value)| Ok((name.to_string(), Self::walk(&value, &mut *leaf)?)))
                    .collect::<Result<_, E>>()?,
            },
            View::Variant { .. } => unreachable!("the tag spine was peeled above"),
            View::Thunk { .. } | View::Builtin(_) | View::Handle(_) => leaf(
                terminal
                    .opacity()
                    .expect("a block, function or handle is opaque"),
            )?,
        };
        for (i, label) in labels.into_iter().rev().enumerate() {
            encoded = Self::Variant {
                label: label.to_string(),
                payload: (!(i == 0 && is_unit)).then(|| Box::new(encoded)),
            };
        }
        Ok(encoded)
    }
}
impl TryFrom<&ValClos> for FOValue {
    type Error = NotData;
    fn try_from(v: &ValClos) -> Result<Self, NotData> {
        Self::walk(v, &mut Err).map_err(|leaf| NotData {
            leaf,
            nested: v.opacity().is_none(),
        })
    }
}
impl FOValue {
    pub(crate) fn scrubbed(v: &ValClos) -> Self {
        let Ok(fo) = Self::walk(v, &mut |kind| Ok::<_, Infallible>(kind.placeholder()));
        fo
    }
}
impl ValClos {
    pub fn from_datum(d: impl Datum) -> Self {
        Self::from(d.encode())
    }
}
impl From<FOValue> for ValClos {
    fn from(fo: FOValue) -> Self {
        let mut labels = Vec::new();
        let mut terminal = fo;
        while let FOValue::Variant { label, payload } = terminal {
            labels.push(label);
            terminal = payload.map_or(FOValue::Unit, |payload| *payload);
        }
        let value = match terminal {
            FOValue::Unit => Self::UNIT,
            FOValue::Bool { value } => Self::bool(value),
            FOValue::Int { value } => Self::int(value),
            FOValue::Float { value } => Self::float(value),
            FOValue::String { value } => Self::string(value),
            FOValue::Bytes { value } => Self::bytes(value),
            FOValue::List { items } => Self::list(items.into_iter().map(Self::from).collect()),
            FOValue::Map { entries } => Self::map(
                entries
                    .into_iter()
                    .map(|(name, value)| (name, Self::from(value)))
                    .collect(),
            ),
            FOValue::Variant { .. } => unreachable!("the tag spine was peeled above"),
            FOValue::Ext(never) => match never {},
        };
        labels
            .into_iter()
            .rev()
            .fold(value, |payload, label| Self::variant(label, Some(payload)))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::shell::runtime::{Env, block_over};
    #[test]
    fn a_leaf_reports_whether_it_is_the_value_or_inside_it() {
        let block = block_over(&Env::EMPTY);
        let top = FOValue::try_from(&block).unwrap_err();
        assert_eq!((top.leaf, top.nested), (Opaque::Block, false));
        let inside = FOValue::try_from(&ValClos::list(vec![ValClos::int(1), block])).unwrap_err();
        assert_eq!((inside.leaf, inside.nested), (Opaque::Block, true));
    }
    #[test]
    fn scrubbing_replaces_each_leaf_by_its_placeholder_and_data_round_trips() {
        let data = ValClos::map(vec![(
            "a".into(),
            ValClos::variant("x", Some(ValClos::int(1))),
        )]);
        assert_eq!(ValClos::from(FOValue::scrubbed(&data)), data);
        assert_eq!(
            FOValue::scrubbed(&block_over(&Env::EMPTY)),
            Opaque::Block.placeholder()
        );
    }
}
