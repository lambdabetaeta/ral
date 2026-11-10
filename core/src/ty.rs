//! The type language of the Hindley-Milner checker.  Data only: unification
//! lives in `unify`, inference in `infer`, rendering in `fmt`.
//!
//! The discipline is call-by-push-value — `Ty` classifies data at rest, `CompTy`
//! effectful processes, and the two meet at `Thunk` (CBPV's `U`) and `Return`
//! (`F`).

mod exec_arg;
mod fmt;
mod intern;
mod kind;
mod scheme;
mod site;
mod typed;

use intern::{Internable, Interned, Table};
use std::sync::{LazyLock, Mutex};

pub use exec_arg::RefusedArg;
pub use fmt::{FmtCtx, fmt_comp_ty_ctx, fmt_ty_ctx};
pub(crate) use kind::Head;
pub use kind::Kind;
pub(crate) use scheme::Role;
#[cfg(any(test, feature = "test-util"))]
pub(crate) use scheme::collect_vars;
pub use scheme::{Binders, Closed, Scheme};
pub(crate) use site::{Fields, Fixed, GradeShape, Id, Node, Shape, Side, Tail, UnitId};
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

/// A variable of any of the four sorts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Var {
    Ty(TyVar),
    Comp(CompTyVar),
    Row(RowVar),
    Grade(GradeVar),
}

/// Declares a handle: an interned node of one sort, with the three accessors
/// every sort has.  `Debug` shows the node, not the handle around it.
macro_rules! handle {
    ($(#[$doc:meta])* $handle:ident over $node:ident) => {
        $(#[$doc])*
        #[derive(Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
        #[serde(transparent)]
        pub struct $handle(Interned<$node>);

        impl $handle {
            /// The handle for `node`: the one allocation every equal node shares.
            pub fn new(node: $node) -> Self {
                Self(Interned::new(node))
            }

            /// The one layer of structure this handle stands for.
            pub fn node(&self) -> &$node {
                self.0.node()
            }

            /// Whether no variable of any sort occurs in it, grade variables
            /// included.
            pub fn is_ground(&self) -> bool {
                self.0.is_ground()
            }
        }

        impl Internable for $node {
            fn ground(&self) -> bool {
                Layer::ground(self)
            }

            fn table() -> &'static Mutex<Table<Self>> {
                static TABLE: LazyLock<Mutex<Table<$node>>> =
                    LazyLock::new(|| Mutex::new(Table::new()));
                &TABLE
            }
        }

        impl std::fmt::Debug for $handle {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                std::fmt::Debug::fmt(self.node(), f)
            }
        }
    };
}

handle! {
    /// A value type (`A` in CBPV), interned.
    ///
    /// Equal types are one allocation, so `==` is a pointer comparison, `clone`
    /// a reference count, and `Hash` the one computed when the type was built.
    /// Every type is made by [`Self::new`] or a constructor over it, so none
    /// exists that the interner has not seen.
    Ty over TyNode
}

handle! {
    /// A row, interned like [`Ty`].
    Row over RowNode
}

handle! {
    /// A computation type (`B` in CBPV), interned like [`Ty`].
    CompTy over CompTyNode
}

/// One layer of a value type, whose children are handles.  `Var` is a
/// unification variable, gone by the end of inference.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum TyNode {
    Unit,
    Bytes,
    Bool,
    Int,
    Float,
    String,
    List(Ty),
    Map(Ty), // String-keyed
    Record(Row),
    /// Tagged sum, dual to `Record` and over the same `Row`.  Which alphabet a
    /// row's labels are drawn from is [`Label`]'s to say, and `unify_row`
    /// refuses a row that mixes the two.
    Variant(Row),
    Thunk(CompTy),
    /// A suspended producer of grade `ε` and value `α`: `await` of a
    /// `Handle^ε α` is an `F^ε α`.
    Handle(Grade, Ty),
    Var(TyVar),
}

/// The six types with no components, built once: asking for one never takes
/// the interner's lock.
macro_rules! leaves {
    ($($name:ident => $variant:ident),* $(,)?) => {
        $(pub fn $name() -> Self {
            static LEAF: LazyLock<Ty> = LazyLock::new(|| Ty::new(TyNode::$variant));
            LEAF.clone()
        })*
    };
}

impl Ty {
    leaves! {
        unit => Unit,
        bytes => Bytes,
        bool => Bool,
        int => Int,
        float => Float,
        string => String,
    }

    pub fn list(elem: Self) -> Self {
        Self::new(TyNode::List(elem))
    }

    pub fn map(elem: Self) -> Self {
        Self::new(TyNode::Map(elem))
    }

    pub fn record(row: Row) -> Self {
        Self::new(TyNode::Record(row))
    }

    pub fn variant(row: Row) -> Self {
        Self::new(TyNode::Variant(row))
    }

    pub fn thunk(body: CompTy) -> Self {
        Self::new(TyNode::Thunk(body))
    }

    pub fn handle(grade: Grade, ty: Self) -> Self {
        Self::new(TyNode::Handle(grade, ty))
    }

    pub fn var(v: TyVar) -> Self {
        Self::new(TyNode::Var(v))
    }

    /// This record type with one more field.
    ///
    /// # Panics
    /// On a type that is not a record.
    #[must_use]
    pub fn extend(&self, field: &str, ty: Self) -> Self {
        let TyNode::Record(row) = self.node() else {
            panic!("extend: not a record type");
        };
        Self::record(Row::extend(Label::Field(field.into()), ty, row.clone()))
    }

    /// An argv: `List String`, and the one place that type is written down.
    /// Every argv boundary — a handler arm, a base frame, an external — takes
    /// this and nothing else, because every element crosses it rendered.
    pub fn argv() -> Self {
        Self::list(Self::string())
    }
}

/// One layer of a row: a finite sequence of labelled fields, closed by `Empty`
/// or left open by a tail variable.
///
/// `Unifier::unify_row` follows the Rémy (1989) rewrite: two `Extend` nodes
/// with different labels are swapped past each other into a shared fresh tail.
///
/// `Empty` says *every label not on the spine is absent*.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum RowNode {
    Empty,
    Extend(Label, Ty, Row),
    Var(RowVar),
}

impl Row {
    pub fn empty() -> Self {
        static EMPTY: LazyLock<Row> = LazyLock::new(|| Row::new(RowNode::Empty));
        EMPTY.clone()
    }

    pub fn extend(label: Label, ty: Ty, rest: Self) -> Self {
        Self::new(RowNode::Extend(label, ty, rest))
    }

    pub fn var(v: RowVar) -> Self {
        Self::new(RowNode::Var(v))
    }

    /// The spine, outermost link first.
    fn spine(&self) -> impl Iterator<Item = &Self> {
        std::iter::successors(Some(self), |row| match row.node() {
            RowNode::Extend(_, _, rest) => Some(rest),
            _ => None,
        })
    }

    /// The spine's fields, each label once at its head payload: where
    /// selection and `unify_row` stop looking.
    pub fn fields(&self) -> impl Iterator<Item = (&Label, &Ty)> {
        let mut seen = std::collections::HashSet::new();
        self.spine()
            .map_while(|row| match row.node() {
                RowNode::Extend(label, ty, _) => Some((label, ty)),
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
        matches!(self.tail().node(), RowNode::Var(_))
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

/// One layer of a computation type, whose children are handles.
#[derive(Debug, Clone, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum CompTyNode {
    /// `F A` under a grade: `F^p A` returns a value `A`; `F^w Unit` is a
    /// command, whose value is its output.
    Return(Grade, Ty),
    /// `A -> B`.
    Fun(Ty, CompTy),
    /// Unification variable.
    Var(CompTyVar),
}

impl CompTy {
    pub fn ret(grade: Grade, ty: Ty) -> Self {
        Self::new(CompTyNode::Return(grade, ty))
    }

    pub fn fun(param: Ty, body: Self) -> Self {
        Self::new(CompTyNode::Fun(param, body))
    }

    pub fn var(v: CompTyVar) -> Self {
        Self::new(CompTyNode::Var(v))
    }

    /// A computation returning `ty`.
    pub fn pure(ty: Ty) -> Self {
        Self::ret(Grade::Value, ty)
    }

    /// A command: it produces output and nothing else.
    pub fn command() -> Self {
        Self::ret(Grade::Output, Ty::unit())
    }

    /// `Fun(p₁, Fun(p₂, …, tail))`.
    pub fn arrows(
        params: impl IntoIterator<Item = Ty, IntoIter: DoubleEndedIterator>,
        tail: Self,
    ) -> Self {
        params
            .into_iter()
            .rev()
            .fold(tail, |body, param| Self::fun(param, body))
    }

    /// Its arrows before the first non-arrow; a variable counts as none.
    pub(crate) fn arity(&self) -> usize {
        match self.node() {
            CompTyNode::Fun(_, body) => 1 + body.arity(),
            CompTyNode::Return(..) | CompTyNode::Var(_) => 0,
        }
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

/// What a node's `children` yields: at most two sub-terms.
type Children<'a> = std::iter::Flatten<std::array::IntoIter<Option<Term<'a>>, 2>>;

/// What every layer of the type language says of itself, so that [`Term`] and
/// groundness are stated once over all three sorts.
trait Layer {
    /// The immediate sub-terms, in the order the types print.  Every
    /// constructor is enumerated, never `_`: a new one carrying a sub-term must
    /// fail the build here rather than be walked past.
    fn children(&self) -> Children<'_>;

    /// The grade this layer carries: an `F`'s, or a handle's.
    fn grade(&self) -> Option<Grade>;

    /// Whether this layer is a unification variable.
    fn is_var(&self) -> bool;

    /// Whether no variable occurs in the layer, given that each child's flag is
    /// already cached.
    fn ground(&self) -> bool {
        !self.is_var()
            && !matches!(self.grade(), Some(Grade::Var(_)))
            && self.children().all(Term::is_ground)
    }
}

impl Layer for TyNode {
    fn children(&self) -> Children<'_> {
        let children = match self {
            Self::List(a) | Self::Map(a) | Self::Handle(_, a) => [Some(Term::Ty(a)), None],
            Self::Record(r) | Self::Variant(r) => [Some(Term::Row(r)), None],
            Self::Thunk(b) => [Some(Term::Comp(b)), None],
            Self::Unit
            | Self::Bytes
            | Self::Bool
            | Self::Int
            | Self::Float
            | Self::String
            | Self::Var(_) => [None, None],
        };
        children.into_iter().flatten()
    }

    fn grade(&self) -> Option<Grade> {
        match self {
            Self::Handle(g, _) => Some(*g),
            _ => None,
        }
    }

    fn is_var(&self) -> bool {
        matches!(self, Self::Var(_))
    }
}

impl Layer for RowNode {
    fn children(&self) -> Children<'_> {
        let children = match self {
            Self::Extend(_, ty, rest) => [Some(Term::Ty(ty)), Some(Term::Row(rest))],
            Self::Empty | Self::Var(_) => [None, None],
        };
        children.into_iter().flatten()
    }

    fn grade(&self) -> Option<Grade> {
        None
    }

    fn is_var(&self) -> bool {
        matches!(self, Self::Var(_))
    }
}

impl Layer for CompTyNode {
    fn children(&self) -> Children<'_> {
        let children = match self {
            Self::Return(_, a) => [Some(Term::Ty(a)), None],
            Self::Fun(a, b) => [Some(Term::Ty(a)), Some(Term::Comp(b))],
            Self::Var(_) => [None, None],
        };
        children.into_iter().flatten()
    }

    fn grade(&self) -> Option<Grade> {
        match self {
            Self::Return(g, _) => Some(*g),
            _ => None,
        }
    }

    fn is_var(&self) -> bool {
        matches!(self, Self::Var(_))
    }
}

impl<'a> Term<'a> {
    /// The immediate sub-terms, in the order the types print.
    pub(crate) fn children(self) -> Children<'a> {
        match self {
            Self::Ty(ty) => ty.node().children(),
            Self::Row(row) => row.node().children(),
            Self::Comp(cty) => cty.node().children(),
        }
    }

    /// The grade this node carries: an `F`'s, or a handle's.
    pub(crate) fn grade(self) -> Option<Grade> {
        match self {
            Self::Ty(ty) => ty.node().grade(),
            Self::Row(row) => row.node().grade(),
            Self::Comp(cty) => cty.node().grade(),
        }
    }

    /// Whether the node contains no variable of any sort.
    pub(crate) fn is_ground(self) -> bool {
        match self {
            Self::Ty(ty) => ty.is_ground(),
            Self::Row(row) => row.is_ground(),
            Self::Comp(cty) => cty.is_ground(),
        }
    }
}

/// A `Return` taken apart: the grade, and the value past it.
#[derive(Debug, Clone)]
pub(crate) struct Producer {
    pub(crate) grade: Grade,
    pub(crate) ty: Ty,
}

impl From<Producer> for CompTy {
    fn from(Producer { grade, ty }: Producer) -> Self {
        Self::ret(grade, ty)
    }
}
