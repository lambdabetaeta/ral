//! Core type definitions for the Hindley-Milner type checker.
//!
//! This module is data-only: enums, structs, and simple constructors.
//! No unification, inference, or display logic lives here.
//!
//! Types follow the CBPV (call-by-push-value) discipline:
//!
//! - Value types (`Ty`) classify data at rest: booleans, strings, lists,
//!   row-polymorphic records, and suspended computations (thunks).
//! - Computation types (`CompTy`) classify effectful processes: a command
//!   that reads stdin, writes stdout, and produces a value; or a function
//!   from a value type to a computation type.
//! - Pipeline modes ([`PipeMode`]) classify the I/O channels connecting
//!   pipeline stages: none or raw bytes.  The mode lattice itself lives in
//!   [`crate::mode`]; this module re-exports it so the existing
//!   `typecheck::{PipeMode, PipeSpec, …}` paths keep resolving.

pub use crate::mode::{ModeVar, PipeMode, PipeSpec};

// ─────────────────────────────────────────────────────────────────────────────
// Type variables
// ─────────────────────────────────────────────────────────────────────────────

/// Unification variable for value types.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct TyVar(pub u32);

/// Unification variable for row types (record tails).
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct RowVar(pub u32);

/// Unification variable for computation types.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct CompTyVar(pub u32);

// ─────────────────────────────────────────────────────────────────────────────
// Value types  (A in CBPV)
// ─────────────────────────────────────────────────────────────────────────────

/// Value types (A in CBPV).
///
/// Ground types (`Unit`, `Bool`, `Int`, `Float`, `String`, `Bytes`) are leaves.
/// Compound types are `List`, `Map` (homogeneous, string-keyed), `Record`
/// (row-polymorphic), `Thunk` (suspended computation), and `Handle` (a running
/// concurrent block parameterized by the type its body returns — `await` of a
/// `Handle α` resolves to a record with a `value: α` field).  `Var` is a unification
/// variable, eliminated during inference.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Ty {
    Unit,
    Bytes,
    Bool,
    Int,
    Float,
    String,
    List(Box<Self>),
    Map(Box<Self>), // String-keyed; values are homogeneous
    Record(Row),  // row-typed record {l₁:A₁, …, lₙ:Aₙ | ρ}
    /// Tagged sum: a value carrying one of `{`l₁: A₁, …, `lₙ: Aₙ | ρ}`.
    /// Dual to `Record`; shares the `Row` data structure.  By convention
    /// the labels in a `Variant` row are *tag* labels (begin with `` ` ``),
    /// while a `Record` row's labels are *bare* (no leading backtick).  The
    /// two alphabets do not unify.
    Variant(Row),
    Thunk(Box<CompTy>), // U B — suspended computation
    Handle(Box<Self>),    // Handle α — await produces a record with `value: α`
    Var(TyVar),
}

/// A row type: a finite sequence of labeled types with an optional tail.
///
/// `Empty`          — closed row: no more fields permitted.
/// `Extend(l, A, ρ)` — field l has type A; ρ is the rest of the row.
/// `Var(ρ)`         — open tail: unknown remaining fields (unification variable).
///
/// Row unification uses the Rémy (1989) rewrite rule: two `Extend` nodes with
/// different labels are swapped past each other into a shared fresh tail variable.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Row {
    Empty,
    Extend(String, Box<Ty>, Box<Self>),
    Var(RowVar),
}

// ─────────────────────────────────────────────────────────────────────────────
// Computation types  (B in CBPV)
// ─────────────────────────────────────────────────────────────────────────────

/// Computation types (B in CBPV).
///
/// `Return(spec, A)` — `F[I,O] A`: an effectful command with pipeline
/// specification `spec` that produces a value of type `A`.
/// `Fun(A, B)` — `A -> B`: a function taking a value and yielding a
/// computation.  `Var` is a unification variable.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CompTy {
    /// `F[I,O] A` — effectful command with pipeline modes and a return type.
    Return(PipeSpec, Box<Ty>),
    /// `A -> B` — function from a value type to a computation type.
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
