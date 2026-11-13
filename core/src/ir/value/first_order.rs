//! Value closures and the first-order vocabulary.

use super::{VClosRef, ValClos, View};
use crate::data::datum::Datum;
use crate::data::{FOValue, NotData, Opaque};
use crate::fact::{NonData, OpaqueKind, ValueShape};
use crate::ir::{Env, Name, Val};
use crate::source::Spanned;
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

impl VClosRef<'_> {
    /// What is not data about this value, by [`VClosRef::shape`].
    #[must_use]
    pub fn opacity(self) -> Option<Opaque> {
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
        v: VClosRef<'_>,
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
                    .map(|value| Self::walk(value, &mut *leaf))
                    .collect::<Result<_, E>>()?,
            },
            View::Map(entries) => Self::Map {
                entries: entries
                    .iter()
                    .map(|(name, value)| Ok((name.to_string(), Self::walk(value, &mut *leaf)?)))
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
impl TryFrom<VClosRef<'_>> for FOValue {
    type Error = NotData;
    fn try_from(v: VClosRef<'_>) -> Result<Self, NotData> {
        Self::walk(v, &mut Err).map_err(|leaf| NotData {
            leaf,
            nested: v.opacity().is_none(),
        })
    }
}
impl TryFrom<&ValClos> for FOValue {
    type Error = NotData;
    fn try_from(v: &ValClos) -> Result<Self, NotData> {
        Self::try_from(VClosRef::from(v))
    }
}
impl FOValue {
    pub(crate) fn scrubbed(v: VClosRef<'_>) -> Self {
        let Ok(fo) = Self::walk(v, &mut |kind| Ok::<_, Infallible>(kind.placeholder()));
        fo
    }
}
impl ValClos {
    pub fn from_datum(d: impl Datum) -> Self {
        Self::from(d.encode())
    }
}
/// Data is closed syntax: ⟨V, ∅⟩.
impl From<FOValue> for ValClos {
    fn from(fo: FOValue) -> Self {
        Self::from_parts(datum_syntax(fo), Env::EMPTY)
    }
}
fn datum_syntax(fo: FOValue) -> Val {
    let mut labels = Vec::new();
    let mut terminal = fo;
    while let FOValue::Variant { label, payload } = terminal {
        labels.push(label);
        terminal = payload.map_or(FOValue::Unit, |payload| *payload);
    }
    let value = match terminal {
        FOValue::Unit => Val::Unit,
        FOValue::Bool { value } => Val::Bool(value),
        FOValue::Int { value } => Val::Int(value),
        FOValue::Float { value } => Val::Float(value),
        FOValue::String { value } => Val::String(value.into()),
        FOValue::Bytes { value } => Val::Bytes(value.into()),
        FOValue::List { items } => Val::list(
            items
                .into_iter()
                .map(|item| Spanned::synthetic(datum_syntax(item))),
        ),
        FOValue::Map { entries } => Val::map(
            entries
                .into_iter()
                .map(|(name, value)| (Name::from(name), Spanned::synthetic(datum_syntax(value)))),
        ),
        FOValue::Variant { .. } => unreachable!("the tag spine was peeled above"),
        FOValue::Ext(never) => match never {},
    };
    labels
        .into_iter()
        .rev()
        .fold(value, |payload, label| Val::Variant {
            label: label.into(),
            payload: Box::new(payload),
        })
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::block_over;
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
        assert_eq!(
            ValClos::from(FOValue::scrubbed(VClosRef::from(&data))),
            data
        );
        assert_eq!(
            FOValue::scrubbed(VClosRef::from(&block_over(&Env::EMPTY))),
            Opaque::Block.placeholder()
        );
    }
}
