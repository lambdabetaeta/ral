//! Kinds: the closed set of predicates a type variable may carry.
//!
//! A kind is a set of admissible heads and a *deep* bit.  Five are named and
//! user-facing (`number`, `comparable`, `scalar`, `sized`, `data`); meets of
//! those are the rest.  Binding a kinded variable checks the head, and a deep
//! kind imposes `data` on every component (see `typecheck::unify`).

use super::Ty;
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
        Some(match ty {
            Ty::Var(_) => return None,
            Ty::Unit => Self::Unit,
            Ty::Bool => Self::Bool,
            Ty::Int => Self::Int,
            Ty::Float => Self::Float,
            Ty::String => Self::String,
            Ty::Bytes => Self::Bytes,
            Ty::List(_) => Self::List,
            Ty::Map(_) => Self::Map,
            Ty::Record(_) => Self::Record,
            Ty::Variant(_) => Self::Variant,
            Ty::Thunk(_) => Self::Thunk,
            Ty::Handle(_) => Self::Handle,
        })
    }

    const fn bit(self) -> u16 {
        1 << self as u16
    }

    /// The type a head with no components is.
    fn nullary(self) -> Option<Ty> {
        Some(match self {
            Self::Unit => Ty::Unit,
            Self::Bool => Ty::Bool,
            Self::Int => Ty::Int,
            Self::Float => Ty::Float,
            Self::String => Ty::String,
            Self::Bytes => Ty::Bytes,
            Self::List | Self::Map | Self::Record | Self::Variant | Self::Thunk | Self::Handle => {
                return None;
            }
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::Unit => "Unit",
            Self::Bool => "Bool",
            Self::Int => "Int",
            Self::Float => "Float",
            Self::String => "String",
            Self::Bytes => "Bytes",
            Self::List => "List",
            Self::Map => "Map",
            Self::Record => "Record",
            Self::Variant => "Variant",
            Self::Thunk => "Block",
            Self::Handle => "Handle",
        }
    }
}

const fn heads(of: &[Head]) -> u16 {
    let (mut set, mut i) = (0, 0);
    while i < of.len() {
        set |= of[i].bit();
        i += 1;
    }
    set
}

/// The heads that have components, which is where a deep bit means anything.
const STRUCTURAL: u16 = heads(&[Head::List, Head::Map, Head::Record, Head::Variant]);

/// Admissible heads, and whether every component must be `data` too.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct Kind {
    heads: u16,
    deep: bool,
}

impl Kind {
    const fn new(heads: u16, deep: bool) -> Self {
        Self {
            heads,
            deep: deep && heads & STRUCTURAL != 0,
        }
    }

    /// Every head, shallow: the kind of a variable nothing constrains.
    pub const ANY: Self = Self::new(heads(&Head::ALL), false);
    /// `+ - * /`, unary `-`.
    pub const NUMBER: Self = Self::new(heads(&[Head::Int, Head::Float]), false);
    /// `< > <= >=`, `lt`, `gt`, `sort-list`, `int`, `float`.
    pub const COMPARABLE: Self = Self::new(heads(&[Head::Int, Head::Float, Head::String]), false);
    /// Each interpolation part.
    pub const SCALAR: Self = Self::new(
        heads(&[Head::Bool, Head::Int, Head::Float, Head::String]),
        false,
    );
    /// `length`, `is-empty`.
    pub const SIZED: Self = Self::new(
        heads(&[Head::String, Head::Bytes, Head::List, Head::Map]),
        false,
    );
    /// Everything but a block or a handle, all the way down: `==`, `str`, the
    /// encoders, and the host doors that serialise their argument.
    pub const DATA: Self = Self::new(
        heads(&[
            Head::Unit,
            Head::Bool,
            Head::Int,
            Head::Float,
            Head::String,
            Head::Bytes,
            Head::List,
            Head::Map,
            Head::Record,
            Head::Variant,
        ]),
        true,
    );
    /// What a bare-label read needs of its target.
    pub(crate) const LABELLED: Self = Self::new(heads(&[Head::Record, Head::Map]), false);
    /// What a computed-key read needs of its target.
    pub(crate) const INDEXED: Self = Self::new(heads(&[Head::List, Head::Map]), false);
    /// What a computed-key read needs of its key.
    pub(crate) const KEY: Self = Self::new(heads(&[Head::Int, Head::String]), false);

    const NAMED: [(&'static str, Self); 5] = [
        ("number", Self::NUMBER),
        ("comparable", Self::COMPARABLE),
        ("scalar", Self::SCALAR),
        ("sized", Self::SIZED),
        ("data", Self::DATA),
    ];

    pub fn is_any(self) -> bool {
        self == Self::ANY
    }

    pub(crate) fn is_deep(self) -> bool {
        self.deep
    }

    pub(crate) fn admits(self, head: Head) -> bool {
        self.heads & head.bit() != 0
    }

    /// The kind of a variable united with one of each: the heads both admit,
    /// deep if either is.  `None` when nothing is both.
    pub(crate) fn meet(self, other: Self) -> Option<Self> {
        let heads = self.heads & other.heads;
        (heads != 0).then(|| Self::new(heads, self.deep || other.deep))
    }

    /// The one type a meet with a single nullary head leaves: a variable of
    /// this kind *is* that type.
    pub(crate) fn pin(self) -> Option<Ty> {
        Head::ALL
            .into_iter()
            .find(|h| self.heads == h.bit())
            .and_then(Head::nullary)
    }
}

impl fmt::Display for Kind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let named = Self::NAMED.iter().find(|(_, k)| k.heads == self.heads);
        if let Some((name, _)) = named {
            f.write_str(name)?;
        } else {
            let heads: Vec<_> = Head::ALL
                .into_iter()
                .filter(|h| self.admits(*h))
                .map(Head::name)
                .collect();
            write!(f, "{{{}}}", heads.join("|"))?;
        }
        let implied = named.is_some_and(|(_, k)| k.deep);
        if self.deep && !implied {
            f.write_str("^d")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_meet_is_the_intersection_of_heads_with_the_deep_bits_or_ed() {
        assert_eq!(
            Kind::NUMBER.meet(Kind::COMPARABLE),
            Some(Kind::NUMBER),
            "number is comparable"
        );
        assert_eq!(Kind::DATA.meet(Kind::SCALAR), Some(Kind::SCALAR));
        let deep_sized = Kind::DATA.meet(Kind::SIZED).expect("sized data exists");
        assert!(
            deep_sized.is_deep() && deep_sized.admits(Head::List) && !deep_sized.admits(Head::Int)
        );
        assert_eq!(Kind::ANY.meet(Kind::SIZED), Some(Kind::SIZED));
    }

    #[test]
    fn a_meet_with_nothing_in_common_is_empty() {
        assert_eq!(Kind::NUMBER.meet(Kind::SIZED), None);
    }

    #[test]
    fn a_single_nullary_head_pins_the_variable() {
        let text = Kind::SIZED.meet(Kind::COMPARABLE).expect("both admit text");
        assert_eq!(text.pin(), Some(Ty::String));
        assert_eq!(Kind::SIZED.pin(), None);
        assert_eq!(Kind::LABELLED.meet(Kind::SIZED).and_then(Kind::pin), None);
    }

    #[test]
    fn a_kind_prints_by_name() {
        assert_eq!(Kind::NUMBER.to_string(), "number");
        assert_eq!(Kind::DATA.to_string(), "data");
        let deep_sized = Kind::DATA.meet(Kind::SIZED).expect("sized data exists");
        assert_eq!(deep_sized.to_string(), "sized^d");
        assert_eq!(Kind::LABELLED.to_string(), "{Map|Record}");
    }

    #[test]
    fn a_kind_round_trips_through_postcard() {
        for kind in [Kind::ANY, Kind::NUMBER, Kind::DATA, Kind::SIZED] {
            let bytes = postcard::to_allocvec(&kind).expect("a kind serialises");
            let back: Kind = postcard::from_bytes(&bytes).expect("and reads back");
            assert_eq!(back, kind);
        }
    }
}
