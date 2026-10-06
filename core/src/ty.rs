//! The type language of the Hindley-Milner checker.  Data only: unification
//! lives in `unify`, inference in `infer`, rendering in `fmt`.
//!
//! The discipline is call-by-push-value — `Ty` classifies data at rest, `CompTy`
//! effectful processes, and the two meet at `Thunk` (CBPV's `U`) and `Return`
//! (`F`).

mod exec_arg;
mod fmt;
mod kind;
mod scheme;
mod site;
mod typed;

pub(crate) use exec_arg::RefusedArg;
pub use fmt::{FmtCtx, fmt_comp_ty_ctx, fmt_ty_ctx};
pub(crate) use kind::Head;
pub use kind::Kind;
pub use scheme::Scheme;
pub(crate) use scheme::{CachedFreeVars, WeakVars};
pub(crate) use site::{Fields, Fixed, GradeShape, Id, Node, Shape, Side, Tail};
pub use site::{Fixings, Site};
pub use typed::{
    Typed, closed_record, closed_variant, field_ty, open_record, open_variant, optional_ty,
    record_row, variant_row,
};

/// The sigil that marks a tag: bare on the label itself, written where a
/// variant or a tag label is shown.
pub const TAG_PREFIX: char = '`';

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
    /// This record type with one more field.
    ///
    /// # Panics
    /// On a type that is not a record.
    #[must_use]
    pub fn extend(self, field: &str, ty: Self) -> Self {
        let Self::Record(row) = self else {
            panic!("extend: not a record type");
        };
        Self::Record(Row::Extend(
            Label::Field(field.into()),
            Box::new(ty),
            Box::new(row),
        ))
    }

    /// An argv: `List String`, and the one place that type is written down.
    /// Every argv boundary — a handler arm, a base frame, an external — takes
    /// this and nothing else, because every element crosses it rendered.
    pub fn argv() -> Self {
        Self::list(Self::String)
    }

    pub fn list(elem: Self) -> Self {
        Self::List(Box::new(elem))
    }

    pub fn map(elem: Self) -> Self {
        Self::Map(Box::new(elem))
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

impl Row {
    /// The spine, outermost link first.
    fn spine(&self) -> impl Iterator<Item = &Self> {
        std::iter::successors(Some(self), |row| match row {
            Self::Extend(_, _, rest) => Some(&**rest),
            _ => None,
        })
    }

    /// The spine's fields, each label once at its head payload: where
    /// selection and `unify_row` stop looking.
    pub fn fields(&self) -> impl Iterator<Item = (&Label, &Ty)> {
        let mut seen = std::collections::HashSet::new();
        self.spine()
            .map_while(|row| match row {
                Self::Extend(label, ty, _) => Some((label, &**ty)),
                _ => None,
            })
            .filter(move |(label, _)| seen.insert(*label))
    }

    /// What closes the spine: `Empty`, or the variable that leaves it open.
    pub fn tail(&self) -> &Self {
        self.spine().last().unwrap_or(self)
    }

    /// Whether a label off the spine may yet turn out to be there.
    pub fn is_open(&self) -> bool {
        matches!(self.tail(), Self::Var(_))
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

/// A node of the type language: the one enumeration every walk over types
/// shares, so none repeats the match on which constructor holds what.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Term<'a> {
    Ty(&'a Ty),
    Row(&'a Row),
    Comp(&'a CompTy),
}

impl<'a> Term<'a> {
    /// The immediate sub-terms, in the order the types print.
    pub(crate) fn children(self) -> impl Iterator<Item = Term<'a>> {
        let (first, second) = match self {
            Self::Ty(Ty::List(a) | Ty::Map(a) | Ty::Handle(a))
            | Self::Comp(CompTy::Return(_, a)) => (Some(Self::Ty(a)), None),
            Self::Ty(Ty::Record(r) | Ty::Variant(r)) => (Some(Self::Row(r)), None),
            Self::Ty(Ty::Thunk(b)) => (Some(Self::Comp(b)), None),
            Self::Comp(CompTy::Fun(a, b)) => (Some(Self::Ty(a)), Some(Self::Comp(b))),
            Self::Row(Row::Extend(_, ty, rest)) => (Some(Self::Ty(ty)), Some(Self::Row(rest))),
            // Enumerated, not `_`: a new constructor carrying a sub-term must
            // fail the build here rather than be walked past.
            Self::Ty(
                Ty::Unit | Ty::Bytes | Ty::Bool | Ty::Int | Ty::Float | Ty::String | Ty::Var(_),
            )
            | Self::Comp(CompTy::Var(_))
            | Self::Row(Row::Empty | Row::Var(_)) => (None, None),
        };
        first.into_iter().chain(second)
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
