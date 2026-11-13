//! Kinds: the chain of predicates a type variable may carry.
//!
//! number ⊂ comparable ⊂ scalar ⊂ data ⊂ any. Each head has a least kind and
//! a kind admits the heads at or below it, so the admitted sets nest and two
//! kinds meet at the smaller. Binding a kinded variable checks the head, and
//! `data` imposes itself on every component (see `typecheck::unify`).

use super::{Ty, TyNode};
use std::fmt;

/// The outermost constructor of a non-variable value type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Head {
    Unit,
    Bool,
    Int,
    Float,
    String,
    Bytes,
    List,
    Map,
    Record,
    Variant,
    Thunk,
    Handle,
}

impl Head {
    const ALL: [Self; 12] = [
        Self::Unit,
        Self::Bool,
        Self::Int,
        Self::Float,
        Self::String,
        Self::Bytes,
        Self::List,
        Self::Map,
        Self::Record,
        Self::Variant,
        Self::Thunk,
        Self::Handle,
    ];

    /// `None` for a variable, which is not yet a head.
    pub(crate) fn of(ty: &Ty) -> Option<Self> {
        Some(match ty.node() {
            TyNode::Var(_) => return None,
            TyNode::Unit => Self::Unit,
            TyNode::Bool => Self::Bool,
            TyNode::Int => Self::Int,
            TyNode::Float => Self::Float,
            TyNode::String => Self::String,
            TyNode::Bytes => Self::Bytes,
            TyNode::List(_) => Self::List,
            TyNode::Map(_) => Self::Map,
            TyNode::Record(_) => Self::Record,
            TyNode::Variant(_) => Self::Variant,
            TyNode::Thunk(_) => Self::Thunk,
            TyNode::Handle(..) => Self::Handle,
        })
    }

    /// The least kind that admits this head.
    const fn least(self) -> Kind {
        match self {
            Self::Int | Self::Float => Kind::Number,
            Self::String => Kind::Comparable,
            Self::Bool => Kind::Scalar,
            Self::Unit | Self::Bytes | Self::List | Self::Map | Self::Record | Self::Variant => {
                Kind::Data
            }
            Self::Thunk | Self::Handle => Kind::Any,
        }
    }
}

impl From<Head> for crate::fact::DataHead {
    fn from(head: Head) -> Self {
        match head {
            Head::Unit => Self::Unit,
            Head::Bool => Self::Bool,
            Head::Int => Self::Int,
            Head::Float => Self::Float,
            Head::String => Self::String,
            Head::Bytes => Self::Bytes,
            Head::List => Self::List,
            Head::Map => Self::Map,
            Head::Record => Self::Record,
            Head::Variant => Self::Variant,
            Head::Thunk => Self::Thunk,
            Head::Handle => Self::Handle,
        }
    }
}

/// Ordered by inclusion of the heads admitted, least first.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
    strum::IntoStaticStr,
)]
#[cfg_attr(test, derive(strum::VariantArray))]
#[serde(rename_all = "kebab-case")]
#[strum(serialize_all = "kebab-case")]
pub enum Kind {
    /// `+ - * /`, unary `-`.
    Number,
    /// `< > <= >=`, `lt`, `gt`, `sort-list`, `int`, `float`.
    Comparable,
    /// Each interpolation part.
    Scalar,
    /// No block or handle at any depth: `==`, `str`, the encoders, and the
    /// host builtins that serialise their argument.
    Data,
    /// What nothing constrains.
    Any,
}

impl Kind {
    pub(crate) fn admits(self, head: Head) -> bool {
        head.least() <= self
    }

    /// Whether every component must be data too: only `data` both admits
    /// structure and constrains it.
    pub(crate) fn is_deep(self) -> bool {
        self == Self::Data
    }
}

impl From<Kind> for crate::fact::DataKind {
    fn from(kind: Kind) -> Self {
        Self {
            heads: Head::ALL
                .into_iter()
                .filter(|head| kind.admits(*head))
                .map(Into::into)
                .collect(),
            deep: kind.is_deep(),
        }
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use strum::VariantArray as _;

    /// Every kind admits at least two heads, so no meet pins a variable.
    #[test]
    fn every_kind_admits_at_least_two_heads() {
        for &kind in Kind::VARIANTS {
            assert!(
                Head::ALL
                    .into_iter()
                    .filter(|&head| kind.admits(head))
                    .count()
                    >= 2
            );
        }
    }

    #[test]
    fn the_four_named_kinds_keep_their_heads() {
        use Head::*;
        for (kind, heads) in [
            (Kind::Number, vec![Int, Float]),
            (Kind::Comparable, vec![Int, Float, String]),
            (Kind::Scalar, vec![Bool, Int, Float, String]),
            (
                Kind::Data,
                vec![
                    Unit, Bool, Int, Float, String, Bytes, List, Map, Record, Variant,
                ],
            ),
        ] {
            let admitted: Vec<_> = Head::ALL
                .into_iter()
                .filter(|&head| kind.admits(head))
                .collect();
            assert_eq!(admitted, heads, "{kind}");
        }
    }
}
