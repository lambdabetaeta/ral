//! The type language of the Hindley-Milner checker.  Data only: unification
//! lives in `unify`, inference in `infer`, rendering in `fmt`.
//!
//! The discipline is call-by-push-value — `Ty` classifies data at rest, `CompTy`
//! effectful processes, and the two meet at `Thunk` (CBPV's `U`) and `Return`
//! (`F`).  The pipeline-mode lattice is [`crate::mode`]'s, re-exported here so
//! that `typecheck`'s surface carries it.

pub use crate::mode::{ModeVar, PipeMode, PipeSpec};

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
    /// `F[I,O] A` — an effectful command with pipeline modes, returning `A`.
    Return(PipeSpec, Box<Ty>),
    /// `A -> B`.
    Fun(Box<Ty>, Box<Self>),
    /// Unification variable.
    Var(CompTyVar),
}

impl CompTy {
    /// Pure computation: no pipeline I/O.
    pub fn pure(ty: Ty) -> Self {
        Self::Return(PipeSpec::none(), Box::new(ty))
    }
}
