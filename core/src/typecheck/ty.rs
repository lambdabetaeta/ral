//! The type language of the Hindley-Milner checker.  Data only: unification
//! lives in `unify`, inference in `infer`, rendering in `fmt`.
//!
//! The discipline is call-by-push-value — `Ty` classifies data at rest, `CompTy`
//! effectful processes, and the two meet at `Thunk` (CBPV's `U`) and `Return`
//! (`F`).

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
/// `Empty` says *every label not on the spine is absent*.  The field type is
/// boxed because `Ty → Row → Ty` has no indirection anywhere else on the cycle.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Row {
    Empty,
    Extend(Label, Box<Ty>, Box<Self>),
    Var(RowVar),
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

/// Unification variable for grades.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub struct GradeVar(pub u32);

/// What an `F` produces: a value, or its output.  Atomic — a variable binds
/// only to `Value` or `Output` — so grades need no occurs check and no kind.
///
/// Invariant, kept by the rules rather than the representation: `Output` is
/// only ever paired with `Unit`.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Grade {
    Value,
    Output,
    Var(GradeVar),
}

/// Computation types (`B` in CBPV).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum CompTy {
    /// `F A` under a grade: `F^p A` returns a value `A`; `F^w Unit` is a
    /// command, whose value is its output.
    Return(Grade, Box<Ty>),
    /// `A -> B`.
    Fun(Box<Ty>, Box<Self>),
    /// Unification variable.
    Var(CompTyVar),
}

impl CompTy {
    /// A computation returning `ty`.
    pub fn pure(ty: Ty) -> Self {
        Self::Return(Grade::Value, Box::new(ty))
    }

    /// A command: it produces output and nothing else.
    pub fn command() -> Self {
        Self::Return(Grade::Output, Box::new(Ty::Unit))
    }

    /// `Fun(p₁, Fun(p₂, …, tail))`.
    pub fn arrows(
        params: impl IntoIterator<Item = Ty, IntoIter: DoubleEndedIterator>,
        tail: Self,
    ) -> Self {
        params.into_iter().rev().fold(tail, |body, param| {
            Self::Fun(Box::new(param), Box::new(body))
        })
    }
}

/// A computation type's curry spine: its parameters, and what is past them.
#[derive(Debug, Clone)]
pub(crate) struct Spine {
    pub(crate) params: Vec<Ty>,
    pub(crate) tail: CompTy,
}

impl Spine {
    pub(crate) fn arity(&self) -> usize {
        self.params.len()
    }

    /// What is produced once every parameter is supplied, if that is settled.
    pub(crate) fn producer(&self) -> Option<Producer> {
        Producer::of(&self.tail)
    }

    /// The same parameters over another tail.
    pub(crate) fn over(self, tail: CompTy) -> CompTy {
        CompTy::arrows(self.params, tail)
    }
}

/// A `Return` taken apart: the grade, and the value past it.
#[derive(Debug, Clone)]
pub(crate) struct Producer {
    pub(crate) grade: Grade,
    pub(crate) ty: Ty,
}

impl Producer {
    pub(crate) fn of(cty: &CompTy) -> Option<Self> {
        match cty {
            CompTy::Return(grade, ty) => Some(Self {
                grade: *grade,
                ty: (**ty).clone(),
            }),
            _ => None,
        }
    }
}

impl From<Producer> for CompTy {
    fn from(Producer { grade, ty }: Producer) -> Self {
        Self::Return(grade, Box::new(ty))
    }
}
