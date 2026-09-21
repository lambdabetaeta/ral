//! The type language of the Hindley-Milner checker.  Data only: unification
//! lives in `unify`, inference in `infer`, rendering in `fmt`.
//!
//! The discipline is call-by-push-value — `Ty` classifies data at rest, `CompTy`
//! effectful processes, and the two meet at `Thunk` (CBPV's `U`) and `Return`
//! (`F`).  The payload route is [`super::route`]'s, re-exported here so that
//! `typecheck`'s surface carries it.

pub(in crate::typecheck) use super::route::GroundRoute;
pub use super::route::{PayloadRoute, PayloadVar};

use crate::syntax::tag::TAG_PREFIX;

/// Unification variable for value types.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct TyVar(pub u32);

/// Unification variable for row tails, in records and in variants alike.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct RowVar(pub u32);

/// Unification variable for a field's presence flag.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct PresenceVar(pub u32);

/// Whether a presence variable turned out to name a field that is there.
/// Two constants: a variable standing for another variable is the store's
/// business, not this type's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Presence {
    Present,
    Absent,
}

/// Unification variable for computation types.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct CompTyVar(pub u32);

/// Value types (`A` in CBPV).  `Var` is a unification variable, gone by the
/// end of inference.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Ty {
    Unit,
    Bytes,
    Bool,
    Int,
    Float,
    String,
    List(Box<Self>),
    Map(Box<Self>), // String-keyed
    Record(Row),
    /// Tagged sum, dual to `Record` and over the same `Row`.  Which alphabet a
    /// row's labels are drawn from is [`Label`]'s to say, and `unify_row`
    /// refuses a row that mixes the two.
    Variant(Row),
    Thunk(Box<CompTy>),
    /// A running concurrent block; `await` of a `Handle α` gives a record with
    /// a `value: α` field.
    Handle(Box<Self>),
    Var(TyVar),
}

impl Ty {
    /// An argv: `List String`, and the one place that type is written down.
    /// Every argv boundary — a handler arm, a base frame, an external — takes
    /// this and nothing else, because every element crosses it rendered.
    pub fn argv() -> Self {
        Self::List(Box::new(Self::String))
    }
}

/// A finite sequence of labelled fields, closed by `Empty` or left open by a
/// tail variable.
///
/// `Unifier::unify_row` follows the Rémy (1989) rewrite: two `Extend` nodes
/// with different labels are swapped past each other into a shared fresh tail.
///
/// `Empty` says *every label not on the spine is absent*, full stop — the same
/// sentence [`Field::Absent`] makes about one label.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Row {
    Empty,
    Extend(Label, Field, Box<Self>),
    Var(RowVar),
}

/// A slot on a row: whether the record has this label, and what it holds.
///
/// `Absent` stores nothing because nothing can read it: no rule looks under an
/// absent flag, so there is no payload beside an absent field to read, key,
/// quantify or occurs-check.  A field whose presence is still unknown does
/// carry its type: `Var(θ, τ)` reads "a `τ`, if it is there".
///
/// The payload is boxed because `Ty → Row → Field → Ty` has no indirection
/// anywhere else on the cycle.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Field {
    Present(Box<Ty>),
    Absent,
    Var(PresenceVar, Box<Ty>),
}

impl Field {
    /// The one field every term rule builds: one the program wrote down.
    pub fn present(ty: Ty) -> Self {
        Self::Present(Box::new(ty))
    }

    /// What the field holds, or `None` for an absent one, which holds nothing.
    pub fn payload(&self) -> Option<&Ty> {
        match self {
            Self::Present(t) | Self::Var(_, t) => Some(t),
            Self::Absent => None,
        }
    }
}

/// A row label together with the alphabet it is drawn from.
///
/// A record's field names and a variant's constructors are different labels
/// however they are spelled, so no spelling of one can pass for the other.
#[derive(
    Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Label {
    Field(String),
    Case(String),
}

impl Label {
    /// The label as written, without the backtick a tag is printed with.
    pub fn name(&self) -> &str {
        match self {
            Self::Field(s) | Self::Case(s) => s,
        }
    }
}

impl std::fmt::Display for Label {
    /// How a label reads back to the user: a tag wears its sigil.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Field(s) => write!(f, "{s}"),
            Self::Case(s) => write!(f, "{TAG_PREFIX}{s}"),
        }
    }
}

/// Computation types (`B` in CBPV).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CompTy {
    /// `F[ρ] A` — an effectful command returning `A`, whose payload a value
    /// boundary reads by the route `ρ`.
    Return(PayloadRoute, Box<Ty>),
    /// `A -> B`.
    Fun(Box<Ty>, Box<Self>),
    /// Unification variable.
    Var(CompTyVar),
}

impl CompTy {
    /// A computation whose payload is its returned value.
    pub fn pure(ty: Ty) -> Self {
        Self::Return(PayloadRoute::Value, Box::new(ty))
    }

    /// The one byte-routed computation WF-2 admits: captured from stdout,
    /// returning `Unit`.  Landing on the byte side of any decision means
    /// unifying with this whole, so the `Bytes`/`Unit` pairing travels
    /// structurally and no grounding site carries half of it from memory.
    pub(crate) fn bytes() -> Self {
        Self::Return(PayloadRoute::Bytes, Box::new(Ty::Unit))
    }
}
