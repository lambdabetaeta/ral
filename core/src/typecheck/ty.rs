//! The type language of the Hindley-Milner checker.  Data only: unification
//! lives in `unify`, inference in `infer`, rendering in `fmt`.
//!
//! The discipline is call-by-push-value — `Ty` classifies data at rest, `CompTy`
//! effectful processes, and the two meet at `Thunk` (CBPV's `U`) and `Return`
//! (`F`).  The payload route is [`super::route`]'s, re-exported here so that
//! `typecheck`'s surface carries it.

pub(in crate::typecheck) use super::route::GroundRoute;
pub use super::route::{PayloadRoute, PayloadVar};

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
    /// Tagged sum, dual to `Record` and over the same `Row`.  Its labels carry
    /// the leading `` ` `` that `syntax::tag` stamps on a tag, a `Record`'s do
    /// not, and `Unifier::unify_row` refuses to unify across the two alphabets.
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

/// A finite sequence of labelled types, closed by `Empty` or left open by a
/// tail variable.
///
/// `Unifier::unify_row` follows the Rémy (1989) rewrite: two `Extend` nodes
/// with different labels are swapped past each other into a shared fresh tail.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Row {
    Empty,
    Extend(String, Box<Ty>, Box<Self>),
    Var(RowVar),
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
    pub fn bytes() -> Self {
        Self::Return(PayloadRoute::Bytes, Box::new(Ty::Unit))
    }
}
