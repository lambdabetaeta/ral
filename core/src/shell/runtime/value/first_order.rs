//! Value closures and the first-order vocabulary.

use super::{ValClos, View};
use crate::data::datum::Datum;
use crate::data::json::drop_value;
use crate::data::{FOValue, NoExt, NotData, Opaque};
use crate::fact::{NonData, OpaqueKind};
use crate::ir::Comp;
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

pub(crate) enum Leaf<'a> {
    Thunk(&'a Comp),
    Builtin,
    Handle,
}
impl Leaf<'_> {
    pub(crate) fn kind(&self) -> Opaque {
        match self {
            Self::Thunk(comp) if comp.lam().is_none() => Opaque::Block,
            Self::Thunk(_) | Self::Builtin => Opaque::Function,
            Self::Handle => Opaque::Handle,
        }
    }
}
impl ValClos {
    fn leaf(&self) -> Option<Leaf<'_>> {
        match self.view() {
            View::Thunk { comp, .. } => Some(Leaf::Thunk(comp)),
            View::Builtin(_) => Some(Leaf::Builtin),
            View::Handle(_) => Some(Leaf::Handle),
            _ => None,
        }
    }
    #[must_use]
    pub fn opacity(&self) -> Option<Opaque> {
        self.leaf().map(|l| l.kind())
    }
}
impl<X> FOValue<X> {
    pub(crate) fn walk<E>(
        v: &ValClos,
        leaf: &mut impl FnMut(Leaf<'_>) -> Result<Self, E>,
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
            View::List(items) => {
                let mut values = Vec::with_capacity(items.len());
                for value in items {
                    match Self::walk(&value, leaf) {
                        Ok(value) => values.push(value),
                        Err(error) => {
                            values.into_iter().for_each(drop_value);
                            return Err(error);
                        }
                    }
                }
                Self::List { items: values }
            }
            View::Map(entries) => {
                let mut values = Vec::with_capacity(entries.len());
                for (name, value) in entries {
                    match Self::walk(&value, leaf) {
                        Ok(value) => values.push((name.to_string(), value)),
                        Err(error) => {
                            for (_, value) in values {
                                drop_value(value);
                            }
                            return Err(error);
                        }
                    }
                }
                Self::Map { entries: values }
            }
            View::Variant { .. } => unreachable!("the tag spine was peeled above"),
            View::Thunk { comp, .. } => leaf(Leaf::Thunk(comp))?,
            View::Builtin(_) => leaf(Leaf::Builtin)?,
            View::Handle(_) => leaf(Leaf::Handle)?,
        };
        for (i, label) in labels.into_iter().rev().enumerate() {
            encoded = Self::Variant {
                label: label.to_string(),
                payload: (!(i == 0 && is_unit)).then(|| Box::new(encoded)),
            };
        }
        Ok(encoded)
    }
    pub(crate) fn unwalk<E>(
        self,
        ext: &mut impl FnMut(X) -> Result<ValClos, E>,
    ) -> Result<ValClos, E> {
        let mut labels = Vec::new();
        let mut terminal = self;
        while let Self::Variant { label, payload } = terminal {
            labels.push(label);
            terminal = payload.map_or(Self::Unit, |payload| *payload);
        }
        let mut value = match terminal {
            Self::Unit => ValClos::UNIT,
            Self::Bool { value } => ValClos::bool(value),
            Self::Int { value } => ValClos::int(value),
            Self::Float { value } => ValClos::float(value),
            Self::String { value } => ValClos::string(value),
            Self::Bytes { value } => ValClos::bytes(value),
            Self::List { items } => {
                let mut values = Vec::with_capacity(items.len());
                let mut remaining = items.into_iter();
                while let Some(value) = remaining.next() {
                    match value.unwalk(ext) {
                        Ok(value) => values.push(value),
                        Err(error) => {
                            remaining.for_each(drop_value);
                            return Err(error);
                        }
                    }
                }
                ValClos::list(values)
            }
            Self::Map { entries } => {
                let mut values = Vec::with_capacity(entries.len());
                let mut remaining = entries.into_iter();
                while let Some((name, value)) = remaining.next() {
                    match value.unwalk(ext) {
                        Ok(value) => values.push((name, value)),
                        Err(error) => {
                            for (_, value) in remaining {
                                drop_value(value);
                            }
                            return Err(error);
                        }
                    }
                }
                ValClos::map(values)
            }
            Self::Variant { .. } => unreachable!("the tag spine was peeled above"),
            Self::Ext(x) => ext(x)?,
        };
        for label in labels.into_iter().rev() {
            value = ValClos::variant(label, Some(value));
        }
        Ok(value)
    }
}
impl TryFrom<&ValClos> for FOValue {
    type Error = NotData;
    fn try_from(v: &ValClos) -> Result<Self, NotData> {
        Self::walk(v, &mut |l| Err(l.kind())).map_err(|leaf| NotData {
            leaf,
            nested: v.opacity().is_none(),
        })
    }
}
impl FOValue {
    pub(crate) fn scrubbed(v: &ValClos) -> Self {
        let Ok(fo) = Self::walk(v, &mut |l| Ok::<_, Infallible>(l.kind().placeholder()));
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
        let Ok(v) = fo.unwalk(&mut |x: NoExt| -> Result<Self, Infallible> { match x {} });
        v
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
